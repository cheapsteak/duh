"""Gold test: `rm -rf` and verify df agrees with what duh predicted.
Builds its own tree (session `scanned` must stay intact for other tests)."""
import os
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
    print(f"gold[{victim}] outside_family={also_outside}: predicted={predicted:,} observed_delta={delta:,}")
    assert abs(delta - predicted) <= tol, (
        f"rm -rf {victim}: predicted {predicted:,}, df says {delta:,}")
    shutil.rmtree(root, ignore_errors=True)
