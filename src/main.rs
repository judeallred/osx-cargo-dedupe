//! Deduplicates identical files across cargo target directories on macOS by
//! replacing duplicates with APFS copy-on-write clones.

#[cfg(not(target_os = "macos"))]
compile_error!("osx-cargo-dedupe only supports macOS (APFS clones)");

mod apfs;
mod dedupe;
mod scan;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use clap::Parser;
use indicatif::{HumanBytes, HumanCount, HumanDuration, ProgressBar, ProgressStyle};

use dedupe::{Outcome, Replacer, Skip};
use scan::{IndexOptions, Indexed};

/// Find identical files across cargo target directories and replace the
/// duplicates with APFS clones, so they share disk blocks.
///
/// Dry run by default; pass --apply to modify files.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Directories to search for cargo target directories [default: $HOME]
    paths: Vec<PathBuf>,

    /// Replace duplicates with clones (default is a dry run)
    #[arg(long)]
    apply: bool,

    /// Leave files alone if they changed within this many seconds
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    min_age: u64,

    /// Ignore files smaller than this many bytes
    #[arg(long, value_name = "BYTES", default_value_t = 16 * 1024)]
    min_size: u64,

    /// Also deduplicate files in `incremental/` directories
    #[arg(long)]
    include_incremental: bool,

    /// Directories to skip while searching (default: ~/Library)
    #[arg(long, value_name = "PATH")]
    exclude: Vec<PathBuf>,

    /// List every skipped file in the report
    #[arg(short, long)]
    verbose: bool,
}

fn spinner(template: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::with_template(template).expect("valid template"));
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn bar(len: u64, template: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::with_template(template)
            .expect("valid template")
            .progress_chars("=> "),
    );
    pb
}

#[derive(Default)]
struct Report {
    cargo_dirs: usize,
    in_use: Vec<(PathBuf, PathBuf)>,
    files_indexed: u64,
    recent: Vec<PathBuf>,
    locked: Vec<PathBuf>,
    protected_flag: Vec<PathBuf>,
    changed: Vec<PathBuf>,
    errors: Vec<(PathBuf, std::io::Error)>,
    hardlinked: u64,
    incremental: u64,
    small: u64,
    groups: usize,
    replaced: u64,
    bytes_deduped: u64,
    /// Per volume: a directory on it, its mount point, and the free space
    /// before and after.
    free: BTreeMap<u64, (PathBuf, PathBuf, u64, Option<u64>)>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let started = Instant::now();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let roots = if cli.paths.is_empty() {
        vec![home.clone().context("HOME is not set; pass a path")?]
    } else {
        cli.paths.clone()
    };
    for root in &roots {
        if !root.is_dir() {
            bail!("{} is not a directory", root.display());
        }
    }
    let excludes = if cli.exclude.is_empty() {
        home.map(|h| vec![h.join("Library")]).unwrap_or_default()
    } else {
        cli.exclude.clone()
    };

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || cancel.store(true, Ordering::Relaxed))
            .context("installing Ctrl-C handler")?;
    }

    eprintln!(
        "{} · files changed in the last {}s or locked are left alone",
        if cli.apply {
            "Deduplicating"
        } else {
            "Dry run (pass --apply to deduplicate)"
        },
        cli.min_age
    );

    // 1. Find cargo directories.
    let pb = spinner("{spinner} Searching {msg}");
    let found = scan::discover(&roots, &excludes, &pb, &cancel);
    pb.finish_and_clear();
    eprintln!(
        "Found {} cargo directories in {} directories",
        found.cargo_dirs.len(),
        HumanCount(found.dirs_visited)
    );

    // 2. Lock and index them.
    let mut report = Report {
        cargo_dirs: found.cargo_dirs.len(),
        ..Report::default()
    };
    let opts = IndexOptions {
        min_size: cli.min_size,
        min_age: Duration::from_secs(cli.min_age),
        include_incremental: cli.include_incremental,
    };
    let pb = bar(
        found.cargo_dirs.len() as u64,
        "Indexing  [{bar:30}] {pos}/{len} {wide_msg}",
    );
    let now = SystemTime::now();
    let mut files = Vec::new();
    let mut held_locks = Vec::new();
    for dir in &found.cargo_dirs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        pb.set_message(dir.display().to_string());
        match scan::index(dir, &opts, now) {
            Indexed::InUse { lock } => report.in_use.push((dir.clone(), lock)),
            Indexed::Ready {
                files: dir_files,
                skips,
                locks,
            } => {
                if let Some(first) = dir_files.first()
                    && !report.free.contains_key(&first.dev)
                    && let Some((mount, free)) = apfs::volume_space(dir)
                {
                    report
                        .free
                        .insert(first.dev, (dir.clone(), mount, free, None));
                }
                report.files_indexed += dir_files.len() as u64;
                report.recent.extend(skips.recent);
                report.protected_flag.extend(skips.protected_flag);
                report.hardlinked += skips.hardlinked;
                report.incremental += skips.incremental;
                report.small += skips.small;
                files.extend(dir_files);
                // A dry run doesn't need to keep builds out.
                if cli.apply {
                    held_locks.extend(locks);
                }
            }
        }
        pb.inc(1);
    }
    pb.finish_and_clear();
    eprintln!(
        "Indexed {} files{}",
        HumanCount(report.files_indexed),
        if report.in_use.is_empty() {
            String::new()
        } else {
            format!(", skipped {} directories being built", report.in_use.len())
        }
    );

    // 3. Group by size and block set, then hash.
    let pb = spinner("{spinner} Comparing sizes and clone ids…");
    let size_groups = dedupe::size_groups(files);
    pb.finish_and_clear();
    let to_hash: u64 = size_groups.iter().flatten().map(|set| set[0].len).sum();
    let pb = bar(
        to_hash,
        "Hashing   [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta}",
    );
    let groups = dedupe::find_duplicates(size_groups, &pb, &cancel);
    pb.finish_and_clear();
    report.groups = groups.len();
    let savable: u64 = groups.iter().map(|g| g.savable()).sum();
    eprintln!(
        "Found {} sets of identical files, {} duplicated",
        HumanCount(groups.len() as u64),
        HumanBytes(savable)
    );

    // 4. Replace duplicates with clones.
    let replacer = Replacer {
        min_age: opts.min_age,
        apply: cli.apply,
    };
    let to_replace: u64 = groups
        .iter()
        .map(|g| g.sets.iter().skip(1).map(Vec::len).sum::<usize>() as u64)
        .sum();
    let pb = bar(
        to_replace,
        "Cloning   [{bar:30}] {pos}/{len} files, {msg} deduplicated",
    );
    'groups: for group in &groups {
        let src = &group.sets[0][0];
        for dst in group.sets.iter().skip(1).flatten() {
            if cancel.load(Ordering::Relaxed) {
                break 'groups;
            }
            match replacer.replace(src, dst, &group.hash) {
                Outcome::Replaced => {
                    report.replaced += 1;
                    report.bytes_deduped += group.len;
                }
                Outcome::Skipped(path, Skip::Recent) => report.recent.push(path),
                Outcome::Skipped(path, Skip::Locked) => report.locked.push(path),
                Outcome::Skipped(path, Skip::Changed) => report.changed.push(path),
                Outcome::Skipped(path, Skip::Error(err)) => report.errors.push((path, err)),
            }
            pb.set_message(HumanBytes(report.bytes_deduped).to_string());
            pb.inc(1);
        }
    }
    pb.finish_and_clear();
    drop(held_locks);

    for (dir, _, _, after) in report.free.values_mut() {
        *after = apfs::volume_space(dir).map(|(_, free)| free);
    }
    print_report(&report, &cli, started, cancel.load(Ordering::Relaxed));
    Ok(())
}

fn print_paths(title: &str, paths: &[PathBuf], verbose: bool) {
    if paths.is_empty() {
        return;
    }
    println!("  {title}: {}", HumanCount(paths.len() as u64));
    let shown = if verbose { paths.len() } else { 5 };
    for path in paths.iter().take(shown) {
        println!("      {}", path.display());
    }
    if paths.len() > shown {
        println!("      … {} more (use --verbose)", paths.len() - shown);
    }
}

fn print_report(r: &Report, cli: &Cli, started: Instant, cancelled: bool) {
    println!();
    println!(
        "{}{}",
        if cli.apply {
            "Summary"
        } else {
            "Summary (dry run, nothing was changed)"
        },
        if cancelled {
            " — cancelled early"
        } else {
            ""
        }
    );
    println!(
        "  cargo directories:    {} ({} being built, skipped)",
        r.cargo_dirs,
        r.in_use.len()
    );
    println!("  files indexed:        {}", HumanCount(r.files_indexed));
    println!("  identical file sets:  {}", HumanCount(r.groups as u64));
    let (files_label, bytes_label) = if cli.apply {
        ("files replaced:", "space deduplicated:")
    } else {
        ("files to replace:", "space to deduplicate:")
    };
    println!("  {files_label:<21} {}", HumanCount(r.replaced));
    println!("  {bytes_label:<21} {}", HumanBytes(r.bytes_deduped));
    if cli.apply {
        for (_, mount, before, after) in r.free.values() {
            if let Some(after) = after {
                let delta = *after as i128 - *before as i128;
                let sign = if delta < 0 { "-" } else { "+" };
                println!(
                    "  free space change:    {sign}{} on {}",
                    HumanBytes(delta.unsigned_abs() as u64),
                    mount.display()
                );
            }
        }
    }
    println!(
        "  time:                 {}",
        HumanDuration(started.elapsed())
    );

    let anything_skipped = !(r.in_use.is_empty()
        && r.recent.is_empty()
        && r.locked.is_empty()
        && r.protected_flag.is_empty()
        && r.changed.is_empty()
        && r.errors.is_empty());
    if anything_skipped {
        println!();
        println!("Left alone");
    }
    if !r.in_use.is_empty() {
        println!(
            "  cargo directories being built (lock held): {}",
            r.in_use.len()
        );
        for (dir, lock) in &r.in_use {
            println!("      {} ({})", dir.display(), file_name(lock));
        }
    }
    print_paths(
        &format!("files changed in the last {}s", cli.min_age),
        &r.recent,
        cli.verbose,
    );
    print_paths("files locked by another process", &r.locked, cli.verbose);
    print_paths(
        "files flagged locked/append-only/compressed",
        &r.protected_flag,
        cli.verbose,
    );
    print_paths("files that changed during the run", &r.changed, cli.verbose);
    if !r.errors.is_empty() {
        println!("  errors: {}", r.errors.len());
        let shown = if cli.verbose { r.errors.len() } else { 5 };
        for (path, err) in r.errors.iter().take(shown) {
            println!("      {}: {err}", path.display());
        }
    }
    if cli.verbose {
        println!(
            "  not considered: {} hardlinked, {} in incremental/, {} under {}",
            HumanCount(r.hardlinked),
            HumanCount(r.incremental),
            HumanCount(r.small),
            HumanBytes(cli.min_size)
        );
    }
    if cli.apply && r.replaced > 0 {
        println!();
        println!(
            "Note: Time Machine local snapshots can keep the old blocks until they expire, \
             so free space may grow later."
        );
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
