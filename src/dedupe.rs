//! Grouping identical files and replacing duplicates with clones.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::apfs::{self, BlockSet};
use crate::scan::FileEntry;

/// Files with identical contents, split by the block set backing them.
pub(crate) struct DupGroup {
    pub(crate) len: u64,
    pub(crate) hash: blake3::Hash,
    /// Every block set holding a copy of the contents, each with its files.
    pub(crate) sets: Vec<Vec<FileEntry>>,
}

impl DupGroup {
    /// Bytes that become shared if every set is replaced by clones of one.
    pub(crate) fn savable(&self) -> u64 {
        let replaced: usize = self.sets.iter().skip(1).map(Vec::len).sum();
        self.len * replaced as u64
    }
}

fn hash_file(path: &Path) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(File::open(path)?)?;
    Ok(hasher.finalize())
}

/// Returns files whose size is shared by another file on the same volume,
/// grouped by their block set. Groups already backed by one block set are
/// dropped because they are already deduplicated.
pub(crate) fn size_groups(files: Vec<FileEntry>) -> Vec<Vec<Vec<FileEntry>>> {
    let mut by_size: HashMap<(u64, u64), Vec<FileEntry>> = HashMap::new();
    for file in files {
        by_size.entry((file.dev, file.len)).or_default().push(file);
    }
    by_size
        .into_values()
        .filter(|files| files.len() > 1)
        .collect::<Vec<_>>()
        .into_par_iter()
        .filter_map(|files| {
            let mut by_set: HashMap<BlockSet, Vec<FileEntry>> = HashMap::new();
            for file in files {
                let set = apfs::block_set(&file.path, file.dev, file.ino);
                by_set.entry(set).or_default().push(file);
            }
            (by_set.len() > 1).then(|| by_set.into_values().collect())
        })
        .collect()
}

/// Hashes one file per block set and returns the groups of identical
/// contents backed by more than one block set.
pub(crate) fn find_duplicates(
    size_groups: Vec<Vec<Vec<FileEntry>>>,
    pb: &ProgressBar,
    cancel: &AtomicBool,
) -> Vec<DupGroup> {
    let hashed: Vec<(blake3::Hash, Vec<FileEntry>)> = size_groups
        .into_par_iter()
        .flatten()
        .filter_map(|set| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            // Files in one block set have identical contents; hash one.
            let hash = hash_file(&set[0].path).ok();
            pb.inc(set[0].len);
            Some((hash?, set))
        })
        .collect();

    let mut by_content: HashMap<(u64, u64, blake3::Hash), Vec<Vec<FileEntry>>> = HashMap::new();
    for (hash, set) in hashed {
        by_content
            .entry((set[0].dev, set[0].len, hash))
            .or_default()
            .push(set);
    }
    by_content
        .into_iter()
        .filter(|(_, sets)| sets.len() > 1)
        .map(|((_, len, hash), mut sets)| {
            // Keep the largest set first: it becomes the clone source, so the
            // fewest files are replaced. Ties go to the oldest file.
            sets.sort_by_key(|set| (std::cmp::Reverse(set.len()), set[0].mtime));
            DupGroup { len, hash, sets }
        })
        .collect()
}

/// Why a duplicate was left alone.
#[derive(Debug)]
pub(crate) enum Skip {
    /// Changed since it was indexed.
    Changed,
    /// Modified more recently than the minimum age.
    Recent,
    /// Another process holds a lock on it (or on the clone source).
    Locked,
    /// A system call failed.
    Error(io::Error),
}

/// Outcome of trying to replace one file.
pub(crate) enum Outcome {
    Replaced,
    Skipped(PathBuf, Skip),
}

/// Removes the temporary clone unless it was renamed into place.
struct TempGuard<'a>(&'a Path);

impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

pub(crate) struct Replacer {
    pub(crate) min_age: Duration,
    pub(crate) apply: bool,
}

impl Replacer {
    /// Replaces `dst` with a clone of `src`, which must have contents `hash`.
    ///
    /// The clone is created next to `dst`, given `dst`'s metadata and
    /// timestamps (cargo's freshness checks use mtimes), verified, and then
    /// atomically renamed over `dst`. Without `apply`, only the checks run.
    pub(crate) fn replace(&self, src: &FileEntry, dst: &FileEntry, hash: &blake3::Hash) -> Outcome {
        let skip = |reason| Outcome::Skipped(dst.path.clone(), reason);
        let now = SystemTime::now();
        if !src.unchanged() || !dst.unchanged() {
            return skip(Skip::Changed);
        }
        if src.age(now) < self.min_age || dst.age(now) < self.min_age {
            return skip(Skip::Recent);
        }
        if apfs::is_locked(&src.path) || apfs::is_locked(&dst.path) {
            return skip(Skip::Locked);
        }
        if !self.apply {
            return Outcome::Replaced;
        }
        match Self::clone_over(src, dst, hash) {
            Ok(true) => Outcome::Replaced,
            Ok(false) => skip(Skip::Changed),
            Err(err) => skip(Skip::Error(err)),
        }
    }

    fn clone_over(src: &FileEntry, dst: &FileEntry, hash: &blake3::Hash) -> io::Result<bool> {
        let name = dst.path.file_name().unwrap_or_default().to_string_lossy();
        let tmp = dst
            .path
            .with_file_name(format!(".{name}.osx-cargo-dedupe-{}", std::process::id()));
        apfs::clone_file(&src.path, &tmp)?;
        let guard = TempGuard(&tmp);
        apfs::copy_metadata(&dst.path, &tmp)?;
        apfs::set_times(&tmp, dst.atime, dst.mtime)?;
        if hash_file(&tmp)? != *hash || !src.unchanged() || !dst.unchanged() {
            return Ok(false);
        }
        fs::rename(&tmp, &dst.path)?;
        std::mem::forget(guard);
        Ok(true)
    }
}
