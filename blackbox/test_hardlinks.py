"""Hardlink-family semantics, end to end. Builds its own tree in tmp_path, so it
runs on every platform (APFS on macOS, ext4 on the Linux CI runners) — this is
the core of what duh answers on Linux, where hardlinks are the only families."""
import os
import sqlite3

import pytest

from conftest import IS_MACOS, MiB, Scanned, approx, node_id_for, run_duh
from test_freeable import freeable_of


def _rand(p, mib):
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(os.urandom(mib * MiB))


@pytest.fixture(scope="module")
def hl(tmp_path_factory):
    base = tmp_path_factory.mktemp("hl")
    root = base / "root"
    store = base / "store"  # a package store outside the scanned root

    # Family fully inside one dir: 3 MiB, two names.
    _rand(root / "whole/f", 3)
    os.link(root / "whole/f", root / "whole/g")

    # Family spanning two siblings: credited at their parent, locked there.
    _rand(root / "span/x/f", 4)
    (root / "span/y").mkdir(parents=True)
    os.link(root / "span/x/f", root / "span/y/f")

    # Family with a member outside the scanned root (a uv/pnpm store link).
    _rand(store / "pkg.so", 5)
    (root / "clone/lib").mkdir(parents=True)
    os.link(store / "pkg.so", root / "clone/lib/pkg.so")
    _rand(root / "clone/own.bin", 1)

    # Same, but under a default-excluded name.
    _rand(store / "dep.js", 2)
    (root / "app/node_modules").mkdir(parents=True)
    os.link(store / "dep.js", root / "app/node_modules/dep.js")

    return root


@pytest.fixture(scope="module")
def hl_scanned(hl, tmp_path_factory):
    db = tmp_path_factory.mktemp("hldb") / "scan.db"
    proc = run_duh("scan", hl, db=db)
    return Scanned(db=db, root=hl), proc.stderr


def test_links_share_inode_in_db(hl_scanned):
    scanned, _ = hl_scanned
    con = sqlite3.connect(scanned.db)
    rows = [
        con.execute("SELECT dev, ino, nlinks FROM files WHERE id = ?",
                    (node_id_for(con, scanned.root / p, scanned.root),)).fetchone()
        for p in ("whole/f", "whole/g")
    ]
    assert rows[0] == rows[1]
    assert rows[0][2] == 2


def test_family_inside_dir_credited_once(hl_scanned):
    scanned, _ = hl_scanned
    f, _ = freeable_of(scanned, scanned.root / "whole")
    assert approx(f, 3 * MiB)


def test_family_with_member_outside_dir_credits_nothing(hl_scanned):
    scanned, _ = hl_scanned
    f, _ = freeable_of(scanned, scanned.root / "span/x")
    assert approx(f, 0)


def test_family_with_member_outside_scan_credits_nothing(hl_scanned):
    scanned, _ = hl_scanned
    # clone/ owns own.bin; lib/pkg.so is still held by the store.
    f, _ = freeable_of(scanned, scanned.root / "clone")
    assert approx(f, 1 * MiB)
    f_lib, _ = freeable_of(scanned, scanned.root / "clone/lib")
    assert approx(f_lib, 0)


def test_sibling_family_credited_at_common_ancestor(hl_scanned):
    scanned, _ = hl_scanned
    f, locked = freeable_of(scanned, scanned.root / "span")
    assert approx(f, 4 * MiB)
    assert approx(locked, 4 * MiB)


def test_sibling_family_shows_in_clusters(hl_scanned):
    scanned, _ = hl_scanned
    out = run_duh("clusters", "--min-bytes", str(1 << 20), db=scanned.db).stdout
    assert "span" in out


def test_root_counts_each_inside_family_once(hl_scanned):
    scanned, _ = hl_scanned
    f, _ = freeable_of(scanned, scanned.root)
    # whole (3) + span (4) + clone/own.bin (1), plus whatever the excluded
    # app/node_modules aggregate contributes (platform-dependent: see
    # test_excluded_aggregate_credit).
    f_app, _ = freeable_of(scanned, scanned.root / "app")
    assert approx(f, 8 * MiB + f_app, tol=1 << 20)


def test_excluded_aggregate_credit(hl_scanned):
    scanned, _ = hl_scanned
    f_app, _ = freeable_of(scanned, scanned.root / "app")
    if IS_MACOS:
        # Every APFS regular file carries a clone id, so the excluded subtree's
        # blocks become single-member excluded families, which credit nothing.
        assert approx(f_app, 0)
    else:
        # No clone ids: the aggregate is credited whole, including the 2 MiB
        # still held by the store. This is the overstatement the scan warns
        # about (test_excluded_hardlink_warning).
        assert approx(f_app, 2 * MiB)


def test_excluded_hardlink_warning(hl_scanned):
    _, stderr = hl_scanned
    if IS_MACOS:
        assert "warning: excluded directories contain" not in stderr
    else:
        assert "warning: excluded directories contain" in stderr
        assert "--include" in stderr


def test_included_dir_uses_hardlink_accounting(hl, tmp_path):
    db = tmp_path / "inc.db"
    proc = run_duh("scan", hl, "--include", "node_modules", db=db)
    assert "warning: excluded directories contain" not in proc.stderr
    scanned = Scanned(db=db, root=hl)
    f, _ = freeable_of(scanned, hl / "app")
    assert approx(f, 0)  # dep.js is still held by the store
