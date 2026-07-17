"""Black-box tests for `GET /api/share/{id}` — secret-gist transport.

Pivoted 2026-07-17 (docs/superpowers/specs/2026-07-15-share-urls-design.md):
the endpoint no longer takes a `budget` query param or produces a URL
fragment link. It builds a full snapshot (share::FULL_BUDGET, bounded by the
~20k-node reveal cap rather than a char budget), uploads it to a SECRET
GitHub gist via `gh gist create` (gists are secret by default; the stub
enforces that `--public`/`-p` is never passed), and returns the viewer URL
`{"url": "<base>?gist=<id>", "gist_id": "<id>", "nodes": N}`.

All tests here inject a fake `gh` (blackbox/gh-stub.sh) via `DUH_GH_BIN` so
they never touch the network or a real GitHub account. `DUH_STUB_CAPTURE`
tells the stub where to copy the snapshot file it was handed, so tests can
inspect exactly what would have been uploaded.

`/api/share` is CSRF-guarded (src/serve.rs `share_csrf_guard`): every request
must carry `X-Duh-Share: 1`, so all requests below go through `_share_get`
rather than the bare `_get` helper.
"""
import json
import os
import pathlib
import subprocess
import zlib
import base64

import pytest

from conftest import DUH_BIN, REPO
from test_serve import _free_port, _wait_port, _get

rust_only = pytest.mark.skipif("target" not in str(DUH_BIN), reason="rust-only feature")
STUB = REPO / "blackbox" / "gh-stub.sh"


def _share_get(url, extra_headers=None):
    """`_get`, but with the `X-Duh-Share: 1` header /api/share requires."""
    import urllib.request
    headers = {"X-Duh-Share": "1"}
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(url, headers=headers)
    return urllib.request.urlopen(req, timeout=10)


def _decode(fragment):
    assert fragment.startswith("1.")
    data = fragment[2:]
    data += "=" * (-len(data) % 4)
    return json.loads(zlib.decompress(base64.urlsafe_b64decode(data)))


def _count_nodes(node):
    kids = node[2] if len(node) > 2 else []
    return 1 + sum(_count_nodes(k) for k in kids)


def _start_server(scanned, extra_env):
    port = _free_port()
    env = {**os.environ, "DUH_DB": str(scanned.db), "PATH": "/usr/bin:/bin", "HOME": "/tmp", **extra_env}
    proc = subprocess.Popen(
        [str(DUH_BIN), "serve", "--port", str(port), "--no-browser"],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    _wait_port(port)
    return proc, f"http://127.0.0.1:{port}"


@pytest.fixture()
def gist_server(scanned, tmp_path):
    """A `duh serve` instance whose `gh` is the test stub (success path)."""
    cap = tmp_path / "capture.txt"
    proc, base = _start_server(
        scanned, {"DUH_GH_BIN": str(STUB), "DUH_STUB_CAPTURE": str(cap)}
    )
    yield base, cap
    proc.terminate(); proc.wait(timeout=10)


@rust_only
def test_share_creates_gist_and_returns_viewer_url(gist_server, scanned):
    base, cap = gist_server
    root = json.load(_get(f"{base}/api/root"))
    resp = json.load(_share_get(f"{base}/api/share/{root['id']}"))
    assert resp["gist_id"] == "deadbeefcafebabefeed0000deadbeef"
    assert resp["url"] == "https://cheapsteak.github.io/duh/v/?gist=deadbeefcafebabefeed0000deadbeef"
    assert "budget" not in resp and "fragment" not in resp and "chars" not in resp
    assert resp["nodes"] > 0

    # the snapshot the stub was handed decodes and is the full tree the
    # server actually revealed — proof the fragment that got uploaded is not
    # capped by any URL char budget (FULL_BUDGET, not the old default).
    frag = pathlib.Path(cap).read_text().strip()
    doc = _decode(frag)
    assert doc["v"] == 1 and doc["t"] == str(scanned.root)
    assert resp["nodes"] == _count_nodes(doc["n"])


@rust_only
def test_share_file_node_400(gist_server, scanned):
    import sqlite3, contextlib
    import urllib.error
    base, _ = gist_server
    with contextlib.closing(
        sqlite3.connect(f"file:{scanned.db}?mode=ro", uri=True)
    ) as con:
        (file_id,) = con.execute("SELECT id FROM files WHERE name='u.bin'").fetchone()
    with pytest.raises(urllib.error.HTTPError) as e:
        _share_get(f"{base}/api/share/{file_id}")
    assert e.value.code == 400
    body = json.loads(e.value.read())
    assert body["error"] == "not a shareable directory"


@rust_only
def test_share_unknown_node_404(gist_server):
    import urllib.error
    base, _ = gist_server
    with pytest.raises(urllib.error.HTTPError) as e:
        _share_get(f"{base}/api/share/999999999")
    assert e.value.code == 404


@rust_only
def test_share_gh_missing_returns_400(scanned):
    """DUH_GH_BIN pointing at a nonexistent binary simulates `gh` not being
    installed (ENOENT on spawn); the endpoint must map that to a clear 400,
    not a generic 500."""
    import urllib.error
    proc, base = _start_server(scanned, {"DUH_GH_BIN": "/nonexistent/gh-does-not-exist"})
    try:
        root = json.load(_get(f"{base}/api/root"))
        with pytest.raises(urllib.error.HTTPError) as e:
            _share_get(f"{base}/api/share/{root['id']}")
        assert e.value.code == 400
        body = json.loads(e.value.read())
        assert body["error"] == "Sharing needs the GitHub CLI (gh) installed and signed in."
    finally:
        proc.terminate(); proc.wait(timeout=10)


@rust_only
def test_share_gh_failure_returns_502(scanned, tmp_path):
    """A `gh` that exits nonzero (e.g. not signed in, API error) must map to
    502 with the trimmed stderr in the error message."""
    import urllib.error
    cap = tmp_path / "capture.txt"
    proc, base = _start_server(
        scanned, {"DUH_GH_BIN": str(STUB), "DUH_STUB_FAIL": "1", "DUH_STUB_CAPTURE": str(cap)}
    )
    try:
        root = json.load(_get(f"{base}/api/root"))
        with pytest.raises(urllib.error.HTTPError) as e:
            _share_get(f"{base}/api/share/{root['id']}")
        assert e.value.code == 502
        body = json.loads(e.value.read())
        assert body["error"].startswith("gist creation failed:")
        assert "Bad credentials" in body["error"]
    finally:
        proc.terminate(); proc.wait(timeout=10)


@rust_only
def test_share_without_header_is_forbidden(gist_server, scanned):
    """`/api/share` has a side effect (a gist upload), so unlike the other
    read-only /api/* routes it requires the `X-Duh-Share: 1` header a
    cross-site page cannot set without triggering a CORS preflight this
    server never approves. A request missing the header must be rejected
    with a 403 before any gist is created."""
    import urllib.error
    base, cap = gist_server
    root = json.load(_get(f"{base}/api/root"))
    with pytest.raises(urllib.error.HTTPError) as e:
        _get(f"{base}/api/share/{root['id']}")  # no X-Duh-Share header
    assert e.value.code == 403
    body = json.loads(e.value.read())
    assert body["error"] == "forbidden"
    assert not cap.exists()  # the guard ran before create_gist was ever called


@rust_only
def test_share_cross_origin_forbidden(gist_server, scanned):
    """Even with the `X-Duh-Share` header, a foreign `Origin` is rejected
    (defense-in-depth: the header check alone should already stop a browser,
    but this covers any client that sends both)."""
    import urllib.error
    base, cap = gist_server
    root = json.load(_get(f"{base}/api/root"))
    with pytest.raises(urllib.error.HTTPError) as e:
        _share_get(f"{base}/api/share/{root['id']}", extra_headers={"Origin": "https://evil.example"})
    assert e.value.code == 403
    body = json.loads(e.value.read())
    assert body["error"] == "forbidden"
    assert not cap.exists()
