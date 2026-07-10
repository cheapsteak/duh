# duh — du, honest

`du` sums per-file sizes. On APFS, clones make that a lie: a directory can
"contain" 20 GB and free 3 MB when you delete it. `duh` is a macOS disk usage
analyzer that reports what deleting actually frees.

## The problem

On APFS, `du`, Finder, and GUI analyzers like DaisyDisk sum per-file sizes.
But APFS clones — created by `clonefile(2)`, i.e. `cp -c`, and used heavily by
tools like pnpm, uv, Postgres's `FILE_COPY` clone-based database branching,
and git-worktree-heavy setups — report their **full** size on every clone
while sharing physical blocks on disk. A directory tree full of clones can
overstate its real disk cost by 10–100x.

Hardlinks have the mirror problem: several paths, one set of blocks.

The only ground truth is the volume's allocated-block count (what `df`
reports) — and `df` attributes nothing to folders. duh bridges that gap.

## What duh does

- **Scans** a directory tree into a SQLite database. The design is
  memory-bounded and streaming; trees of ~4 million files are fine.
- **Detects clone families** by APFS clone ID (via a `getattrlist` binding
  for `ATTR_CMNEXT_CLONEID`) and **hardlink families** by inode.
- **Computes `freeable(dir)`** — exactly what `rm -rf dir` would return to
  `df`. A family's blocks are credited **once**, at the lowest common
  ancestor of its members. Families with members outside the directory
  credit nothing to it: deleting your copy doesn't free blocks something
  else still references.
- **Reports `locked_here` / `clusters`** — "delete-together" groups: space
  that no single child of a directory can free alone, but that frees if you
  delete the sibling set jointly.

## The treemap UI

```
duh serve
```

opens http://127.0.0.1:7777/ — a zoomable treemap with three size modes:

- **Freeable** — marginal cost; what deleting this subtree returns to `df`
- **Allocated** — physical blocks, double-counting clones (the `du -A`-ish view)
- **Logical** — apparent file sizes (the classic `du`/Finder view)

Clone families that span siblings show up as "shared across N children"
rather than being silently attributed to one of them.

## Quickstart

Install the binary onto your `PATH` (`duh`), then:

```sh
# Build & install from source
cargo install --path .        # or: cargo build --release → ./target/release/duh

# 1. Verify clone detection works on your filesystem
duh selftest

# 2. Scan (this can take a while on a large home directory)
duh scan ~

# 3. Ask questions
duh freeable ~/some/dir     # what would rm -rf actually free?
duh top --under ~ -d 2      # biggest directories
duh clones                  # clone families ranked by apparent "waste"
duh clusters                # delete-together groups

# 4. Or explore visually
duh serve
```

Useful knobs:

- `--exclude NAME` adds a directory name to the skip list; a default list
  already skips regenerable trees (`node_modules`, `.venv`, `__pycache__`,
  `.git/objects`, Rust `target`, etc.). `--include NAME` removes a default;
  `--no-default-excludes` disables the list. `duh excluded` shows what
  was skipped and how big it was.
- `--min-free GIB` aborts a scan if the volume's free space drops below the
  threshold (default 3 GiB) — a guard against the scan's own DB growth on a
  nearly-full disk.
- The database lives at `~/.local/share/duh/scan.db`; override with the
  `DUH_DB` environment variable or `--db`.

Other subcommands: `marginal PATH`, `file PATH`, `stats`, and `sql` (opens
the database in `sqlite3` with convenience views).

## Requirements

- macOS on APFS (the tool exits immediately on other platforms; clone
  detection is APFS-specific).
- The `duh` binary. Build it with `cargo install --path .` (or
  `cargo build --release` and run `./target/release/duh`) — a Rust toolchain
  is needed only to build, not to run. Python is **not** required to use duh.
- No network access needed. The `serve` treemap UI ships embedded in the
  binary and runs fully offline.

## Gotchas

- **`ATTR_CMNEXT_CLONEID` is `0x100`, not the documented `0x40`.** Apple's
  headers/docs suggest `0x40`, but empirically on current macOS the attribute
  is returned for bit `0x100`. The constant in `src/attrs.rs` is the
  empirically correct one; the self-test (`duh selftest`) verifies it
  end-to-end by creating a real clone and a real copy and checking their
  clone IDs.
- **Sparse files** (e.g. a Docker `Docker.raw`) are counted by allocated
  blocks, not logical size — a "64 GB" sparse image that occupies 9 GB counts
  as 9 GB. This is correct for "what would deleting free" but differs from
  what Finder shows.
- **The SQLite file never shrinks between rescans.** Deleted rows leave free
  pages that get reused, but the file itself only grows. If you want the
  space back, delete the DB (`rm ~/.local/share/duh/scan.db`) or run
  `VACUUM` via `duh sql`.
- Freeable numbers are relative to the scanned root: a clone family member
  *outside* anything you scanned can't be seen, so its family may look
  fully-freeable when it isn't. Scan the broadest root you care about.

## Reference implementation

duh began as a single Python script. That original implementation now lives at
[`reference/duh-py`](reference/duh-py), frozen as the **parity oracle**: the
Rust port was built against it test-for-test. The black-box suite in
[`blackbox/`](blackbox/) runs against either implementation (set `DUH_BIN`), and
`blackbox/test_db_parity.py` scans the same tree with both and diffs the
resulting databases. The `oracle-baseline` git tag marks the frozen state the
port was validated against.

The one behavioral addition in the Rust binary: `duh serve` binds `localhost`
only and rejects requests whose `Host` header isn't a loopback name, guarding
against DNS-rebinding from a browser.

## License

MIT — see [LICENSE](LICENSE).
