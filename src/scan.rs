//! Single-threaded scanner — a faithful port of the Python oracle's `cmd_scan`
//! (`./duh:540-837`), `walk_for_aggregate` (`./duh:436-484`), and the batch/insert
//! SQL (`./duh:489-537`).
//!
//! Parity target: the databases written here must be identical in *content*
//! (compared by path, not by row id) to the Python scanner. Control flow mirrors
//! the reference line-for-line; deviations are documented in the task report.
//!
//! IDs: unlike Python (which relies on SQLite autoincrement / `lastrowid`), we
//! allocate ids from an explicit monotonic `next_id` counter seeded from
//! `SELECT COALESCE(MAX(id),0) FROM files`, and insert with an explicit `id`.
//! Task 10's parallel scanner depends on this pattern (workers can preassign ids
//! without a writer round-trip). Content is unaffected because the acceptance
//! suite compares by path.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

/// Insert a directory row with an explicit id and return the id actually used.
/// Mirrors `_insert_dir_get_id` (`./duh:508-537`): on an `OR IGNORE` conflict
/// (row already existed) look the existing id up by `(parent_id, name, scan_id)`.
#[allow(clippy::too_many_arguments)]
fn insert_dir_get_id(
    con: &Connection,
    next_id: &mut i64,
    parent_id: Option<i64>,
    name: &[u8],
    is_excluded: i64,
    dev: i32,
    ino: u64,
    clone_id: Option<u64>,
    nlinks: u32,
    size_logical: i64,
    size_blocks: i64,
    excluded_file_count: Option<i64>,
    mtime: i64,
    scan_id: i64,
) -> rusqlite::Result<i64> {
    let id = *next_id + 1;
    let changed = con.execute(
        INSERT_SQL,
        rusqlite::params![
            id,
            parent_id,
            RawText(name),
            1i64, // is_dir
            0i64, // is_symlink
            is_excluded,
            dev,
            ino as i64,
            clone_id.map(|c| c as i64),
            nlinks as i64,
            size_logical,
            size_blocks,
            excluded_file_count,
            mtime,
            scan_id,
        ],
    )?;
    if changed > 0 {
        *next_id = id;
        return Ok(id);
    }
    // Row already existed (IGNORE fired) — look it up. `parent_id IS ?` matches
    // NULL parents too, mirroring the Python lookup at ./duh:531-534.
    con.query_row(
        "SELECT id FROM files WHERE parent_id IS ? AND name = ? AND scan_id = ?",
        rusqlite::params![parent_id, RawText(name), scan_id],
        |r| r.get(0),
    )
}

/// A batched leaf (file or symlink) row awaiting flush.
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

/// Flush the batched leaf rows inside a single transaction (Python's
/// `executemany` + `commit`, `./duh:614-619`).
fn flush(con: &Connection, batch: &mut Vec<FileRow>, scan_id: i64) -> rusqlite::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    con.execute_batch("BEGIN")?;
    {
        let mut stmt = con.prepare_cached(INSERT_SQL)?;
        for r in batch.iter() {
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
        }
    }
    con.execute_batch("COMMIT")?;
    batch.clear();
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

    match run_inner(&args, db_path, &con, &root) {
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
    con: &Connection,
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
    let mut next_id: i64 =
        con.query_row("SELECT COALESCE(MAX(id),0) FROM files", [], |r| r.get(0))?;

    // Install the SIGINT handler (graceful stop → flush + finalize).
    INTERRUPTED.store(false, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as libc::sighandler_t);
    }

    let mut total_files: i64 = 0;
    let mut total_logical: i64 = 0;
    let mut total_blocks: i64 = 0;
    let mut total_excluded: i64 = 0;
    let mut batch: Vec<FileRow> = Vec::new();

    let mut disk_abort = false;
    let scan_start = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_disk_check = Instant::now();

    // Insert the root dir. Its name is the FULL realpath; clone_id comes from
    // get_clone_id(root) regardless of it being a directory (./duh:628-650) —
    // this is why we do NOT reuse root_stat.clone_id (which is None for dirs).
    let root_clone = if args.no_clones {
        None
    } else {
        attrs::get_clone_id(root)
    };
    let root_dir_id = insert_dir_get_id(
        con,
        &mut next_id,
        None,
        root_bytes,
        0,
        root_stat.dev,
        root_stat.ino,
        root_clone,
        root_stat.nlink,
        root_stat.size_logical as i64,
        root_stat.size_blocks as i64,
        None,
        root_stat.mtime,
        scan_id,
    )?;

    // DFS stack: (parent_id, dir_path, rel_from_root).
    let mut stack: Vec<(i64, PathBuf, String)> = vec![(root_dir_id, root.to_path_buf(), String::new())];
    let mut permission_errors: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    while let Some((parent_id, current_dir, rel_from_root)) = stack.pop() {
        if INTERRUPTED.load(Ordering::SeqCst) || disk_abort {
            break;
        }

        let entries: Vec<EntryAttrs> = match attrs::read_dir_attrs(&current_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                if permission_errors.insert(current_dir.clone()) {
                    eprintln!("error: permission denied: {}", current_dir.display());
                }
                continue;
            }
            Err(e) => {
                eprintln!("error: scandir {}: {}", current_dir.display(), e);
                continue;
            }
        };

        for entry in &entries {
            if INTERRUPTED.load(Ordering::SeqCst) || disk_abort {
                break;
            }

            // Cross-device skip (compare to root dev) unless --cross-device.
            if !args.cross_device && entry.dev != root_dev {
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
                flush(con, &mut batch, scan_id)?;
                let (agg_blocks, agg_logical, agg_count, families) =
                    walk_for_aggregate(&entry_path, root_dev);
                let excl_id = insert_dir_get_id(
                    con,
                    &mut next_id,
                    Some(parent_id),
                    name_bytes,
                    1, // is_excluded
                    entry.dev,
                    entry.ino,
                    None, // clone_id NULL for excluded dirs
                    entry.nlink,
                    agg_logical,
                    agg_blocks,
                    Some(agg_count),
                    entry.mtime,
                    scan_id,
                )?;
                for (cid, (cnt, bsum, bmax)) in &families {
                    con.execute(
                        "INSERT OR REPLACE INTO excluded_families \
                         (excluded_id, clone_id, member_count, blocks_sum, max_blocks) \
                         VALUES (?,?,?,?,?)",
                        rusqlite::params![excl_id, *cid as i64, cnt, bsum, bmax],
                    )?;
                }
                total_excluded += 1;
                continue; // don't recurse
            }

            // clone_id for every non-symlink entry (files AND dirs) unless
            // --no-clones (./duh:726-731). We call get_clone_id per entry rather
            // than reusing EntryAttrs.clone_id because the latter is None for
            // directories, whereas the reference records a dir's own clone id.
            let clone_id = if args.no_clones || is_symlink {
                None
            } else {
                attrs::get_clone_id(&entry_path)
            };

            if is_dir && !is_symlink {
                flush(con, &mut batch, scan_id)?;
                let new_id = insert_dir_get_id(
                    con,
                    &mut next_id,
                    Some(parent_id),
                    name_bytes,
                    0,
                    entry.dev,
                    entry.ino,
                    clone_id,
                    entry.nlink,
                    entry.size_logical as i64,
                    entry.size_blocks as i64,
                    None,
                    entry.mtime,
                    scan_id,
                )?;
                stack.push((new_id, entry_path, entry_rel));
            } else {
                next_id += 1;
                batch.push(FileRow {
                    id: next_id,
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
                total_files += 1;
                total_logical += entry.size_logical as i64;
                total_blocks += entry.size_blocks as i64;
                if batch.len() >= BATCH_SIZE {
                    flush(con, &mut batch, scan_id)?;
                }
            }

            // Progress line every 5s (./duh:776-784).
            if !args.quiet && last_progress.elapsed().as_secs_f64() >= PROGRESS_INTERVAL {
                let elapsed = scan_start.elapsed().as_secs_f64();
                let rate = if elapsed > 0.0 { total_files as f64 / elapsed } else { 0.0 };
                eprintln!(
                    "[{} files, {} excluded, {} scanned, {:.0} files/sec]",
                    total_files,
                    total_excluded,
                    fmt_bytes(total_blocks),
                    rate
                );
                last_progress = Instant::now();
            }

            // Disk safety check every 10s (./duh:787-796).
            if last_disk_check.elapsed().as_secs_f64() >= DISK_CHECK_INTERVAL {
                let free = free_gib(&db_dir);
                if free < args.min_free {
                    eprintln!(
                        "error: free disk below {} GiB (currently {:.2} GiB), aborting scan",
                        args.min_free, free
                    );
                    disk_abort = true;
                }
                last_disk_check = Instant::now();
            }
        }
    }

    flush(con, &mut batch, scan_id)?;

    con.execute(
        "UPDATE scans SET finished_at=?, files_count=?, excluded_count=?, \
         bytes_logical=?, bytes_blocks=? WHERE id=?",
        rusqlite::params![
            now_secs(),
            total_files,
            total_excluded,
            total_logical,
            total_blocks,
            scan_id
        ],
    )?;

    // Invalidate the freeable cache (./duh:808-816). The table always exists here
    // (db::open applies the full schema), so no OperationalError to swallow.
    con.execute("DELETE FROM freeable_cache", [])?;
    eprintln!("[scan] freeable cache invalidated");

    if disk_abort {
        return Ok(ExitCode::from(EX_TEMPFAIL));
    }

    if INTERRUPTED.load(Ordering::SeqCst) {
        eprintln!("\n[interrupted] partial scan committed: {total_files} files");
        return Ok(ExitCode::from(130));
    }

    let elapsed = now_secs() - started_at;
    let rate = if elapsed > 0.0 { total_files as f64 / elapsed } else { 0.0 };
    if !args.quiet {
        eprintln!(
            "Scan complete: {} files, {} excluded dirs in {:.1}s ({:.0} files/sec)\n\
             \x20 logical: {}  blocks: {}\n\
             \x20 DB: {}",
            total_files,
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
