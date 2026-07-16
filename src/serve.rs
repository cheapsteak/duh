//! `duh serve` — local web UI HTTP server (port of the Python `_DuhHandler`
//! and `cmd_serve`, `reference/duh-py:2118-3018`).
//!
//! Startup order mirrors the oracle: build directory aggregates (`_build_dir_agg`),
//! compute the freeable/locked maps, seed a path cache, then bind a threaded
//! `tiny_http` server on `127.0.0.1`. The JSON emitted by every `/api/*` route is
//! byte-for-byte shape-compatible with the oracle (integers 0/1 for `is_dir` /
//! `is_excluded`, `null` for a root's `parent_id`, `freeable`/`locked_here`
//! defaulting to 0).
//!
//! Rust-only additions (deliberate, sanctioned): the static UI assets are
//! `include_bytes!`-embedded so the release binary is self-contained, and every
//! request is screened by a Host-header guard (DNS-rebinding protection).

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

use crate::freeable;
use crate::share;

/// Default base URL for share fragments, overridable via `DUH_SHARE_BASE`
/// (e.g. for local development against a different viewer deployment).
const DEFAULT_SHARE_BASE: &str = "https://cheapsteak.github.io/duh/v/";

// --- embedded static assets (Rust-only; the oracle serves inline HTML) --------
static INDEX_HTML: &[u8] = include_bytes!("../static/index.html");
static STYLE_CSS: &[u8] = include_bytes!("../static/style.css");
static APP_JS: &[u8] = include_bytes!("../static/app.js");
static TREEMAP_JS: &[u8] = include_bytes!("../static/treemap.js");
static ECHARTS_JS: &[u8] = include_bytes!("../static/vendor/echarts.min.js");

const NUM_THREADS: usize = 4;

/// Per-directory subtree aggregate (port of a `dir_agg` entry, `reference/duh-py:2128-2238`).
#[derive(Clone, Copy, Default)]
struct Agg {
    total_blocks: i64,
    total_logical: i64,
    total_files: i64,
}

/// Immutable server state shared across worker threads.
struct State {
    db_path: PathBuf,
    dir_agg: HashMap<i64, Agg>,
    freeable_map: HashMap<i64, u64>,
    locked_here_map: HashMap<i64, u64>,
    /// Memoized id -> full path (port of `_PathCache`, `reference/duh-py:2242-2267`).
    path_cache: Mutex<HashMap<i64, String>>,
    /// Latest scan root — statvfs'd per request so the header's free-space
    /// readout tracks deletions live without a rescan.
    scan_root: Option<PathBuf>,
    /// Clone-family qualification set for the share encoder (`share::multi_clone_set`),
    /// built lazily on the first `/api/share` request since it costs a full
    /// `files`/`excluded_families` scan that most `serve` sessions never need.
    multi_clone: OnceLock<HashSet<i64>>,
}

/// (free, total) bytes on the volume containing `path`, or None on failure.
fn disk_free_total(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frsize = s.f_frsize;
    Some((s.f_bavail as u64 * frsize, s.f_blocks as u64 * frsize))
}

/// Entry point for `duh serve`.
pub fn run(db_path: &Path, port: u16, no_browser: bool) -> ExitCode {
    // Open the DB read/write for the one-shot pre-compute pass (matches the
    // oracle, which opens normally then relies on PRAGMA query_only).
    let con = match crate::db::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot open database: {e}");
            return ExitCode::FAILURE;
        }
    };

    let dir_agg = match build_dir_agg(&con) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!("[serve] computing freeable metrics...");
    let (freeable_map, locked_here_map) = match freeable::compute(&con) {
        Ok(maps) => maps,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let scan_root: Option<PathBuf> = con
        .query_row("SELECT root FROM scans ORDER BY id DESC LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .map(PathBuf::from);
    drop(con);

    let state = Arc::new(State {
        db_path: db_path.to_path_buf(),
        dir_agg,
        freeable_map,
        locked_here_map,
        path_cache: Mutex::new(HashMap::new()),
        scan_root,
        multi_clone: OnceLock::new(),
    });

    // Bind, trying up to port+10 (port range fallback, `reference/duh-py:2988-2997`).
    let max_port = port.saturating_add(10);
    let mut server = None;
    let mut bound_port = port;
    let mut p = port;
    while p < max_port {
        match Server::http(("127.0.0.1", p)) {
            Ok(s) => {
                server = Some(s);
                bound_port = p;
                break;
            }
            Err(_) => p += 1,
        }
    }
    let server = match server {
        Some(s) => Arc::new(s),
        None => {
            eprintln!("error: could not bind to any port in range {port}–{}", max_port - 1);
            return ExitCode::FAILURE;
        }
    };

    let url = format!("http://127.0.0.1:{bound_port}/");
    eprintln!("[serve] listening on {url}");
    println!("{url}"); // stdout for scripts (mirrors the oracle)

    // Auto-open the browser unless suppressed. Non-fatal on failure (a sanctioned
    // improvement over the oracle, which would crash if `open` were missing).
    if !no_browser {
        let _ = std::process::Command::new("open")
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    // Small thread pool: each worker owns a read-only connection and pulls
    // requests off the shared server.
    let mut handles = Vec::with_capacity(NUM_THREADS);
    for _ in 0..NUM_THREADS {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || worker(&server, &state)));
    }
    for h in handles {
        let _ = h.join();
    }
    ExitCode::SUCCESS
}

/// A worker thread: open one per-thread connection, then serve requests forever.
fn worker(server: &Server, state: &State) {
    let con = match open_thread_con(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[serve] worker failed to open DB: {e}");
            return;
        }
    };
    for req in server.incoming_requests() {
        // Compute the response inside catch_unwind so a panicking handler can't
        // kill this worker and permanently shrink the pool. AssertUnwindSafe is
        // sound here: the closure only borrows `con` (read-only connection;
        // rusqlite finalizes statements during unwinding) and `state`, whose
        // sole interior mutability is the path-cache Mutex — that Mutex poisons
        // on panic and `path_for` recovers the poison explicitly, and cache
        // entries are complete Strings inserted atomically, so no handler can
        // observe a broken invariant after a panic.
        let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            route(&con, state, req.url(), req.headers())
        }));
        let (status, body, content_type) = dispatched.unwrap_or_else(|_| {
            eprintln!("[serve] handler panicked; responding 500");
            json_response(&json!({"error": "internal error"}), 500)
        });
        let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
            .expect("valid content-type header");
        let response = Response::new(
            tiny_http::StatusCode(status),
            vec![header],
            Cursor::new(body),
            None,
            None,
        );
        let _ = req.respond(response);
    }
}

/// Open a per-thread connection: read-only at the file level, plus PRAGMA
/// query_only and a large page cache, mirroring the oracle's per-thread
/// connections (`reference/duh-py:2795-2805`).
fn open_thread_con(db_path: &Path) -> rusqlite::Result<Connection> {
    let con = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    con.pragma_update(None, "query_only", "ON")?;
    con.pragma_update(None, "cache_size", -262144)?;
    // Wait instead of instantly failing SQLITE_BUSY during a concurrent scan
    // (matches Python's sqlite3 default busy timeout).
    con.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(con)
}

/// Allowed `Host` header values for the DNS-rebinding guard. Fails closed: an
/// absent or empty Host header is rejected. Hostname comparison is
/// case-insensitive (RFC 4343), so e.g. `LOCALHOST:7777` is accepted.
fn host_allowed(host: &str) -> bool {
    let h = match host.split_once(':') {
        Some((h, _port)) => h,
        None => host,
    };
    let h = h.to_ascii_lowercase();
    h == "localhost" || h == "127.0.0.1"
}

/// Route one request. Returns `(status, body, content_type)`.
fn route(
    con: &Connection,
    state: &State,
    raw_url: &str,
    headers: &[Header],
) -> (u16, Vec<u8>, &'static str) {
    // Host-header guard applies to ALL routes.
    let host = headers
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str())
        .unwrap_or("");
    if !host_allowed(host) {
        return (403, b"403 Forbidden: bad Host header\n".to_vec(), "text/plain; charset=utf-8");
    }

    // Strip query string, then trailing slashes (`path.rstrip("/") or "/"`).
    let path = raw_url.split('?').next().unwrap_or(raw_url);
    let trimmed = path.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };

    match path {
        "/" | "/index.html" => (200, INDEX_HTML.to_vec(), "text/html; charset=utf-8"),
        "/style.css" => (200, STYLE_CSS.to_vec(), "text/css; charset=utf-8"),
        "/app.js" => (200, APP_JS.to_vec(), "application/javascript"),
        "/treemap.js" => (200, TREEMAP_JS.to_vec(), "application/javascript"),
        "/vendor/echarts.min.js" => (200, ECHARTS_JS.to_vec(), "application/javascript"),
        "/api/root" => json_result(api_root(con)),
        _ if path.starts_with("/api/node/") => {
            dispatch_id(&path["/api/node/".len()..], |id| api_node(con, state, id))
        }
        _ if path.starts_with("/api/marginal/") => {
            dispatch_id(&path["/api/marginal/".len()..], |id| api_marginal(con, id))
        }
        _ if path.starts_with("/api/breadcrumb/") => {
            dispatch_id(&path["/api/breadcrumb/".len()..], |id| api_breadcrumb(con, id))
        }
        _ if path.starts_with("/api/share/") => {
            let query = raw_url.split_once('?').map(|(_, q)| q).unwrap_or("");
            dispatch_id(&path["/api/share/".len()..], |id| api_share(con, state, id, query))
        }
        _ => json_response(&json!({"error": "not found"}), 404),
    }
}

/// Parse an id path segment and dispatch, mapping a bad id to 400 (the oracle's
/// `except ValueError`) and any handler error to 500.
fn dispatch_id<F>(seg: &str, f: F) -> (u16, Vec<u8>, &'static str)
where
    F: FnOnce(i64) -> ApiResult,
{
    match seg.parse::<i64>() {
        Ok(id) => json_result(f(id)),
        Err(_) => json_response(&json!({"error": "invalid id"}), 400),
    }
}

/// An API handler's outcome: either a JSON value (200) or an explicit
/// `(status, error-json)` pair (404 for not-found).
type ApiResult = rusqlite::Result<Result<Value, (u16, Value)>>;

fn json_result(r: ApiResult) -> (u16, Vec<u8>, &'static str) {
    match r {
        Ok(Ok(value)) => json_response(&value, 200),
        Ok(Err((status, value))) => json_response(&value, status),
        Err(e) => json_response(&json!({"error": e.to_string()}), 500),
    }
}

fn json_response(value: &Value, status: u16) -> (u16, Vec<u8>, &'static str) {
    (status, serde_json::to_vec(value).unwrap_or_default(), "application/json")
}

// --- handlers ----------------------------------------------------------------

/// `GET /api/root` — `{id, path, name}` for the most recent scan's root.
fn api_root(con: &Connection) -> ApiResult {
    let root_path: Option<String> = con
        .query_row("SELECT root FROM scans ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
        .ok();
    let Some(root_path) = root_path else {
        return Ok(Err((404, json!({"error": "no scans"}))));
    };
    let id: Option<i64> = con
        .query_row(
            "SELECT id FROM files WHERE parent_id IS NULL AND name = ?",
            [&root_path],
            |r| r.get(0),
        )
        .ok();
    let Some(id) = id else {
        return Ok(Err((404, json!({"error": "root node not found"}))));
    };
    Ok(Ok(json!({"id": id, "path": root_path, "name": root_path})))
}

/// `GET /api/node/{id}` — node info + immediate children (`reference/duh-py:2860-2929`).
fn api_node(con: &Connection, state: &State, node_id: i64) -> ApiResult {
    let node = con
        .query_row(
            "SELECT id, parent_id, name, is_dir, is_excluded FROM files WHERE id = ?",
            [node_id],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(1)?, // parent_id
                    r.get::<_, String>(2)?,      // name
                    r.get::<_, i64>(3)?,         // is_dir
                    r.get::<_, i64>(4)?,         // is_excluded
                ))
            },
        )
        .ok();
    let Some((parent_id, name, is_dir, is_excluded)) = node else {
        return Ok(Err((404, json!({"error": "not found"}))));
    };

    let agg = state.dir_agg.get(&node_id).copied().unwrap_or_default();
    let node_path = path_for(con, state, node_id)?;
    let disk = state.scan_root.as_deref().and_then(disk_free_total);

    let node_info = json!({
        "id": node_id,
        "path": node_path,
        "name": name,
        "is_dir": is_dir,
        "is_excluded": is_excluded,
        "total_blocks": agg.total_blocks,
        "total_logical": agg.total_logical,
        "total_files": agg.total_files,
        "parent_id": parent_id,
        "freeable": state.freeable_map.get(&node_id).copied().unwrap_or(0),
        "locked_here": state.locked_here_map.get(&node_id).copied().unwrap_or(0),
        "disk_free": disk.map(|d| d.0),
        "disk_total": disk.map(|d| d.1),
    });

    // Immediate children, in the DB's natural (parent-index) order.
    struct Child {
        id: i64,
        name: String,
        is_dir: i64,
        is_excluded: i64,
        total_blocks: i64,
        total_logical: i64,
        total_files: i64,
        freeable: u64,
        clone_id: Option<i64>,
        nlinks: i64,
        shared: bool,
    }
    let mut stmt = con.prepare(
        "SELECT id, name, is_dir, is_excluded, size_blocks, size_logical, excluded_file_count, \
                clone_id, nlinks \
         FROM files WHERE parent_id = ?",
    )?;
    let rows = stmt.query_map([node_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,           // id
            r.get::<_, String>(1)?,        // name
            r.get::<_, i64>(2)?,           // is_dir
            r.get::<_, i64>(3)?,           // is_excluded
            r.get::<_, i64>(4)?,           // size_blocks
            r.get::<_, i64>(5)?,           // size_logical
            r.get::<_, Option<i64>>(6)?,   // excluded_file_count
            r.get::<_, Option<i64>>(7)?,   // clone_id
            r.get::<_, i64>(8)?,           // nlinks
        ))
    })?;

    let mut children: Vec<Child> = Vec::new();
    for row in rows {
        let (cid, cname, c_is_dir, c_is_excluded, size_blocks, size_logical, excl_count, clone_id, nlinks) =
            row?;
        let (total_blocks, total_logical, total_files) = if c_is_dir != 0 && c_is_excluded == 0 {
            let cagg = state.dir_agg.get(&cid).copied().unwrap_or_default();
            (cagg.total_blocks, cagg.total_logical, cagg.total_files)
        } else if c_is_excluded != 0 {
            (size_blocks, size_logical, excl_count.unwrap_or(0))
        } else {
            (size_blocks, size_logical, 1)
        };
        // Dirs get their computed freeable. Files get a PROVISIONAL freeable of
        // size_blocks (the compute engine only produces per-directory values;
        // a file's blocks are credited to its parent's bucket). The exact value
        // (0 when the blocks are also held by a clone twin or hardlink) is
        // resolved below, but only for the rows that survive the 200-cap, so a
        // 100k-file directory doesn't pay 100k clone-family lookups per click.
        let freeable = if c_is_dir != 0 {
            state.freeable_map.get(&cid).copied().unwrap_or(0)
        } else {
            size_blocks.max(0) as u64
        };
        children.push(Child {
            id: cid,
            name: cname,
            is_dir: c_is_dir,
            is_excluded: c_is_excluded,
            total_blocks,
            total_logical,
            total_files,
            freeable,
            clone_id,
            nlinks,
            shared: false,
        });
    }

    // Stable sort by freeable desc, limit 200 (`reference/duh-py:2926-2927`). Stability
    // keeps the DB order for ties, matching Python's stable sort. Deliberate deviation
    // from the reference: file rows sort by their provisional (unshared) size, so a
    // large clone-shared file stays visible near the top wearing a "shared" tag
    // instead of sinking below every tiny unique file as a bare 0.
    children.sort_by(|a, b| b.freeable.cmp(&a.freeable));
    children.truncate(200);

    // Resolve exact file freeable for the survivors: blocks also reachable via
    // another path (hardlink, clone twin among files, or a twin inside an
    // excluded aggregate) mean deleting this one path frees nothing.
    {
        let mut shared_stmt = con.prepare(
            "SELECT EXISTS(SELECT 1 FROM files WHERE clone_id = ?1 AND id != ?2 AND is_dir=0) \
                 OR EXISTS(SELECT 1 FROM excluded_families WHERE clone_id = ?1)",
        )?;
        for c in children.iter_mut() {
            if c.is_dir != 0 {
                continue;
            }
            let clone_shared = match c.clone_id {
                Some(cl) => shared_stmt.query_row((cl, c.id), |r| r.get::<_, i64>(0))? != 0,
                None => false,
            };
            if c.nlinks > 1 || clone_shared {
                c.shared = true;
                c.freeable = 0;
            }
        }
    }

    let children_json: Vec<Value> = children
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "name": c.name,
                "is_dir": c.is_dir,
                "is_excluded": c.is_excluded,
                "total_blocks": c.total_blocks,
                "total_logical": c.total_logical,
                "total_files": c.total_files,
                "freeable": c.freeable,
                "locked_here": state.locked_here_map.get(&c.id).copied().unwrap_or(0),
                "shared": if c.shared { 1 } else { 0 },
            })
        })
        .collect();

    Ok(Ok(json!({"node": node_info, "children": children_json})))
}

/// `GET /api/marginal/{id}` — marginal freeable for a subtree (`reference/duh-py:2931-2938`).
fn api_marginal(con: &Connection, node_id: i64) -> ApiResult {
    let exists: Option<i64> =
        con.query_row("SELECT id FROM files WHERE id = ?", [node_id], |r| r.get(0)).ok();
    if exists.is_none() {
        return Ok(Err((404, json!({"error": "not found"}))));
    }
    let t0 = Instant::now();
    let result = freeable::compute_marginal_freeable(con, node_id)?;
    let duration_ms = t0.elapsed().as_millis() as i64;
    Ok(Ok(json!({
        "id": node_id,
        "strict_bytes": result.strict_bytes,
        "proportional_bytes": result.strict_bytes, // backward compat
        "apparent_bytes": result.apparent_bytes,
        "duration_ms": duration_ms,
    })))
}

/// `GET /api/breadcrumb/{id}` — root-first chain of `{id, name}` (`reference/duh-py:2940-2953`).
fn api_breadcrumb(con: &Connection, node_id: i64) -> ApiResult {
    let mut stmt = con.prepare(
        "WITH RECURSIVE chain(id, parent_id, name) AS ( \
           SELECT id, parent_id, name FROM files WHERE id = ? \
           UNION ALL \
           SELECT f.id, f.parent_id, f.name FROM files f, chain c WHERE f.id = c.parent_id ) \
         SELECT id, name FROM chain",
    )?;
    let rows = stmt.query_map([node_id], |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?}))
    })?;
    let mut crumbs: Vec<Value> = rows.collect::<rusqlite::Result<_>>()?;
    crumbs.reverse();
    Ok(Ok(Value::Array(crumbs)))
}

/// `GET /api/share/{id}?budget=N` — a shareable URL fragment for the subtree
/// rooted at `id`, sized to fit `budget` characters (default 8000, clamped to
/// `100..=100000`).
///
/// Node existence is checked up front (same query pattern as [`api_node`]) so
/// unknown ids map to 404, and its `is_dir` is captured for later.
/// [`share::build_share`] returns `None` for either a non-directory node or a
/// budget too small to fit even the bare root; since existence is already
/// confirmed, a `None` is disambiguated by the captured `is_dir`: a file node
/// gets a distinct "not a shareable directory" 400, while a too-small budget
/// gets the generic budget message (in practice unreachable above the
/// validated 100-char floor, since the bare-root fragment is far smaller, but
/// the mapping is correct regardless).
fn api_share(con: &Connection, state: &State, node_id: i64, query: &str) -> ApiResult {
    let budget: usize = match query_param(query, "budget") {
        None => 8000,
        Some(s) => match s.parse::<usize>() {
            Ok(b) if (100..=100_000).contains(&b) => b,
            _ => return Ok(Err((400, json!({"error": "budget must be 100..=100000"})))),
        },
    };

    let node_is_dir: Option<i64> =
        con.query_row("SELECT is_dir FROM files WHERE id = ?", [node_id], |r| r.get(0)).ok();
    let Some(node_is_dir) = node_is_dir else {
        return Ok(Err((404, json!({"error": "not found"}))));
    };

    // Built once per server lifetime, on the first share request: a full
    // `files`/`excluded_families` scan that most `serve` sessions never need.
    // Computed to a `Result` first so a query error (e.g. SQLITE_BUSY under a
    // concurrent scan) never caches an empty set forever — that would make
    // every future share double-count clone twins. Only a successful build is
    // stored in the `OnceLock`; on error this request 500s and a later
    // request gets to retry the build from scratch.
    let multi_clone: &HashSet<i64> = match state.multi_clone.get() {
        Some(set) => set,
        None => {
            let t0 = Instant::now();
            match share::multi_clone_set(con) {
                Ok(set) => {
                    eprintln!(
                        "[serve] clone-family set built in {:.1}s ({} families)",
                        t0.elapsed().as_secs_f64(),
                        set.len()
                    );
                    // Another thread may have won the race to set() first;
                    // either way `get()` now returns a built set.
                    let _ = state.multi_clone.set(set);
                    state.multi_clone.get().expect("just set or set by a racing thread")
                }
                Err(e) => {
                    return Ok(Err((
                        500,
                        json!({"error": format!("could not build clone-family set: {e}")}),
                    )));
                }
            }
        }
    };

    let node_path = path_for(con, state, node_id)?;
    let started_at: Option<f64> = con
        .query_row("SELECT started_at FROM scans ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
        .ok();
    let scan_date = epoch_to_ymd(started_at.unwrap_or(0.0));

    let inp = share::ShareInput {
        con,
        freeable_map: &state.freeable_map,
        multi_clone,
    };
    let Some(result) = share::build_share(&inp, node_id, &node_path, &scan_date, budget)? else {
        if node_is_dir == 0 {
            return Ok(Err((400, json!({"error": "not a shareable directory"}))));
        }
        return Ok(Err((400, json!({"error": "budget must be 100..=100000"}))));
    };

    let base = std::env::var("DUH_SHARE_BASE").unwrap_or_else(|_| DEFAULT_SHARE_BASE.to_string());
    Ok(Ok(json!({
        "fragment": result.fragment,
        "url": format!("{base}#{}", result.fragment),
        "nodes": result.nodes,
        "chars": result.chars,
    })))
}

/// Look up `key`'s value in a raw (unescaped) query string like `a=1&budget=2`.
/// Only ever fed numeric query params, so no percent-decoding is needed.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Convert a Unix epoch timestamp (seconds, UTC) to a `YYYY-MM-DD` string.
/// `chrono` is not a dependency; this is Howard Hinnant's `civil_from_days`
/// (http://howardhinnant.github.io/date_algorithms.html), a small integer-only
/// algorithm for the proleptic Gregorian calendar.
fn epoch_to_ymd(epoch_secs: f64) -> String {
    let days = (epoch_secs / 86400.0).floor() as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the Unix epoch (1970-01-01) -> (year, month, day), proleptic
/// Gregorian. Port of Hinnant's `civil_from_days`; correct for the full `i64`
/// range the algorithm supports (no leap-second handling, as epoch seconds
/// don't carry them either).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Memoized full path for a node id (port of `_PathCache.get`).
///
/// A poisoned lock is recovered with `into_inner()`: cache entries are complete
/// Strings inserted atomically under the lock, so a panic elsewhere can never
/// leave the map in a half-written state.
fn path_for(con: &Connection, state: &State, id: i64) -> rusqlite::Result<String> {
    fn lock(state: &State) -> std::sync::MutexGuard<'_, HashMap<i64, String>> {
        state.path_cache.lock().unwrap_or_else(|e| e.into_inner())
    }
    if let Some(p) = lock(state).get(&id) {
        return Ok(p.clone());
    }
    let path = freeable::full_path(con, id)?;
    lock(state).insert(id, path.clone());
    Ok(path)
}

// --- directory aggregates (port of `_build_dir_agg`, `reference/duh-py:2128-2238`) -------

/// A single `files` row, loaded once for the bottom-up aggregation pass.
struct AggRow {
    id: i64,
    parent_id: Option<i64>,
    is_dir: bool,
    is_excluded: bool,
    size_blocks: i64,
    size_logical: i64,
    excluded_file_count: i64,
}

/// Walk the DB bottom-up and build `dir_agg[id]` for every directory. Excluded
/// subtree rows are treated as pre-summed leaves. Faithful port of the oracle's
/// iterative post-order DFS (`reference/duh-py:2168-2234`).
fn build_dir_agg(con: &Connection) -> rusqlite::Result<HashMap<i64, Agg>> {
    let t0 = Instant::now();
    eprintln!("[serve] pre-computing directory aggregates...");

    let mut stmt = con.prepare(
        "SELECT id, parent_id, is_dir, is_excluded, size_blocks, size_logical, \
                excluded_file_count FROM files",
    )?;
    let rows: Vec<AggRow> = stmt
        .query_map([], |r| {
            Ok(AggRow {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                is_dir: r.get::<_, i64>(2)? != 0,
                is_excluded: r.get::<_, i64>(3)? != 0,
                size_blocks: r.get(4)?,
                size_logical: r.get(5)?,
                excluded_file_count: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    eprintln!("[serve]   loaded {} rows in {:.1}s", rows.len(), t0.elapsed().as_secs_f64());

    // Index rows by id and build parent -> child-index adjacency.
    let mut index_of: HashMap<i64, usize> = HashMap::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        index_of.insert(r.id, i);
    }
    let mut children_of: HashMap<i64, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match r.parent_id {
            Some(pid) => children_of.entry(pid).or_default().push(i),
            None => roots.push(i),
        }
    }

    let mut dir_agg: HashMap<i64, Agg> = HashMap::new();

    // Iterative post-order DFS. phase 0 = descend/push children; phase 1 = aggregate.
    let mut stack: Vec<(usize, u8)> = roots.iter().map(|&i| (i, 0u8)).collect();
    while let Some((idx, phase)) = stack.pop() {
        let row = &rows[idx];
        if phase == 0 {
            if row.is_dir && !row.is_excluded {
                stack.push((idx, 1));
                if let Some(kids) = children_of.get(&row.id) {
                    for &c in kids {
                        stack.push((c, 0));
                    }
                }
            } else if row.is_excluded {
                // Excluded subtree: pre-summed leaf aggregate.
                dir_agg.insert(
                    row.id,
                    Agg {
                        total_blocks: row.size_blocks,
                        total_logical: row.size_logical,
                        total_files: row.excluded_file_count,
                    },
                );
            }
            // Regular leaf files contribute in their parent's phase 1.
        } else {
            let mut agg = Agg::default();
            if let Some(kids) = children_of.get(&row.id) {
                for &ci in kids {
                    let child = &rows[ci];
                    if child.is_dir && !child.is_excluded {
                        if let Some(sub) = dir_agg.get(&child.id) {
                            agg.total_blocks += sub.total_blocks;
                            agg.total_logical += sub.total_logical;
                            agg.total_files += sub.total_files;
                        }
                    } else if child.is_excluded {
                        agg.total_blocks += child.size_blocks;
                        agg.total_logical += child.size_logical;
                        agg.total_files += child.excluded_file_count;
                    } else {
                        agg.total_blocks += child.size_blocks;
                        agg.total_logical += child.size_logical;
                        agg.total_files += 1;
                    }
                }
            }
            dir_agg.insert(row.id, agg);
        }
    }

    eprintln!(
        "[serve] pre-compute done: {} dirs in {:.1}s",
        dir_agg.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(dir_agg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_to_ymd_known_dates() {
        assert_eq!(epoch_to_ymd(0.0), "1970-01-01");
        // date -u -r 1752537600 +%F => 2025-07-15
        assert_eq!(epoch_to_ymd(1_752_537_600.0), "2025-07-15");
    }

    #[test]
    fn host_guard_allows_local_and_rejects_others() {
        assert!(host_allowed("localhost"));
        assert!(host_allowed("localhost:7777"));
        assert!(host_allowed("LOCALHOST:7777")); // hostnames are case-insensitive
        assert!(host_allowed("127.0.0.1"));
        assert!(host_allowed("127.0.0.1:65000"));
        assert!(!host_allowed("")); // absent/empty Host fails closed
        assert!(!host_allowed("evil.example.com"));
        assert!(!host_allowed("evil.example.com:7777"));
        assert!(!host_allowed("169.254.1.1"));
    }

    /// End-to-end aggregate build against a tiny in-memory tree, exercising the
    /// dir/excluded/leaf branches of `build_dir_agg`.
    #[test]
    fn build_dir_agg_matches_hand_computed() {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch(crate::db::SCHEMA).unwrap();
        con.execute(
            "INSERT INTO scans(id, root, started_at) VALUES (1, '/r', 0)",
            [],
        )
        .unwrap();
        // id 1: root dir; 2: subdir; 3: file under subdir; 4: excluded dir under root.
        // Placeholders map to: id, parent_id, name, is_dir, is_excluded, ino,
        // size_logical, size_blocks, excluded_file_count (9 values per row).
        let ins = "INSERT INTO files(id, parent_id, name, is_dir, is_symlink, is_excluded, \
                   dev, ino, nlinks, size_logical, size_blocks, excluded_file_count, mtime, scan_id) \
                   VALUES (?,?,?,?,0,?,1,?,1,?,?,?,0,1)";
        let none = Option::<i64>::None;
        con.execute(ins, rusqlite::params![1, none, "/r", 1, 0, 10, 0, 0, none]).unwrap();
        con.execute(ins, rusqlite::params![2, 1, "sub", 1, 0, 11, 0, 0, none]).unwrap();
        con.execute(ins, rusqlite::params![3, 2, "f.bin", 0, 0, 12, 100, 40, none]).unwrap();
        con.execute(ins, rusqlite::params![4, 1, "node_modules", 1, 1, 13, 200, 50, 7]).unwrap();

        let agg = build_dir_agg(&con).unwrap();
        // subdir aggregates its one file.
        assert_eq!(agg[&2].total_blocks, 40);
        assert_eq!(agg[&2].total_logical, 100);
        assert_eq!(agg[&2].total_files, 1);
        // excluded dir stored as pre-summed leaf.
        assert_eq!(agg[&4].total_blocks, 50);
        assert_eq!(agg[&4].total_files, 7);
        // root = subdir subtree + excluded leaf.
        assert_eq!(agg[&1].total_blocks, 40 + 50);
        assert_eq!(agg[&1].total_logical, 100 + 200);
        assert_eq!(agg[&1].total_files, 1 + 7);
    }
}
