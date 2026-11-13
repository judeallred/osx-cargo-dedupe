//! Deduplicates identical files across cargo build directories on macOS by
//! replacing duplicates with APFS copy-on-write clones.

#[cfg(not(target_os = "macos"))]
compile_error!("osx-cargo-dedupe only supports macOS (APFS clones)");

mod apfs;
mod dedupe;
mod scan;
mod ui;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use clap::Parser;
use console::style;
use indicatif::{HumanBytes, HumanCount, HumanDuration};
use rayon::prelude::*;

use dedupe::{DupGroup, Outcome, Replacer, Skip};
use scan::{FileEntry, IndexOptions, Indexed};
use ui::Report;

/// Find identical files across cargo build directories and replace the
/// duplicates with APFS clones, so they share disk blocks.
///
/// Dry run by default; pass --apply to modify files.
#[derive(Debug, Parser)]
#[command(version)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent command-line flags"
)]
struct Cli {
    /// Directories to search for cargo build directories [default: $HOME]
    paths: Vec<PathBuf>,

    /// Replace duplicates with clones (default is a dry run)
    #[arg(long)]
    apply: bool,

    /// Leave files alone if they were modified within this many seconds
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    min_age: u64,

    /// Ignore files smaller than this many bytes
    #[arg(long, value_name = "BYTES", default_value_t = 16 * 1024)]
    min_size: u64,

    /// Also deduplicate files in `incremental/` directories
    #[arg(long)]
    include_incremental: bool,

    /// Walk every directory instead of asking Spotlight for `target` folders
    /// (finds target directories with other names; much slower)
    #[arg(long)]
    thorough: bool,

    /// Directories to skip while searching (default: ~/Library)
    #[arg(long, value_name = "PATH")]
    exclude: Vec<PathBuf>,

    /// List every skipped file in the report
    #[arg(short, long)]
    verbose: bool,
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
        let cancel = Arc::clone(&cancel);
        ctrlc::set_handler(move || cancel.store(true, Ordering::Relaxed))
            .context("installing Ctrl-C handler")?;
    }

    eprintln!(
        "{} {}",
        style("osx-cargo-dedupe").bold(),
        if cli.apply {
            style("· deduplicating".to_string()).green()
        } else {
            style("· dry run (pass --apply to deduplicate)".to_string()).yellow()
        }
    );
    eprintln!(
        "{}",
        style(format!(
            "Skipping builds in progress, locked files, and files modified \
             in the last {}s",
            cli.min_age
        ))
        .dim()
    );
    eprintln!();

    let mut report = Report {
        apply: cli.apply,
        min_age: cli.min_age,
        min_size: cli.min_size,
        verbose: cli.verbose,
        ..Report::default()
    };
    let profile_dirs = search(&roots, &excludes, cli.thorough, &cancel);
    report.profile_dirs = profile_dirs.len();
    let opts = IndexOptions {
        min_size: cli.min_size,
        min_age: Duration::from_secs(cli.min_age),
        include_incremental: cli.include_incremental,
    };
    let (files, free_before, held_locks) =
        index_all(&profile_dirs, &opts, cli.apply, &mut report, &cancel);
    let groups = find_duplicates(files, &mut report, &cancel);
    let replacer = Replacer {
        min_age: opts.min_age,
        apply: cli.apply,
    };
    replace_all(&groups, &replacer, &mut report, &cancel);
    drop(held_locks);

    report.free_change = free_before
        .into_values()
        .filter_map(|(dir, mount, before)| {
            let (_, after) = apfs::volume_space(&dir)?;
            Some((mount, i128::from(after) - i128::from(before)))
        })
        .collect();
    report.cancelled = cancel.load(Ordering::Relaxed);
    report.elapsed = started.elapsed();
    report.print();
    Ok(())
}

/// Finds the cargo profile directories to work on.
fn search(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    thorough: bool,
    cancel: &AtomicBool,
) -> Vec<PathBuf> {
    let started = Instant::now();
    let progress = ui::SearchProgress(ui::spinner("Searching"));
    let found = scan::discover(roots, excludes, thorough, &progress, cancel);
    progress.0.finish_and_clear();
    let how = if found.walked_roots.len() == roots.len() {
        format!("walked {} dirs", HumanCount(found.dirs_walked))
    } else {
        format!("Spotlight + {} dirs walked", HumanCount(found.dirs_walked))
    };
    ui::done(&format!(
        "Found {} build directories {}",
        style(HumanCount(found.profile_dirs.len() as u64)).bold(),
        style(format!("({how}, {})", HumanDuration(started.elapsed()))).dim()
    ));
    found.profile_dirs
}

/// Per volume: a directory on it, its mount point, and its free space.
type FreeSpace = BTreeMap<u64, (PathBuf, PathBuf, u64)>;

/// Locks and indexes every profile directory. Returns the candidate files,
/// the free space of their volumes, and, when `keep_locks` is set, cargo's
/// locks for the directories indexed.
fn index_all(
    dirs: &[PathBuf],
    opts: &IndexOptions,
    keep_locks: bool,
    report: &mut Report,
    cancel: &AtomicBool,
) -> (Vec<FileEntry>, FreeSpace, Vec<File>) {
    let started = Instant::now();
    let pb = ui::count_bar(dirs.len() as u64, "Indexing");
    let now = SystemTime::now();
    let indexed: Vec<(&PathBuf, Indexed)> = dirs
        .par_iter()
        .filter_map(|dir| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            pb.set_message(ui::short_path(dir));
            let indexed = scan::index(dir, opts, now);
            pb.inc(1);
            Some((dir, indexed))
        })
        .collect();
    let mut files = Vec::new();
    let mut free = FreeSpace::new();
    let mut held_locks = Vec::new();
    for (dir, indexed) in indexed {
        match indexed {
            Indexed::InUse { lock } => report.in_use.push((dir.clone(), lock)),
            Indexed::Ready {
                files: dir_files,
                skips,
                locks,
            } => {
                if let Some(first) = dir_files.first()
                    && !free.contains_key(&first.dev)
                    && let Some((mount, bytes)) = apfs::volume_space(dir)
                {
                    free.insert(first.dev, (dir.clone(), mount, bytes));
                }
                report.files_indexed += dir_files.len() as u64;
                report.recent.extend(skips.recent);
                report.protected_flag.extend(skips.protected_flag);
                report.hardlinked += skips.hardlinked;
                report.incremental += skips.incremental;
                report.small += skips.small;
                files.extend(dir_files);
                // A dry run doesn't need to keep builds out.
                if keep_locks {
                    held_locks.extend(locks);
                }
            }
        }
    }
    pb.finish_and_clear();
    let busy = if report.in_use.is_empty() {
        String::new()
    } else {
        let n = report.in_use.len();
        format!(
            ", {n} {} skipped: cargo building",
            ui::plural(n, "dir", "dirs")
        )
    };
    ui::done(&format!(
        "Indexed {} files {}",
        style(HumanCount(report.files_indexed)).bold(),
        style(format!("({}{busy})", HumanDuration(started.elapsed()))).dim()
    ));
    (files, free, held_locks)
}

/// Groups candidates by size and block set, then hashes them.
fn find_duplicates(
    files: Vec<FileEntry>,
    report: &mut Report,
    cancel: &AtomicBool,
) -> Vec<DupGroup> {
    let started = Instant::now();
    let pb = ui::spinner("Comparing");
    pb.set_message("sizes and APFS clone ids");
    let size_groups = dedupe::size_groups(files);
    pb.finish_and_clear();
    let to_hash: u64 = size_groups.iter().flatten().map(|set| set[0].len).sum();
    let pb = ui::bytes_bar(to_hash, "Hashing");
    let groups = dedupe::find_duplicates(size_groups, &pb, cancel);
    pb.finish_and_clear();
    report.groups = groups.len();
    let savable: u64 = groups.iter().map(DupGroup::savable).sum();
    ui::done(&format!(
        "Found {} of duplicates in {} {}",
        style(HumanBytes(savable)).bold(),
        HumanCount(groups.len() as u64),
        style(format!(
            "sets ({} hashed, {})",
            HumanBytes(to_hash),
            HumanDuration(started.elapsed())
        ))
        .dim()
    ));
    groups
}

/// Replaces every duplicate with a clone of its group's source file.
fn replace_all(groups: &[DupGroup], replacer: &Replacer, report: &mut Report, cancel: &AtomicBool) {
    let to_replace: usize = groups
        .iter()
        .map(|g| g.sets.iter().skip(1).map(Vec::len).sum::<usize>())
        .sum();
    if to_replace == 0 {
        return;
    }
    let started = Instant::now();
    let pb = ui::count_bar(
        to_replace as u64,
        if replacer.apply {
            "Cloning"
        } else {
            "Checking"
        },
    );
    'groups: for group in groups {
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
            pb.set_message(format!("{} deduplicated", HumanBytes(report.bytes_deduped)));
            pb.inc(1);
        }
    }
    pb.finish_and_clear();
    ui::done(&format!(
        "{} {} files {}",
        if replacer.apply { "Cloned" } else { "Checked" },
        style(HumanCount(report.replaced)).bold(),
        style(format!(
            "({}, {})",
            HumanBytes(report.bytes_deduped),
            HumanDuration(started.elapsed())
        ))
        .dim()
    ));
}
