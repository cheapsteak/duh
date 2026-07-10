# duh Rust Rewrite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port `duh` (APFS-clone-aware disk usage analyzer) from a single-file Python script to a single-binary Rust CLI, preserving the SQLite schema and freeable semantics exactly, verified by a black-box test suite that runs against both implementations.

**Architecture:** The scan → SQLite → analyze/serve pipeline stays. The existing Python script is **frozen** as the reference oracle (never edited during the port). A pytest black-box suite, parameterized by `DUH_BIN`, defines the behavioral contract; the SQLite schema (v2) is the data contract; a DB-diff harness compares Python and Rust scans of the same tree path-by-path. The Rust scanner replaces per-file `scandir`+`lstat`+`getattrlist` with `getattrlistbulk(2)` (one syscall per directory batch) plus a parallel walker.

**Tech Stack:** Rust 2021 (clap 4, rusqlite bundled, crossbeam-channel, serde_json, tiny_http, libc), pytest black-box harness, `hdiutil`-created APFS sparse image for hermetic filesystem tests.

## Global Constraints

- macOS/APFS only. The binary must exit with an error on any other platform.
- `ATTR_CMNEXT_CLONEID = 0x100` — NOT the documented `0x40` (empirically verified; see `duh:49`). The self-test is the ground truth for this constant.
- SQLite schema v2 (`scans`, `files`, `excluded_families` — see `duh:294-345` — plus `freeable_cache`, see `duh:1606-1623`) must remain identical between implementations through Phase 4. No new columns, no renames.
- Row `id`s WILL differ between implementations (parallel scan order); all cross-implementation comparison is **by path**, never by id.
- Sizes: `size_blocks = st_blocks * 512` (allocated), `size_logical = st_size`. IEC formatting per `fmt_bytes` (`duh:175-181`).
- DB location: `~/.local/share/duh/scan.db`, overridable by `DUH_DB` env var and `--db` flag. Tests always set `DUH_DB`.
- The Python script at `./duh` is read-only during Phases 0–4. It moves to `reference/duh-py` only in Task 16.
- HTTP server binds `127.0.0.1` only and must reject requests whose `Host` header is not `localhost`/`127.0.0.1[:port]` (DNS-rebinding guard). This is a deliberate behavioral improvement over the Python version.
- Filenames are bytes on APFS in principle; Rust code uses `OsString`/`Vec<u8>` for names end-to-end and SQLite TEXT with the same surrogateescape-compatible handling the Python uses (store as-is; APFS enforces valid UTF-8 in practice, so `String::from_utf8_lossy` is acceptable at display boundaries only).
- Commit style: conventional commits (`feat:`, `test:`, `refactor:`, `chore:`), one commit per task minimum, per step where marked.

## Repository Layout (end state)

```
duh/
  Cargo.toml
  src/
    main.rs          # clap CLI, subcommand dispatch
    attrs.rs         # getattrlist/getattrlistbulk FFI, clone IDs, selftest
    db.rs            # schema v2, open helpers, path<->id resolution
    excludes.rs      # default exclude lists + matching
    scan.rs          # parallel walker, writer thread, disk guard
    freeable.rs      # LCA credit algorithm + freeable_cache
    reports.rs       # top / clones / clusters / marginal / file / stats / excluded / sql
    serve.rs         # tiny_http server, /api/* endpoints, embedded static
  static/
    index.html       # extracted from Python _HTML_PAGE
    app.js
    style.css
    vendor/echarts.min.js
  blackbox/          # pytest suite (language-agnostic, DUH_BIN-parameterized)
    conftest.py
    test_scan.py
    test_freeable.py
    test_reports.py
    test_gold.py
    test_serve.py
    db_diff.py
  reference/
    duh-py           # the frozen Python implementation (moved here in Task 16)
  .github/workflows/ci.yml
  README.md
  LICENSE
```

---

# Phase 0 — Oracle test harness (Python untouched)

The suite must be green against the Python binary before any Rust exists. Every later phase reuses it via `DUH_BIN`.

### Task 1: Harness skeleton + hermetic APFS fixture

**Files:**
- Create: `blackbox/conftest.py`
- Create: `blackbox/pytest.ini`
- Modify: `.gitignore` (add `*.sparseimage`, `blackbox/__pycache__/`)

**Interfaces:**
- Produces: pytest fixtures `apfs_volume` (session; mounted APFS volume path), `fixture_tree` (session; built tree root `Path`), `scanned` (session; `Scanned(db=Path, root=Path)` after one `duh scan`), and helpers `run_duh(*argv, db, check=True) -> CompletedProcess`, `node_id_for(con, path, root) -> int`, `EXPECT` dict of fixture byte sizes. All later test files consume these exact names.

- [ ] **Step 1: Write `blackbox/pytest.ini`**

```ini
[pytest]
markers =
    slow: gold tests that create/destroy real data and poll df
addopts = -m "not slow"
```

(Slow tests run explicitly with `pytest -m slow`.)

- [ ] **Step 2: Write `blackbox/conftest.py`**

```python
"""Black-box harness for duh. Runs against any implementation via DUH_BIN."""
import os
import pathlib
import shutil
import sqlite3
import subprocess
import time
from dataclasses import dataclass

import pytest

MiB = 1 << 20
REPO = pathlib.Path(__file__).resolve().parent.parent
DUH_BIN = pathlib.Path(os.environ.get("DUH_BIN", REPO / "duh"))


def run_duh(*argv, db, check=True, timeout=300):
    env = {**os.environ, "DUH_DB": str(db)}
    return subprocess.run(
        [str(DUH_BIN), *map(str, argv)],
        env=env, check=check, capture_output=True, text=True, timeout=timeout,
    )


@pytest.fixture(scope="session")
def apfs_volume(tmp_path_factory):
    """Dedicated APFS volume: hermetic df numbers, safe rm -rf."""
    img = tmp_path_factory.mktemp("img") / "duhtest.sparseimage"
    subprocess.run(
        ["hdiutil", "create", "-size", "512m", "-fs", "APFS",
         "-volname", "duhtest", "-type", "SPARSE", str(img)],
        check=True, capture_output=True,
    )
    out = subprocess.run(
        ["hdiutil", "attach", str(img), "-nobrowse"],
        check=True, capture_output=True, text=True,
    ).stdout
    mount = out.strip().splitlines()[-1].split("\t")[-1].strip()
    assert mount.startswith("/Volumes/"), f"unexpected mount: {mount!r}"
    yield pathlib.Path(mount)
    subprocess.run(["hdiutil", "detach", mount, "-force"], check=False)


def _rand(p: pathlib.Path, mib: int):
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(os.urandom(mib * MiB))


def build_tree(root: pathlib.Path):
    """Known layout exercising every semantic: clone family with LCA above a
    dir (freeable=0), sibling-spanning family (locked_here/cluster),
    hardlinks, unique data, sparse file, default-excluded dir."""
    _rand(root / "big.bin", 8)
    (root / "clones").mkdir()
    subprocess.run(["cp", "-c", root / "big.bin", root / "clones/a.bin"], check=True)
    subprocess.run(["cp", "-c", root / "big.bin", root / "clones/b.bin"], check=True)
    _rand(root / "siblings/x/data.bin", 4)
    (root / "siblings/y").mkdir(parents=True)
    subprocess.run(
        ["cp", "-c", root / "siblings/x/data.bin", root / "siblings/y/data.bin"],
        check=True)
    _rand(root / "hardlinks/h1", 2)
    os.link(root / "hardlinks/h1", root / "hardlinks/h2")
    _rand(root / "unique/u.bin", 2)
    (root / "sparse").mkdir()
    with open(root / "sparse/s.bin", "wb") as f:
        f.truncate(64 * MiB)
        f.seek(0)
        f.write(os.urandom(1 * MiB))
    _rand(root / "node_modules/junk.bin", 1)


EXPECT = {
    "family_big": 8 * MiB,      # {big.bin, clones/a.bin, clones/b.bin}
    "family_siblings": 4 * MiB, # {siblings/x/data.bin, siblings/y/data.bin}
    "hardlinks": 2 * MiB,       # {h1, h2} one set of blocks
    "unique": 2 * MiB,
    "sparse_alloc": 1 * MiB,    # allocated, not the 64 MiB logical size
    "sparse_logical": 64 * MiB,
    "tolerance": 256 * 1024,    # fs block slack / metadata
}


@dataclass
class Scanned:
    db: pathlib.Path
    root: pathlib.Path


@pytest.fixture(scope="session")
def fixture_tree(apfs_volume):
    root = apfs_volume / "tree"
    root.mkdir()
    build_tree(root)
    return root


@pytest.fixture(scope="session")
def scanned(fixture_tree, tmp_path_factory) -> Scanned:
    db = tmp_path_factory.mktemp("db") / "scan.db"
    run_duh("scan", fixture_tree, db=db)
    return Scanned(db=db, root=fixture_tree)


def node_id_for(con: sqlite3.Connection, path, root) -> int:
    """Resolve absolute path -> files.id by walking parent_id/name."""
    row = con.execute(
        "SELECT id FROM files WHERE parent_id IS NULL AND name = ?", (str(root),)
    ).fetchone()
    assert row, f"root {root} not in DB"
    nid = row[0]
    rel = pathlib.Path(path).resolve().relative_to(pathlib.Path(root).resolve())
    for part in rel.parts:
        row = con.execute(
            "SELECT id FROM files WHERE parent_id = ? AND name = ?", (nid, part)
        ).fetchone()
        assert row, f"{part} not found under node {nid}"
        nid = row[0]
    return nid


def approx(actual: int, expected: int, tol: int = EXPECT["tolerance"]) -> bool:
    return abs(actual - expected) <= tol
```

- [ ] **Step 3: Verify the harness stands up**

Run: `cd /Users/chang/projects/duh && python3 -m pytest blackbox/ --collect-only -q`
Expected: collects 0 tests, no errors (fixtures import cleanly).

- [ ] **Step 4: Smoke-test the volume fixture with a throwaway test**

Add temporarily to `conftest.py` bottom, run, then delete:

```python
def test_volume_smoke(apfs_volume):
    assert apfs_volume.exists()
```

Run: `python3 -m pytest blackbox/ -q`
Expected: `1 passed` (an APFS volume mounted and detached cleanly). Delete the smoke test.

- [ ] **Step 5: Commit**

```bash
git add blackbox/ .gitignore
git commit -m "test: black-box harness with hermetic APFS fixture volume"
```

### Task 2: Scan contract tests (DB-level), green on Python

**Files:**
- Create: `blackbox/test_scan.py`

**Interfaces:**
- Consumes: `scanned`, `node_id_for`, `EXPECT`, `approx` from conftest.
- Produces: the scan behavioral contract every implementation must pass.

- [ ] **Step 1: Write `blackbox/test_scan.py`**

```python
import sqlite3

from conftest import EXPECT, approx, node_id_for


def _con(scanned):
    con = sqlite3.connect(scanned.db)
    con.row_factory = sqlite3.Row
    return con


def test_scan_completes_and_records_metadata(scanned):
    con = _con(scanned)
    scan = con.execute("SELECT * FROM scans ORDER BY id DESC LIMIT 1").fetchone()
    assert scan["root"] == str(scanned.root)
    assert scan["finished_at"] is not None
    assert scan["schema_version"] == 2
    assert scan["files_count"] > 0


def test_clone_family_shares_clone_id(scanned):
    con = _con(scanned)
    cids = {}
    for name in ("big.bin", "clones/a.bin", "clones/b.bin"):
        nid = node_id_for(con, scanned.root / name, scanned.root)
        cids[name] = con.execute(
            "SELECT clone_id FROM files WHERE id = ?", (nid,)).fetchone()[0]
    assert cids["big.bin"] is not None
    assert len(set(cids.values())) == 1, f"family split: {cids}"


def test_unique_file_not_in_multi_family(scanned):
    con = _con(scanned)
    nid = node_id_for(con, scanned.root / "unique/u.bin", scanned.root)
    cid = con.execute("SELECT clone_id FROM files WHERE id = ?", (nid,)).fetchone()[0]
    if cid is not None:
        count = con.execute(
            "SELECT COUNT(*) FROM files WHERE clone_id = ?", (cid,)).fetchone()[0]
        assert count == 1


def test_hardlinks_share_inode(scanned):
    con = _con(scanned)
    rows = [
        con.execute("SELECT ino, nlinks FROM files WHERE id = ?",
                    (node_id_for(con, scanned.root / f"hardlinks/{n}", scanned.root),)
                    ).fetchone()
        for n in ("h1", "h2")
    ]
    assert rows[0]["ino"] == rows[1]["ino"]
    assert all(r["nlinks"] >= 2 for r in rows)


def test_sparse_file_sizes(scanned):
    con = _con(scanned)
    nid = node_id_for(con, scanned.root / "sparse/s.bin", scanned.root)
    row = con.execute(
        "SELECT size_logical, size_blocks FROM files WHERE id = ?", (nid,)).fetchone()
    assert row["size_logical"] == EXPECT["sparse_logical"]
    assert approx(row["size_blocks"], EXPECT["sparse_alloc"])


def test_default_exclusion_recorded_as_aggregate(scanned):
    con = _con(scanned)
    nid = node_id_for(con, scanned.root / "node_modules", scanned.root)
    row = con.execute(
        "SELECT is_excluded, size_blocks, excluded_file_count FROM files WHERE id = ?",
        (nid,)).fetchone()
    assert row["is_excluded"] == 1
    assert approx(row["size_blocks"], 1 << 20)
    assert row["excluded_file_count"] == 1
    # nothing recorded beneath it
    assert con.execute(
        "SELECT COUNT(*) FROM files WHERE parent_id = ?", (nid,)).fetchone()[0] == 0


def test_adjacency_is_consistent(scanned):
    con = _con(scanned)
    orphans = con.execute("""
        SELECT COUNT(*) FROM files f
        WHERE f.parent_id IS NOT NULL
          AND NOT EXISTS (SELECT 1 FROM files p WHERE p.id = f.parent_id)
    """).fetchone()[0]
    assert orphans == 0
```

- [ ] **Step 2: Run against the Python oracle**

Run: `python3 -m pytest blackbox/test_scan.py -v`
Expected: all PASS. If any fail, the test's expectation is wrong (the oracle defines truth in Phase 0) — fix the test, not the script.

- [ ] **Step 3: Commit**

```bash
git add blackbox/test_scan.py
git commit -m "test: scan behavioral contract, green against Python oracle"
```

### Task 3: Freeable / clusters / excluded contract tests, green on Python

**Files:**
- Create: `blackbox/test_freeable.py`
- Create: `blackbox/test_reports.py`

**Interfaces:**
- Consumes: conftest fixtures; the `freeable_cache` table (written by any `freeable` invocation) as the exact-bytes channel — CLI output is only checked for shape, numbers come from the cache table.
- Produces: helper `freeable_of(scanned, path) -> tuple[int, int]` (freeable, locked_here) used by the gold test.

- [ ] **Step 1: Write `blackbox/test_freeable.py`**

```python
import sqlite3

from conftest import EXPECT, approx, node_id_for, run_duh


def freeable_of(scanned, path):
    """Exact (freeable, locked_here) via freeable_cache after a CLI run."""
    run_duh("freeable", path, db=scanned.db)
    con = sqlite3.connect(scanned.db)
    nid = node_id_for(con, path, scanned.root)
    row = con.execute(
        "SELECT freeable, locked_here FROM freeable_cache WHERE node_id = ?",
        (nid,)).fetchone()
    return (row[0], row[1]) if row else (0, 0)


def test_clone_dir_with_member_outside_frees_nothing(scanned):
    # family LCA is the root; deleting clones/ leaves big.bin holding the blocks
    f, _ = freeable_of(scanned, scanned.root / "clones")
    assert approx(f, 0)


def test_sibling_family_credits_lca(scanned):
    f_x, _ = freeable_of(scanned, scanned.root / "siblings/x")
    f_sib, lh_sib = freeable_of(scanned, scanned.root / "siblings")
    assert approx(f_x, 0)
    assert approx(f_sib, EXPECT["family_siblings"])
    assert approx(lh_sib, EXPECT["family_siblings"])


def test_hardlink_family_counted_once(scanned):
    f, _ = freeable_of(scanned, scanned.root / "hardlinks")
    assert approx(f, EXPECT["hardlinks"])


def test_unique_dir_fully_freeable(scanned):
    f, _ = freeable_of(scanned, scanned.root / "unique")
    assert approx(f, EXPECT["unique"])


def test_root_freeable_counts_each_family_once(scanned):
    f, _ = freeable_of(scanned, scanned.root)
    expected = (EXPECT["family_big"] + EXPECT["family_siblings"]
                + EXPECT["hardlinks"] + EXPECT["unique"]
                + EXPECT["sparse_alloc"] + (1 << 20))  # + excluded node_modules
    assert approx(f, expected, tol=1 << 20)


def test_freeable_cli_output_shape(scanned):
    out = run_duh("freeable", scanned.root / "unique", db=scanned.db).stdout
    assert "Freeable:" in out and "Locked here:" in out
```

- [ ] **Step 2: Write `blackbox/test_reports.py`**

```python
from conftest import run_duh


def test_top_lists_biggest_dirs(scanned):
    out = run_duh("top", "--under", scanned.root, "-d", "1", db=scanned.db).stdout
    assert "clones" in out and "siblings" in out


def test_clones_lists_family(scanned):
    out = run_duh("clones", db=scanned.db).stdout
    assert "big.bin" in out or "a.bin" in out


def test_clusters_finds_sibling_group(scanned):
    out = run_duh("clusters", db=scanned.db).stdout
    assert "siblings" in out


def test_excluded_lists_node_modules(scanned):
    out = run_duh("excluded", db=scanned.db).stdout
    assert "node_modules" in out


def test_stats_runs(scanned):
    assert run_duh("stats", db=scanned.db).returncode == 0
```

(Report tests assert substance, not formatting — the Rust port may improve layout.)

- [ ] **Step 3: Run against the Python oracle**

Run: `python3 -m pytest blackbox/test_freeable.py blackbox/test_reports.py -v`
Expected: all PASS. If `test_root_freeable_counts_each_family_once` is off by more than the tolerance, inspect with `DUH_DB=<db> ./duh sql` before touching the test — directory-entry blocks may need a tolerance bump, but understand the delta first.

- [ ] **Step 4: Commit**

```bash
git add blackbox/test_freeable.py blackbox/test_reports.py
git commit -m "test: freeable semantics and report contracts, green against Python"
```

### Task 4: Gold test — reported freeable vs. actual df delta

**Files:**
- Create: `blackbox/test_gold.py`

**Interfaces:**
- Consumes: `apfs_volume`, `build_tree`, `run_duh`, `freeable_of`.
- Produces: the end-to-end truth check both implementations must pass (Task 13 re-runs it against Rust).

- [ ] **Step 1: Write `blackbox/test_gold.py`**

```python
"""Gold test: `rm -rf` and verify df agrees with what duh predicted.
Builds its own tree (session `scanned` must stay intact for other tests)."""
import os
import pathlib
import shutil
import time

import pytest

from conftest import Scanned, build_tree, run_duh
from test_freeable import freeable_of


def free_bytes(path) -> int:
    st = os.statvfs(path)
    return st.f_bavail * st.f_frsize


@pytest.mark.slow
@pytest.mark.parametrize("victim,also_outside", [
    ("siblings", False),   # sibling-spanning family: frees exactly once
    ("unique", False),     # plain data
    ("clones", True),      # family member outside -> only dir overhead freed
])
def test_rm_rf_matches_prediction(apfs_volume, tmp_path, victim, also_outside):
    root = apfs_volume / f"gold-{victim}"
    root.mkdir()
    build_tree(root)
    db = tmp_path / "gold.db"
    run_duh("scan", root, db=db)
    scanned = Scanned(db=db, root=root)

    target = root / victim
    predicted, _ = freeable_of(scanned, target)

    before = free_bytes(apfs_volume)
    shutil.rmtree(target)

    tol = 1 << 20  # 1 MiB: fs metadata + df jitter on a private volume
    deadline = time.time() + 30
    delta = 0
    while time.time() < deadline:  # APFS reclaims asynchronously
        delta = free_bytes(apfs_volume) - before
        if abs(delta - predicted) <= tol:
            break
        time.sleep(1)
    assert abs(delta - predicted) <= tol, (
        f"rm -rf {victim}: predicted {predicted:,}, df says {delta:,}")
    shutil.rmtree(root, ignore_errors=True)
```

- [ ] **Step 2: Run against the Python oracle**

Run: `python3 -m pytest blackbox/test_gold.py -m slow -v`
Expected: 3 PASS (takes ~1–2 min). This is the test that makes the whole rewrite safe — do not proceed until it's green.

- [ ] **Step 3: Commit and tag the oracle baseline**

```bash
git add blackbox/test_gold.py
git commit -m "test: gold test - predicted freeable matches actual df delta"
git tag oracle-baseline
```

---

# Phase 1 — Rust skeleton + clone-ID FFI

### Task 5: Cargo project, CLI skeleton, schema v2

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/db.rs`
- Modify: `.gitignore` (add `/target`)

**Interfaces:**
- Produces: `db::open(path: &Path) -> rusqlite::Result<Connection>` (applies schema, WAL, `PRAGMA foreign_keys=ON`); `db::DEFAULT_DB_PATH()` honoring `DUH_DB`; clap `Cli` enum with all subcommands (`scan`, `freeable`, `marginal`, `file`, `top`, `clones`, `clusters`, `excluded`, `stats`, `sql`, `serve`, `selftest`) — unimplemented ones exit 2 with "not yet ported".

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "duh"
version = "3.0.0-dev"
edition = "2021"

[dependencies]
clap = { version = "4", features = ["derive"] }
rusqlite = { version = "0.31", features = ["bundled"] }
crossbeam-channel = "0.5"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tiny_http = "0.12"
libc = "0.2"

[profile.release]
lto = true
```

- [ ] **Step 2: `src/db.rs` — schema is a verbatim copy**

Copy the `executescript` SQL from `duh:295-341` plus the `freeable_cache` DDL from `duh:1606-1613` into one `SCHEMA: &str`. Implement:

```rust
use std::path::{Path, PathBuf};
use rusqlite::Connection;

pub const SCHEMA: &str = r#"
-- verbatim from reference duh:295-341 + freeable_cache from duh:1606-1613
"#;

pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("DUH_DB") { return PathBuf::from(p); }
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".local/share/duh/scan.db")
}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).ok(); }
    let con = Connection::open(path)?;
    con.pragma_update(None, "journal_mode", "WAL")?;
    con.pragma_update(None, "synchronous", "NORMAL")?;
    con.execute_batch(SCHEMA)?;
    Ok(con)
}
```

- [ ] **Step 3: `src/main.rs` — clap skeleton**

Subcommand names, flags, and defaults copied from `build_parser` (`duh:3098-3177`): `scan PATH [--rescan] [--no-clones] [--cross-device] [--exclude NAME]... [--include NAME]... [--no-default-excludes] [--min-free GIB]`, `top [--under PATH] [-d/--depth N] [-n/--limit N] [--by blocks|logical|freeable]`, etc. Add `--db PATH` as a global flag and `--version`. Platform gate first thing in `main`:

```rust
#[cfg(not(target_os = "macos"))]
compile_error!("duh is macOS-only");
```

- [ ] **Step 4: Build + sanity test**

Run: `cargo build --release && ./target/release/duh --version && ./target/release/duh stats; echo "exit=$?"`
Expected: version prints; `stats` exits 2 with "not yet ported".

Write one Rust unit test in `db.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn schema_applies_and_tables_exist() {
        let tmp = std::env::temp_dir().join(format!("duh-test-{}.db", std::process::id()));
        let con = super::open(&tmp).unwrap();
        for t in ["scans", "files", "excluded_families", "freeable_cache"] {
            let n: i64 = con.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [t], |r| r.get(0)).unwrap();
            assert_eq!(n, 1, "missing table {t}");
        }
        std::fs::remove_file(&tmp).ok();
    }
}
```

Run: `cargo test` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/ .gitignore
git commit -m "feat: rust skeleton - CLI surface and schema v2 (verbatim from reference)"
```

### Task 6: `attrs.rs` — getattrlistbulk FFI + selftest

The hairiest task. TDD anchor: the existing selftest semantics (`duh:121-169`) as a `cargo test` **and** as `duh selftest`.

**Files:**
- Create: `src/attrs.rs`
- Create: `tests/selftest.rs` (cargo integration test)
- Modify: `src/main.rs` (wire `selftest` subcommand)

**Interfaces:**
- Produces:
  ```rust
  pub struct EntryAttrs {
      pub name: std::ffi::OsString,
      pub is_dir: bool,
      pub is_symlink: bool,
      pub dev: i32,
      pub ino: u64,
      pub nlink: u32,
      pub size_logical: u64,
      pub size_blocks: u64,   // allocated bytes (ATTR_FILE_ALLOCSIZE), NOT blocks*512 math needed
      pub mtime: i64,
      pub clone_id: Option<u64>,
  }
  pub fn read_dir_attrs(dir: &std::path::Path) -> std::io::Result<Vec<EntryAttrs>>;
  pub fn get_clone_id(path: &std::path::Path) -> Option<u64>;  // single-path getattrlist
  pub fn stat_root(path: &std::path::Path) -> std::io::Result<EntryAttrs>;
  ```
- Scan (Task 9) consumes exactly these.

- [ ] **Step 1: Write the failing integration test `tests/selftest.rs`**

```rust
use std::process::Command;

fn sh(cmd: &str) { assert!(Command::new("sh").args(["-c", cmd]).status().unwrap().success()); }

#[test]
fn clone_ids_detect_clones_not_copies() {
    let dir = std::env::temp_dir().join(format!("duh-attrs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.bin");
    std::fs::write(&src, vec![0xABu8; 1 << 20]).unwrap();
    sh(&format!("cp -c {0}/src.bin {0}/clone.bin", dir.display()));
    sh(&format!("cp {0}/src.bin {0}/copy.bin", dir.display()));

    let src_id = duh::attrs::get_clone_id(&src);
    let clone_id = duh::attrs::get_clone_id(&dir.join("clone.bin"));
    let copy_id = duh::attrs::get_clone_id(&dir.join("copy.bin"));

    assert!(src_id.is_some(), "no clone id on APFS?");
    assert_eq!(src_id, clone_id, "clone must share clone_id");
    assert_ne!(src_id, copy_id, "byte copy must NOT share clone_id");

    // bulk read agrees with per-path read, and sizes/inodes are sane
    let entries = duh::attrs::read_dir_attrs(&dir).unwrap();
    assert_eq!(entries.len(), 3);
    let e = entries.iter().find(|e| e.name == "src.bin").unwrap();
    assert_eq!(e.size_logical, 1 << 20);
    assert_eq!(e.clone_id, src_id);
    assert!(!e.is_dir && e.nlink == 1 && e.ino > 0);

    std::fs::remove_dir_all(&dir).ok();
}
```

(Requires `src/lib.rs` exposing `pub mod attrs;` — add it; `main.rs` uses `duh::` too.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test selftest`
Expected: compile error (`attrs` undefined).

- [ ] **Step 3: Implement `src/attrs.rs`**

Key structure (constants verified against `<sys/attr.h>` at implementation time; CLONEID is `0x100` per the empirical note at `duh:49` — trust the selftest over the header comment):

```rust
use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

#[repr(C)]
struct AttrList {
    bitmapcount: u16, reserved: u16,
    commonattr: u32, volattr: u32, dirattr: u32, fileattr: u32, forkattr: u32,
}

const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_CMN_NAME: u32        = 0x0000_0001;
const ATTR_CMN_DEVID: u32       = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32     = 0x0000_0008;
const ATTR_CMN_MODTIME: u32     = 0x0000_0400;
const ATTR_CMN_FILEID: u32      = 0x0200_0000;
const ATTR_CMN_ERROR: u32       = 0x2000_0000;
const ATTR_FILE_LINKCOUNT: u32  = 0x0000_0001;
const ATTR_FILE_TOTALSIZE: u32  = 0x0000_0002;
const ATTR_FILE_ALLOCSIZE: u32  = 0x0000_0004;
const ATTR_CMNEXT_CLONEID: u32  = 0x0000_0100; // empirically; header says 0x40 — selftest is truth
const FSOPT_NOFOLLOW: u32           = 0x1;
const FSOPT_PACK_INVAL_ATTRS: u32   = 0x8;
const FSOPT_ATTR_CMN_EXTENDED: u32  = 0x20;

pub fn read_dir_attrs(dir: &Path) -> std::io::Result<Vec<EntryAttrs>> {
    let f = std::fs::File::open(dir)?;           // O_RDONLY dirfd
    let attrs = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT, reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | ATTR_CMN_NAME
            | ATTR_CMN_DEVID | ATTR_CMN_OBJTYPE | ATTR_CMN_MODTIME | ATTR_CMN_FILEID,
        volattr: 0, dirattr: 0,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
        forkattr: ATTR_CMNEXT_CLONEID,           // with FSOPT_ATTR_CMN_EXTENDED
    };
    let mut buf = vec![0u8; 256 * 1024];
    let mut out = Vec::new();
    loop {
        let n = unsafe { libc::getattrlistbulk(
            f.as_raw_fd(), &attrs as *const _ as *mut libc::c_void,
            buf.as_mut_ptr() as *mut libc::c_void, buf.len(),
            (FSOPT_NOFOLLOW | FSOPT_PACK_INVAL_ATTRS | FSOPT_ATTR_CMN_EXTENDED) as u64,
        )};
        if n < 0 { return Err(std::io::Error::last_os_error()); }
        if n == 0 { break; }
        let mut off = 0usize;
        for _ in 0..n {
            let rec_len = u32_at(&buf, off) as usize;
            out.push(parse_record(&buf[off..off + rec_len])?);
            off += rec_len;
        }
    }
    Ok(out)
}
```

`parse_record` walks the record in canonical attribute order: `attribute_set_t returned` (20 bytes, after the u32 length), then for each *returned* bit in order: NAME is an `attrreference { off: i32, len: u32 }` relative to the attrreference's own position; ERROR u32; DEVID i32; OBJTYPE u32 (`VDIR=2`? — use `libc::fsobj_type_t`, `VREG=1, VDIR=2, VLNK=5` verified from `<sys/vnode.h>` at impl time); MODTIME `timespec`; FILEID u64; then fileattr fields (LINKCOUNT u32, TOTALSIZE u64, ALLOCSIZE u64); then forkattr/CMNEXT CLONEID u64. Skip fields whose bit is absent; entries with ERROR set are skipped with a warning. Directories return no fileattr fields — for dirs use `size_logical=0, size_blocks=0, nlink=1` (the Python scanner stores `st_size`/`st_blocks` of the dir entry itself; match that by falling back to a per-path `lstat` for directories only — one `lstat` per dir is cheap).

`get_clone_id` is the direct port of `duh:74-115` using `libc::getattrlist`.

- [ ] **Step 4: Run tests until green**

Run: `cargo test --test selftest`
Expected: PASS. Iterate on parse offsets here — this test failing tells you the record layout is wrong; do not adjust the test.

- [ ] **Step 5: Wire `duh selftest` subcommand** reproducing the Python output shape (`duh:121-169`: create 1 MiB file in `~/tmp-duh-selftest/run-<pid>`, `cp -c`, `shutil.copy2`-equivalent, print the three ids, PASS/FAIL, cleanup).

Run: `cargo build --release && ./target/release/duh selftest`
Expected: `PASS: src and clone share clone_id; copy has different clone_id`

- [ ] **Step 6: Commit**

```bash
git add src/attrs.rs src/lib.rs src/main.rs tests/selftest.rs
git commit -m "feat: getattrlistbulk FFI with clone-id detection, selftest green"
```

---

# Phase 2 — Scanner writing identical schema

### Task 7: Exclude rules

**Files:**
- Create: `src/excludes.rs`
- Reference: `duh:198-255` (`DEFAULT_EXCLUDES`, `_MULTI_COMPONENT_EXCLUDES`, `build_excludes`, `is_excluded`)

**Interfaces:**
- Produces: `Excludes::from_args(exclude: &[String], include: &[String], no_defaults: bool) -> Excludes`; `Excludes::matches(&self, name: &str, rel_path: &str) -> bool`.

- [ ] **Step 1: Copy the exact default lists** from `duh:198-223` into constants. Write unit tests first:

```rust
#[test]
fn default_excludes_match_by_name() {
    let ex = Excludes::from_args(&[], &[], false);
    assert!(ex.matches("node_modules", "a/b/node_modules"));
    assert!(!ex.matches("src", "a/b/src"));
}
#[test]
fn multi_component_excludes_match_by_rel_suffix() {
    let ex = Excludes::from_args(&[], &[], false);
    assert!(ex.matches("objects", ".git/objects"));       // matches duh:246-255 semantics
    assert!(!ex.matches("objects", "src/objects"));
}
#[test]
fn include_removes_default_and_no_defaults_clears() {
    let ex = Excludes::from_args(&[], &["node_modules".into()], false);
    assert!(!ex.matches("node_modules", "x/node_modules"));
    let ex = Excludes::from_args(&[], &[], true);
    assert!(!ex.matches(".venv", "x/.venv"));
}
```

- [ ] **Step 2: Implement, run `cargo test`, expect PASS.**

- [ ] **Step 3: Commit** — `git commit -m "feat: port exclude rules with unit tests"`

### Task 8: Single-threaded scanner (correctness before parallelism)

**Files:**
- Create: `src/scan.rs`
- Modify: `src/main.rs` (wire `scan`)
- Reference: `duh:436-484` (`walk_for_aggregate`), `duh:489-537` (batch/insert SQL), `duh:540-837` (`cmd_scan`)

**Interfaces:**
- Produces: `scan::run(args: ScanArgs, db: &Path) -> anyhow-style Result<()>` writing rows identical in *content* (not id) to the Python scanner: root node with `parent_id NULL, name = full realpath`; children with `name = basename`; excluded dirs as leaf aggregates with `excluded_file_count` and `excluded_families` rows (`clone_id, member_count, blocks_sum, max_blocks` per family found inside — semantics at `duh:436-484`); `--rescan` deleting prior rows for the root (delete order: `excluded_families` before `files`, `duh:550-565`); scans row with `started_at/finished_at/files_count/excluded_count/bytes_logical/bytes_blocks/schema_version=2`.

- [ ] **Step 1: The failing test already exists** — the Phase 0 suite. Verify it fails:

Run: `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_scan.py -x -q`
Expected: FAIL (scan not implemented).

- [ ] **Step 2: Implement single-threaded walk**

Iterative stack of `(parent_id, PathBuf, rel_path)` exactly like `duh:655+`, but each directory is one `attrs::read_dir_attrs` call instead of scandir+lstat+getattrlist per entry. Same-device check via `dev` (skip entries with different dev unless `--cross-device`). Batched inserts (1000/`executemany`-equivalent via a prepared statement inside a transaction). IDs come from an explicit `next_id: i64` counter (`SELECT COALESCE(MAX(id),0) FROM files` at start) so Task 10's parallel version can preassign ids without writer round-trips — insert with explicit `id`. Disk-space guard: `statvfs` on the DB dir every 10s, abort with exit code 75 below `--min-free` (default 3.0 GiB), mirroring `duh:260-271`. SIGINT: flag checked per directory; finalize the scans row before exit (`duh:620-626` behavior).

- [ ] **Step 3: Run the black-box suite against Rust**

Run: `cargo build --release && DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_scan.py -v`
Expected: all PASS.

- [ ] **Step 4: Commit** — `git commit -m "feat: single-threaded scanner passing scan contract"`

### Task 9: DB-diff harness — Rust scan ≡ Python scan

**Files:**
- Create: `blackbox/db_diff.py`
- Create: `blackbox/test_db_parity.py`

**Interfaces:**
- Produces: `db_diff.diff(db_a, db_b) -> list[str]` (empty = identical), also runnable as `python3 blackbox/db_diff.py A.db B.db` for manual use on real scans.

- [ ] **Step 1: Write `blackbox/db_diff.py`**

```python
"""Compare two duh databases by PATH (ids differ across implementations).
Clone/family comparison is structural: clone_id VALUES may legitimately match
(they come from the fs), but we compare family *partitions* not raw ids."""
import sqlite3
import sys
from collections import defaultdict

FIELDS = ("is_dir", "is_symlink", "is_excluded", "ino", "nlinks",
          "size_logical", "size_blocks", "excluded_file_count")


def load(db):
    con = sqlite3.connect(db)
    con.row_factory = sqlite3.Row
    rows = {r["id"]: r for r in con.execute("SELECT * FROM files")}
    paths, families = {}, defaultdict(set)

    def path_of(nid, memo={}):
        if nid in memo: return memo[nid]
        r = rows[nid]
        p = r["name"] if r["parent_id"] is None else path_of(r["parent_id"]) + "/" + r["name"]
        memo[nid] = p
        return p

    for nid, r in rows.items():
        p = path_of(nid)
        paths[p] = {f: r[f] for f in FIELDS}
        if r["clone_id"] is not None and not r["is_dir"]:
            families[r["clone_id"]].add(p)
    partition = {frozenset(v) for v in families.values() if len(v) > 1}
    return paths, partition


def diff(db_a, db_b):
    pa, fa = load(db_a)
    pb, fb = load(db_b)
    out = []
    out += [f"only in A: {p}" for p in sorted(pa.keys() - pb.keys())]
    out += [f"only in B: {p}" for p in sorted(pb.keys() - pa.keys())]
    for p in sorted(pa.keys() & pb.keys()):
        for f in FIELDS:
            if pa[p][f] != pb[p][f]:
                out.append(f"{p}: {f} {pa[p][f]!r} != {pb[p][f]!r}")
    if fa != fb:
        out.append(f"family partitions differ: {len(fa ^ fb)} families")
    return out


if __name__ == "__main__":
    problems = diff(sys.argv[1], sys.argv[2])
    print("\n".join(problems) or "IDENTICAL")
    sys.exit(1 if problems else 0)
```

- [ ] **Step 2: Write `blackbox/test_db_parity.py`**

```python
import subprocess
import pathlib

import db_diff
from conftest import REPO, run_duh, DUH_BIN


def test_rust_scan_matches_python_scan(fixture_tree, tmp_path):
    py_bin = REPO / "duh"           # after Task 16: REPO / "reference/duh-py"
    rs_bin = REPO / "target/release/duh"
    if not rs_bin.exists() or DUH_BIN == py_bin:
        import pytest; pytest.skip("rust binary not built")
    dbs = {}
    for label, binary in (("py", py_bin), ("rs", rs_bin)):
        db = tmp_path / f"{label}.db"
        subprocess.run([str(binary), "scan", str(fixture_tree)],
                       env={"DUH_DB": str(db), "PATH": "/usr/bin:/bin"},
                       check=True, capture_output=True)
        dbs[label] = db
    problems = db_diff.diff(dbs["py"], dbs["rs"])
    assert not problems, "\n".join(problems[:50])
```

- [ ] **Step 3: Run** — `python3 -m pytest blackbox/test_db_parity.py -v` — Expected: PASS. Then run it once against a real directory manually for confidence:

```bash
DUH_DB=/tmp/py.db ./duh scan ~/projects && DUH_DB=/tmp/rs.db ./target/release/duh scan ~/projects
python3 blackbox/db_diff.py /tmp/py.db /tmp/rs.db
```
Expected: `IDENTICAL` (permission-denied subtrees may legitimately differ if files changed between scans — rerun on a quiet tree if noisy).

- [ ] **Step 4: Commit** — `git commit -m "test: db parity harness - rust scan identical to python scan by path"`

### Task 10: Parallel scanner

**Files:**
- Modify: `src/scan.rs`

**Interfaces:**
- Consumes/produces unchanged — this is a pure internal change; the parity test is the acceptance gate.

- [ ] **Step 1: Restructure to workers + single writer**

- One writer thread owns the `Connection`, receives `Vec<FileRow>` batches over a `crossbeam_channel::bounded(64)`.
- N worker threads (`std::thread::available_parallelism()`) share a work queue of `(preassigned_dir_id, PathBuf, rel_path)`; ids preassigned from an `AtomicI64` so children can reference `parent_id` before any DB write lands. Track in-flight count with an `AtomicUsize` + condvar (or `crossbeam` `WaitGroup`) for termination.
- Excluded-dir aggregation (`walk_for_aggregate` port) runs inside the worker that hit it.
- SIGINT (via `libc::signal` handler setting an `AtomicBool`) and the 10s disk guard live on the writer thread; on abort, drain queues, finalize the scans row, exit 75/130 as appropriate.

- [ ] **Step 2: Verify parity still holds** — `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/ -q && python3 -m pytest blackbox/test_db_parity.py -q` — Expected: all PASS.

- [ ] **Step 3: Benchmark and record** — `time ./duh scan ~/projects` (Python) vs `time ./target/release/duh scan ~/projects` (Rust). Record both numbers in the commit message.

- [ ] **Step 4: Commit** — `git commit -m "perf: parallel scanner (NxM speedup on ~/projects: <before> -> <after>)"`

---

# Phase 3 — Freeable + reports

### Task 11: `compute_freeable` port + cache

**Files:**
- Create: `src/freeable.rs`
- Modify: `src/main.rs` (wire `freeable`, `marginal`, `file`)
- Reference: `duh:1529-1568` (`_lca_arr`, `_direct_child_of_arr`), `duh:1654-2118` (`compute_freeable`), `duh:972-1080` (`_compute_marginal_freeable`), `duh:1084-1218` (`cmd_marginal`), `duh:1223-1273` (`cmd_file`), `duh:3076-3093` (`cmd_freeable` output format — match it exactly, tests parse it)

**Interfaces:**
- Produces: `freeable::compute(con: &Connection) -> (HashMap<i64,u64>, HashMap<i64,u64>)` (freeable, locked_here by node_id), reading `freeable_cache` when valid for the latest scan_id and persisting non-zero rows after compute (schema + clear-stale semantics at `duh:1606-1650`). `serve` (Task 14) consumes these maps.

- [ ] **Step 1: Failing tests exist** — run them:

Run: `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_freeable.py -x -q`
Expected: FAIL.

- [ ] **Step 2: Port the algorithm**

Same shape as Python, natively fast in Rust: dense `Vec<i64>` parent array + `Vec<i32>` depth (BFS), excluded-blocks map, multi-member clone families streamed grouped by `clone_id` (temp-table trick at `duh:1756-1780` is unnecessary in Rust — a `HashMap<u64, SmallVec>` of family members is fine at 4M scale), hardlink families by `(dev, ino) WHERE nlinks > 1`, per-family: credit = family blocks counted once at the members' LCA (walk-up-to-equal-depth LCA, port of `duh:1529-1557`); `locked_here` credited when members span ≥2 distinct direct children of the LCA (`_direct_child_of_arr`, `duh:1559-1568`); then a bottom-up pass accumulating subtree credits into `freeable`. **Port the remaining details from `duh:1780-2118` faithfully — every branch; where the Python is subtle, copy its comments across.** Persist to `freeable_cache`.

- [ ] **Step 3: Exact-parity check** — beyond the semantic tests, assert byte-exact equality with Python on the fixture. Add to `blackbox/test_db_parity.py`:

```python
def test_freeable_cache_exact_parity(fixture_tree, tmp_path):
    """Both implementations, same tree: freeable_cache identical by path."""
    import sqlite3
    from conftest import REPO, node_id_for
    py_bin, rs_bin = REPO / "duh", REPO / "target/release/duh"
    if not rs_bin.exists():
        import pytest; pytest.skip("rust binary not built")
    results = {}
    for label, binary in (("py", py_bin), ("rs", rs_bin)):
        db = tmp_path / f"f{label}.db"
        env = {"DUH_DB": str(db), "PATH": "/usr/bin:/bin"}
        subprocess.run([str(binary), "scan", str(fixture_tree)], env=env,
                       check=True, capture_output=True)
        subprocess.run([str(binary), "freeable", str(fixture_tree)], env=env,
                       check=True, capture_output=True)
        con = sqlite3.connect(db)
        con.row_factory = sqlite3.Row
        by_path = {}
        for r in con.execute("""SELECT fc.freeable, fc.locked_here, fc.node_id
                                FROM freeable_cache fc"""):
            by_path[_path_of(con, r["node_id"])] = (r["freeable"], r["locked_here"])
        results[label] = by_path
    assert results["py"] == results["rs"]
```

(with `_path_of` reusing the parent-walk from `db_diff.load`). Run until PASS — every mismatch is an algorithm-port bug; diff the specific path against the Python code path.

- [ ] **Step 4: Wire `marginal` and `file`** (ports of `duh:1084-1218`, `duh:1223-1273`; `marginal` = freeable restricted to a subtree treating outside-subtree members as anchored — reference `duh:972-1080`).

Run: `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_freeable.py -v`
Expected: all PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: freeable/marginal/file with byte-exact parity to reference"`

### Task 12: Reports + `--json`

**Files:**
- Create: `src/reports.rs`
- Modify: `src/main.rs`
- Reference: `top` `duh:840-967` (port the recursive-CTE SQL verbatim — rusqlite runs it as-is), `clones` `duh:1278-1352`, `excluded` `duh:1357-1392`, `sql` + views `duh:1397-1445`, `stats` `duh:1450-1523`, `clusters` `duh:3023-3072`

**Interfaces:**
- Produces: each report has a plain-text formatter (numbers matching Python; layout may improve) and `--json` emitting an array of objects with snake_case keys mirroring the SQL column names (`path`, `total_blocks`, `total_logical`, `freeable`, ...). `sql` spawns `sqlite3` with the convenience views created (verbatim `_VIEWS_SQL` from `duh:1397-1429`).

- [ ] **Step 1: Run failing tests** — `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_reports.py -x -q` → FAIL.
- [ ] **Step 2: Port each command**, reusing the Python SQL strings unchanged wherever they exist (they are the spec). Add `--json` to `top`, `clones`, `clusters`, `excluded`, `freeable`.
- [ ] **Step 3: Green** — `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/ -q` → all PASS.
- [ ] **Step 4: Gold test against Rust** — `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_gold.py -m slow -v` → 3 PASS. **This is the port-correctness milestone.**
- [ ] **Step 5: Commit** — `git commit -m "feat: reports with --json; gold test green on rust"`

---

# Phase 4 — Serve + UI reorg

### Task 13: Extract the UI to `static/`, vendor ECharts

**Files:**
- Create: `static/index.html`, `static/app.js`, `static/style.css`, `static/vendor/echarts.min.js`
- Reference: `duh:2287-2775` (`_HTML_PAGE`)

**Interfaces:**
- Produces: `static/index.html` referencing `./style.css`, `./app.js`, `./vendor/echarts.min.js` (relative URLs — the server serves the directory). No behavioral change to the JS besides the script src. This is pure extraction; the Python server keeps serving its inline copy untouched.

- [ ] **Step 1: Mechanically split `_HTML_PAGE`** — `<style>` body → `style.css`, `<script>` body (the app code, not the CDN tag) → `app.js`, remaining skeleton → `index.html` with:

```html
<link rel="stylesheet" href="./style.css">
<script src="./vendor/echarts.min.js"></script>
<script src="./app.js"></script>
```

- [ ] **Step 2: Vendor ECharts (pinned, hash-recorded)**

```bash
curl -fsSL https://fastly.jsdelivr.net/npm/echarts@5.5.1/dist/echarts.min.js -o static/vendor/echarts.min.js
shasum -a 256 static/vendor/echarts.min.js > static/vendor/echarts.min.js.sha256
echo "echarts 5.5.1 (BSD-3) via jsdelivr" > static/vendor/VERSIONS.txt
```

- [ ] **Step 3: Sanity check** — `python3 -m http.server 8899 -d static` and load `http://127.0.0.1:8899/` in a browser: page skeleton renders, console shows only failed `/api/root` fetch (no server yet) and **zero CDN requests**.

- [ ] **Step 4: Commit** — `git commit -m "refactor: extract treemap UI to static/, vendor echarts 5.5.1"`

### Task 14: Rust HTTP server

**Files:**
- Create: `src/serve.rs`
- Create: `blackbox/test_serve.py`
- Reference: `duh:2128-2283` (`_build_dir_agg`, `_PathCache`, `_marginal_for_id`), `duh:2780-3018` (`_DuhHandler`, `cmd_serve`)

**Interfaces:**
- Produces: `duh serve [--port 7777]` on `127.0.0.1`, endpoints with JSON shapes identical to the Python handler: `GET /` (static index), `/style.css`, `/app.js`, `/vendor/echarts.min.js`, `/api/root` `{id,path,name}`, `/api/node/{id}` (node info + children array — field list at `duh:2874-2890`), `/api/marginal/{id}`, `/api/breadcrumb/{id}`. Assets embedded via `include_bytes!("../static/...")` so the release binary is self-contained. Requests with a `Host` header other than `localhost[:port]` / `127.0.0.1[:port]` get 403.

- [ ] **Step 1: Write `blackbox/test_serve.py` (failing first)**

```python
import json
import socket
import subprocess
import time
import urllib.request

import pytest

from conftest import DUH_BIN


@pytest.fixture()
def server(scanned):
    port = _free_port()
    proc = subprocess.Popen(
        [str(DUH_BIN), "serve", "--port", str(port)],
        env={"DUH_DB": str(scanned.db), "PATH": "/usr/bin:/bin", "HOME": "/tmp"},
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    _wait_port(port)
    yield f"http://127.0.0.1:{port}"
    proc.terminate(); proc.wait(timeout=10)


def _free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0)); return s.getsockname()[1]


def _wait_port(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), 0.2): return
        except OSError: time.sleep(0.2)
    raise TimeoutError


def _get(url, host=None):
    req = urllib.request.Request(url, headers={"Host": host} if host else {})
    return urllib.request.urlopen(req, timeout=10)


def test_api_walk(server, scanned):
    root = json.load(_get(f"{server}/api/root"))
    assert root["path"] == str(scanned.root)
    node = json.load(_get(f"{server}/api/node/{root['id']}"))
    names = {c["name"] for c in node["children"]}
    assert {"clones", "siblings", "unique"} <= names
    assert node["total_blocks"] > 0 and node["freeable"] > 0


def test_index_and_assets_served(server):
    html = _get(f"{server}/").read().decode()
    assert "duh" in html


def test_dns_rebinding_rejected(server):
    with pytest.raises(urllib.error.HTTPError) as e:
        _get(f"{server}/api/root", host="evil.example.com")
    assert e.value.code == 403
```

Run: `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_serve.py -x -q` → FAIL.
(Note: `test_dns_rebinding_rejected` and asset tests will *never* pass against the Python oracle — mark them `@pytest.mark.skipif(DUH_BIN.name != "duh" or "reference" in str(DUH_BIN), reason="rust-only behavior")`, and gate `test_index_and_assets_served` the same way. `test_api_walk` must pass on both.)

- [ ] **Step 2: Implement `src/serve.rs`** — on startup: build `dir_agg` (port of `_build_dir_agg`, `duh:2128-2238`), path map, and `freeable::compute` maps; then a small threadpool (4 threads) around `tiny_http::Server::http("127.0.0.1:PORT")`, per-thread read-only `Connection`. JSON via `serde_json::json!`. Match the child-node aggregation semantics at `duh:2890-2960` (dirs report subtree aggregates; excluded dirs report their stored aggregate).

- [ ] **Step 3: Green** — `DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/test_serve.py -v`, plus `test_api_walk` against Python (`python3 -m pytest blackbox/test_serve.py::test_api_walk`) to confirm shape parity. Then a manual check: `./target/release/duh serve` against your real scan DB, click around the treemap, all three modes.

- [ ] **Step 4: Commit** — `git commit -m "feat: rust serve with embedded UI and Host-header guard"`

---

# Phase 5 — Cutover

### Task 15: CI

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Write the workflow**

```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: macos-14   # APFS + Apple Silicon
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --release
      - run: cargo build --release
      - run: pip3 install pytest
      - name: black-box suite vs rust
        run: DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/ -q
      - name: gold tests vs rust
        run: DUH_BIN=$PWD/target/release/duh python3 -m pytest blackbox/ -m slow -q
      - name: parity vs python reference
        run: python3 -m pytest blackbox/test_db_parity.py -q
```

- [ ] **Step 2: Push a branch, confirm green** — `git push -u origin rust-rewrite` and watch `gh run watch`. (hdiutil works on GitHub macOS runners; if the volume fixture proves flaky there, fall back to `tmp_path` on the runner's APFS root for non-gold tests only.)

- [ ] **Step 3: Commit** — part of the branch; merge when green.

### Task 16: Swap binaries, retire Python to reference/

**Files:**
- Move: `duh` → `reference/duh-py`
- Modify: `README.md`, `blackbox/conftest.py` + `blackbox/test_db_parity.py` (python path → `reference/duh-py`), `.gitignore` (drop `__pycache__` root entry if present)
- Delete: `__pycache__/`

- [ ] **Step 1: Move and rewire**

```bash
mkdir -p reference && git mv duh reference/duh-py && rm -rf __pycache__
```

Update `blackbox/conftest.py`: `DUH_BIN` default becomes `REPO / "target/release/duh"`; `test_db_parity.py`'s `py_bin = REPO / "reference/duh-py"`.

- [ ] **Step 2: Full suite, both directions**

```bash
cargo test --release && cargo build --release
python3 -m pytest blackbox/ -q                                   # rust (new default)
python3 -m pytest blackbox/ -m slow -q                           # gold on rust
DUH_BIN=$PWD/reference/duh-py python3 -m pytest blackbox/ -q -k "not serve"  # oracle still green
```
Expected: all PASS.

- [ ] **Step 3: README** — update Quickstart (`cargo install --path .` / brew note), Requirements (drop Python; keep macOS/APFS), keep the Gotchas section verbatim (all four still true — the CLONEID note now points at `src/attrs.rs`), add a "Reference implementation" section pointing at `reference/duh-py` and the parity suite, and drop the CDN sentence (UI is now fully offline).

- [ ] **Step 4: Commit + tag**

```bash
git add -A
git commit -m "chore: rust binary is now duh; python retired to reference/"
git tag v3.0.0
```

---

## Post-port backlog (explicitly NOT in this plan — YAGNI until the port is done)

- Incremental rescan (skip unchanged dirs by mtime) — the DB makes this possible; biggest UX win next.
- Scan-age banner on every report command ("scan of ~ from 3 days ago — rerun `duh scan`?").
- Replace ECharts with a hand-rolled squarified treemap (~150 lines, zero JS deps).
- FSEvents live mode; Homebrew tap; `VACUUM` housekeeping subcommand.

## Self-review notes

- Every Phase-0 test is oracle-validated before Rust exists; every Rust task's acceptance gate is a pre-existing failing test (TDD at the black-box level; unit TDD inside Tasks 6–7).
- Cross-impl comparisons are by path everywhere (Global Constraints); `db_diff` compares clone families as partitions, not raw ids, since clone_id values come from the filesystem and DO match — but partition comparison is robust either way.
- Names used across tasks checked: `run_duh`, `node_id_for`, `EXPECT`, `approx`, `Scanned`, `build_tree`, `freeable_of`, `db_diff.diff`, `attrs::read_dir_attrs`, `attrs::get_clone_id`, `Excludes::matches`, `freeable::compute` — consistent.
- Known judgment call: `EntryAttrs.size_blocks` uses `ATTR_FILE_ALLOCSIZE` (allocated bytes) where Python uses `st_blocks*512`; these are the same quantity on APFS, and Task 9's parity test is the enforcement — if they ever disagree, the parity test catches it and `lstat` fallback is the fix.
- Constants in Task 6 (attr bits, vnode types) are best-effort from headers; the selftest + parity tests are the enforcement mechanism, per Global Constraints.
