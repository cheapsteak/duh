import json, zlib, base64
import pytest
from conftest import DUH_BIN, EXPECT, approx
from test_serve import server, _get  # noqa: F401  (fixture + helper reuse)

rust_only = pytest.mark.skipif("target" not in str(DUH_BIN), reason="rust-only feature")

def _decode(fragment):
    assert fragment.startswith("1.")
    data = fragment[2:]
    pad = "=" * (-len(data) % 4)
    raw = zlib.decompress(base64.urlsafe_b64decode(data + pad))
    return json.loads(raw)

def _check_sums(node, errs, path=""):
    if len(node) < 3: return
    name, size, kids = node[0], node[1], node[2]
    s = sum(k[1] for k in kids)
    if abs(s - size) > max(size * 0.02, 2048):
        errs.append(f"{path}/{name}: {s} vs {size}")
    for k in kids:
        if k[0] != "*": _check_sums(k, errs, path + "/" + name)

@rust_only
def test_share_fragment_decodes_with_invariants(server, scanned):
    root = json.load(_get(f"{server}/api/root"))
    for budget in (1900, 8000):
        resp = json.load(_get(f"{server}/api/share/{root['id']}?budget={budget}"))
        assert resp["chars"] <= budget and resp["fragment"].startswith("1.")
        assert resp["url"].endswith("#" + resp["fragment"])
        doc = _decode(resp["fragment"])
        assert doc["v"] == 1 and doc["t"] == str(scanned.root)
        assert approx(doc["tot"], EXPECT["family_big"] + EXPECT["family_siblings"]
                      + EXPECT["hardlinks"] + EXPECT["unique"] + EXPECT["sparse_alloc"]
                      + (1 << 20), tol=2 << 20)
        errs = []; _check_sums(doc["n"], errs)
        assert not errs, errs

@rust_only
def test_share_clone_pair_not_double_counted(server):
    """siblings/x + siblings/y are clones of each other: their files are shared,
    so neither dir may show a file child at full size (the spike's Postgres bug)."""
    root = json.load(_get(f"{server}/api/root"))
    resp = json.load(_get(f"{server}/api/share/{root['id']}?budget=32000"))
    def find(node, name):
        for k in (node[2] if len(node) > 2 else []):
            if k[0] == name: return k
            r = find(k, name)
            if r: return r
    x = find(_decode(resp["fragment"])["n"], "x")
    if x and len(x) > 2:
        assert all(k[0] == "*" for k in x[2]), f"shared clone revealed at full size: {x[2]}"

@rust_only
def test_share_file_node_returns_400(server, scanned):
    """A share request against a FILE id is rejected: build_share only encodes
    directory subtrees. Per the Task 1 handoff, the known-node/None case maps
    to 400 (the budget message doubles as the catch-all client error)."""
    import sqlite3
    import urllib.error
    with sqlite3.connect(scanned.db) as con:
        (file_id,) = con.execute(
            "SELECT id FROM files WHERE name='u.bin'").fetchone()
    with pytest.raises(urllib.error.HTTPError) as e:
        _get(f"{server}/api/share/{file_id}?budget=8000")
    assert e.value.code == 400

@rust_only
def test_share_errors(server):
    import urllib.error
    with pytest.raises(urllib.error.HTTPError) as e:
        _get(f"{server}/api/share/999999999?budget=8000")
    assert e.value.code == 404
    with pytest.raises(urllib.error.HTTPError) as e:
        _get(f"{server}/api/share/1?budget=5")
    assert e.value.code == 400
