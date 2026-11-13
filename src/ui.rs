//! Terminal output: progress bars on stderr, the report on stdout.
//!
//! `console` disables colors when the stream isn't a terminal or `NO_COLOR`
//! is set, and `indicatif` hides progress bars when stderr isn't a terminal.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use console::style;
use indicatif::{HumanBytes, HumanCount, HumanDuration, ProgressBar, ProgressStyle};

use crate::scan::Progress;

const TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";
const SPINNER: &str = "{spinner:.cyan} {prefix:<10.bold} {wide_msg:.dim}";
const COUNT_BAR: &str =
    "{spinner:.cyan} {prefix:<10.bold} {bar:28.cyan/blue} {pos:>6}/{len:<6} {wide_msg:.dim}";
const BYTES_BAR: &str = "{spinner:.cyan} {prefix:<10.bold} {bar:28.cyan/blue} {bytes:>10}/{total_bytes:<10} \
                         {binary_bytes_per_sec} · {eta} left";

/// Shortens `path` for display: `$HOME` becomes `~`, and long paths keep
/// their first and last components.
pub(crate) fn short_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (prefix, rest) = home
        .as_deref()
        .and_then(|h| path.strip_prefix(h).ok())
        .map_or(("", path), |rest| ("~/", rest));
    let parts: Vec<_> = rest
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy()),
            Component::RootDir => Some("".into()),
            _ => None,
        })
        .collect();
    if parts.len() <= 5 {
        return format!("{prefix}{}", parts.join("/"));
    }
    format!(
        "{prefix}{}/…/{}",
        parts[..2].join("/"),
        parts[parts.len() - 2..].join("/")
    )
}

fn with_style(pb: ProgressBar, template: &str, prefix: &'static str) -> ProgressBar {
    if let Ok(style) = ProgressStyle::with_template(template) {
        pb.set_style(style.tick_chars(TICKS).progress_chars("━╸ "));
    }
    pb.set_prefix(prefix);
    pb
}

/// A spinner for work of unknown size.
pub(crate) fn spinner(prefix: &'static str) -> ProgressBar {
    let pb = with_style(ProgressBar::new_spinner(), SPINNER, prefix);
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// A bar counting items.
pub(crate) fn count_bar(len: u64, prefix: &'static str) -> ProgressBar {
    let pb = with_style(ProgressBar::new(len), COUNT_BAR, prefix);
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// A bar counting bytes.
pub(crate) fn bytes_bar(len: u64, prefix: &'static str) -> ProgressBar {
    let pb = with_style(ProgressBar::new(len), BYTES_BAR, prefix);
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Returns `one` or `many` to agree with `n`.
pub(crate) const fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// Prints a finished step.
pub(crate) fn done(message: &str) {
    eprintln!("{} {message}", style("✔").green().bold());
}

/// Shows search progress on a spinner.
#[derive(Debug)]
pub(crate) struct SearchProgress(pub(crate) ProgressBar);

impl Progress for SearchProgress {
    fn update(&self, dirs_walked: u64, found: usize, path: &Path) {
        self.0.set_message(format!(
            "{} dirs · {found} found · {}",
            HumanCount(dirs_walked),
            short_path(path)
        ));
    }
}

/// Everything the report shows.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) profile_dirs: usize,
    pub(crate) in_use: Vec<(PathBuf, PathBuf)>,
    pub(crate) files_indexed: u64,
    pub(crate) recent: Vec<PathBuf>,
    pub(crate) locked: Vec<PathBuf>,
    pub(crate) protected_flag: Vec<PathBuf>,
    pub(crate) changed: Vec<PathBuf>,
    pub(crate) errors: Vec<(PathBuf, std::io::Error)>,
    pub(crate) hardlinked: u64,
    pub(crate) incremental: u64,
    pub(crate) small: u64,
    pub(crate) groups: usize,
    pub(crate) replaced: u64,
    pub(crate) bytes_deduped: u64,
    /// Mount point and change in free space, per volume.
    pub(crate) free_change: Vec<(PathBuf, i128)>,
    pub(crate) apply: bool,
    pub(crate) min_age: u64,
    pub(crate) min_size: u64,
    pub(crate) verbose: bool,
    pub(crate) cancelled: bool,
    pub(crate) elapsed: Duration,
}

fn row(label: &str, value: &str) {
    println!("  {:<22}{value}", style(label).dim());
}

/// Prints one "Left alone" entry: a count, a title (`one` or `many`), and
/// the first few items.
fn skipped<T>((one, many): (&str, &str), items: &[T], verbose: bool, show: impl Fn(&T) -> String) {
    if items.is_empty() {
        return;
    }
    println!(
        "  {} {} {}",
        style("!").yellow().bold(),
        style(HumanCount(items.len() as u64)).yellow().bold(),
        if items.len() == 1 { one } else { many }
    );
    let shown = if verbose { items.len() } else { 3 };
    for item in items.iter().take(shown) {
        println!("      {}", style(show(item)).dim());
    }
    if items.len() > shown {
        println!(
            "      {}",
            style(format!(
                "… and {} more (--verbose lists all)",
                items.len() - shown
            ))
            .dim()
        );
    }
}

impl Report {
    pub(crate) fn print(&self) {
        println!();
        let title = match (self.apply, self.cancelled) {
            (true, false) => style("Summary".to_string()).bold(),
            (false, false) => style("Summary · dry run, nothing was changed".to_string()).bold(),
            (_, true) => style("Summary · cancelled early".to_string())
                .bold()
                .yellow(),
        };
        println!("{title}");

        let bytes = style(HumanBytes(self.bytes_deduped)).bold().green();
        if self.apply {
            row("Space deduplicated", &bytes.to_string());
            for (mount, delta) in self.free_change.iter().filter(|_| self.replaced > 0) {
                let sign = if *delta < 0 { "-" } else { "+" };
                let amount = HumanBytes(u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX));
                row(
                    "Free space change",
                    &format!("{sign}{amount} on {}", mount.display()),
                );
            }
            row("Files replaced", &HumanCount(self.replaced).to_string());
        } else {
            row("Space to deduplicate", &bytes.to_string());
            row("Files to replace", &HumanCount(self.replaced).to_string());
        }
        row(
            "Identical file sets",
            &HumanCount(self.groups as u64).to_string(),
        );
        row("Files indexed", &HumanCount(self.files_indexed).to_string());
        let busy = if self.in_use.is_empty() {
            String::new()
        } else {
            format!(" ({} skipped: cargo is building)", self.in_use.len())
        };
        row(
            "Build directories",
            &format!("{}{busy}", HumanCount(self.profile_dirs as u64)),
        );
        row("Time", &HumanDuration(self.elapsed).to_string());
        if self.verbose {
            row(
                "Not considered",
                &format!(
                    "{} hardlinked, {} incremental/ dirs, {} under {}",
                    HumanCount(self.hardlinked),
                    HumanCount(self.incremental),
                    HumanCount(self.small),
                    HumanBytes(self.min_size)
                ),
            );
        }
        self.print_left_alone();
        if !self.apply && self.replaced > 0 {
            println!();
            println!(
                "Run again with {} to deduplicate.",
                style("--apply").bold().cyan()
            );
        }
        if self.apply && self.replaced > 0 {
            println!();
            println!(
                "{}",
                style(
                    "Time Machine local snapshots keep old blocks until they expire,\n\
                     so free space may keep growing for a while."
                )
                .dim()
            );
        }
    }

    fn print_left_alone(&self) {
        let any = !(self.in_use.is_empty()
            && self.recent.is_empty()
            && self.locked.is_empty()
            && self.protected_flag.is_empty()
            && self.changed.is_empty()
            && self.errors.is_empty());
        if !any {
            return;
        }
        println!();
        println!("{}", style("Left alone").bold());
        let v = self.verbose;
        skipped(
            (
                "build directory cargo is building in",
                "build directories cargo is building in",
            ),
            &self.in_use,
            v,
            |(dir, lock)| {
                format!(
                    "{} ({} held)",
                    short_path(dir),
                    lock.file_name().unwrap_or_default().to_string_lossy()
                )
            },
        );
        let recent = (
            format!("file modified in the last {}s", self.min_age),
            format!("files modified in the last {}s", self.min_age),
        );
        skipped((&recent.0, &recent.1), &self.recent, v, |p| short_path(p));
        skipped(
            (
                "file locked by another process",
                "files locked by another process",
            ),
            &self.locked,
            v,
            |p| short_path(p),
        );
        skipped(
            (
                "file flagged locked, append-only or compressed",
                "files flagged locked, append-only or compressed",
            ),
            &self.protected_flag,
            v,
            |p| short_path(p),
        );
        skipped(
            (
                "file that changed during the run",
                "files that changed during the run",
            ),
            &self.changed,
            v,
            |p| short_path(p),
        );
        skipped(("error", "errors"), &self.errors, v, |(p, e)| {
            format!("{}: {e}", short_path(p))
        });
    }
}
