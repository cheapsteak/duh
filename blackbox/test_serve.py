"""Black-box tests for `duh serve` — the local web UI HTTP server.

`test_api_walk` asserts JSON shape parity and MUST pass against both the Python
oracle and the Rust binary. The remaining tests exercise Rust-only behaviour
(embedded static assets, the Host-header / DNS-rebinding guard) and are skipped
when DUH_BIN is the frozen Python oracle. The Rust release binary lives under
`target/`, the oracle at the repo root, so path membership discriminates them.
"""
import json
import socket
import subprocess
import time
import urllib.error
import urllib.request

import pytest

from conftest import DUH_BIN

rust_only = pytest.mark.skipif(
    "target" not in str(DUH_BIN),
    reason="rust-only behavior (embedded assets, Host-header guard)",
)


@pytest.fixture()
def server(scanned):
    port = _free_port()
    proc = subprocess.Popen(
        [str(DUH_BIN), "serve", "--port", str(port), "--no-browser"],
        env={"DUH_DB": str(scanned.db), "PATH": "/usr/bin:/bin", "HOME": "/tmp"},
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    _wait_port(port)
    yield f"http://127.0.0.1:{port}"
    proc.terminate(); proc.wait(timeout=10)


def _free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0)); return s.getsockname()[1]


def _wait_port(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), 0.2): return
        except OSError: time.sleep(0.2)
    raise TimeoutError


def _get(url, host=None):
    req = urllib.request.Request(url, headers={"Host": host} if host else {})
    return urllib.request.urlopen(req, timeout=10)


def test_api_walk(server, scanned):
    root = json.load(_get(f"{server}/api/root"))
    assert root["path"] == str(scanned.root)
    resp = json.load(_get(f"{server}/api/node/{root['id']}"))
    names = {c["name"] for c in resp["children"]}
    assert {"clones", "siblings", "unique"} <= names
    # /api/node wraps the node fields under "node" (oracle shape).
    node = resp["node"]
    assert node["total_blocks"] > 0 and node["freeable"] > 0


@rust_only
def test_index_and_assets_served(server):
    html = _get(f"{server}/").read().decode()
    assert "duh" in html


@rust_only
def test_dns_rebinding_rejected(server):
    with pytest.raises(urllib.error.HTTPError) as e:
        _get(f"{server}/api/root", host="evil.example.com")
    assert e.value.code == 403
