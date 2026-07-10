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
