# osx-cargo-dedupe

[![CI](https://github.com/judeallred/osx-cargo-dedupe/actions/workflows/ci.yml/badge.svg)](https://github.com/judeallred/osx-cargo-dedupe/actions/workflows/ci.yml)

Reclaim disk space from duplicate Rust build output on macOS.

When you keep several checkouts or git worktrees of a project (for example,
one per coding agent), each compiles the same dependencies into its own
`target/` directory. `osx-cargo-dedupe` finds files that are byte-for-byte
identical across those directories and replaces the copies with APFS clones.
Clones share their disk blocks until one of them is modified, so the space is
stored once.

Nothing changes from cargo's point of view: each file keeps its contents,
permissions, extended attributes and modification time. No crate rebuilds.

A dry run over a home directory with many agent worktrees:

```console
$ osx-cargo-dedupe
osx-cargo-dedupe · dry run (pass --apply to deduplicate)
Skipping builds in progress, locked files, and files modified in the last 60s

✔ Found 1,145 build directories (Spotlight + 55,241 dirs walked, 3 seconds)
✔ Indexed 68,038 files (70 seconds, 1 dir skipped: cargo building)
✔ Found 11.65 GiB of duplicates in 8,994 sets (19.42 GiB hashed, 87 seconds)
✔ Checked 29,846 files (11.65 GiB, 2 minutes)

Summary · dry run, nothing was changed
  Space to deduplicate  11.65 GiB
  Files to replace      29,846
  Identical file sets   8,994
  Files indexed         68,038
  Build directories     1,145 (1 skipped: cargo is building)
  Time                  5 minutes

Left alone
  ! 1 build directory cargo is building in
  ! 5 files modified in the last 60s
  …

Run again with --apply to deduplicate.
```

## Install

Requires macOS (APFS) and Rust 1.89 or newer.

```sh
cargo install --git https://github.com/judeallred/osx-cargo-dedupe
```

Uninstall with `cargo uninstall osx-cargo-dedupe`.

## Usage

```sh
osx-cargo-dedupe                     # dry run over $HOME: show what would be saved
osx-cargo-dedupe --apply             # deduplicate
osx-cargo-dedupe --apply ~/code ~/.cursor/worktrees
```

Without `--apply` nothing is modified. If you've set cargo's
`build.build-dir` (or `CARGO_TARGET_DIR`) to a location outside the search
paths, add that location too.

| Option | Default | |
|---|---|---|
| `--apply` | off | Replace duplicates with clones. |
| `--min-age <SECONDS>` | `60` | Leave files modified within this window alone. |
| `--min-size <BYTES>` | `16384` | Ignore smaller files. |
| `--include-incremental` | off | Also consider `incremental/` directories (rarely identical). |
| `--thorough` | off | Walk every directory instead of asking Spotlight (see below). |
| `--exclude <PATH>` | `~/Library` | Directories not to search. Repeatable. |
| `-v`, `--verbose` | off | List every file that was left alone. |

## What it leaves alone

The report ends with a list of everything it skipped and why.

- **Directories cargo is building in.** Cargo holds `flock(2)` locks on
  `.cargo-lock` / `.cargo-build-lock` in each profile directory while it
  builds. If either is held, that directory is skipped. Otherwise
  `osx-cargo-dedupe` takes those locks itself (without waiting) and, with
  `--apply`, keeps them until it finishes. A `cargo build` started meanwhile
  waits ("Blocking waiting for file lock") instead of racing it.
- **Recently modified files**: modified within `--min-age` seconds.
- **Locked files**: files another process holds a `flock` or `fcntl` lock on,
  and files flagged immutable or append-only (Finder's "Locked").
- **Hardlinked and compressed files.** Replacing one hardlink would break the
  link; a clone of a compressed file would lose the compression.

## How it works

1. **Find build directories.** It looks for cargo *profile directories*
   (`target/debug`, `target/release`, `target/<triple>/debug`, a separate
   `build-dir`'s profiles…), recognized by the lock files cargo keeps in them.
   Only files inside those are touched, never other files you keep under
   `target/`.
   - By default it asks Spotlight (`mdfind`) for every folder named `target`
     and walks those in parallel, including build directories nested deep
     inside them. Spotlight doesn't index hidden folders, so it also walks the
     hidden folders directly inside each search path (where tools like Cursor
     keep agent worktrees), skipping package caches like `~/.cargo` and
     `~/.rustup`.
   - Search paths Spotlight can't see are walked completely: paths inside a
     hidden or `.noindex` folder, or on a volume with indexing turned off.
   - `--thorough` walks everything, which also finds build directories under
     folders with other names (`CARGO_TARGET_DIR=/somewhere/else`).
   - On one home directory with 1,150 profile directories, with a warm disk
     cache: the default search walked 55,000 directories in 2 seconds and found
     1,144. `--thorough` walked 149,000 in 3 seconds and found all 1,150.
2. **Index** files of at least `--min-size`, in parallel, without walking
   into `incremental/` (unless asked) or `.fingerprint/`.
3. **Find duplicates.** Group files by volume and size. Ask APFS which files
   already share all of their blocks (`getattrlist(2)` clone id and flags), so
   those aren't hashed or counted again. Hash one file per block set with
   BLAKE3.
4. **Replace** each duplicate:
   1. Re-check that it and its clone source are unchanged (inode, size, mtime,
      ctime), still older than `--min-age`, and not locked.
   2. Create an APFS clone of the source next to it (`clonefile(2)`).
   3. Copy the original's metadata onto the clone (`copyfile(3)`) and restore
      its exact access and modification times.
   4. Re-hash the clone, re-check the original, and atomically `rename(2)` the
      clone over it.

   On any mismatch or error the original is left as it was.
5. **Report** files replaced, space deduplicated, and the measured change in
   each volume's free space.

## Notes

- **Measure with free space** (`df -h`), not `du` or Finder. Those count every
  clone at full size, before and after.
- **Time Machine local snapshots** keep the old blocks until they expire, so
  free space can grow later.
- **What dedupes well:** compiled third-party dependencies, which come out
  identical in every checkout of the same lockfile. Your own crates and
  `incremental/` embed their directory's path, so they rarely match. Within one
  target directory, cargo on macOS already clones binaries it copies into place.
- **First launch:** a replaced executable is a new file, so macOS may scan it
  again the first time it runs, as after any rebuild.
- **Ctrl-C** stops after the current file and still prints the report.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

CI runs rustfmt, clippy with the `pedantic`, `nursery` and `cargo` lint
groups as errors, the tests, rustdoc with warnings as errors, a build with
the minimum supported Rust version (1.89), and `cargo package`. All on
macOS.

## License

[MIT](LICENSE)
