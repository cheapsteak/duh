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
