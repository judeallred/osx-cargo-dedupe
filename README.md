# osx-cargo-dedupe

Finds identical files across Rust `cargo` target directories on macOS and
replaces the duplicates with APFS copy-on-write clones, so they share disk
blocks. Useful when you have many git worktrees (for example, one per coding
agent) that each compile the same dependencies into their own `target/`.

Files keep their contents, permissions, extended attributes and timestamps, so
cargo still considers everything fresh and nothing rebuilds.

Three `ripgrep` worktrees, each built once:

```console
$ osx-cargo-dedupe --apply /tmp/ocd-test/vol
Deduplicating · files changed in the last 60s or locked are left alone
Found 3 cargo directories in 185 directories
Indexed 498 files
Found 139 sets of identical files, 129.91 MiB duplicated

Summary
  cargo directories:    3 (0 being built, skipped)
  files indexed:        498
  identical file sets:  139
  files replaced:       150
  space deduplicated:   129.91 MiB
  free space change:    +124.12 MiB on /private/tmp/ocd-test/vol
  time:                 1 second
```

## Install

Requires macOS (APFS) and Rust 1.89 or newer.

```sh
cargo install --git https://github.com/judeallred/osx-cargo-dedupe
```

## Usage

```sh
osx-cargo-dedupe                  # dry run over $HOME: report what would be saved
osx-cargo-dedupe --apply          # deduplicate
osx-cargo-dedupe --apply ~/code /Volumes/Work/builds
```

| Option | Default | |
|---|---|---|
| `--apply` | off | Replace duplicates. Without it, nothing is modified. |
| `--min-age <SECONDS>` | 60 | Leave files modified within this window alone. |
| `--min-size <BYTES>` | 16384 | Ignore smaller files. |
| `--include-incremental` | off | Also consider `incremental/` directories. |
| `--exclude <PATH>` | `~/Library` | Directories not to search. |
| `-v`, `--verbose` | off | List every skipped file. |

The report lists everything that was left alone and why.

## How it works

1. **Find cargo directories.** It walks the given paths without following
   symlinks or crossing into other volumes, skipping `.git`, `node_modules`
   and similar. A directory counts as a cargo target (or build) directory if
   it holds the `CACHEDIR.TAG` file cargo writes there.
2. **Keep builds out.** For each cargo directory it tries to take cargo's own
   lock files (`.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`)
   without waiting. These use `flock(2)`, the same as cargo. If cargo is
   building there, the directory is skipped. Otherwise the locks are held
   until the run ends, so a `cargo build` started meanwhile waits ("Blocking
   waiting for file lock") instead of racing. Dry runs release them right away.
3. **Pick candidates.** Regular files of at least `--min-size`. It skips files
   modified within `--min-age`, hardlinked files, and files flagged immutable,
   append-only (Finder's "Locked") or compressed.
4. **Find duplicates.** It groups files by volume and size, then asks APFS
   (`getattrlist(2)` clone id and flags) which ones already share all their
   blocks, so those aren't hashed or counted again. It hashes one file per
   block set with BLAKE3.
5. **Replace.** For each duplicate it first re-checks that the file and its
   clone source are unchanged (inode, size, mtime, ctime), still older than
   `--min-age`, and not locked by another process (`flock`/`fcntl`). Then it:
   - creates an APFS clone of the source next to the file (`clonefile(2)`)
   - copies the original file's metadata onto it (`copyfile(3)`) and restores
     its exact access and modification times
   - re-hashes the clone, re-checks the original, and atomically `rename(2)`s
     the clone over it.

   On any mismatch or error the original stays untouched.
6. **Report** files replaced, bytes deduplicated, and the measured change in
   the volume's free space.

## Notes

- Measure savings with free space (`df -h`), not `du` or Finder: they count
  each clone at full size.
- Time Machine local snapshots keep the old blocks until the snapshots expire.
- Only files cargo produced identically in different directories can be
  shared. These are mostly your third-party dependencies. Your own crates and
  `incremental/` embed their directory's path and rarely match. Within one
  target directory, macOS cargo already clones its uplifted binaries.
- A replaced executable is a new file, so macOS may scan it again on its first
  launch. This is the same as after any rebuild.
- Press Ctrl-C to stop after the current file. The report still prints.

## License

MIT
