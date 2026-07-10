//! Port of the reference `./duh` freeable engine: `compute_freeable`
//! (`./duh:1654-2116`) plus the `freeable`, `marginal`, and `file` subcommands
//! (`./duh:3076-3092`, `./duh:1084-1217`, `./duh:1223-1272`).
//!
//! The intellectual core is [`compute`], which mirrors `compute_freeable`
//! phase-for-phase. The Python uses dense `array.array` structures indexed by
//! node_id and a streaming temp-table trick to stay under a memory budget on
//! 4M-file trees; Rust uses dense `Vec`s and in-memory `HashMap` grouping. The
//! grouping shape differs but the *set of families that qualify* and the *credit
//! arithmetic* are byte-identical, so the persisted `freeable_cache` matches the
//! reference exactly (verified by `blackbox/test_db_parity.py`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::ExitCode;

use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, ToSql};
use std::os::unix::ffi::OsStrExt;

use crate::scan::realpath;

/// `(freeable, locked_here)` maps keyed by node_id (blocks).
type FreeableMaps = (HashMap<i64, u64>, HashMap<i64, u64>);

/// Hardlink families grouped by `(dev, ino)` -> `[(node_id, nlinks, blocks)]`.
type HlFam = HashMap<(i64, i64), Vec<(i64, i64, i64)>>;

/// Bind a raw byte string as a SQLite TEXT value (names are stored as bytes; see
/// `scan.rs`'s identical helper). Kept private here to avoid widening `scan`'s API.
struct RawText<'a>(&'a [u8]);

impl ToSql for RawText<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0)))
    }
}

// ---------------------------------------------------------------------------
// Dense-array LCA helpers (ports of `_lca_arr` / `_direct_child_of_arr`,
// `./duh:1529-1567`).
// ---------------------------------------------------------------------------

/// Compute the LCA of two nodes using dense arrays. Returns -1 if no common
/// ancestor (spans multiple roots or an id is out of range).
fn lca_arr(node_a: i64, node_b: i64, depth: &[i32], parent: &[i64], max_id: i64) -> i64 {
    let (mut a, mut b) = (node_a, node_b);
    if a < 0 || a > max_id || b < 0 || b > max_id {
        return -1;
    }
    let mut da = depth[a as usize];
    let mut db = depth[b as usize];
    if da < 0 || db < 0 {
        return -1;
    }
    while da > db {
        a = parent[a as usize];
        if a < 0 {
            return -1;
        }
        da -= 1;
    }
    while db > da {
        b = parent[b as usize];
        if b < 0 {
            return -1;
        }
        db -= 1;
    }
    while a != b {
        let pa = parent[a as usize];
        let pb = parent[b as usize];
        if pa < 0 && pb < 0 {
            return -1;
        }
        if pa >= 0 {
            a = pa;
        }
        if pb >= 0 {
            b = pb;
        }
    }
    a
}

/// Find the direct child of `ancestor` on the path down to `descendant`.
fn direct_child_of_arr(ancestor: i64, descendant: i64, parent: &[i64]) -> i64 {
    let mut cur = descendant;
    loop {
        let p = parent[cur as usize];
        if p < 0 || p == ancestor {
            return cur;
        }
        cur = p;
    }
}

// ---------------------------------------------------------------------------
// Cache (ports of `_get_latest_scan_id`, `_load_freeable_cache`,
// `_persist_freeable_cache`, `./duh:1570-1651`).
// ---------------------------------------------------------------------------

fn get_latest_scan_id(con: &Connection) -> rusqlite::Result<Option<i64>> {
    con.query_row("SELECT id FROM scans ORDER BY id DESC LIMIT 1", [], |r| {
        r.get(0)
    })
    .optional()
}

/// Try to load freeable results from cache. Returns `(freeable, locked_here)` or
/// `None` when there is no cache for `scan_id`. Only non-zero columns populate
/// their map, matching the reference's truthiness filter.
fn load_cache(con: &Connection, scan_id: i64) -> rusqlite::Result<Option<FreeableMaps>> {
    let cnt: i64 = con.query_row(
        "SELECT COUNT(*) FROM freeable_cache WHERE scan_id = ?",
        [scan_id],
        |r| r.get(0),
    )?;
    if cnt == 0 {
        return Ok(None);
    }
    let mut freeable = HashMap::new();
    let mut locked_here = HashMap::new();
    let mut stmt = con
        .prepare("SELECT node_id, freeable, locked_here FROM freeable_cache WHERE scan_id = ?")?;
    let rows = stmt.query_map([scan_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })?;
    for row in rows {
        let (nid, f, lh) = row?;
        if f != 0 {
            freeable.insert(nid, f as u64);
        }
        if lh != 0 {
            locked_here.insert(nid, lh as u64);
        }
    }
    Ok(Some((freeable, locked_here)))
}

/// Write freeable results to `freeable_cache`, clearing stale rows for other
/// scan_ids first. Only non-zero rows are persisted (union of both maps).
fn persist_cache(
    con: &Connection,
    scan_id: i64,
    freeable: &HashMap<i64, u64>,
    locked_here: &HashMap<i64, u64>,
) -> rusqlite::Result<()> {
    con.execute(
        "DELETE FROM freeable_cache WHERE scan_id != ?",
        [scan_id],
    )?;
    let mut all_ids: HashSet<i64> = HashSet::with_capacity(freeable.len() + locked_here.len());
    all_ids.extend(freeable.keys());
    all_ids.extend(locked_here.keys());

    let mut stmt = con.prepare(
        "INSERT OR REPLACE INTO freeable_cache (node_id, freeable, locked_here, scan_id) \
         VALUES (?,?,?,?)",
    )?;
    for nid in all_ids {
        let f = freeable.get(&nid).copied().unwrap_or(0);
        let lh = locked_here.get(&nid).copied().unwrap_or(0);
        if f != 0 || lh != 0 {
            stmt.execute(params![nid, f as i64, lh as i64, scan_id])?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// compute_freeable (`./duh:1654-2116`)
// ---------------------------------------------------------------------------

/// A pending locked-here contribution: (lca_node, credit_blocks, child_set).
/// `child_set` holds the direct children of the LCA that members descend into
/// (or the LCA itself when a member *is* the LCA), exactly as the reference
/// accumulates it for later resolution in phase 9.
struct LockedContrib {
    lca: i64,
    credit: i64,
    child_set: HashSet<i64>,
}

/// Compute freeable bytes for every directory node, returning
/// `(freeable, locked_here)` keyed by node_id (blocks). Loads from
/// `freeable_cache` when valid for the latest scan_id, otherwise computes from
/// scratch and persists non-zero rows.
pub fn compute(con: &Connection) -> rusqlite::Result<FreeableMaps> {
    // Phase 0: cache.
    let scan_id = get_latest_scan_id(con)?;
    if let Some(sid) = scan_id {
        if let Some(cached) = load_cache(con, sid)? {
            return Ok(cached);
        }
    }

    // ------------------------------------------------------------------
    // Phase 1: dense parent/depth/is_dir/excl_blocks arrays indexed by node_id.
    // ------------------------------------------------------------------
    let max_id: i64 = con
        .query_row("SELECT MAX(id) FROM files", [], |r| r.get::<_, Option<i64>>(0))?
        .unwrap_or(0);
    let n = (max_id + 1) as usize;

    // parent[i] = parent_id or -1 (root / absent).
    let mut parent = vec![-1i64; n];
    // depth[i] = depth (-1 = unset).
    let mut depth = vec![-1i32; n];
    // is_dir[i]: 0=file/absent, 1=regular dir, 2=excluded dir.
    let mut is_dir = vec![0i8; n];
    // excl_blocks[i]: aggregate blocks for excluded dirs (leaf).
    let mut excl_blocks = vec![0i64; n];

    let mut roots: Vec<i64> = Vec::new();
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();

    {
        let mut stmt =
            con.prepare("SELECT id, parent_id, is_dir, is_excluded, size_blocks FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (nid, pid, isd, isx, blk) = row?;
            match pid {
                None => {
                    parent[nid as usize] = -1;
                    roots.push(nid);
                }
                Some(p) => {
                    parent[nid as usize] = p;
                    children.entry(p).or_default().push(nid);
                }
            }
            if isd != 0 {
                is_dir[nid as usize] = if isx != 0 { 2 } else { 1 };
            }
            if isx != 0 {
                excl_blocks[nid as usize] = blk;
            }
        }
    }

    // BFS to assign depths.
    let mut q: VecDeque<i64> = VecDeque::new();
    for &r in &roots {
        depth[r as usize] = 0;
        q.push_back(r);
    }
    while let Some(nid) = q.pop_front() {
        let d = depth[nid as usize];
        if let Some(ch) = children.get(&nid) {
            for &c in ch {
                if depth[c as usize] < 0 {
                    depth[c as usize] = d + 1;
                    q.push_back(c);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase 2: excluded-family blocks-sum totals per excluded_id.
    // ------------------------------------------------------------------
    let mut excl_ef_total: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = con.prepare(
            "SELECT excluded_id, SUM(blocks_sum) FROM excluded_families GROUP BY excluded_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0)))
        })?;
        for row in rows {
            let (eid, total) = row?;
            excl_ef_total.insert(eid, total);
        }
    }

    // credit[i] = direct credit assigned to node i.
    let mut credit = vec![0i64; n];
    let mut locked_contrib: Vec<LockedContrib> = Vec::new();

    // ------------------------------------------------------------------
    // Phase 3+4: clone families. Union of real file members and excluded
    // pseudo-members grouped by clone_id. A clone_id is "multi" (belongs in the
    // reference's temp `multi_cids`) iff its family has >= 2 members across the
    // union — independent of whether every member is in-tree. That set drives
    // the singleton pass below.
    // ------------------------------------------------------------------
    // clone_id -> Vec<(node_id, blocks)>.
    let mut clone_fam: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    {
        let mut stmt = con.prepare(
            "SELECT clone_id, id, size_blocks FROM files WHERE clone_id IS NOT NULL AND is_dir=0",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (cid, nid, blk) = row?;
            clone_fam.entry(cid).or_default().push((nid, blk));
        }
    }
    {
        // excluded_families pseudo-members: node_id = excluded_id, blocks = max_blocks.
        let mut stmt = con.prepare("SELECT clone_id, excluded_id, max_blocks FROM excluded_families")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (cid, eid, blk) = row?;
            clone_fam.entry(cid).or_default().push((eid, blk));
        }
    }

    let mut multi_cids: HashSet<i64> = HashSet::new();
    for (cid, members) in &clone_fam {
        if members.len() < 2 {
            // Singleton family — handled in the singleton pass.
            continue;
        }
        multi_cids.insert(*cid);

        let max_blk = members.iter().map(|&(_, b)| b).max().unwrap_or(0);
        finalize_family(
            members.iter().map(|&(nid, _)| nid),
            max_blk,
            &depth,
            &parent,
            max_id,
            &mut credit,
            &mut locked_contrib,
        );
    }

    // ------------------------------------------------------------------
    // Phase 5: hardlink families, grouped by (dev, ino). Files carrying a
    // clone_id are excluded (treated as clones above).
    // ------------------------------------------------------------------
    // (dev, ino) -> Vec<(node_id, nlinks, blocks)>.
    let mut hl_fam: HlFam = HashMap::new();
    {
        let mut stmt = con.prepare(
            "SELECT dev, ino, id, nlinks, size_blocks FROM files \
             WHERE is_dir=0 AND clone_id IS NULL AND nlinks > 1",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (dev, ino, nid, nlinks, blk) = row?;
            hl_fam.entry((dev, ino)).or_default().push((nid, nlinks, blk));
        }
    }
    for members in hl_fam.values() {
        if members.is_empty() {
            continue;
        }
        let nlinks = members[0].1;
        if (members.len() as i64) < nlinks {
            // External links present — conservatively credit nothing.
            continue;
        }
        // All members in DB: credit inode blocks once (any member — same inode).
        let credit_blk = members[0].2;
        finalize_family(
            members.iter().map(|&(nid, _, _)| nid),
            credit_blk,
            &depth,
            &parent,
            max_id,
            &mut credit,
            &mut locked_contrib,
        );
    }

    // ------------------------------------------------------------------
    // Phase 6: singleton pass. nlinks=1 files not in a multi-member clone family
    // are credited to their parent directory.
    // ------------------------------------------------------------------
    {
        let mut stmt = con.prepare(
            "SELECT parent_id, size_blocks, clone_id FROM files WHERE is_dir=0 AND nlinks=1",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        for row in rows {
            let (pid, blk, cid) = row?;
            // Mirror `clone_id IS NULL OR clone_id NOT IN multi_cids`.
            if let Some(c) = cid {
                if multi_cids.contains(&c) {
                    continue;
                }
            }
            if let Some(p) = pid {
                if p > 0 && p <= max_id {
                    credit[p as usize] += blk;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase 7: excluded-subtree residual credits.
    // residual = excl_blocks - sum(ef.blocks_sum); credited to the excluded node.
    // ------------------------------------------------------------------
    for nid in 1..=max_id {
        if is_dir[nid as usize] == 2 {
            let total_ef = excl_ef_total.get(&nid).copied().unwrap_or(0);
            let residual = excl_blocks[nid as usize] - total_ef;
            if residual > 0 {
                credit[nid as usize] += residual;
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase 8: bottom-up subtree accumulation via depth buckets.
    // ------------------------------------------------------------------
    let mut max_depth = 0i32;
    for nid in 1..=max_id {
        let d = depth[nid as usize];
        if d > max_depth {
            max_depth = d;
        }
    }
    let mut buckets: Vec<Vec<i64>> = vec![Vec::new(); (max_depth + 1) as usize];
    for nid in 1..=max_id {
        let d = depth[nid as usize];
        if d >= 0 {
            buckets[d as usize].push(nid);
        }
    }
    let mut subtree = vec![0i64; n];
    for d in (0..=max_depth).rev() {
        for &nid in &buckets[d as usize] {
            match is_dir[nid as usize] {
                1 => {
                    // Regular dir: propagate own credit + subtree to parent.
                    let pid = parent[nid as usize];
                    if pid > 0 && pid <= max_id {
                        subtree[pid as usize] += credit[nid as usize] + subtree[nid as usize];
                    }
                }
                2 => {
                    // Excluded dir (leaf aggregate): propagate own credit only.
                    let pid = parent[nid as usize];
                    if pid > 0 && pid <= max_id {
                        subtree[pid as usize] += credit[nid as usize];
                    }
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase 9: materialise freeable + locked_here (non-zero only).
    // ------------------------------------------------------------------
    let mut freeable: HashMap<i64, u64> = HashMap::new();
    for nid in 1..=max_id {
        if is_dir[nid as usize] > 0 {
            let val = credit[nid as usize] + subtree[nid as usize];
            if val != 0 {
                freeable.insert(nid, val as u64);
            }
        }
    }

    // locked_here: resolve each contribution's child_set to *distinct direct
    // children* of the LCA (excluding the LCA itself); credit only when >= 2.
    let mut locked_here: HashMap<i64, u64> = HashMap::new();
    for lc in &locked_contrib {
        let mut direct: HashSet<i64> = HashSet::new();
        for &cs in &lc.child_set {
            if cs == lc.lca {
                continue;
            }
            if cs > 0 && cs <= max_id && parent[cs as usize] == lc.lca {
                direct.insert(cs);
            } else {
                // Walk up to the direct child of the LCA.
                let mut cur = cs;
                while cur > 0 && cur <= max_id {
                    let p = parent[cur as usize];
                    if p < 0 || p == lc.lca {
                        direct.insert(cur);
                        break;
                    }
                    cur = p;
                }
            }
        }
        if direct.len() >= 2 {
            *locked_here.entry(lc.lca).or_insert(0) += lc.credit as u64;
        }
    }

    // Phase 10: persist.
    if let Some(sid) = scan_id {
        persist_cache(con, sid, &freeable, &locked_here)?;
    }

    Ok((freeable, locked_here))
}

/// Finalize one family (clone or hardlink): compute the incremental LCA over
/// `member_ids`, credit `credit_blk` there, and record a locked contribution
/// when the members span >= 2 distinct direct children. Skips the whole family
/// if any member is out of tree or the members span multiple roots — the
/// reference's "member outside → nothing freeable inside" semantics.
fn finalize_family<I: Iterator<Item = i64>>(
    member_ids: I,
    credit_blk: i64,
    depth: &[i32],
    parent: &[i64],
    max_id: i64,
    credit: &mut [i64],
    locked_contrib: &mut Vec<LockedContrib>,
) {
    let mut running_lca: i64 = -1;
    let mut members: Vec<i64> = Vec::new();
    for nid in member_ids {
        if nid > max_id || depth[nid as usize] < 0 {
            return; // member outside scanned tree
        }
        members.push(nid);
        if running_lca < 0 {
            running_lca = nid;
        } else {
            running_lca = lca_arr(running_lca, nid, depth, parent, max_id);
            if running_lca < 0 {
                return; // spans multiple roots
            }
        }
    }
    if running_lca < 0 {
        return;
    }

    credit[running_lca as usize] += credit_blk;

    let mut child_set: HashSet<i64> = HashSet::new();
    for &nid in &members {
        if nid == running_lca {
            child_set.insert(running_lca);
        } else {
            child_set.insert(direct_child_of_arr(running_lca, nid, parent));
        }
    }
    if child_set.len() >= 2 {
        locked_contrib.push(LockedContrib {
            lca: running_lca,
            credit: credit_blk,
            child_set,
        });
    }
}

// ---------------------------------------------------------------------------
// Path helpers (ports of `resolve_path_to_id` / `full_path`, `./duh:348-418`).
// ---------------------------------------------------------------------------

/// Walk path components top-down to find the node_id, or `None` if not indexed.
pub(crate) fn resolve_path_to_id(con: &Connection, path: &Path) -> rusqlite::Result<Option<i64>> {
    let norm = path.as_os_str().as_bytes();

    // Fast path: exact root-level entry.
    if let Some(id) = con
        .query_row(
            "SELECT id FROM files WHERE parent_id IS NULL AND name = ?",
            [RawText(norm)],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(Some(id));
    }

    // Find the root whose name is a prefix of `norm`, then walk the suffix.
    let roots: Vec<(i64, Vec<u8>)> = {
        let mut stmt = con.prepare("SELECT id, name FROM files WHERE parent_id IS NULL")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get_ref(1)?.as_bytes()?.to_vec()))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut start: Option<i64> = None;
    let mut remaining: Vec<Vec<u8>> = Vec::new();
    for (id, rname) in roots {
        if norm == rname.as_slice() {
            return Ok(Some(id));
        }
        if norm.len() > rname.len() && norm.starts_with(&rname) && norm[rname.len()] == b'/' {
            remaining = norm[rname.len() + 1..]
                .split(|&c| c == b'/')
                .filter(|s| !s.is_empty())
                .map(<[u8]>::to_vec)
                .collect();
            start = Some(id);
            break;
        }
    }

    let Some(mut cur) = start else {
        return Ok(None);
    };
    for part in remaining {
        match con
            .query_row(
                "SELECT id FROM files WHERE parent_id = ? AND name = ?",
                params![cur, RawText(&part)],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            Some(id) => cur = id,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// Reconstruct the full filesystem path from a node_id (port of `full_path`).
pub(crate) fn full_path(con: &Connection, file_id: i64) -> rusqlite::Result<String> {
    let mut stmt = con.prepare(
        "WITH RECURSIVE chain(id, parent_id, name) AS ( \
           SELECT id, parent_id, name FROM files WHERE id = ? \
           UNION ALL \
           SELECT f.id, f.parent_id, f.name FROM files f, chain c WHERE f.id = c.parent_id \
         ) SELECT name FROM chain",
    )?;
    let rows = stmt.query_map([file_id], |r| {
        Ok(String::from_utf8_lossy(r.get_ref(0)?.as_bytes()?).into_owned())
    })?;
    let mut parts: Vec<String> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    parts.reverse();
    if parts.is_empty() {
        return Ok(String::new());
    }
    if parts[0] == "/" {
        return Ok(format!("/{}", parts[1..].join("/")));
    }
    if parts.len() == 1 {
        return Ok(parts[0].clone());
    }
    Ok(format!("{}/{}", parts[0], parts[1..].join("/")))
}

// ---------------------------------------------------------------------------
// Formatting helpers (ports of `fmt_bytes`, `./duh:175-181`, plus Python's
// thousands grouping and `time.ctime`).
// ---------------------------------------------------------------------------

/// IEC byte formatting with 1 decimal (port of `fmt_bytes`).
pub(crate) fn fmt_bytes(n: i64) -> String {
    if n < 0 {
        return format!("-{}", fmt_bytes(-n));
    }
    for (unit, thr) in [("GiB", 1i64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if n >= thr {
            return format!("{:.1} {}", n as f64 / thr as f64, unit);
        }
    }
    format!("{n} B")
}

/// Group an integer with commas, matching Python's `{:,}`.
pub(crate) fn commafy(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let bytes = digits.as_bytes();
    let mut out = String::new();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// Format an epoch second as `time.ctime` does (local time, trailing newline
/// stripped) by delegating to the same libc the reference ultimately uses.
pub(crate) fn ctime(secs: i64) -> String {
    unsafe {
        let t: libc::time_t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0i8; 32];
        if libc::asctime_r(&tm, buf.as_mut_ptr()).is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .trim_end()
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// FREEABLE subcommand (`./duh:3076-3092`)
// ---------------------------------------------------------------------------

pub fn cmd_freeable(con: &Connection, path: &str, json: bool) -> rusqlite::Result<ExitCode> {
    let real = realpath(Path::new(path));
    let Some(node_id) = resolve_path_to_id(con, &real)? else {
        eprintln!(
            "error: path not in DB: {}\nRun `duh scan` first.",
            real.display()
        );
        return Ok(ExitCode::FAILURE);
    };

    let (freeable, locked_here) = compute(con)?;
    let f = freeable.get(&node_id).copied().unwrap_or(0);
    let lh = locked_here.get(&node_id).copied().unwrap_or(0);

    if json {
        // Rust-side addition (the reference `freeable` subcommand has no
        // `--json`); keys mirror the text fields below.
        let out = serde_json::json!({
            "path": real.to_string_lossy(),
            "freeable": f,
            "locked_here": lh,
        });
        println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
        return Ok(ExitCode::SUCCESS);
    }

    println!("Path: {}", real.display());
    println!("  Freeable:    {}", fmt_bytes(f as i64));
    println!("  Locked here: {}", fmt_bytes(lh as i64));
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// MARGINAL subcommand (`./duh:1084-1217`, `_compute_marginal_freeable`
// `./duh:972-1078`)
// ---------------------------------------------------------------------------

struct MarginalResult {
    strict_bytes: i64,
    apparent_bytes: i64,
    total_blocks: i64,
    file_count: i64,
    dir_count: i64,
}

pub(crate) const DESC_CTE: &str = "WITH RECURSIVE desc_ids(id) AS ( \
    SELECT id FROM files WHERE id = ?1 \
    UNION ALL \
    SELECT f.id FROM files f JOIN desc_ids d ON f.parent_id = d.id ) ";

fn compute_marginal_freeable(con: &Connection, root_id: i64) -> rusqlite::Result<MarginalResult> {
    // Counts (apparent, total_blocks, file_count, dir_count).
    let counts_sql = format!(
        "{DESC_CTE} SELECT \
           SUM(CASE WHEN f.is_dir=0 THEN 1 ELSE 0 END), \
           SUM(CASE WHEN f.is_dir=1 AND f.is_excluded=0 THEN 1 ELSE 0 END), \
           SUM(CASE WHEN f.is_dir=0 THEN f.size_logical WHEN f.is_excluded=1 THEN f.size_logical ELSE 0 END), \
           SUM(CASE WHEN f.is_dir=0 THEN f.size_blocks WHEN f.is_excluded=1 THEN f.size_blocks ELSE 0 END) \
         FROM files f JOIN desc_ids d ON f.id = d.id"
    );
    let (file_count, dir_count, apparent, total_blocks) =
        con.query_row(&counts_sql, [root_id], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            ))
        })?;

    // Inside files (leaves only).
    let inside_sql = format!(
        "{DESC_CTE} SELECT f.clone_id, f.dev, f.ino, f.nlinks, f.size_blocks \
         FROM files f JOIN desc_ids d ON f.id = d.id WHERE f.is_dir = 0"
    );
    let mut clone_inside: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut hl_inside: HashMap<(i64, i64), [i64; 3]> = HashMap::new(); // (inside_cnt, nlinks, blocks)
    let mut singleton_blocks: i64 = 0;
    {
        let mut stmt = con.prepare(&inside_sql)?;
        let rows = stmt.query_map([root_id], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (cid, dev, ino, nlinks, blk) = row?;
            if let Some(c) = cid {
                clone_inside.entry(c).or_default().push(blk);
            } else if nlinks > 1 {
                let e = hl_inside.entry((dev, ino)).or_insert([0, nlinks, blk]);
                e[0] += 1;
            } else {
                singleton_blocks += blk;
            }
        }
    }

    // Global clone counts for clone_ids seen inside (batched IN queries).
    let mut clone_strict: i64 = 0;
    if !clone_inside.is_empty() {
        let cids: Vec<i64> = clone_inside.keys().copied().collect();
        let mut global: HashMap<i64, i64> = HashMap::new();
        for batch in cids.chunks(500) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT clone_id, COUNT(*) FROM files WHERE clone_id IN ({placeholders}) GROUP BY clone_id"
            );
            let mut stmt = con.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(batch.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (cid, cnt) = row?;
                global.insert(cid, cnt);
            }
        }
        for (cid, blocks) in &clone_inside {
            let inside_cnt = blocks.len() as i64;
            let total_cnt = global.get(cid).copied().unwrap_or(inside_cnt);
            if inside_cnt >= total_cnt {
                clone_strict += blocks.iter().copied().max().unwrap_or(0);
            }
        }
    }

    // Hardlink families fully inside.
    let mut hl_strict: i64 = 0;
    for [inside_cnt, nlinks, blocks] in hl_inside.values() {
        if inside_cnt >= nlinks {
            hl_strict += blocks;
        }
    }

    Ok(MarginalResult {
        strict_bytes: clone_strict + hl_strict + singleton_blocks,
        apparent_bytes: apparent,
        total_blocks,
        file_count,
        dir_count,
    })
}

pub fn cmd_marginal(con: &Connection, path: &str, json: bool) -> rusqlite::Result<ExitCode> {
    let real = realpath(Path::new(path));
    let real_str = real.to_string_lossy().into_owned();
    let Some(root_id) = resolve_path_to_id(con, &real)? else {
        eprintln!("error: no data for path {real_str} — run `duh scan` first");
        return Ok(ExitCode::FAILURE);
    };

    let result = compute_marginal_freeable(con, root_id)?;

    if json {
        // A struct (not json!) preserves the reference dict's field order.
        let out = OrderedMarginal {
            path: real_str,
            freeable_bytes: result.strict_bytes,
            strict_marginal_bytes: result.strict_bytes,
            apparent_bytes: result.apparent_bytes,
            total_blocks_bytes: result.total_blocks,
            file_count: result.file_count,
            dir_count: result.dir_count,
        };
        println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
        return Ok(ExitCode::SUCCESS);
    }

    println!("Marginal disk cost: {real_str}");
    println!(
        "  (family-once semantics: clone families count max(blocks) once, not per-member sum)"
    );
    println!();
    println!(
        "  {:<24} {:>12}  (what rm -rf returns to df)",
        "Freeable (strict):",
        fmt_bytes(result.strict_bytes)
    );
    println!(
        "  {:<24} {:>12}",
        "Apparent (du-style):",
        fmt_bytes(result.apparent_bytes)
    );
    println!(
        "  {:<24} {:>12}",
        "On-disk blocks:",
        fmt_bytes(result.total_blocks)
    );
    println!("  {:<24} {:>12}", "Files:", commafy(result.file_count));
    println!("  {:<24} {:>12}", "Dirs:", commafy(result.dir_count));

    print_marginal_leaks(con, root_id)?;

    Ok(ExitCode::SUCCESS)
}

/// One external-leak candidate: a family with members outside the target
/// subtree. Mirrors the reference's 6-tuple `(fid, blocks_ext, clone_id, dev,
/// ino, nlinks)`; nlinks is carried by the reference but never printed.
struct LeakCand {
    fid: i64,
    blocks_ext: f64,
    clone_id: Option<i64>,
    dev: i64,
    ino: i64,
}

/// Port of the external-leaks section of `cmd_marginal` (`./duh:1100-1217`):
/// families (clone or hardlink) with members outside the target get an
/// approximate outside-blocks figure, top 5 printed. The reference computes
/// this even in `--json` mode but never includes it in the JSON output, so the
/// Rust port only runs it on the text path (observably identical).
fn print_marginal_leaks(con: &Connection, root_id: i64) -> rusqlite::Result<()> {
    // Re-run the inside query (the reference re-queries rather than reusing
    // `_compute_marginal_freeable`'s pass).
    let inside_sql = format!(
        "{DESC_CTE} SELECT f.id, f.dev, f.ino, f.clone_id, f.nlinks, f.size_blocks \
         FROM files f JOIN desc_ids d ON f.id = d.id WHERE f.is_dir = 0"
    );
    // Member tuple: (id, nlinks, size_blocks).
    type Member = (i64, i64, i64);
    let mut inside_ids: HashSet<i64> = HashSet::new();
    // Python dicts preserve insertion order, and that order feeds the stable
    // sort below — so group with first-seen-ordered Vecs, not a bare HashMap.
    let mut clone_grp: Vec<(i64, Vec<Member>)> = Vec::new();
    let mut clone_idx: HashMap<i64, usize> = HashMap::new();
    let mut hl_grp: Vec<((i64, i64), Vec<Member>)> = Vec::new();
    let mut hl_idx: HashMap<(i64, i64), usize> = HashMap::new();
    {
        let mut stmt = con.prepare(&inside_sql)?;
        let rows = stmt.query_map([root_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        for row in rows {
            let (id, dev, ino, cid, nlinks, blk) = row?;
            inside_ids.insert(id);
            if let Some(c) = cid {
                let i = *clone_idx.entry(c).or_insert_with(|| {
                    clone_grp.push((c, Vec::new()));
                    clone_grp.len() - 1
                });
                clone_grp[i].1.push((id, nlinks, blk));
            } else if nlinks > 1 {
                let i = *hl_idx.entry((dev, ino)).or_insert_with(|| {
                    hl_grp.push(((dev, ino), Vec::new()));
                    hl_grp.len() - 1
                });
                hl_grp[i].1.push((id, nlinks, blk));
            }
        }
    }

    // Global clone counts (batched IN queries of 500, as in the reference).
    let mut global: HashMap<i64, i64> = HashMap::new();
    if !clone_grp.is_empty() {
        let cids: Vec<i64> = clone_grp.iter().map(|(c, _)| *c).collect();
        for batch in cids.chunks(500) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT clone_id, COUNT(*) FROM files WHERE clone_id IN ({placeholders}) GROUP BY clone_id"
            );
            let mut stmt = con.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(batch.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (cid, cnt) = row?;
                global.insert(cid, cnt);
            }
        }
    }

    // Collect leak candidates: families with members outside. Clone families
    // first, then hardlink families — the reference appends in that order and
    // relies on stable sort to keep ties in insertion order.
    let mut cands: Vec<LeakCand> = Vec::new();
    for (cid, members) in &clone_grp {
        let inside_cnt = members.len() as i64;
        let total_cnt = global.get(cid).copied().unwrap_or(inside_cnt);
        if inside_cnt < total_cnt {
            // Some members outside — approximate blocks held outside.
            let max_blk = members.iter().map(|m| m.2).max().unwrap_or(0);
            let frac_outside = (total_cnt - inside_cnt) as f64 / total_cnt as f64;
            cands.push(LeakCand {
                fid: members[0].0,
                blocks_ext: max_blk as f64 * frac_outside,
                clone_id: Some(*cid),
                dev: 0,
                ino: 0,
            });
        }
    }
    for ((dev, ino), members) in &hl_grp {
        let nlinks = members[0].1;
        let inside_cnt = members.len() as i64;
        if inside_cnt < nlinks {
            let blk = members[0].2;
            let frac_outside = (nlinks - inside_cnt) as f64 / nlinks as f64;
            cands.push(LeakCand {
                fid: members[0].0,
                blocks_ext: blk as f64 * frac_outside,
                clone_id: None,
                dev: *dev,
                ino: *ino,
            });
        }
    }
    // Stable sort descending by blocks_ext (Python `sort(key=..., reverse=True)`
    // keeps ties in insertion order; so does Vec::sort_by).
    cands.sort_by(|a, b| {
        b.blocks_ext
            .partial_cmp(&a.blocks_ext)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cands.truncate(5);

    if cands.is_empty() {
        return Ok(());
    }

    println!();
    println!("  Blocks shared with paths OUTSIDE this directory (top 5):");
    let home = std::env::var("HOME").unwrap_or_default();
    let shorten = |p: &str| -> String {
        if !home.is_empty() && p.starts_with(&home) {
            p.replacen(&home, "~", 1)
        } else {
            p.to_string()
        }
    };
    // os.path.dirname for the normalized absolute paths full_path produces.
    fn dirname(p: &str) -> &str {
        match p.rfind('/') {
            None => "",
            Some(0) => "/",
            Some(i) => &p[..i],
        }
    }

    for cand in &cands {
        let fpath = full_path(con, cand.fid)?;
        let short_fpath = shorten(&fpath);

        // External family members (ids outside the target subtree).
        let ext_ids: Vec<i64> = if let Some(cid) = cand.clone_id {
            // The reference collects the family into a set, subtracts
            // inside_id_set, and takes the first 10. CPython's small-int set
            // iteration approximates ascending order; sort to match.
            let mut stmt = con.prepare("SELECT id FROM files WHERE clone_id = ? AND id != ?")?;
            let rows = stmt.query_map(params![cid, cand.fid], |r| r.get::<_, i64>(0))?;
            let mut ext: Vec<i64> = rows
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|id| !inside_ids.contains(id))
                .collect();
            ext.sort_unstable();
            ext.dedup();
            ext.truncate(10);
            ext
        } else {
            // Hardlink branch keeps query order (the reference filters a list).
            let mut stmt = con
                .prepare("SELECT id FROM files WHERE dev = ? AND ino = ? AND nlinks > 1 AND id != ?")?;
            let rows = stmt.query_map(params![cand.dev, cand.ino, cand.fid], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|id| !inside_ids.contains(id))
                .collect()
        };

        // Distinct parent dirs of the first 5 external members, sorted, top 2.
        let mut ext_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for &eid in ext_ids.iter().take(5) {
            ext_dirs.insert(shorten(dirname(&full_path(con, eid)?)));
        }
        let ext_summary = ext_dirs
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "    {:>10}  {}  → {}",
            fmt_bytes(cand.blocks_ext as i64),
            short_fpath,
            ext_summary
        );
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct OrderedMarginal {
    path: String,
    freeable_bytes: i64,
    strict_marginal_bytes: i64,
    apparent_bytes: i64,
    total_blocks_bytes: i64,
    file_count: i64,
    dir_count: i64,
}

// ---------------------------------------------------------------------------
// FILE subcommand (`./duh:1223-1272`)
// ---------------------------------------------------------------------------

pub fn cmd_file(con: &Connection, path: &str) -> rusqlite::Result<ExitCode> {
    let real = realpath(Path::new(path));
    let real_str = real.to_string_lossy().into_owned();
    let Some(file_id) = resolve_path_to_id(con, &real)? else {
        eprintln!(
            "error: file not in DB: {real_str}\nRun `duh scan` on the containing directory first."
        );
        return Ok(ExitCode::FAILURE);
    };

    let row = con
        .query_row(
            "SELECT size_logical, size_blocks, ino, dev, nlinks, clone_id, mtime, \
                    is_excluded, excluded_file_count FROM files WHERE id = ?",
            [file_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                ))
            },
        )
        .optional()?;
    let Some((size_logical, size_blocks, ino, dev, nlinks, clone_id, mtime, is_excluded, excl_count)) =
        row
    else {
        eprintln!("error: file not in DB: {real_str}");
        return Ok(ExitCode::FAILURE);
    };

    println!("File: {real_str}");
    println!(
        "  size_logical:  {} ({} bytes)",
        fmt_bytes(size_logical),
        commafy(size_logical)
    );
    println!(
        "  size_blocks:   {} ({} bytes)",
        fmt_bytes(size_blocks),
        commafy(size_blocks)
    );
    println!("  inode:         {ino}  dev={dev}");
    println!("  nlinks:        {nlinks}");
    println!(
        "  clone_id:      {}",
        clone_id.map_or_else(|| "None".to_string(), |c| c.to_string())
    );
    println!("  mtime:         {}", ctime(mtime));
    if is_excluded != 0 {
        println!(
            "  [EXCLUDED DIR: {} files, aggregate sizes above]",
            excl_count.unwrap_or(0)
        );
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let shorten = |p: &str| -> String {
        if !home.is_empty() && p.starts_with(&home) {
            p.replacen(&home, "~", 1)
        } else {
            p.to_string()
        }
    };

    if let Some(cid) = clone_id {
        let family: Vec<i64> = {
            let mut stmt =
                con.prepare("SELECT id FROM files WHERE clone_id = ? AND id != ?")?;
            let rows = stmt.query_map(params![cid, file_id], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !family.is_empty() {
            println!("\n  Clone family ({} other members):", family.len());
            for &m in family.iter().take(10) {
                println!("    {}", shorten(&full_path(con, m)?));
            }
            if family.len() > 10 {
                println!("    ... and {} more", family.len() - 10);
            }
        }
    }

    if nlinks > 1 {
        let hl_family: Vec<i64> = {
            let mut stmt =
                con.prepare("SELECT id FROM files WHERE dev=? AND ino=? AND id != ?")?;
            let rows = stmt.query_map(params![dev, ino, file_id], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !hl_family.is_empty() {
            println!("\n  Hardlink family ({} other members):", hl_family.len());
            for &m in hl_family.iter().take(10) {
                println!("    {}", shorten(&full_path(con, m)?));
            }
            if hl_family.len() > 10 {
                println!("    ... and {} more", hl_family.len() - 10);
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_matches_reference() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1 << 10), "1.0 KiB");
        assert_eq!(fmt_bytes(1 << 20), "1.0 MiB");
        assert_eq!(fmt_bytes(3 << 20), "3.0 MiB");
        assert_eq!(fmt_bytes(1 << 30), "1.0 GiB");
        assert_eq!(fmt_bytes(-(1 << 20)), "-1.0 MiB");
    }

    #[test]
    fn commafy_matches_python() {
        assert_eq!(commafy(0), "0");
        assert_eq!(commafy(100), "100");
        assert_eq!(commafy(1000), "1,000");
        assert_eq!(commafy(1234567), "1,234,567");
        assert_eq!(commafy(-1000), "-1,000");
    }

    #[test]
    fn lca_and_direct_child() {
        // Tree: 1 -> {2, 3}; 2 -> {4, 5}; 3 -> {6}
        //   depth: 1=0, 2=1, 3=1, 4=2, 5=2, 6=2
        let parent = vec![-1i64, -1, 1, 1, 2, 2, 3];
        let depth = vec![-1i32, 0, 1, 1, 2, 2, 2];
        let max_id = 6;
        assert_eq!(lca_arr(4, 5, &depth, &parent, max_id), 2);
        assert_eq!(lca_arr(4, 6, &depth, &parent, max_id), 1);
        assert_eq!(lca_arr(2, 4, &depth, &parent, max_id), 2);
        assert_eq!(direct_child_of_arr(1, 4, &parent), 2);
        assert_eq!(direct_child_of_arr(1, 6, &parent), 3);
        assert_eq!(direct_child_of_arr(2, 5, &parent), 5);
    }
}
