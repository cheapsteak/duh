"""Black-box tests for `duh serve` — the local web UI HTTP server.

`test_api_walk` asserts JSON shape parity and MUST pass against both the Python
oracle and the Rust binary. The remaining tests exercise Rust-only behaviour
(embedded static assets, the Host-header / DNS-rebinding guard) and are skipped
when DUH_BIN is the frozen Python oracle. The Rust release binary lives under
`target/`, the oracle at `reference/duh-py`, so path membership discriminates them.
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


@rust_only
def test_missing_host_rejected(server):
    """Host guard fails closed: a request without any Host header gets 403.

    urllib always adds a Host header, so speak raw HTTP over a socket.
    """
    port = int(server.rsplit(":", 1)[1])
    with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
        s.settimeout(5)
        s.sendall(b"GET /api/root HTTP/1.0\r\n\r\n")
        resp = b""
        while b"\r\n" not in resp:
            chunk = s.recv(4096)
            if not chunk:
                break
            resp += chunk
    status_line = resp.split(b"\r\n", 1)[0].decode()
    assert status_line.split()[1] == "403", status_line


@rust_only
def test_static_asset_content_types(server):
    expected = {
        "/style.css": "text/css",
        "/app.js": "application/javascript",
        "/vendor/echarts.min.js": "application/javascript",
    }
    for path, ctype in expected.items():
        resp = _get(f"{server}{path}")
        assert resp.status == 200, path
        assert resp.headers["Content-Type"].startswith(ctype), (
            path, resp.headers["Content-Type"])
        assert resp.read(), path  # non-empty body
