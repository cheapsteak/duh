import os
import sqlite3

from conftest import EXPECT, MiB, approx, macos_only, node_id_for, run_duh


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


@macos_only
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


# ---------------------------------------------------------------------------
# Overlap guard: a DB holding two scans whose roots nest indexes the same
# physical files twice, giving clone/hardlink families phantom members and
# collapsing `freeable` to nonsense. `scan` must refuse such roots.
# ---------------------------------------------------------------------------

def _overlap_tree(tmp_path):
    root = tmp_path / "outer"
    (root / "inner").mkdir(parents=True)
    (root / "inner/f.bin").write_bytes(os.urandom(1 * MiB))
    return root


def _scan_roots(db):
    return [r[0] for r in sqlite3.connect(db).execute("SELECT root FROM scans")]


def test_scan_refuses_nested_root(tmp_path):
    root = _overlap_tree(tmp_path)
    db = tmp_path / "scan.db"
    run_duh("scan", root, "-q", db=db)
    for conflicting in (root / "inner", root, tmp_path):  # child, equal, parent
        r = run_duh("scan", conflicting, "-q", db=db, check=False)
        assert r.returncode != 0, conflicting
        assert "overlap" in r.stderr, r.stderr
    assert _scan_roots(db) == [str(root)]  # refused scans left no rows


def test_scan_overlap_is_by_component_not_string_prefix(tmp_path):
    cha, chang = tmp_path / "cha", tmp_path / "chang"
    for d in (cha, chang):
        d.mkdir()
        (d / "f.bin").write_bytes(os.urandom(1 * MiB))
    db = tmp_path / "scan.db"
    run_duh("scan", cha, "-q", db=db)
    run_duh("scan", chang, "-q", db=db)  # sibling: not a conflict
    assert sorted(_scan_roots(db)) == [str(cha), str(chang)]


def test_rescan_same_root_unaffected_by_overlap_guard(tmp_path):
    root = _overlap_tree(tmp_path)
    db = tmp_path / "scan.db"
    run_duh("scan", root, "-q", db=db)
    run_duh("scan", root, "--rescan", "-q", db=db)
    assert _scan_roots(db) == [str(root)]


def test_replace_overlapping_evicts_conflicting_scan(tmp_path):
    root = _overlap_tree(tmp_path)
    db = tmp_path / "scan.db"
    run_duh("scan", root, "-q", db=db)
    run_duh("scan", root / "inner", "--replace-overlapping", "-q", db=db)
    con = sqlite3.connect(db)
    scans = con.execute("SELECT id, root FROM scans").fetchall()
    assert [r[1] for r in scans] == [str(root / "inner")]
    # every surviving row belongs to the surviving scan
    stray = con.execute(
        "SELECT COUNT(*) FROM files WHERE scan_id != ?", (scans[0][0],)).fetchone()[0]
    assert stray == 0
