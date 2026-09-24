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

from conftest import DUH_BIN, IS_MACOS

# The browser opener `duh serve` invokes: `open` on macOS, `xdg-open` on Linux.
OPENER = "open" if IS_MACOS else "xdg-open"

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
    assert "share-btn" in html
    assert "share-dialog" in html


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
def test_file_children_freeable_semantics(server):
    """File rows carry real freeable values (rust improvement over the oracle,
    which reported 0 for every file): a unique file's freeable is its allocated
    size; a clone with a twin elsewhere is 0 and tagged shared."""
    root = json.load(_get(f"{server}/api/root"))

    def child(node_id, name):
        resp = json.load(_get(f"{server}/api/node/{node_id}"))
        return next(c for c in resp["children"] if c["name"] == name)

    unique_dir = child(root["id"], "unique")
    u_bin = child(unique_dir["id"], "u.bin")
    assert u_bin["shared"] == 0
    assert u_bin["freeable"] == u_bin["total_blocks"] > 0

    siblings = child(root["id"], "siblings")
    x_dir = child(siblings["id"], "x")
    data_bin = child(x_dir["id"], "data.bin")  # clone twin lives in siblings/y
    assert data_bin["shared"] == 1
    assert data_bin["freeable"] == 0
    assert data_bin["total_blocks"] > 0


def _serve_with_open_stub(scanned, tmp_path, *extra_args, display=True):
    """Start `duh serve` with a stub opener (`open` / `xdg-open`) shadowing the
    real one on PATH (so no actual browser is ever launched) and return
    (proc, port, log). On Linux `display` controls whether DISPLAY is set."""
    stub_dir = tmp_path / "bin"
    stub_dir.mkdir()
    log = tmp_path / "open.log"
    stub = stub_dir / OPENER
    stub.write_text(f'#!/bin/sh\necho "$@" >> {log}\n')
    stub.chmod(0o755)
    port = _free_port()
    env = {"DUH_DB": str(scanned.db), "PATH": f"{stub_dir}:/usr/bin:/bin",
           "HOME": "/tmp"}
    if display and not IS_MACOS:
        env["DISPLAY"] = ":0"
    proc = subprocess.Popen(
        [str(DUH_BIN), "serve", "--port", str(port), *extra_args],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    return proc, port, log


@rust_only
def test_serve_auto_opens_browser(scanned, tmp_path):
    proc, port, log = _serve_with_open_stub(scanned, tmp_path)
    try:
        _wait_port(port)
        deadline = time.time() + 10
        while time.time() < deadline and not log.exists():
            time.sleep(0.1)
        assert log.exists(), f"`{OPENER}` was never invoked"
        assert f"http://127.0.0.1:{port}/" in log.read_text()
    finally:
        proc.terminate(); proc.wait(timeout=10)


@rust_only
@pytest.mark.skipif(IS_MACOS, reason="headless handling is Linux-only")
def test_headless_linux_prints_url_instead_of_opening(scanned, tmp_path):
    """No DISPLAY/WAYLAND_DISPLAY (an SSH session on a server): the opener is
    never invoked, the server still comes up, and stderr carries the URL."""
    proc, port, log = _serve_with_open_stub(scanned, tmp_path, display=False)
    try:
        _wait_port(port)
        time.sleep(0.5)
        assert not log.exists(), log.read_text()
        assert proc.poll() is None, "serve must keep running without a display"
    finally:
        proc.terminate()
        _, err = proc.communicate(timeout=10)
    assert "no display detected" in err
    assert f"http://127.0.0.1:{port}/" in err


@rust_only
def test_no_browser_suppresses_open(scanned, tmp_path):
    proc, port, log = _serve_with_open_stub(scanned, tmp_path, "--no-browser")
    try:
        _wait_port(port)
        time.sleep(0.5)  # would-be `open` happens before the listener loop
        assert not log.exists(), log.read_text()
    finally:
        proc.terminate(); proc.wait(timeout=10)


@rust_only
def test_static_asset_content_types(server):
    expected = {
        "/style.css": "text/css",
        "/app.js": "application/javascript",
        "/treemap.js": "application/javascript",
        "/vendor/echarts.min.js": "application/javascript",
    }
    for path, ctype in expected.items():
        resp = _get(f"{server}{path}")
        assert resp.status == 200, path
        assert resp.headers["Content-Type"].startswith(ctype), (
            path, resp.headers["Content-Type"])
        assert resp.read(), path  # non-empty body
