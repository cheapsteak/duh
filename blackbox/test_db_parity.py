import subprocess

import db_diff
from conftest import REPO


def test_rust_scan_matches_python_scan(fixture_tree, tmp_path):
    py_bin = REPO / "duh"           # after Task 16: REPO / "reference/duh-py"
    rs_bin = REPO / "target/release/duh"
    if not rs_bin.exists():
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


def _path_of(con, node_id):
    row = con.execute("SELECT parent_id, name FROM files WHERE id = ?", (node_id,)).fetchone()
    if row is None:
        return f"<missing:{node_id}>"
    pid, name = row
    return name if pid is None else _path_of(con, pid) + "/" + name


def test_freeable_cache_exact_parity(fixture_tree, tmp_path):
    """Both implementations, same tree: freeable_cache identical by path."""
    import sqlite3
    py_bin, rs_bin = REPO / "duh", REPO / "target/release/duh"
    if not rs_bin.exists():
        import pytest; pytest.skip("rust binary not built")
    results = {}
    for label, binary in (("py", py_bin), ("rs", rs_bin)):
        db = tmp_path / f"f{label}.db"
        env = {"DUH_DB": str(db), "PATH": "/usr/bin:/bin"}
        subprocess.run([str(binary), "scan", str(fixture_tree)], env=env, check=True, capture_output=True)
        subprocess.run([str(binary), "freeable", str(fixture_tree)], env=env, check=True, capture_output=True)
        con = sqlite3.connect(db)
        by_path = {}
        for r in con.execute("SELECT node_id, freeable, locked_here FROM freeable_cache"):
            by_path[_path_of(con, r[0])] = (r[1], r[2])
        results[label] = by_path
    assert results["py"] == results["rs"]
