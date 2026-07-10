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
