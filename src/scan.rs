//! Finding cargo target/build directories and indexing their files.

use std::fs::{self, File, Metadata};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use indicatif::ProgressBar;
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

/// Lock files cargo holds while building (see `src/compiler/layout.rs`).
const CARGO_LOCK_NAMES: &[&str] = &[".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

/// File flags that mean a file must not be replaced (`<sys/stat.h>`).
/// `UF_COMPRESSED` files would lose their compression when cloned.
const PROTECTED_FLAGS: u32 = libc::UF_IMMUTABLE
    | libc::UF_APPEND
    | libc::SF_IMMUTABLE
    | libc::SF_APPEND
    | libc::UF_COMPRESSED;

/// Returns whether `dir` is a cargo target or build directory, identified by
/// the `CACHEDIR.TAG` cargo writes into them.
fn is_cargo_dir(dir: &Path) -> bool {
    fs::read_to_string(dir.join("CACHEDIR.TAG")).is_ok_and(|tag| tag.contains("created by cargo"))
}

#[derive(Default)]
pub struct Discovery {
    pub cargo_dirs: Vec<PathBuf>,
    pub dirs_visited: u64,
    pub unreadable: u64,
}

/// Walks `roots` (without crossing file systems or following symlinks) and
/// returns every cargo target/build directory found.
pub fn discover(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    pb: &ProgressBar,
    cancel: &AtomicBool,
) -> Discovery {
    let mut found = Discovery::default();
    for root in roots {
        let mut it = WalkDir::new(root).same_file_system(true).into_iter();
        while let Some(entry) = it.next() {
            if cancel.load(Ordering::Relaxed) {
                return found;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    found.unreadable += 1;
                    continue;
                }
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            found.dirs_visited += 1;
            let path = entry.path();
            let skip_name = entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIP_DIR_NAMES.contains(&name));
            if skip_name || excludes.iter().any(|e| path == e) {
                it.skip_current_dir();
                continue;
            }
            if is_cargo_dir(path) {
                found.cargo_dirs.push(path.to_path_buf());
                it.skip_current_dir();
            }
            if found.dirs_visited % 64 == 0 {
                pb.set_message(format!(
                    "{} dirs, {} cargo dirs · {}",
                    found.dirs_visited,
                    found.cargo_dirs.len(),
                    path.display()
                ));
            }
        }
    }
    found
}

/// A regular file inside a cargo directory.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub len: u64,
    pub dev: u64,
    pub ino: u64,
    pub atime: (i64, i64),
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

impl FileEntry {
    fn new(path: PathBuf, meta: &Metadata) -> Self {
        FileEntry {
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
    pub fn unchanged(&self) -> bool {
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
    pub fn age(&self, now: SystemTime) -> Duration {
        let (secs, nanos) = self.mtime;
        let modified = SystemTime::UNIX_EPOCH
            + Duration::new(secs.max(0) as u64, nanos.clamp(0, 999_999_999) as u32);
        now.duration_since(modified).unwrap_or_default()
    }
}

/// Why a file was not considered for deduplication.
#[derive(Default)]
pub struct FileSkips {
    pub recent: Vec<PathBuf>,
    pub protected_flag: Vec<PathBuf>,
    pub hardlinked: u64,
    pub small: u64,
    pub incremental: u64,
}

/// The result of indexing one cargo directory.
pub enum Indexed {
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

pub struct IndexOptions {
    pub min_size: u64,
    pub min_age: Duration,
    pub include_incremental: bool,
}

/// Takes cargo's locks for `dir` (without waiting) and indexes its files.
pub fn index(dir: &Path, opts: &IndexOptions, now: SystemTime) -> Indexed {
    // Lock files live in profile directories near the root, e.g.
    // `target/debug/.cargo-lock` or `target/<triple>/release/.cargo-build-lock`.
    let mut locks = Vec::new();
    let lock_paths = WalkDir::new(dir)
        .max_depth(3)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.file_name()
                    .to_str()
                    .is_some_and(|name| CARGO_LOCK_NAMES.contains(&name))
        });
    for entry in lock_paths {
        match File::open(entry.path()).map(|f| f.try_lock().map(|()| f)) {
            Ok(Ok(file)) => locks.push(file),
            // Can't even open it: treat as in use, to be safe.
            _ => {
                return Indexed::InUse {
                    lock: entry.into_path(),
                };
            }
        }
    }

    let mut files = Vec::new();
    let mut skips = FileSkips::default();
    for entry in WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() < opts.min_size {
            skips.small += 1;
            continue;
        }
        let in_incremental = entry
            .path()
            .strip_prefix(dir)
            .is_ok_and(|rel| rel.components().any(|c| c.as_os_str() == "incremental"));
        if in_incremental && !opts.include_incremental {
            skips.incremental += 1;
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
