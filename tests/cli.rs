//! End-to-end tests that run the binary against fake cargo profile
//! directories in a temporary directory (on macOS, on the APFS system volume).
//! Temporary directories are hidden from Spotlight, so these also exercise the
//! full directory walk.

#![expect(clippy::unwrap_used, reason = "tests")]

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// Creates `<root>/<name>/target/debug`, laid out like a cargo profile
/// directory, with `deps/libdep.rlib` holding `contents`, modified an hour ago.
fn profile_dir(root: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let debug = root.join(name).join("target/debug");
    fs::create_dir_all(debug.join("deps")).unwrap();
    fs::write(debug.join(".cargo-lock"), "").unwrap();
    let rlib = debug.join("deps/libdep.rlib");
    fs::write(&rlib, contents).unwrap();
    set_modified(&rlib, SystemTime::now() - Duration::from_secs(3600));
    rlib
}

fn set_modified(path: &Path, time: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

/// 256 KiB of data that differs per `seed`.
fn contents(seed: u8) -> Vec<u8> {
    (0..256 * 1024_u32)
        .map(|i| u8::try_from(i % 251).unwrap() ^ seed)
        .collect()
}

fn run(root: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_osx-cargo-dedupe"))
        .args(args)
        .arg(root)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// Returns the value of a summary row.
fn row<'a>(report: &'a str, label: &str) -> &'a str {
    report
        .lines()
        .find_map(|l| l.trim().strip_prefix(label))
        .map_or("", str::trim)
}

/// Returns the count of a "Left alone" entry, e.g. `! 2 files modified …`.
fn left_alone(report: &str, title: &str) -> u64 {
    report
        .lines()
        .find_map(|l| l.trim().strip_prefix("! ")?.strip_suffix(title))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

fn inode(path: &Path) -> u64 {
    fs::metadata(path).unwrap().ino()
}

#[test]
fn replaces_duplicates_and_preserves_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let a = profile_dir(tmp.path(), "a", &contents(1));
    let b = profile_dir(tmp.path(), "b", &contents(1));
    let unique = profile_dir(tmp.path(), "c", &contents(2));
    let before = fs::metadata(&b).unwrap();

    let report = run(tmp.path(), &["--apply"]);
    assert_eq!(row(&report, "Build directories"), "3");
    assert_eq!(row(&report, "Files replaced"), "1", "{report}");
    assert_eq!(row(&report, "Space deduplicated"), "256.00 KiB");

    // One duplicate was replaced by a clone: new inode, same contents, mode
    // and modification time.
    let replaced = if inode(&b) == before.ino() { &a } else { &b };
    let replaced = fs::metadata(replaced).unwrap();
    assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
    assert_eq!(replaced.mtime(), before.mtime());
    assert_eq!(replaced.mtime_nsec(), before.mtime_nsec());
    assert_eq!(replaced.mode(), before.mode());
    assert_eq!(fs::read(&unique).unwrap(), contents(2));

    // Already deduplicated: nothing left to do.
    let again = run(tmp.path(), &["--apply"]);
    assert_eq!(row(&again, "Identical file sets"), "0");
    assert_eq!(row(&again, "Files replaced"), "0");
}

#[test]
fn dry_run_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let a = profile_dir(tmp.path(), "a", &contents(1));
    let b = profile_dir(tmp.path(), "b", &contents(1));
    let inodes = (inode(&a), inode(&b));

    let report = run(tmp.path(), &[]);
    assert_eq!(row(&report, "Files to replace"), "1", "{report}");
    assert_eq!(row(&report, "Space to deduplicate"), "256.00 KiB");
    assert_eq!((inode(&a), inode(&b)), inodes);
}

#[test]
fn skips_directories_cargo_is_building_in() {
    let tmp = tempfile::tempdir().unwrap();
    profile_dir(tmp.path(), "a", &contents(1));
    let b = profile_dir(tmp.path(), "b", &contents(1));
    let lock = File::open(tmp.path().join("b/target/debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();
    let before = inode(&b);

    let report = run(tmp.path(), &["--apply"]);
    assert_eq!(
        row(&report, "Build directories"),
        "2 (1 skipped: cargo is building)",
        "{report}"
    );
    assert_eq!(row(&report, "Files replaced"), "0");
    assert_eq!(
        left_alone(&report, "build directory cargo is building in"),
        1
    );
    assert!(
        report.contains("target/debug (.cargo-lock held)"),
        "{report}"
    );
    assert_eq!(inode(&b), before);
}

#[test]
fn skips_recently_modified_and_locked_files() {
    let tmp = tempfile::tempdir().unwrap();
    let a = profile_dir(tmp.path(), "a", &contents(1));
    let b = profile_dir(tmp.path(), "b", &contents(1));
    for name in ["c", "d"] {
        let recent = profile_dir(tmp.path(), name, &contents(3));
        set_modified(&recent, SystemTime::now());
    }
    let held = File::open(&b).unwrap();
    held.lock().unwrap();
    let before = inode(&a);

    let report = run(tmp.path(), &["--apply"]);
    assert_eq!(row(&report, "Files replaced"), "0", "{report}");
    assert_eq!(left_alone(&report, "files modified in the last 60s"), 2);
    assert_eq!(left_alone(&report, "file locked by another process"), 1);
    assert_eq!(inode(&a), before);
}
