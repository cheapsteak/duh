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
