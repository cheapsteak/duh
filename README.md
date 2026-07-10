# dskdb

APFS-clone-aware disk usage analyzer for macOS. It measures what deleting a
directory would *actually* free — not what per-file sizes add up to.

## The problem

On APFS, `du`, Finder, and GUI analyzers like DaisyDisk sum per-file sizes.
But APFS clones — created by `clonefile(2)`, i.e. `cp -c`, and used heavily by
tools like pnpm, uv, Postgres's `FILE_COPY` clone-based database branching,
and git-worktree-heavy setups — report their **full** size on every clone
while sharing physical blocks on disk. A directory tree full of clones can
overstate its real disk cost by 10–100x. You can delete a "20 GB" folder and
get 3 MB back.

Hardlinks have the mirror problem: several paths, one set of blocks.

The only ground truth is the volume's allocated-block count (what `df`
reports) — and `df` attributes nothing to folders. dskdb bridges that gap.

## What dskdb does

- **Scans** a directory tree into a SQLite database. The design is
  memory-bounded and streaming; trees of ~4 million files are fine.
- **Detects clone families** by APFS clone ID (via a `getattrlist` ctypes
  binding for `ATTR_CMNEXT_CLONEID`) and **hardlink families** by inode.
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
./dskdb serve
```

opens http://127.0.0.1:7777/ — a zoomable treemap with three size modes:

- **Freeable** — marginal cost; what deleting this subtree returns to `df`
- **Allocated** — physical blocks, double-counting clones (the `du -A`-ish view)
- **Logical** — apparent file sizes (the classic `du`/Finder view)

Clone families that span siblings show up as "shared across N children"
rather than being silently attributed to one of them.

## Quickstart

```sh
# 1. Verify clone detection works on your filesystem
./dskdb selftest

# 2. Scan (this can take a while on a large home directory)
./dskdb scan ~

# 3. Ask questions
./dskdb freeable ~/some/dir     # what would rm -rf actually free?
./dskdb top --under ~ -d 2      # biggest directories
./dskdb clones                  # clone families ranked by apparent "waste"
./dskdb clusters                # delete-together groups

# 4. Or explore visually
./dskdb serve
```

Useful knobs:

- `--exclude NAME` adds a directory name to the skip list; a default list
  already skips regenerable trees (`node_modules`, `.venv`, `__pycache__`,
  `.git/objects`, Rust `target`, etc.). `--include NAME` removes a default;
  `--no-default-excludes` disables the list. `./dskdb excluded` shows what
  was skipped and how big it was.
- `--min-free GIB` aborts a scan if the volume's free space drops below the
  threshold (default 3 GiB) — a guard against the scan's own DB growth on a
  nearly-full disk.
- The database lives at `~/.local/share/dskdb/scan.db`; override with the
  `DSKDB_DB` environment variable or `--db`.

Other subcommands: `marginal PATH`, `file PATH`, `stats`, and `sql` (opens
the database in `sqlite3` with convenience views).

## Requirements

- macOS on APFS (the tool exits immediately on other platforms; clone
  detection is APFS-specific).
- Python 3 — standard library only, no packages to install.
- The `serve` UI loads Apache ECharts from a CDN in your browser, so the
  treemap needs internet access on first load. Everything else is fully
  offline.

## Gotchas

- **`ATTR_CMNEXT_CLONEID` is `0x100`, not the documented `0x40`.** Apple's
  headers/docs suggest `0x40`, but empirically on current macOS the attribute
  is returned for bit `0x100`. The constant in this script is the empirically
  correct one; the self-test (`./dskdb selftest`) verifies it end-to-end by
  creating a real clone and a real copy and checking their clone IDs.
- **Sparse files** (e.g. a Docker `Docker.raw`) are counted by allocated
  blocks, not logical size — a "64 GB" sparse image that occupies 9 GB counts
  as 9 GB. This is correct for "what would deleting free" but differs from
  what Finder shows.
- **The SQLite file never shrinks between rescans.** Deleted rows leave free
  pages that get reused, but the file itself only grows. If you want the
  space back, delete the DB (`rm ~/.local/share/dskdb/scan.db`) or run
  `VACUUM` via `./dskdb sql`.
- Freeable numbers are relative to the scanned root: a clone family member
  *outside* anything you scanned can't be seen, so its family may look
  fully-freeable when it isn't. Scan the broadest root you care about.

## License

MIT — see [LICENSE](LICENSE).
