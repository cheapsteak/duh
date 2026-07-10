//! Port of the reference oracle `reference/duh-py` report subcommands: `top` (`reference/duh-py:840-967`),
//! `clones` (`reference/duh-py:1278-1352`), `excluded` (`reference/duh-py:1357-1392`), `sql` +
//! `_VIEWS_SQL` (`reference/duh-py:1397-1445`), `stats` (`reference/duh-py:1450-1523`), and
//! `clusters` (`reference/duh-py:3023-3072`).
//!
//! Every SQL string below is copied verbatim from the reference — it is the
//! spec. Where the reference already emits `--json` (top, clones, clusters)
//! the JSON shape is copied byte-for-byte from the Python dict literals,
//! including one quirk: `clones --json`'s `waste_bytes` uses the family's
//! *total* blocks (`blocks * max(count-1, 0)`), which differs from the text
//! table's `WASTE` column (`each * max(count-1, 0)`, where `each = blocks /
//! count`). That's what the oracle does, so that's what we do.
//!
//! `excluded --json` and `freeable --json` (the latter lives in
//! `freeable.rs`) have no reference counterpart — the reference's argparse
//! setup never adds `--json` to those two subcommands (confirmed against
//! `reference/duh-py:3149-3151` and `:3159-3161`). Those two schemas are additive Rust
//! choices, kept consistent with each command's own text-report columns.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

use rusqlite::{named_params, Connection, OptionalExtension};

use crate::freeable::{commafy, compute as compute_freeable, ctime, fmt_bytes, full_path, resolve_path_to_id, DESC_CTE};
use crate::scan::realpath;

/// Port of `col_width` (`reference/duh-py:191-193`).
fn col_width(items: &[String], header: &str) -> usize {
    if items.is_empty() {
        header.chars().count()
    } else {
        items
            .iter()
            .map(|s| s.chars().count())
            .chain(std::iter::once(header.chars().count()))
            .max()
            .unwrap_or(0)
    }
}

/// Port of `fmt_bytes_highlight` (`reference/duh-py:184-189`).
fn fmt_bytes_highlight(real: i64, apparent: i64) -> String {
    let s = fmt_bytes(apparent);
    if real > 0 && ((apparent - real).abs() as f64 / real as f64) > 0.05 {
        format!("({s})")
    } else {
        s
    }
}

/// `os.path.basename(p) or p` — basename of a POSIX path, falling back to the
/// full path when the basename is empty (e.g. `p == "/"`).
fn basename(p: &str) -> String {
    let b = p.rsplit('/').next().unwrap_or("");
    if b.is_empty() {
        p.to_string()
    } else {
        b.to_string()
    }
}

/// `~`-shorten a path the way every report command does:
/// `path.replace(os.path.expanduser("~"), "~")` — Python's `str.replace`
/// rewrites *every* occurrence anywhere in the path, including a `$HOME`
/// substring in the middle. This is an accepted deviation: we replace only the
/// leading prefix, which is deliberately saner (a mid-path `$HOME` is almost
/// always coincidental and should not be mangled).
fn shorten<'a>(home: &str, p: &'a str) -> std::borrow::Cow<'a, str> {
    if !home.is_empty() && p.starts_with(home) {
        std::borrow::Cow::Owned(p.replacen(home, "~", 1))
    } else {
        std::borrow::Cow::Borrowed(p)
    }
}

// ---------------------------------------------------------------------------
// TOP (`reference/duh-py:840-967`)
// ---------------------------------------------------------------------------

/// The recursive-CTE query, copied verbatim from `reference/duh-py:872-915`.
const TOP_SQL: &str = "
    WITH RECURSIVE
    tree(id, is_dir, is_excluded, size_blocks, size_logical, clone_id,
         excluded_file_count, cur_depth, bucket_id) AS (
      -- Seed: direct children of root_id at depth=1
      SELECT f.id, f.is_dir, f.is_excluded,
             f.size_blocks, f.size_logical, f.clone_id, f.excluded_file_count,
             1 AS cur_depth,
             f.id AS bucket_id
      FROM files f WHERE f.parent_id = :root_id
      UNION ALL
      SELECT f.id, f.is_dir, f.is_excluded,
             f.size_blocks, f.size_logical, f.clone_id, f.excluded_file_count,
             t.cur_depth + 1,
             -- once we've gone `depth` levels deep, freeze the bucket_id
             CASE WHEN t.cur_depth >= :depth THEN t.bucket_id ELSE f.id END
      FROM files f JOIN tree t ON f.parent_id = t.id
      WHERE t.is_excluded = 0   -- excluded dirs are leaf aggregates, don't recurse
        AND t.is_dir = 1        -- only recurse into directories
    ),
    multi_clones(clone_id) AS (
      SELECT clone_id FROM files WHERE clone_id IS NOT NULL
      GROUP BY clone_id HAVING COUNT(*) > 1
    )
    SELECT
      bucket_id,
      SUM(CASE WHEN is_dir = 0 OR is_excluded = 1 THEN size_blocks ELSE 0 END) AS total_blocks,
      SUM(CASE WHEN is_dir = 0 OR is_excluded = 1 THEN size_logical ELSE 0 END) AS total_logical,
      SUM(CASE WHEN is_dir = 0 THEN 1
               WHEN is_excluded = 1 THEN COALESCE(excluded_file_count, 0)
               ELSE 0 END) AS file_count,
      SUM(CASE WHEN clone_id IS NOT NULL
                    AND clone_id IN (SELECT clone_id FROM multi_clones)
                    AND is_dir = 0
               THEN size_blocks ELSE 0 END) AS clone_blocks
    FROM tree
    GROUP BY bucket_id
    ORDER BY
      CASE :sort_by
        WHEN 'logical' THEN SUM(CASE WHEN is_dir = 0 OR is_excluded = 1 THEN size_logical ELSE 0 END)
        ELSE SUM(CASE WHEN is_dir = 0 OR is_excluded = 1 THEN size_blocks ELSE 0 END)
      END DESC
    LIMIT :limit
    ";

#[allow(clippy::too_many_arguments)]
pub fn cmd_top(
    con: &Connection,
    under: Option<&str>,
    by: &str,
    limit: i64,
    depth: i64,
    json: bool,
) -> rusqlite::Result<ExitCode> {
    let root_id = match under {
        Some(u) => {
            let real = realpath(Path::new(u));
            match resolve_path_to_id(con, &real)? {
                Some(id) => id,
                None => {
                    eprintln!(
                        "error: path not found in DB: {}\nRun `duh scan` first.",
                        real.display()
                    );
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        None => {
            let scan_root: Option<String> = con
                .query_row("SELECT root FROM scans ORDER BY id DESC LIMIT 1", [], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(root_str) = scan_root else {
                eprintln!("error: no scans found. Run `duh scan <path>` first.");
                return Ok(ExitCode::FAILURE);
            };
            match resolve_path_to_id(con, Path::new(&root_str))? {
                Some(id) => id,
                None => {
                    eprintln!("error: root not found in DB: {root_str}");
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
    };

    let rows: Vec<(i64, i64, i64, i64, i64)> = {
        let mut stmt = con.prepare(TOP_SQL)?;
        let mapped = stmt.query_map(
            named_params! { ":root_id": root_id, ":depth": depth, ":sort_by": by, ":limit": limit },
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if rows.is_empty() {
        println!("(no data)");
        return Ok(ExitCode::SUCCESS);
    }

    let mut bucket_paths = Vec::with_capacity(rows.len());
    for &(bucket_id, ..) in &rows {
        bucket_paths.push(full_path(con, bucket_id)?);
    }

    let names: Vec<String> = bucket_paths.iter().map(|p| basename(p)).collect();
    let reals: Vec<String> = rows.iter().map(|&(_, blocks, ..)| fmt_bytes(blocks)).collect();
    let apparents: Vec<String> = rows
        .iter()
        .map(|&(_, blocks, logical, ..)| fmt_bytes_highlight(blocks, logical))
        .collect();
    let counts: Vec<String> = rows.iter().map(|&(_, _, _, fc, _)| fc.to_string()).collect();

    if json {
        let mut out = Vec::with_capacity(rows.len());
        for (i, &(_, blocks, logical, fc, clone_blocks)) in rows.iter().enumerate() {
            out.push(serde_json::json!({
                "path": bucket_paths[i],
                "name": names[i],
                "blocks": blocks,
                "logical": logical,
                "clone_blocks": clone_blocks,
                "file_count": fc,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
        return Ok(ExitCode::SUCCESS);
    }

    let w_name = col_width(&names, "NAME");
    let w_real = col_width(&reals, "REAL");
    let w_appar = col_width(&apparents, "APPARENT");
    let w_count = col_width(&counts, "FILES");

    let header = format!(
        "{:<w_name$}  {:>w_real$}  {:>w_appar$}  {:>w_count$}",
        "NAME", "REAL", "APPARENT", "FILES",
    );
    println!("{header}");
    println!("{}", "-".repeat(header.chars().count()));
    for i in 0..rows.len() {
        println!(
            "{:<w_name$}  {:>w_real$}  {:>w_appar$}  {:>w_count$}",
            names[i], reals[i], apparents[i], counts[i],
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// CLONES (`reference/duh-py:1278-1352`)
// ---------------------------------------------------------------------------

pub fn cmd_clones(
    con: &Connection,
    under: Option<&str>,
    min_bytes: i64,
    limit: i64,
    json: bool,
) -> rusqlite::Result<ExitCode> {
    let rows: Vec<(i64, i64, i64)> = if let Some(u) = under {
        let real = realpath(Path::new(u));
        let Some(root_id) = resolve_path_to_id(con, &real)? else {
            eprintln!("error: path not found in DB: {}", real.display());
            return Ok(ExitCode::FAILURE);
        };
        let sql = format!(
            "{DESC_CTE} SELECT clone_id, id, size_blocks FROM files \
             WHERE clone_id IS NOT NULL AND id IN (SELECT id FROM desc_ids)"
        );
        let mut stmt = con.prepare(&sql)?;
        let mapped = stmt.query_map([root_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt =
            con.prepare("SELECT clone_id, id, size_blocks FROM files WHERE clone_id IS NOT NULL")?;
        let mapped = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Group by clone_id, preserving first-seen order (mirrors Python's
    // insertion-ordered `defaultdict`).
    let mut order: Vec<i64> = Vec::new();
    let mut idx: HashMap<i64, usize> = HashMap::new();
    let mut fam_blocks: Vec<i64> = Vec::new();
    let mut fam_count: Vec<i64> = Vec::new();
    let mut fam_ids: Vec<Vec<i64>> = Vec::new();
    for (cid, id, blk) in rows {
        let i = *idx.entry(cid).or_insert_with(|| {
            order.push(cid);
            fam_blocks.push(0);
            fam_count.push(0);
            fam_ids.push(Vec::new());
            order.len() - 1
        });
        fam_blocks[i] += blk;
        fam_count[i] += 1;
        fam_ids[i].push(id);
    }

    let mut families: Vec<(i64, i64, i64, Vec<i64>)> = Vec::new();
    for (i, &cid) in order.iter().enumerate() {
        let count = fam_count[i];
        let blocks = fam_blocks[i];
        if count > 1 && blocks >= min_bytes {
            families.push((cid, count, blocks, std::mem::take(&mut fam_ids[i])));
        }
    }
    // Stable descending sort by (blocks // count) * max(count - 1, 0) —
    // `sort(key=..., reverse=True)` in Python keeps original relative order
    // for ties, which `sort_by` comparing b-vs-a also does.
    families.sort_by(|a, b| {
        let ka = (a.2 / a.1) * (a.1 - 1).max(0);
        let kb = (b.2 / b.1) * (b.1 - 1).max(0);
        kb.cmp(&ka)
    });
    families.truncate(limit.max(0) as usize);

    if families.is_empty() {
        println!("(no clone families found)");
        return Ok(ExitCode::SUCCESS);
    }

    let home = std::env::var("HOME").unwrap_or_default();

    if json {
        let mut out = Vec::with_capacity(families.len());
        for (cid, count, blocks, ids) in &families {
            let mut sample = Vec::with_capacity(3);
            for &id in ids.iter().take(3) {
                sample.push(shorten(&home, &full_path(con, id)?).into_owned());
            }
            out.push(serde_json::json!({
                "clone_id": cid,
                "member_count": count,
                "blocks_each": if *count != 0 { blocks / count } else { 0 },
                "blocks_total": blocks,
                "waste_bytes": blocks * (count - 1).max(0),
                "sample_paths": sample,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{:<18}  {:>5}  {:>12}  {:>12}  PATHS (sample)",
        "CLONE_ID", "COUNT", "BLOCKS_EACH", "WASTE"
    );
    println!("{}", "-".repeat(90));
    for (cid, count, blocks, ids) in &families {
        let each = if *count != 0 { blocks / count } else { 0 };
        let waste = each * (count - 1).max(0);
        let mut sample = Vec::with_capacity(3);
        for &id in ids.iter().take(3) {
            sample.push(shorten(&home, &full_path(con, id)?).into_owned());
        }
        println!(
            "{:<18}  {:>5}  {:>12}  {:>12}  {}",
            cid,
            count,
            fmt_bytes(each),
            fmt_bytes(waste),
            sample[0]
        );
        for p in &sample[1..] {
            println!("{:<18}  {:>5}  {:>12}  {:>12}  {}", "", "", "", "", p);
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// EXCLUDED (`reference/duh-py:1357-1392`)
// ---------------------------------------------------------------------------

/// `--json` is a Rust-only addition (the reference `excluded` parser has no
/// `--json`, `reference/duh-py:3149-3151`). Keys mirror the text table's RANK/BLOCKS/
/// FILES/PATH columns plus `size_logical`, which the SQL selects but the text
/// report never prints.
pub fn cmd_excluded(con: &Connection, limit: i64, json: bool) -> rusqlite::Result<ExitCode> {
    let rows: Vec<(i64, i64, i64, Option<i64>)> = {
        let mut stmt = con.prepare(
            "SELECT id, size_blocks, size_logical, excluded_file_count \
             FROM files WHERE is_excluded = 1 \
             ORDER BY size_blocks DESC \
             LIMIT ?",
        )?;
        let mapped = stmt.query_map([limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if rows.is_empty() {
        println!("(no excluded directories found)");
        return Ok(ExitCode::SUCCESS);
    }

    let home = std::env::var("HOME").unwrap_or_default();

    let mut paths = Vec::with_capacity(rows.len());
    let mut blocks_strs = Vec::with_capacity(rows.len());
    let mut files_strs = Vec::with_capacity(rows.len());
    for &(id, blk, _logical, fc) in &rows {
        paths.push(shorten(&home, &full_path(con, id)?).into_owned());
        blocks_strs.push(fmt_bytes(blk));
        files_strs.push(fc.unwrap_or(0).to_string());
    }

    if json {
        let mut out = Vec::with_capacity(rows.len());
        for (i, &(_id, blk, logical, fc)) in rows.iter().enumerate() {
            out.push(serde_json::json!({
                "rank": i + 1,
                "path": paths[i],
                "blocks": blk,
                "logical": logical,
                "files": fc.unwrap_or(0),
            }));
        }
        println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
        return Ok(ExitCode::SUCCESS);
    }

    let ranks: Vec<String> = (1..=rows.len()).map(|i| i.to_string()).collect();
    let w_rank = col_width(&ranks, "RANK");
    let w_blocks = col_width(&blocks_strs, "BLOCKS");
    let w_files = col_width(&files_strs, "FILES");
    let w_path = col_width(&paths, "PATH");

    let header = format!(
        "{:>w_rank$}  {:>w_blocks$}  {:>w_files$}  {:<w_path$}",
        "RANK", "BLOCKS", "FILES", "PATH",
    );
    println!("{header}");
    println!("{}", "-".repeat(header.chars().count()));
    for i in 0..rows.len() {
        println!(
            "{:>w_rank$}  {:>w_blocks$}  {:>w_files$}  {:<w_path$}",
            ranks[i], blocks_strs[i], files_strs[i], paths[i],
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// CLUSTERS (`reference/duh-py:3023-3072`)
// ---------------------------------------------------------------------------

pub fn cmd_clusters(
    con: &Connection,
    min_bytes: i64,
    limit: i64,
    json: bool,
) -> rusqlite::Result<ExitCode> {
    let (freeable_map, locked_here_map) = compute_freeable(con)?;

    let mut ranked: Vec<(i64, u64)> = locked_here_map.into_iter().collect();
    // Stable descending sort by locked_here. (The reference sorts a
    // Python dict's `.items()`, whose iteration order is insertion order;
    // Rust's `HashMap` iteration order is unspecified, so ties may print in a
    // different relative order than the oracle. That's fine: `clusters` is
    // not one of the byte-parity-checked commands.)
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let min_bytes_u = min_bytes.max(0) as u64;
    let ranked: Vec<(i64, u64)> = ranked
        .into_iter()
        .filter(|&(_, v)| v >= min_bytes_u)
        .take(limit.max(0) as usize)
        .collect();

    if ranked.is_empty() {
        println!("(no clusters found)");
        return Ok(ExitCode::SUCCESS);
    }

    let home = std::env::var("HOME").unwrap_or_default();

    if json {
        let mut out = Vec::with_capacity(ranked.len());
        for &(nid, locked) in &ranked {
            let path = shorten(&home, &full_path(con, nid)?).into_owned();
            out.push(serde_json::json!({ "path": path, "locked_bytes": locked }));
        }
        println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
        return Ok(ExitCode::SUCCESS);
    }

    for &(nid, locked) in &ranked {
        let path = shorten(&home, &full_path(con, nid)?).into_owned();
        let children: Vec<(i64, String)> = {
            let mut stmt = con.prepare("SELECT id, name FROM files WHERE parent_id = ?")?;
            let mapped = stmt.query_map([nid], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    String::from_utf8_lossy(r.get_ref(1)?.as_bytes()?).into_owned(),
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let n_children = children.len();
        println!(
            "{} locked at {}  (members span ~{} children)",
            fmt_bytes(locked as i64),
            path,
            n_children
        );
        let mut child_freeable: Vec<(String, u64)> = children
            .iter()
            .map(|(id, name)| (name.clone(), freeable_map.get(id).copied().unwrap_or(0)))
            .collect();
        child_freeable.sort_by(|a, b| b.1.cmp(&a.1));
        let mut shown: i64 = 0;
        for (name, fb) in child_freeable.iter().take(6) {
            if *fb > 0 {
                println!("    members in: {name}");
                shown += 1;
            }
        }
        let rest = n_children as i64 - shown;
        if rest > 0 {
            println!("    +{rest} more");
        }
        println!();
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// SQL (`reference/duh-py:1397-1445`)
// ---------------------------------------------------------------------------

/// Verbatim from `reference/duh-py:1397-1429` (`_VIEWS_SQL`). Do not "improve" this SQL.
const VIEWS_SQL: &str = "
CREATE TEMP VIEW IF NOT EXISTS v_dir_real AS
  SELECT f.parent_id,
         SUM(f.size_blocks) AS blocks_total,
         COUNT(*) AS file_count
  FROM files f
  WHERE f.is_dir = 0
  GROUP BY f.parent_id;

CREATE TEMP VIEW IF NOT EXISTS v_clone_families AS
  SELECT clone_id,
         COUNT(*) AS member_count,
         SUM(size_blocks) AS blocks,
         MIN(id) AS sample_id
  FROM files
  WHERE clone_id IS NOT NULL
  GROUP BY clone_id;

CREATE TEMP VIEW IF NOT EXISTS v_hardlink_families AS
  SELECT dev, ino,
         COUNT(*) AS nlinks_in_db,
         MIN(size_blocks) AS blocks,
         MIN(id) AS sample_id
  FROM files
  WHERE nlinks > 1
  GROUP BY dev, ino;

CREATE TEMP VIEW IF NOT EXISTS v_excluded AS
  SELECT id, size_blocks, size_logical, excluded_file_count
  FROM files WHERE is_excluded = 1
  ORDER BY size_blocks DESC;
";

/// Port of `cmd_sql` (`reference/duh-py:1431-1444`). Unlike every other report command,
/// this one operates on the raw DB path rather than an open `Connection` —
/// the reference checks `os.path.exists` (no implicit schema creation) and
/// then hands the whole terminal over to `sqlite3 -init <views> <db>`.
pub fn cmd_sql(db_path: &Path) -> ExitCode {
    if !db_path.exists() {
        eprintln!(
            "error: DB not found: {}\nRun `duh scan <path>` first.",
            db_path.display()
        );
        return ExitCode::FAILURE;
    }

    let init_path =
        std::env::temp_dir().join(format!("duh-init-{}-{}.sql", std::process::id(), std::process::id()));
    if let Err(e) = std::fs::write(&init_path, VIEWS_SQL) {
        eprintln!("error: could not write init script: {e}");
        return ExitCode::FAILURE;
    }

    let status = std::process::Command::new("sqlite3")
        .arg("-init")
        .arg(&init_path)
        .arg(db_path)
        .status();
    let _ = std::fs::remove_file(&init_path);

    match status {
        Ok(_) => ExitCode::SUCCESS, // reference's subprocess.run(check=False) ignores the exit code too
        Err(e) => {
            eprintln!("error: failed to spawn sqlite3: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// STATS (`reference/duh-py:1450-1523`)
// ---------------------------------------------------------------------------

pub fn cmd_stats(con: &Connection) -> rusqlite::Result<ExitCode> {
    type ScanRow = (
        String,
        f64,
        Option<f64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let scan: Option<ScanRow> = con
        .query_row(
            "SELECT root, started_at, finished_at, files_count, excluded_count, \
                    bytes_logical, bytes_blocks FROM scans ORDER BY id DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?;

    let Some((root, started_at, finished_at, files_count, excluded_count, bytes_logical, bytes_blocks)) =
        scan
    else {
        println!("No scans found. Run `duh scan <path>` first.");
        return Ok(ExitCode::SUCCESS);
    };

    println!("=== duh stats (v2) ===");
    println!();
    println!("  Last scan root:    {root}");
    let scanned_at = ctime(started_at as i64);
    let elapsed = match finished_at {
        Some(f) => format!("  (took {:.1}s)", f - started_at),
        None => String::new(),
    };
    println!("  Scanned at:        {scanned_at}{elapsed}");
    match files_count {
        Some(n) if n != 0 => println!("  Files in scan:     {}", commafy(n)),
        _ => println!("  Files in scan:     (in progress)"),
    }
    println!("  Excluded dirs:     {}", commafy(excluded_count.unwrap_or(0)));
    if let Some(bl) = bytes_logical {
        if bl != 0 {
            println!("  Logical size:      {}", fmt_bytes(bl));
        }
    }
    if let Some(bb) = bytes_blocks {
        if bb != 0 {
            println!("  Block usage:       {}", fmt_bytes(bb));
        }
    }

    println!();
    let (cnt, log, blk): (i64, Option<i64>, Option<i64>) = con.query_row(
        "SELECT COUNT(*) AS cnt, SUM(size_logical) AS log, SUM(size_blocks) AS blk \
         FROM files WHERE is_dir = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    println!("  Total files in DB:     {}", commafy(cnt));
    if let Some(l) = log {
        if l != 0 {
            println!("  Total logical:         {}", fmt_bytes(l));
        }
    }
    if let Some(b) = blk {
        if b != 0 {
            println!("  Total blocks:          {}", fmt_bytes(b));
        }
    }

    let (excnt, exfc, exblk): (i64, Option<i64>, Option<i64>) = con.query_row(
        "SELECT COUNT(*) AS cnt, SUM(excluded_file_count) AS fc, SUM(size_blocks) AS blk \
         FROM files WHERE is_excluded = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if excnt != 0 {
        println!(
            "  Excluded subtrees:     {}  ({} files, {})",
            commafy(excnt),
            commafy(exfc.unwrap_or(0)),
            fmt_bytes(exblk.unwrap_or(0))
        );
    }

    let (cfam, cfiles): (Option<i64>, Option<i64>) = con.query_row(
        "SELECT COUNT(*) AS families, SUM(cnt) AS files FROM ( \
           SELECT clone_id, COUNT(*) AS cnt FROM files WHERE clone_id IS NOT NULL \
           GROUP BY clone_id HAVING cnt > 1 \
         )",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    println!(
        "  Clone families (>1):   {}  ({} files)",
        commafy(cfam.unwrap_or(0)),
        commafy(cfiles.unwrap_or(0))
    );

    let (hlfam, hlfiles): (i64, i64) = con.query_row(
        "SELECT COUNT(DISTINCT dev || ':' || ino) AS families, COUNT(*) AS files \
         FROM files WHERE nlinks > 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    println!(
        "  Hardlink families:     {}  ({} files)",
        commafy(hlfam),
        commafy(hlfiles)
    );

    println!();
    println!("  Top 5 directories by block usage:");
    let top_dirs: Vec<(i64, i64, i64)> = {
        let mut stmt = con.prepare(
            "SELECT parent_id, SUM(size_blocks) AS blk, COUNT(*) AS cnt \
             FROM files WHERE is_dir = 0 AND parent_id IS NOT NULL \
             GROUP BY parent_id ORDER BY blk DESC LIMIT 5",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let home = std::env::var("HOME").unwrap_or_default();
    for (parent_id, blk, cnt) in top_dirs {
        let short = match full_path(con, parent_id) {
            Ok(p) => shorten(&home, &p).into_owned(),
            Err(_) => format!("(id={parent_id})"),
        };
        println!("    {:>12}  {:>6} files  {short}", fmt_bytes(blk), cnt);
    }

    Ok(ExitCode::SUCCESS)
}
