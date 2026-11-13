//! Finding cargo profile directories and indexing their files.

use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use walkdir::WalkDir;

/// Directory names never worth descending into while looking for cargo dirs.
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    ".Trash",
    ".Trashes",
    ".Spotlight-V100",
    ".fseventsd",
    ".DocumentRevisions-V100",
];

/// Hidden top-level folders (package caches, toolchains) that hold no cargo
/// build output and are slow to walk. Their names, without the dot.
const SKIP_HIDDEN_NAMES: &[&str] = &[
    "cargo",
    "rustup",
    "npm",
    "cache",
    "Trash",
    "pnpm-store",
    "bun",
    "nvm",
    "gradle",
    "m2",
    "docker",
    "colima",
    "pyenv",
    "conda",
    "android",
    "yarn",
    "deno",
    "swiftpm",
    "vscode",
];

/// Lock files cargo holds while building (see `src/compiler/layout.rs`).
const CARGO_LOCK_NAMES: &[&str] = &[".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

/// File flags that mean a file must not be replaced (`<sys/stat.h>`).
/// `UF_COMPRESSED` files would lose their compression when cloned.
const PROTECTED_FLAGS: u32 = libc::UF_IMMUTABLE
    | libc::UF_APPEND
    | libc::SF_IMMUTABLE
    | libc::SF_APPEND
    | libc::UF_COMPRESSED;

/// Names of the lock files whose presence marks a cargo profile directory
/// (`target/debug`, `target/<triple>/release`, a separate build-dir's
/// profiles, …).
const PROFILE_MARKERS: &[&str] = &[".cargo-lock", ".cargo-build-lock"];

/// Progress of the search, for display.
pub(crate) trait Progress: Sync {
    fn update(&self, dirs_walked: u64, found: usize, path: &Path);
}

#[derive(Debug, Default)]
pub(crate) struct Discovery {
    pub(crate) profile_dirs: Vec<PathBuf>,
    pub(crate) dirs_walked: u64,
    /// Roots that had to be walked completely.
    pub(crate) walked_roots: Vec<PathBuf>,
}

/// Walks directory trees in parallel, recording cargo profile directories.
struct Walker<'a> {
    excludes: &'a [PathBuf],
    progress: &'a dyn Progress,
    cancel: &'a AtomicBool,
    found: Mutex<Vec<PathBuf>>,
    dirs_walked: AtomicU64,
}

impl Walker<'_> {
    /// Walks `dir` and its subdirectories on the same file system, without
    /// following symlinks or descending into profile directories.
    fn walk(&self, dir: &Path) {
        if let Ok(meta) = fs::symlink_metadata(dir)
            && meta.is_dir()
        {
            self.walk_dev(dir, meta.dev());
        }
    }

    fn walk_dev(&self, dir: &Path, dev: u64) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut subdirs = Vec::new();
        let mut is_profile = false;
        for entry in entries.filter_map(Result::ok) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_str();
            if file_type.is_file() && name.is_some_and(|n| PROFILE_MARKERS.contains(&n)) {
                is_profile = true;
                break;
            }
            if file_type.is_dir() && !name.is_some_and(|n| SKIP_DIR_NAMES.contains(&n)) {
                subdirs.push(entry.path());
            }
        }
        let walked = self.dirs_walked.fetch_add(1, Ordering::Relaxed) + 1;
        if is_profile && let Ok(mut found) = self.found.lock() {
            found.push(dir.to_path_buf());
        }
        if walked.is_multiple_of(256) {
            let found = self.found.lock().map_or(0, |f| f.len());
            self.progress.update(walked, found, dir);
        }
        if is_profile {
            return;
        }
        subdirs.par_iter().for_each(|sub| {
            if !self.excludes.contains(sub)
                && fs::symlink_metadata(sub).is_ok_and(|m| m.dev() == dev)
            {
                self.walk_dev(sub, dev);
            }
        });
    }
}

/// Returns the hidden directories directly inside `root`, which Spotlight
/// doesn't index (agent worktrees often live in them), minus package caches.
fn hidden_children(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_dir())
                && e.file_name().to_str().is_some_and(|name| {
                    name.strip_prefix('.')
                        .is_some_and(|rest| !SKIP_HIDDEN_NAMES.contains(&rest))
                })
        })
        .map(|e| e.path())
        .collect()
}

/// Returns the `target` folders under `root` from the Spotlight index, or
/// `None` if Spotlight can't be used for `root`.
fn spotlight_targets(root: &Path) -> Option<Vec<PathBuf>> {
    // Spotlight doesn't index hidden or `.noindex` folders.
    let unindexed = root.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|c| c.starts_with('.') || c.ends_with(".noindex"))
    });
    if unindexed {
        return None;
    }
    let (mount, _) = crate::apfs::volume_space(root)?;
    let status = Command::new("mdutil").arg("-s").arg(&mount).output().ok()?;
    if !String::from_utf8_lossy(&status.stdout).contains("Indexing enabled") {
        return None;
    }
    let found = Command::new("mdfind")
        .arg("-0")
        .arg("-onlyin")
        .arg(root)
        .arg(r#"kMDItemFSName == "target" && kMDItemContentType == "public.folder""#)
        .output()
        .ok()?;
    if !found.status.success() {
        return None;
    }
    Some(
        found
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| PathBuf::from(OsStr::from_bytes(p)))
            .collect(),
    )
}

/// Finds cargo profile directories under `roots`, without crossing file
/// systems or following symlinks.
///
/// Unless `thorough`, it asks Spotlight for folders named `target` and walks
/// those (build directories can be nested deep inside, e.g. via
/// `CARGO_TARGET_DIR=target/<name>`), plus the root's hidden top-level
/// folders, which Spotlight never indexes. Roots Spotlight can't see are
/// walked completely.
pub(crate) fn discover(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    thorough: bool,
    progress: &dyn Progress,
    cancel: &AtomicBool,
) -> Discovery {
    let mut found = Discovery::default();
    let mut starts = Vec::new();
    for root in roots {
        progress.update(0, 0, root);
        let targets = if thorough {
            None
        } else {
            spotlight_targets(root)
        };
        if let Some(targets) = targets {
            starts.extend(targets);
            starts.extend(hidden_children(root));
        } else {
            found.walked_roots.push(root.clone());
            starts.push(root.clone());
        }
    }
    // Drop excluded starts and ones inside another start.
    starts.retain(|s| !excludes.iter().any(|e| s.starts_with(e)));
    starts.sort();
    starts.dedup_by(|later, earlier| later.starts_with(earlier));

    let walker = Walker {
        excludes,
        progress,
        cancel,
        found: Mutex::default(),
        dirs_walked: AtomicU64::new(0),
    };
    starts.par_iter().for_each(|start| walker.walk(start));
    found.dirs_walked = walker.dirs_walked.into_inner();
    found.profile_dirs = walker.found.into_inner().unwrap_or_default();
    found.profile_dirs.sort();
    found.profile_dirs.dedup();
    found
}

/// A regular file inside a cargo profile directory.
#[derive(Clone, Debug)]
pub(crate) struct FileEntry {
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) atime: (i64, i64),
    pub(crate) mtime: (i64, i64),
    pub(crate) ctime: (i64, i64),
}

impl FileEntry {
    fn new(path: PathBuf, meta: &Metadata) -> Self {
        Self {
            path,
            len: meta.len(),
            dev: meta.dev(),
            ino: meta.ino(),
            atime: (meta.atime(), meta.atime_nsec()),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// Re-reads metadata and returns whether the file is unchanged since it
    /// was indexed.
    pub(crate) fn unchanged(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|meta| {
            meta.file_type().is_file()
                && meta.nlink() == 1
                && meta.ino() == self.ino
                && meta.len() == self.len
                && (meta.mtime(), meta.mtime_nsec()) == self.mtime
                && (meta.ctime(), meta.ctime_nsec()) == self.ctime
        })
    }

    /// Time since the file was last modified.
    pub(crate) fn age(&self, now: SystemTime) -> Duration {
        let (secs, nanos) = self.mtime;
        let modified = SystemTime::UNIX_EPOCH
            + Duration::new(
                secs.max(0).cast_unsigned(),
                u32::try_from(nanos.clamp(0, 999_999_999)).unwrap_or(0),
            );
        now.duration_since(modified).unwrap_or_default()
    }
}

/// Why files were not considered for deduplication.
#[derive(Debug, Default)]
pub(crate) struct FileSkips {
    pub(crate) recent: Vec<PathBuf>,
    pub(crate) protected_flag: Vec<PathBuf>,
    pub(crate) hardlinked: u64,
    pub(crate) small: u64,
    pub(crate) incremental: u64,
}

/// The result of indexing one profile directory.
#[derive(Debug)]
pub(crate) enum Indexed {
    /// Cargo is building: one of its lock files is held by another process.
    InUse { lock: PathBuf },
    /// The directory was indexed while holding cargo's locks.
    Ready {
        files: Vec<FileEntry>,
        skips: FileSkips,
        /// Cargo's locks, held until dropped so cargo can't start a build in
        /// this directory meanwhile.
        locks: Vec<File>,
    },
}

#[derive(Debug)]
pub(crate) struct IndexOptions {
    pub(crate) min_size: u64,
    pub(crate) min_age: Duration,
    pub(crate) include_incremental: bool,
}

/// Takes cargo's locks for the profile directory `dir` (without waiting) and
/// indexes its files.
pub(crate) fn index(dir: &Path, opts: &IndexOptions, now: SystemTime) -> Indexed {
    let mut locks = Vec::new();
    for name in CARGO_LOCK_NAMES {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        match File::open(&path).map(|f| f.try_lock().map(|()| f)) {
            Ok(Ok(file)) => locks.push(file),
            // Held by cargo, or can't even be opened: leave the directory alone.
            _ => return Indexed::InUse { lock: path },
        }
    }

    let mut files = Vec::new();
    let mut skips = FileSkips::default();
    // `incremental/` is private to each checkout and `.fingerprint/` holds
    // only tiny files: don't walk into them.
    let walk = WalkDir::new(dir).into_iter().filter_entry(|e| {
        let name = e.file_name();
        if e.depth() == 0 || !e.file_type().is_dir() {
            return true;
        }
        if name == "incremental" && !opts.include_incremental {
            skips.incremental += 1;
            return false;
        }
        name != ".fingerprint"
    });
    for entry in walk.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() < opts.min_size {
            skips.small += 1;
            continue;
        }
        if meta.nlink() > 1 {
            skips.hardlinked += 1;
            continue;
        }
        let path = entry.into_path();
        if meta.st_flags() & PROTECTED_FLAGS != 0 {
            skips.protected_flag.push(path);
            continue;
        }
        let file = FileEntry::new(path, &meta);
        if file.age(now) < opts.min_age {
            skips.recent.push(file.path);
            continue;
        }
        files.push(file);
    }
    Indexed::Ready {
        files,
        skips,
        locks,
    }
}
