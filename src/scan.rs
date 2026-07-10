//! Parallel scanner — a faithful port of the Python oracle's `cmd_scan`
//! (`./duh:540-837`), `walk_for_aggregate` (`./duh:436-484`), and the batch/insert
//! SQL (`./duh:489-537`), restructured into N worker threads + one writer thread
//! (Task 10). The single-threaded control flow it replaces was itself a
//! line-for-line port; each entry is still classified the same way.
//!
//! Parity target: the databases written here must be identical in *content*
//! (compared by path, not by row id) to the Python scanner. Deviations are
//! documented in the task report.
//!
//! Threading model:
//!   - The **writer** thread owns the rusqlite `Connection` and is the *only*
//!     thread that touches the DB during the scan body. It receives `Msg` batches
//!     over a `crossbeam_channel::bounded(64)`, groups ~`BATCH_SIZE` rows per
//!     transaction, and hosts the 5s progress line, the 10s disk guard, and SIGINT
//!     translation (it has natural access to the running counters).
//!   - **Worker** threads share a `Coord` work queue of `(parent_id, PathBuf,
//!     rel_path)` items. Each pops a directory, reads its entries, and for every
//!     child preassigns an id and emits a row; subdirectories are pushed back onto
//!     the queue. Excluded-subtree aggregation (`walk_for_aggregate`) and the
//!     per-directory clone-id syscall run in whichever worker hit them.
//!
//! IDs: unlike Python (which relies on SQLite autoincrement / `lastrowid`), we
//! allocate ids from an explicit monotonic `AtomicI64` counter seeded from
//! `SELECT COALESCE(MAX(id),0) FROM files`, and insert with an explicit `id`. This
//! is what lets a worker reference a child's `parent_id` before the parent row has
//! been written: a directory row is always *sent* before that directory is pushed
//! onto the work queue, so no worker ever emits a child of an unsent parent.
//! Content is unaffected because the acceptance suite compares by path.
//!
//! FK / ordering note: the writer disables `foreign_keys` for the scan body. The
//! `UNIQUE(parent_id, name)` constraint can never fire within one scan (a
//! directory cannot hold two entries of the same name, and roots have a NULL,
//! distinct-under-UNIQUE parent), so `INSERT OR IGNORE` never resolves a conflict
//! and every preassigned id is used verbatim — insert *order* is therefore
//! irrelevant to row content. Disabling FK enforcement removes the only remaining
//! ordering hazard (a child batch reaching the DB before its parent's row), which
//! is otherwise merely a matter of cross-thread arrival order. Referential
//! integrity is still guaranteed structurally (every emitted child's parent row is
//! emitted first) and is checked by the parity harness' path reconstruction.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, ToSql};

use crate::attrs::{self, EntryAttrs};
use crate::excludes::Excludes;

const BATCH_SIZE: usize = 1000;
const PROGRESS_INTERVAL: f64 = 5.0;
const DISK_CHECK_INTERVAL: f64 = 10.0;
const EX_TEMPFAIL: u8 = 75;

/// Parsed CLI arguments for the `scan` subcommand (mirrors the Python argparse
/// namespace fields consumed by `cmd_scan`).
pub struct ScanArgs {
    pub path: String,
    pub quiet: bool,
    pub no_clones: bool,
    pub rescan: bool,
    pub cross_device: bool,
    pub min_free: f64,
    pub exclude: Vec<String>,
    pub include: Vec<String>,
    pub no_default_excludes: bool,
}

/// SIGINT flag. Set by the async-signal handler, polled per directory / per entry
/// exactly like the Python `interrupted` nonlocal (`./duh:621-625`).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Bind raw bytes as a SQLite TEXT value (not BLOB). rusqlite binds `&[u8]` as a
/// BLOB, which would (a) never compare equal to a text `name = ?` lookup and
/// (b) diverge from the Python oracle's TEXT column. Storing the OsString's raw
/// bytes as TEXT gives byte-for-byte parity, including Python's surrogateescape
/// round-trip for UTF-8 names (the common case; genuinely undecodable names make
/// the Python oracle itself raise on insert, so there is no parity to preserve).
struct RawText<'a>(&'a [u8]);

impl ToSql for RawText<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0)))
    }
}

/// Column list shared by file and dir inserts, with an explicit `id` (see module
/// docs). `OR IGNORE` mirrors `_INSERT_FILE_SQL` / `_INSERT_DIR_SQL`
/// (`./duh:493-506`), which lean on the `UNIQUE(parent_id, name)` constraint.
const INSERT_SQL: &str = "\
INSERT OR IGNORE INTO files
  (id, parent_id, name, is_dir, is_symlink, is_excluded, dev, ino, clone_id,
   nlinks, size_logical, size_blocks, excluded_file_count, mtime, scan_id)
VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Free GiB on the volume containing `path` (`os.statvfs` → `f_bavail*f_frsize`,
/// `./duh:264-271`). Returns +inf on error so the disk guard fails open, matching
/// the Python `except OSError: return float("inf")`.
fn free_gib(path: &Path) -> f64 {
    let cpath = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return f64::INFINITY,
    };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut s) } != 0 {
        return f64::INFINITY;
    }
    (s.f_bavail as f64 * s.f_frsize as f64) / (1024f64 * 1024.0 * 1024.0)
}

/// Python `{n:,}` equivalent: thousands separators for the file counts in the
/// stderr progress / interrupt / completion lines (`./duh:780,822,829` — only
/// the file counts get separators there; excluded counts do not).
fn fmt_thousands(n: i64) -> String {
    let (sign, mut digits) = if n < 0 {
        ("-", n.unsigned_abs().to_string())
    } else {
        ("", n.to_string())
    };
    let mut i = digits.len();
    while i > 3 {
        i -= 3;
        digits.insert(i, ',');
    }
    format!("{sign}{digits}")
}

/// IEC byte formatting, ported from `fmt_bytes` (`./duh:175-181`). Used only for
/// stderr progress / completion lines.
fn fmt_bytes(n: i64) -> String {
    if n < 0 {
        return format!("-{}", fmt_bytes(-n));
    }
    for (unit, threshold) in [("GiB", 1i64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if n >= threshold {
            return format!("{:.1} {}", n as f64 / threshold as f64, unit);
        }
    }
    format!("{n} B")
}

/// `os.path.realpath`-equivalent: absolutize, then resolve symlinks over the
/// longest existing prefix, re-appending any non-existent trailing components.
/// Unlike `std::fs::canonicalize`, this does not fail on a missing leaf, so the
/// caller can print the resolved path in the "path not found" error like Python.
fn realpath(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.as_path();
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            let mut result = c;
            for comp in trailing.iter().rev() {
                result.push(comp);
            }
            return result;
        }
        match cur.file_name() {
            Some(name) => {
                trailing.push(name.to_os_string());
                match cur.parent() {
                    Some(pp) => cur = pp,
                    None => return abs,
                }
            }
            None => return abs,
        }
    }
}

/// Per-clone-family accumulator for an excluded subtree: [count, blocks_sum, max_blocks].
type Family = (i64, i64, i64);

/// Port of `walk_for_aggregate` (`./duh:436-484`). Walks `dir_path` WITHOUT
/// inserting rows and returns `(blocks, logical, count, clone_families)` where
/// each family maps `clone_id -> (member_count, blocks_sum, max_blocks)`.
///
/// Note: like the reference, this ALWAYS collects clone families — it takes no
/// `--no-clones` flag, so excluded_families are populated even under `--no-clones`
/// (`./duh:698` calls it unconditionally). Only regular files carry a clone_id
/// (EntryAttrs sets it for VREG only, matching `./duh:463-468`).
fn walk_for_aggregate(dir_path: &Path, root_dev: i32) -> (i64, i64, i64, HashMap<u64, Family>) {
    let mut total_blocks: i64 = 0;
    let mut total_logical: i64 = 0;
    let mut total_count: i64 = 0;
    let mut families: HashMap<u64, Family> = HashMap::new();

    let mut stack: Vec<PathBuf> = vec![dir_path.to_path_buf()];
    while let Some(d) = stack.pop() {
        // PermissionError / OSError on scandir → skip silently (./duh:448-451).
        let entries = match attrs::read_dir_attrs(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in &entries {
            if e.dev != root_dev {
                continue;
            }
            let blk = e.size_blocks as i64;
            total_blocks += blk;
            total_logical += e.size_logical as i64;
            total_count += 1;
            if e.is_dir && !e.is_symlink {
                stack.push(d.join(&e.name));
            } else if !e.is_symlink {
                if let Some(cid) = e.clone_id {
                    let rec = families.entry(cid).or_insert((0, 0, 0));
                    rec.0 += 1;
                    rec.1 += blk;
                    if blk > rec.2 {
                        rec.2 = blk;
                    }
                }
            }
        }
    }
    (total_blocks, total_logical, total_count, families)
}

/// A leaf (file or symlink) row bound for the writer.
struct FileRow {
    id: i64,
    parent_id: i64,
    name: Vec<u8>,
    is_symlink: i64,
    dev: i32,
    ino: u64,
    clone_id: Option<u64>,
    nlinks: u32,
    size_logical: i64,
    size_blocks: i64,
    mtime: i64,
}

/// A directory row bound for the writer, plus any excluded-subtree clone families
/// (empty for a normal, non-excluded directory). `families` carries tuples of
/// `(clone_id, member_count, blocks_sum, max_blocks)`.
struct DirRow {
    id: i64,
    parent_id: Option<i64>,
    name: Vec<u8>,
    is_excluded: i64,
    dev: i32,
    ino: u64,
    clone_id: Option<u64>,
    nlinks: u32,
    size_logical: i64,
    size_blocks: i64,
    excluded_file_count: Option<i64>,
    mtime: i64,
    families: Vec<(u64, i64, i64, i64)>,
}

/// Messages a worker sends to the writer.
enum Msg {
    /// A batch of leaf rows (accumulated per directory, chunked at `BATCH_SIZE`).
    Files(Vec<FileRow>),
    /// A single directory row (and, for excluded dirs, its clone families).
    Dir(DirRow),
}

/// A work item: `(parent_id, dir_path, rel_from_root)`.
type WorkItem = (i64, PathBuf, String);

/// Preassign the next id. Seeded so the first allocation is `MAX(id)+1`, matching
/// the Python oracle's autoincrement. The `OR IGNORE` conflict branch of the
/// reference is unreachable here (see module docs), so every allocated id is used.
fn alloc(next_id: &AtomicI64) -> i64 {
    next_id.fetch_add(1, Ordering::Relaxed) + 1
}

/// Exact running totals for the `scans` row and progress lines. Written by workers
/// as rows are emitted; read exactly by the writer (progress) and by `run_inner`
/// after the workers have joined.
#[derive(Default)]
struct Totals {
    files: AtomicI64,
    logical: AtomicI64,
    blocks: AtomicI64,
    excluded: AtomicI64,
}

/// Shared work queue + termination bookkeeping for the worker pool.
///
/// `in_flight` counts items that are queued OR currently being processed. It
/// starts at 1 (the root item) and is bumped for every subdirectory pushed;
/// `finish_one` decrements it after a directory is fully processed. When it
/// reaches 0 with an empty queue, the scan is structurally complete.
struct Coord {
    inner: Mutex<CoordInner>,
    cv: Condvar,
    /// Cheap poll flag for abort (SIGINT / disk guard). Also gates the wait loop.
    abort: AtomicBool,
}

struct CoordInner {
    queue: Vec<WorkItem>,
    in_flight: usize,
}

impl Coord {
    fn new(root: WorkItem) -> Self {
        Coord {
            inner: Mutex::new(CoordInner {
                queue: vec![root],
                in_flight: 1,
            }),
            cv: Condvar::new(),
            abort: AtomicBool::new(false),
        }
    }

    /// Block until a work item is available, the queue has drained AND all work is
    /// done, or an abort was requested. Returns `None` to tell the worker to exit.
    fn next(&self) -> Option<WorkItem> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if self.abort.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(item) = g.queue.pop() {
                return Some(item);
            }
            if g.in_flight == 0 {
                // Everything is done; wake the other sleepers so they exit too.
                self.cv.notify_all();
                return None;
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    /// Enqueue a subdirectory (bumps in-flight so termination waits for it).
    fn push(&self, item: WorkItem) {
        let mut g = self.inner.lock().unwrap();
        g.in_flight += 1;
        g.queue.push(item);
        drop(g);
        self.cv.notify_one();
    }

    /// Mark the current directory fully processed.
    fn finish_one(&self) {
        let mut g = self.inner.lock().unwrap();
        g.in_flight -= 1;
        if g.in_flight == 0 {
            drop(g);
            self.cv.notify_all();
        }
    }

    /// Request an abort (idempotent). Holding the lock across `notify_all` closes
    /// the lost-wakeup window against a worker about to `wait`.
    fn request_abort(&self) {
        if !self.abort.swap(true, Ordering::SeqCst) {
            let _g = self.inner.lock().unwrap();
            self.cv.notify_all();
        }
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }
}

/// Insert one directory row (and its excluded clone families, if any). Used by the
/// writer for every directory and by `run_inner` for the root. `OR IGNORE` is kept
/// for fidelity with the reference but never resolves a conflict (see module docs).
fn write_dir(con: &Connection, d: &DirRow, scan_id: i64) -> rusqlite::Result<()> {
    con.execute(
        INSERT_SQL,
        rusqlite::params![
            d.id,
            d.parent_id,
            RawText(&d.name),
            1i64, // is_dir
            0i64, // is_symlink
            d.is_excluded,
            d.dev,
            d.ino as i64,
            d.clone_id.map(|c| c as i64),
            d.nlinks as i64,
            d.size_logical,
            d.size_blocks,
            d.excluded_file_count,
            d.mtime,
            scan_id,
        ],
    )?;
    for (cid, cnt, bsum, bmax) in &d.families {
        con.execute(
            "INSERT OR REPLACE INTO excluded_families \
             (excluded_id, clone_id, member_count, blocks_sum, max_blocks) \
             VALUES (?,?,?,?,?)",
            rusqlite::params![d.id, *cid as i64, cnt, bsum, bmax],
        )?;
    }
    Ok(())
}

/// Try to send `msg`; on failure (the writer has gone away) request an abort and
/// report false so the caller stops producing.
fn try_send(tx: &Sender<Msg>, coord: &Coord, msg: Msg) -> bool {
    if tx.send(msg).is_err() {
        coord.request_abort();
        false
    } else {
        true
    }
}

/// One worker: pop directories off the shared queue, classify their entries the
/// same way the reference does, preassign ids, and emit rows to the writer.
#[allow(clippy::too_many_arguments)]
fn worker(
    tx: &Sender<Msg>,
    coord: &Coord,
    totals: &Totals,
    next_id: &AtomicI64,
    excludes: &Excludes,
    perm_errors: &Mutex<HashSet<PathBuf>>,
    root_dev: i32,
    cross_device: bool,
    no_clones: bool,
) {
    while let Some((parent_id, current_dir, rel_from_root)) = coord.next() {
        let entries: Vec<EntryAttrs> = match attrs::read_dir_attrs(&current_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                if perm_errors.lock().unwrap().insert(current_dir.clone()) {
                    eprintln!("error: permission denied: {}", current_dir.display());
                }
                coord.finish_one();
                continue;
            }
            Err(e) => {
                eprintln!("error: scandir {}: {}", current_dir.display(), e);
                coord.finish_one();
                continue;
            }
        };

        let mut batch: Vec<FileRow> = Vec::new();
        for entry in &entries {
            if coord.aborted() {
                break;
            }

            // Cross-device skip (compare to root dev) unless --cross-device.
            if !cross_device && entry.dev != root_dev {
                continue;
            }

            let name_bytes = entry.name.as_bytes();
            let name_str = entry.name.to_string_lossy();
            let entry_path = current_dir.join(&entry.name);
            let entry_rel = if rel_from_root.is_empty() {
                name_str.to_string()
            } else {
                format!("{rel_from_root}/{name_str}")
            };

            let is_dir = entry.is_dir;
            let is_symlink = entry.is_symlink;

            // Exclusion check (dirs only), BEFORE clone-id collection (./duh:694-724).
            if is_dir && !is_symlink && excludes.matches(&name_str, &entry_rel) {
                let (agg_blocks, agg_logical, agg_count, families) =
                    walk_for_aggregate(&entry_path, root_dev);
                let fams: Vec<(u64, i64, i64, i64)> = families
                    .into_iter()
                    .map(|(cid, (cnt, bsum, bmax))| (cid, cnt, bsum, bmax))
                    .collect();
                let d = DirRow {
                    id: alloc(next_id),
                    parent_id: Some(parent_id),
                    name: name_bytes.to_vec(),
                    is_excluded: 1,
                    dev: entry.dev,
                    ino: entry.ino,
                    clone_id: None, // clone_id NULL for excluded dirs
                    nlinks: entry.nlink,
                    size_logical: agg_logical,
                    size_blocks: agg_blocks,
                    excluded_file_count: Some(agg_count),
                    mtime: entry.mtime,
                    families: fams,
                };
                if !try_send(tx, coord, Msg::Dir(d)) {
                    break;
                }
                totals.excluded.fetch_add(1, Ordering::Relaxed);
                continue; // don't recurse
            }

            // clone_id for every non-symlink entry (files AND dirs) unless
            // --no-clones (./duh:726-731). Directories need the extra syscall (the
            // bulk reader gates clone_id to VREG); regular files reuse the id the
            // bulk reader already extracted.
            let clone_id = if no_clones || is_symlink {
                None
            } else if is_dir {
                attrs::get_clone_id(&entry_path)
            } else {
                entry.clone_id
            };

            if is_dir && !is_symlink {
                // Emit the dir row BEFORE enqueuing it, so no worker can dequeue it
                // and emit its children before the parent row exists (module docs).
                let id = alloc(next_id);
                let d = DirRow {
                    id,
                    parent_id: Some(parent_id),
                    name: name_bytes.to_vec(),
                    is_excluded: 0,
                    dev: entry.dev,
                    ino: entry.ino,
                    clone_id,
                    nlinks: entry.nlink,
                    size_logical: entry.size_logical as i64,
                    size_blocks: entry.size_blocks as i64,
                    excluded_file_count: None,
                    mtime: entry.mtime,
                    families: Vec::new(),
                };
                if !try_send(tx, coord, Msg::Dir(d)) {
                    break;
                }
                coord.push((id, entry_path, entry_rel));
            } else {
                batch.push(FileRow {
                    id: alloc(next_id),
                    parent_id,
                    name: name_bytes.to_vec(),
                    is_symlink: if is_symlink { 1 } else { 0 },
                    dev: entry.dev,
                    ino: entry.ino,
                    clone_id,
                    nlinks: entry.nlink,
                    size_logical: entry.size_logical as i64,
                    size_blocks: entry.size_blocks as i64,
                    mtime: entry.mtime,
                });
                totals.files.fetch_add(1, Ordering::Relaxed);
                totals.logical.fetch_add(entry.size_logical as i64, Ordering::Relaxed);
                totals.blocks.fetch_add(entry.size_blocks as i64, Ordering::Relaxed);
                if batch.len() >= BATCH_SIZE && !try_send(tx, coord, Msg::Files(std::mem::take(&mut batch))) {
                    break;
                }
            }
        }

        if !batch.is_empty() {
            try_send(tx, coord, Msg::Files(batch));
        }
        coord.finish_one();
    }
}

/// The sole DB-writing thread. Drains `Msg`s into ~`BATCH_SIZE`-row transactions,
/// hosts the 5s progress line and 10s disk guard, translates SIGINT into an abort,
/// then finalizes the `scans` row and invalidates the freeable cache.
#[allow(clippy::too_many_arguments)]
fn writer(
    con: Connection,
    rx: crossbeam_channel::Receiver<Msg>,
    coord: &Coord,
    totals: &Totals,
    scan_id: i64,
    quiet: bool,
    db_dir: &Path,
    min_free: f64,
    scan_start: Instant,
    disk_abort: &AtomicBool,
) -> rusqlite::Result<()> {
    // Disable FK enforcement for the bulk load; integrity is guaranteed
    // structurally and insert order is irrelevant to content (module docs).
    con.pragma_update(None, "foreign_keys", "OFF")?;

    let mut in_txn = false;
    let mut rows_in_txn = 0usize;
    let mut last_progress = Instant::now();
    let mut last_disk_check = Instant::now();

    loop {
        // Timers are polled every iteration (not just on idle) so progress lines
        // still appear under a continuous message stream.
        if !quiet && last_progress.elapsed().as_secs_f64() >= PROGRESS_INTERVAL {
            let files = totals.files.load(Ordering::Relaxed);
            let elapsed = scan_start.elapsed().as_secs_f64();
            let rate = if elapsed > 0.0 { files as f64 / elapsed } else { 0.0 };
            eprintln!(
                "[{} files, {} excluded, {} scanned, {:.0} files/sec]",
                fmt_thousands(files),
                totals.excluded.load(Ordering::Relaxed),
                fmt_bytes(totals.blocks.load(Ordering::Relaxed)),
                rate
            );
            last_progress = Instant::now();
        }
        if last_disk_check.elapsed().as_secs_f64() >= DISK_CHECK_INTERVAL {
            if !disk_abort.load(Ordering::Relaxed) && free_gib(db_dir) < min_free {
                eprintln!(
                    "error: free disk below {} GiB (currently {:.2} GiB), aborting scan",
                    min_free,
                    free_gib(db_dir)
                );
                disk_abort.store(true, Ordering::SeqCst);
                coord.request_abort();
            }
            last_disk_check = Instant::now();
        }
        // SIGINT → abort (the handler only flips INTERRUPTED; the writer owns the
        // translation so workers have a single stop signal).
        if INTERRUPTED.load(Ordering::SeqCst) && !coord.aborted() {
            coord.request_abort();
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(msg) => {
                if !in_txn {
                    con.execute_batch("BEGIN")?;
                    in_txn = true;
                }
                match msg {
                    Msg::Files(rows) => {
                        let mut stmt = con.prepare_cached(INSERT_SQL)?;
                        for r in &rows {
                            stmt.execute(rusqlite::params![
                                r.id,
                                r.parent_id,
                                RawText(&r.name),
                                0i64, // is_dir
                                r.is_symlink,
                                0i64, // is_excluded
                                r.dev,
                                r.ino as i64,
                                r.clone_id.map(|c| c as i64),
                                r.nlinks as i64,
                                r.size_logical,
                                r.size_blocks,
                                Option::<i64>::None, // excluded_file_count
                                r.mtime,
                                scan_id,
                            ])?;
                            rows_in_txn += 1;
                        }
                    }
                    Msg::Dir(d) => {
                        write_dir(&con, &d, scan_id)?;
                        rows_in_txn += 1;
                    }
                }
                if rows_in_txn >= BATCH_SIZE {
                    con.execute_batch("COMMIT")?;
                    in_txn = false;
                    rows_in_txn = 0;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    if in_txn {
        con.execute_batch("COMMIT")?;
    }

    con.execute(
        "UPDATE scans SET finished_at=?, files_count=?, excluded_count=?, \
         bytes_logical=?, bytes_blocks=? WHERE id=?",
        rusqlite::params![
            now_secs(),
            totals.files.load(Ordering::Relaxed),
            totals.excluded.load(Ordering::Relaxed),
            totals.logical.load(Ordering::Relaxed),
            totals.blocks.load(Ordering::Relaxed),
            scan_id
        ],
    )?;

    // Invalidate the freeable cache (./duh:808-816). The table always exists here.
    con.execute("DELETE FROM freeable_cache", [])?;
    eprintln!("[scan] freeable cache invalidated");
    Ok(())
}

/// Entry point for the `scan` subcommand. Returns the process exit code.
pub fn run(args: ScanArgs, db_path: &Path) -> ExitCode {
    // realpath + existence check BEFORE touching the DB (./duh:541-543), so a bad
    // path never creates a database file.
    let root = realpath(Path::new(&args.path));
    if std::fs::metadata(&root).is_err() {
        eprintln!("error: path not found: {}", root.display());
        return ExitCode::FAILURE;
    }

    let con = match crate::db::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot open database: {e}");
            return ExitCode::FAILURE;
        }
    };

    match run_inner(&args, db_path, con, &root) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(
    args: &ScanArgs,
    db_path: &Path,
    con: Connection,
    root: &Path,
) -> rusqlite::Result<ExitCode> {
    let root_bytes = root.as_os_str().as_bytes();

    // --rescan: delete prior rows for this root. excluded_families BEFORE files
    // (FK), then files, then the scans row (./duh:550-565).
    if args.rescan {
        let old_ids: Vec<i64> = {
            let mut stmt = con.prepare("SELECT id FROM scans WHERE root = ?")?;
            let rows = stmt.query_map(rusqlite::params![RawText(root_bytes)], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<i64>>>()?
        };
        for sid in old_ids {
            con.execute(
                "DELETE FROM excluded_families WHERE excluded_id IN \
                 (SELECT id FROM files WHERE scan_id = ?)",
                rusqlite::params![sid],
            )?;
            con.execute("DELETE FROM files WHERE scan_id = ?", rusqlite::params![sid])?;
        }
        con.execute(
            "DELETE FROM scans WHERE root = ?",
            rusqlite::params![RawText(root_bytes)],
        )?;
        eprintln!("[rescan] deleted existing scan rows for {}", root.display());
    }

    // Create the scans row first so finished_at can always be set on abort.
    let started_at = now_secs();
    con.execute(
        "INSERT INTO scans (root, started_at, schema_version) VALUES (?, ?, 2)",
        rusqlite::params![RawText(root_bytes), started_at],
    )?;
    let scan_id = con.last_insert_rowid();

    // Free-space precheck, measured on the DB's directory (./duh:576-591).
    let db_dir = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let current_free = free_gib(&db_dir);
    if current_free < args.min_free {
        con.execute(
            "UPDATE scans SET finished_at=?, files_count=0, excluded_count=0, \
             bytes_logical=0, bytes_blocks=0 WHERE id=?",
            rusqlite::params![now_secs(), scan_id],
        )?;
        eprintln!(
            "error: free disk below {} GiB (currently {:.2} GiB), aborting scan",
            args.min_free, current_free
        );
        return Ok(ExitCode::from(EX_TEMPFAIL));
    }

    // Root lstat-equivalent metadata.
    let root_stat = match attrs::stat_root(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot stat root: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let root_dev = root_stat.dev;

    let excludes = Excludes::from_args(&args.exclude, &args.include, args.no_default_excludes);

    // Seed the explicit id counter (see module docs).
    let seed: i64 = con.query_row("SELECT COALESCE(MAX(id),0) FROM files", [], |r| r.get(0))?;
    let next_id = AtomicI64::new(seed);

    // Install the SIGINT handler (graceful stop → drain + finalize).
    INTERRUPTED.store(false, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as libc::sighandler_t);
    }

    // Insert the root dir on this setup thread, before the connection moves to the
    // writer. Its name is the FULL realpath; clone_id comes from get_clone_id(root)
    // regardless of it being a directory (./duh:628-650) — this is why we do NOT
    // reuse root_stat.clone_id (which is None for dirs).
    let root_clone = if args.no_clones {
        None
    } else {
        attrs::get_clone_id(root)
    };
    let root_dir = DirRow {
        id: alloc(&next_id),
        parent_id: None,
        name: root_bytes.to_vec(),
        is_excluded: 0,
        dev: root_stat.dev,
        ino: root_stat.ino,
        clone_id: root_clone,
        nlinks: root_stat.nlink,
        size_logical: root_stat.size_logical as i64,
        size_blocks: root_stat.size_blocks as i64,
        excluded_file_count: None,
        mtime: root_stat.mtime,
        families: Vec::new(),
    };
    write_dir(&con, &root_dir, scan_id)?;
    let root_dir_id = root_dir.id;

    // Shared state for the pool. The writer owns `con`; the workers borrow the
    // rest for the duration of the scoped threads (no `Arc` needed).
    let coord = Coord::new((root_dir_id, root.to_path_buf(), String::new()));
    let totals = Totals::default();
    let perm_errors: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
    let disk_abort = AtomicBool::new(false);
    let (tx, rx) = bounded::<Msg>(64);

    let nworkers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let scan_start = Instant::now();

    // Reference bindings so the `move` closures capture (Copy) references to the
    // shared state rather than trying to move the state itself.
    let coord_r = &coord;
    let totals_r = &totals;
    let next_id_r = &next_id;
    let excludes_r = &excludes;
    let perm_r = &perm_errors;
    let disk_r = &disk_abort;
    let db_dir_r = db_dir.as_path();

    let writer_result: rusqlite::Result<()> = std::thread::scope(|s| {
        // Writer thread: owns `con` and `rx`.
        let wh = s.spawn(move || {
            writer(
                con,
                rx,
                coord_r,
                totals_r,
                scan_id,
                args.quiet,
                db_dir_r,
                args.min_free,
                scan_start,
                disk_r,
            )
        });
        // Worker threads: each owns a `Sender` clone (dropped on exit → the writer
        // sees `Disconnected` once every worker has finished).
        let mut whs = Vec::with_capacity(nworkers);
        for _ in 0..nworkers {
            let txc = tx.clone();
            whs.push(s.spawn(move || {
                worker(
                    &txc,
                    coord_r,
                    totals_r,
                    next_id_r,
                    excludes_r,
                    perm_r,
                    root_dev,
                    args.cross_device,
                    args.no_clones,
                );
            }));
        }
        drop(tx); // this thread's clone; workers hold the rest
        for h in whs {
            h.join().unwrap();
        }
        wh.join().unwrap()
    });
    writer_result?;

    let total_files = totals.files.load(Ordering::Relaxed);
    let total_logical = totals.logical.load(Ordering::Relaxed);
    let total_blocks = totals.blocks.load(Ordering::Relaxed);
    let total_excluded = totals.excluded.load(Ordering::Relaxed);

    if disk_abort.load(Ordering::Relaxed) {
        return Ok(ExitCode::from(EX_TEMPFAIL));
    }

    if INTERRUPTED.load(Ordering::SeqCst) {
        eprintln!(
            "\n[interrupted] partial scan committed: {} files",
            fmt_thousands(total_files)
        );
        return Ok(ExitCode::from(130));
    }

    let elapsed = now_secs() - started_at;
    let rate = if elapsed > 0.0 { total_files as f64 / elapsed } else { 0.0 };
    if !args.quiet {
        eprintln!(
            "Scan complete: {} files, {} excluded dirs in {:.1}s ({:.0} files/sec)\n\
             \x20 logical: {}  blocks: {}\n\
             \x20 DB: {}",
            fmt_thousands(total_files),
            total_excluded,
            elapsed,
            rate,
            fmt_bytes(total_logical),
            fmt_bytes(total_blocks),
            db_path.display()
        );
    }

    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::fmt_thousands;

    /// Pin parity with Python's `{n:,}` formatting (used at ./duh:780,822,829).
    #[test]
    fn fmt_thousands_matches_python_comma_format() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(1000), "1,000");
        assert_eq!(fmt_thousands(123456), "123,456");
        assert_eq!(fmt_thousands(1234567), "1,234,567");
        assert_eq!(fmt_thousands(-1234567), "-1,234,567");
    }
}
