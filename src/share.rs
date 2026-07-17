//! Share-snapshot encoder: a faithful Rust port of the validated spike
//! `docs/superpowers/spikes/2026-07-15-share-url/encode.py`.
//!
//! Produces a shareable URL fragment (`1.<base64url(deflate(compact JSON))>`)
//! carrying an explorable treemap snapshot of a subtree, sized to fit a byte
//! budget. The algorithm — global priority-queue reveal, per-directory `*`
//! residue rows, unary chain collapse, 3-significant-figure quantization,
//! deflate + base64url, and a binary search over the reveal prefix — mirrors
//! the spike function-for-function. Two structural deviations from the Python
//! (documented inline where they occur) preserve behavioural parity:
//!   * the revealed tree uses an arena of [`RNode`] (indices) instead of `Rc`;
//!   * `q3` rounds with exact integer arithmetic instead of float `round`,
//!     which yields the same round-half-to-even result the spike intends.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::Write;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

/// The three inputs the encoder shares with the freeable engine / serve layer:
/// a read-only DB connection, the per-directory freeable map, and the set of
/// clone_ids whose family has more than one member (files ∪ excluded
/// aggregates) — the same qualification `multi_clone_set` computes.
pub struct ShareInput<'a> {
    pub con: &'a rusqlite::Connection,
    pub freeable_map: &'a HashMap<i64, u64>,
    pub multi_clone: &'a HashSet<i64>,
}

/// A finished snapshot: the fragment (`1.…`, guaranteed `len() <= budget`), the
/// number of encoded nodes (including `"*"` residue rows), and the char count.
pub struct ShareResult {
    pub fragment: String,
    pub nodes: usize,
    pub chars: usize,
}

/// Compute the set of clone_ids whose family (real file members ∪ excluded
/// aggregates) has more than one member — the same set the freeable engine uses
/// to zero out a shared file's own freeable. Ported from the spike's
/// `MULTI_CLONE` query.
pub fn multi_clone_set(
    con: &rusqlite::Connection,
) -> rusqlite::Result<HashSet<i64>> {
    let mut stmt = con.prepare(
        "SELECT clone_id FROM (\
           SELECT clone_id FROM files WHERE clone_id IS NOT NULL AND is_dir=0 \
           UNION ALL SELECT clone_id FROM excluded_families\
         ) GROUP BY clone_id HAVING COUNT(*) > 1",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

/// Build a share snapshot for `node_id` (titled `node_path`, scanned on
/// `scan_date`) whose fragment fits within `budget` characters.
///
/// Returns `Ok(None)` when `node_id` is unknown, is not a directory, or when
/// even the bare-root snapshot cannot fit the budget (pathologically small
/// budget). `Err` is a genuine DB error.
pub fn build_share(
    inp: &ShareInput,
    node_id: i64,
    node_path: &str,
    scan_date: &str,
    budget: usize,
) -> rusqlite::Result<Option<ShareResult>> {
    // Reject unknown nodes and non-directories up front.
    let is_dir: Option<i64> = inp
        .con
        .query_row("SELECT is_dir FROM files WHERE id=?", [node_id], |r| {
            r.get(0)
        })
        .optional()?;
    match is_dir {
        None => return Ok(None),
        Some(0) => return Ok(None),
        _ => {}
    }

    let root_size = inp.freeable_map.get(&node_id).copied().unwrap_or(0);
    let (arena, seq) = build_reveal_sequence(inp, node_id, node_path, root_size)?;

    let tot_q3 = q3(root_size);
    let Some((k, fragment)) = fit_budget(&arena, &seq, budget, node_path, scan_date, tot_q3)
    else {
        return Ok(None);
    };
    let tree = encode_tree(&arena, 0, k);
    let nodes = count_nodes(&tree);
    let chars = fragment.len();
    Ok(Some(ShareResult {
        fragment,
        nodes,
        chars,
    }))
}

/// A revealed node in the arena. `pos_in_seq` is its index in the reveal
/// sequence (the root's is unused); a node is "revealed for prefix `k`" iff
/// `pos_in_seq < k`.
struct RNode {
    id: i64,
    name: String,
    is_dir: bool,
    size: u64,
    kids: Vec<usize>,
    pos_in_seq: usize,
}

/// A child pulled from the DB, awaiting a reveal decision.
struct ChildRow {
    id: i64,
    name: String,
    is_dir: bool,
    size_blocks: i64,
    clone_id: Option<i64>,
    nlinks: i64,
}

/// A queued reveal candidate (a hidden child of an already-revealed dir).
struct Cand {
    parent: usize,
    size: u64,
    row: ChildRow,
}

/// Priority-queue key: pop the largest `size` first, breaking ties by the
/// smallest `tie` (earliest pushed) — matching the spike's `(-size, tie)`
/// min-heap on a max-heap via `Reverse` on the tiebreak.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct HeapItem {
    size: u64,
    tie: Reverse<u64>,
}

fn children_of(
    con: &rusqlite::Connection,
    parent_id: i64,
) -> rusqlite::Result<Vec<ChildRow>> {
    let mut stmt = con.prepare(
        "SELECT id, name, is_dir, size_blocks, clone_id, nlinks FROM files WHERE parent_id=?",
    )?;
    let rows = stmt.query_map([parent_id], |r| {
        Ok(ChildRow {
            id: r.get(0)?,
            name: String::from_utf8_lossy(r.get_ref(1)?.as_bytes()?).into_owned(),
            is_dir: r.get::<_, i64>(2)? != 0,
            size_blocks: r.get(3)?,
            clone_id: r.get(4)?,
            nlinks: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// Size used to rank/reveal a child. Dirs read the freeable map; files use the
/// serve-layer rule — `size_blocks` unless the blocks are shared (nlinks>1, or
/// the clone family has >1 member), in which case 0 (deleting this path alone
/// frees nothing). Port of the spike's `size_of`.
fn size_of(row: &ChildRow, inp: &ShareInput) -> i64 {
    if row.is_dir {
        return inp.freeable_map.get(&row.id).copied().unwrap_or(0) as i64;
    }
    let clone_shared = row
        .clone_id
        .is_some_and(|c| inp.multi_clone.contains(&c));
    if row.nlinks > 1 || clone_shared {
        0
    } else {
        row.size_blocks
    }
}

/// Quantize `n` to 3 significant figures (`< 1000` is exact). Port of the
/// spike's `q3`; uses exact integer arithmetic with round-half-to-even, which
/// reproduces Python `round(n/d)` for all values at this precision without
/// float representation error.
fn q3(n: u64) -> u64 {
    if n < 1000 {
        return n;
    }
    let d = 10u64.pow(n.ilog10() - 2);
    let quotient = n / d;
    let rem = n % d;
    let half = d / 2; // d is a power of ten >= 1000, so d/2 is exact
    let bump = rem > half || (rem == half && quotient % 2 == 1);
    (quotient + u64::from(bump)) * d
}

/// Build the full revealed tree and the global reveal sequence. Returns the
/// arena (index 0 is the root) and `seq`, where `seq[i]` is the arena index of
/// the `i`-th revealed node. Port of the spike's `build_reveal_sequence`.
/// Per-directory reveal fanout cap: only the `REVEAL_FANOUT` largest children
/// of a revealed directory are ever pushed onto the reveal queue; the rest
/// are never revealed and fold into that directory's `*` residue (via
/// `encode_tree`'s existing sum-based residue computation — no extra
/// bookkeeping needed, since a hidden child simply never becomes part of the
/// arena's `kids` sum).
///
/// This spends the fixed URL-fragment budget on *depth* instead of *breadth*:
/// capping fanout lets the reveal sequence dive into far fewer, deeper
/// subtrees rather than exhausting the budget listing many siblings at a
/// shallow level. Measured on a real scan, this took the deep tier's max
/// depth from 17 to 24 (+24% nodes) and the standard tier's from 14 to 17.
/// The cap applies uniformly, including the root's own children — a big scan
/// root will fold its smaller top-level directories into an "other" residue
/// row same as any other directory.
///
/// Tune this constant to change the depth/breadth tradeoff.
const REVEAL_FANOUT: usize = 3;

fn build_reveal_sequence(
    inp: &ShareInput,
    root_id: i64,
    root_name: &str,
    root_size: u64,
) -> rusqlite::Result<(Vec<RNode>, Vec<usize>)> {
    const MAX_REVEALS: usize = 20000;

    let mut arena: Vec<RNode> = vec![RNode {
        id: root_id,
        name: root_name.to_string(),
        is_dir: true,
        size: root_size,
        kids: Vec::new(),
        pos_in_seq: 0,
    }];
    let mut seq: Vec<usize> = Vec::new();
    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
    let mut cands: Vec<Cand> = Vec::new();

    push_children(inp, &mut arena, &mut cands, &mut heap, 0)?;
    while let Some(item) = heap.pop() {
        if seq.len() >= MAX_REVEALS {
            break;
        }
        let idx = item.tie.0 as usize;
        let (parent, size, id, is_dir) = {
            let c = &cands[idx];
            (c.parent, c.size, c.row.id, c.row.is_dir)
        };
        let name = cands[idx].row.name.clone();
        let child_idx = arena.len();
        arena.push(RNode {
            id,
            name,
            is_dir,
            size,
            kids: Vec::new(),
            pos_in_seq: seq.len(),
        });
        arena[parent].kids.push(child_idx);
        seq.push(child_idx);
        if is_dir {
            push_children(inp, &mut arena, &mut cands, &mut heap, child_idx)?;
        }
    }
    Ok((arena, seq))
}

/// Queue the top [`REVEAL_FANOUT`] positive-size children of `node_idx` onto
/// the reveal heap, largest first. Sizes `<= 0` never enter the queue (spike
/// invariant); children beyond the fanout cap are simply never queued, so
/// they can never be revealed and fold into `node_idx`'s `*` residue.
fn push_children(
    inp: &ShareInput,
    arena: &mut [RNode],
    cands: &mut Vec<Cand>,
    heap: &mut BinaryHeap<HeapItem>,
    node_idx: usize,
) -> rusqlite::Result<()> {
    let node_id = arena[node_idx].id;
    let mut eligible: Vec<(u64, ChildRow)> = children_of(inp.con, node_id)?
        .into_iter()
        .filter_map(|row| {
            let sz = size_of(&row, inp);
            (sz > 0).then_some((sz as u64, row))
        })
        .collect();
    // Sort descending by size; a stable sort keeps DB order as the tiebreak
    // among equal sizes, matching the global heap's insertion-order tiebreak.
    eligible.sort_by(|a, b| b.0.cmp(&a.0));
    for (sz, row) in eligible.into_iter().take(REVEAL_FANOUT) {
        let tie = cands.len() as u64;
        cands.push(Cand {
            parent: node_idx,
            size: sz,
            row,
        });
        heap.push(HeapItem {
            size: sz,
            tie: Reverse(tie),
        });
    }
    Ok(())
}

/// Whether `child_idx` is revealed for prefix length `k`.
fn visible(arena: &[RNode], child_idx: usize, k: usize) -> bool {
    arena[child_idx].pos_in_seq < k
}

/// Encode the revealed subtree rooted at `idx` using only reveals in the first
/// `k` of the sequence: collapse unary chains, append a `["*", residue]` row
/// when the hidden residue exceeds 0.5% of the node. Port of `encode_tree`.
///
/// Each collapse step drops up to ~0.5% residue relative to *its own* node,
/// but over a deep unary chain those drops compound: dropping 0.5% six times
/// in a row sheds ~3% of the chain-root's size, comfortably past the 2% slop
/// the viewer allows when checking a node's size against the sum of its
/// children — a valid link would then get mis-flagged as "damaged". So this
/// tracks the running total dropped across the whole chain and stops
/// collapsing once continuing would exceed ~1% of the chain-root size (the
/// node at `idx`, before any collapsing) — a safety margin under the 2% slop.
fn encode_tree(arena: &[RNode], idx: usize, k: usize) -> Value {
    let mut name = arena[idx].name.clone();
    let mut cur = idx;
    let chain_root_size = arena[idx].size as f64;
    let mut dropped: u64 = 0;

    // Unary chain collapse: a single revealed child with <=0.5% residue folds
    // into its parent with a `/`-joined name, as long as the chain's
    // cumulative dropped residue stays under the ~1% cap above.
    loop {
        let vis: Vec<usize> = arena[cur]
            .kids
            .iter()
            .copied()
            .filter(|&c| visible(arena, c, k))
            .collect();
        let sum: u64 = vis.iter().map(|&c| arena[c].size).sum();
        let residue = arena[cur].size.saturating_sub(sum);
        let would_drop = dropped.saturating_add(residue);
        if arena[cur].is_dir
            && vis.len() == 1
            && (residue as f64) <= arena[cur].size as f64 * 0.005
            && (would_drop as f64) <= chain_root_size * 0.01
        {
            dropped = would_drop;
            name.push('/');
            name.push_str(&arena[vis[0]].name);
            cur = vis[0];
        } else {
            break;
        }
    }

    let mut out = vec![json!(name), json!(q3(arena[cur].size))];
    let vis: Vec<usize> = arena[cur]
        .kids
        .iter()
        .copied()
        .filter(|&c| visible(arena, c, k))
        .collect();
    if !vis.is_empty() {
        let mut enc_kids: Vec<Value> =
            vis.iter().map(|&c| encode_tree(arena, c, k)).collect();
        let sum: u64 = vis.iter().map(|&c| arena[c].size).sum();
        let hidden = arena[cur].size.saturating_sub(sum);
        if (hidden as f64) > arena[cur].size as f64 * 0.005 {
            enc_kids.push(json!(["*", q3(hidden)]));
        }
        out.push(Value::Array(enc_kids));
    }
    Value::Array(out)
}

/// Encode the reveal prefix of length `k` into a full fragment string. Encoding
/// is cheap (sub-ms), so the budget search calls this directly rather than
/// estimating. Port of the spike's `encode_prefix`.
fn encode_prefix(
    arena: &[RNode],
    k: usize,
    node_path: &str,
    scan_date: &str,
    tot_q3: u64,
) -> String {
    let tree = encode_tree(arena, 0, k);
    let doc = json!({
        "v": 1,
        "t": node_path,
        "d": scan_date,
        "tot": tot_q3,
        "n": tree,
    });
    let raw = serde_json::to_vec(&doc).expect("serialize share doc");
    let comp = deflate(&raw);
    let b64 = URL_SAFE_NO_PAD.encode(&comp);
    format!("1.{b64}")
}

/// zlib deflate at level 9 (browser `DecompressionStream('deflate')`-compatible).
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(9));
    enc.write_all(data).expect("deflate write");
    enc.finish().expect("deflate finish")
}

/// Binary-search the longest reveal prefix whose fragment fits `budget`. Returns
/// `(k, fragment)` or `None` if even the empty prefix overflows. Port of
/// `fit_budget`.
fn fit_budget(
    arena: &[RNode],
    seq: &[usize],
    budget: usize,
    node_path: &str,
    scan_date: &str,
    tot_q3: u64,
) -> Option<(usize, String)> {
    let (mut lo, mut hi) = (0i64, seq.len() as i64);
    let mut best: Option<(usize, String)> = None;
    while lo <= hi {
        let mid = ((lo + hi) / 2) as usize;
        let frag = encode_prefix(arena, mid, node_path, scan_date, tot_q3);
        if frag.len() <= budget {
            best = Some((mid, frag));
            lo = mid as i64 + 1;
        } else {
            hi = mid as i64 - 1;
        }
    }
    best
}

/// Count encoded nodes (each JSON array node, including `"*"` residue rows).
fn count_nodes(v: &Value) -> usize {
    let kids = match v.get(2) {
        Some(Value::Array(kids)) => kids.iter().map(count_nodes).sum(),
        _ => 0,
    };
    1 + kids
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use rusqlite::{params, Connection};
    use std::io::Read;

    const MB: i64 = 1024 * 1024;

    fn new_db() -> Connection {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch(crate::db::SCHEMA).unwrap();
        con.execute(
            "INSERT INTO scans (id, root, started_at, schema_version) VALUES (1,'/',0,2)",
            [],
        )
        .unwrap();
        con
    }

    #[allow(clippy::too_many_arguments)]
    fn ins(
        con: &Connection,
        id: i64,
        parent: Option<i64>,
        name: &str,
        is_dir: i64,
        size_blocks: i64,
        clone_id: Option<i64>,
        nlinks: i64,
    ) {
        con.execute(
            "INSERT INTO files \
             (id, parent_id, name, is_dir, is_symlink, is_excluded, dev, ino, \
              clone_id, nlinks, size_logical, size_blocks, mtime, scan_id) \
             VALUES (?,?,?,?,0,0,1,?,?,?,?,?,0,1)",
            params![id, parent, name, is_dir, id, clone_id, nlinks, size_blocks, size_blocks],
        )
        .unwrap();
    }

    /// The brief's fixture: cross-dir clone family (Postgres farm) in dirA/dirB,
    /// intra-dir family in dirC, a unique file in dirD, hand-built freeable map.
    fn fixture() -> (Connection, HashMap<i64, u64>, HashSet<i64>) {
        let con = new_db();
        ins(&con, 1, None, "root", 1, 0, None, 1);
        ins(&con, 2, Some(1), "dirA", 1, 0, None, 1);
        ins(&con, 3, Some(1), "dirB", 1, 0, None, 1);
        ins(&con, 4, Some(1), "dirC", 1, 0, None, 1);
        ins(&con, 5, Some(1), "dirD", 1, 0, None, 1);
        ins(&con, 10, Some(2), "fileA1", 0, 100 * MB, Some(77), 1);
        ins(&con, 11, Some(3), "fileB1", 0, 100 * MB, Some(77), 1);
        ins(&con, 12, Some(4), "fileC1", 0, 50 * MB, Some(88), 1);
        ins(&con, 13, Some(4), "fileC2", 0, 50 * MB, Some(88), 1);
        ins(&con, 14, Some(5), "fileD1", 0, 30 * MB, None, 1);

        let mut fm = HashMap::new();
        fm.insert(1, (180 * MB) as u64);
        fm.insert(2, 0u64);
        fm.insert(3, 0u64);
        fm.insert(4, (50 * MB) as u64);
        fm.insert(5, (30 * MB) as u64);

        let mc: HashSet<i64> = [77, 88].into_iter().collect();
        (con, fm, mc)
    }

    fn decode(frag: &str) -> Value {
        let b64 = frag.strip_prefix("1.").expect("version prefix");
        let comp = URL_SAFE_NO_PAD.decode(b64).unwrap();
        let mut d = ZlibDecoder::new(&comp[..]);
        let mut s = String::new();
        d.read_to_string(&mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    fn node_size(v: &Value) -> u64 {
        v[1].as_u64().unwrap()
    }

    fn node_kids(v: &Value) -> &[Value] {
        match v.get(2) {
            Some(Value::Array(k)) => k,
            _ => &[],
        }
    }

    fn find_named<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
        if v[0].as_str() == Some(name) {
            return Some(v);
        }
        for k in node_kids(v) {
            if let Some(found) = find_named(k, name) {
                return Some(found);
            }
        }
        None
    }

    fn collect_names(v: &Value, out: &mut HashSet<String>) {
        out.insert(v[0].as_str().unwrap().to_string());
        for k in node_kids(v) {
            collect_names(k, out);
        }
    }

    #[test]
    fn shared_files_are_zero_and_fold_into_residue() {
        let (con, fm, mc) = fixture();
        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        let res = build_share(&inp, 1, "root", "2026-07-15", 100_000)
            .unwrap()
            .unwrap();
        let doc = decode(&res.fragment);
        let n = &doc["n"];

        // Shared files (clone twins) are size 0, so they never reveal.
        let mut names = HashSet::new();
        collect_names(n, &mut names);
        for gone in ["fileA1", "fileB1", "fileC1", "fileC2"] {
            assert!(!names.contains(gone), "{gone} should never reveal (size 0)");
        }

        // dirA / dirB are themselves 0-freeable → absent; if present, childless.
        for d in ["dirA", "dirB"] {
            if let Some(node) = find_named(n, d) {
                assert!(node_kids(node).is_empty(), "{d} must have no children");
            }
        }

        // dirC reveals as a leaf (its files are shared → 0); no `*` row.
        let dir_c = find_named(n, "dirC").expect("dirC revealed");
        assert!(node_kids(dir_c).is_empty(), "dirC has no revealed children");
        assert_eq!(node_size(dir_c), q3((50 * MB) as u64));

        // dirD's unique 30 MB file reveals; the unary dir/file chain collapses
        // into one node "dirD/fileD1" (single child, zero residue).
        let file_d1 = find_named(n, "dirD/fileD1").expect("dirD/fileD1 revealed");
        assert_eq!(node_size(file_d1), q3((30 * MB) as u64));
        assert!(node_kids(file_d1).is_empty());

        // The cross-dir family's 100 MB surfaces as root residue, not doubled.
        let star = node_kids(n)
            .iter()
            .find(|k| k[0].as_str() == Some("*"))
            .expect("root has a `*` residue row");
        assert_eq!(node_size(star), q3((100 * MB) as u64));
    }

    #[test]
    fn sum_invariant_holds_everywhere() {
        let (con, fm, mc) = fixture();
        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        let res = build_share(&inp, 1, "root", "2026-07-15", 100_000)
            .unwrap()
            .unwrap();
        let doc = decode(&res.fragment);

        fn check(v: &Value) {
            let kids = node_kids(v);
            if !kids.is_empty() {
                let size = node_size(v) as f64;
                let sum: f64 = kids.iter().map(|k| node_size(k) as f64).sum();
                let slop = (0.02 * size).max(2048.0);
                assert!(
                    (sum - size).abs() <= slop,
                    "node {:?}: |{sum} - {size}| > {slop}",
                    v[0]
                );
            }
            for k in kids {
                check(k);
            }
        }
        check(&doc["n"]);
    }

    #[test]
    fn budget_monotone_and_respected() {
        let (con, fm, mc) = fixture();
        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        let mut prev: Option<HashSet<String>> = None;
        for budget in [300usize, 1000, 5000] {
            let res = build_share(&inp, 1, "root", "2026-07-15", budget)
                .unwrap()
                .unwrap();
            assert!(res.chars <= budget, "fragment {} > budget {budget}", res.chars);
            assert_eq!(res.chars, res.fragment.len());
            let doc = decode(&res.fragment);
            let mut names = HashSet::new();
            collect_names(&doc["n"], &mut names);
            if let Some(smaller) = &prev {
                assert!(
                    smaller.is_subset(&names),
                    "names at smaller budget must be a subset of larger"
                );
            }
            prev = Some(names);
        }
    }

    #[test]
    fn quantize_3_sig_figs() {
        assert_eq!(q3(141_901_824), 142_000_000);
        assert_eq!(q3(999), 999);
        assert_eq!(q3(1000), 1000);
        assert_eq!(q3(123_456), 123_000);
        // Tie boundary pins Python-round() parity (banker's rounding at .5):
        // python3 -c "print(round(100.5), round(101.5))" -> 100 102
        assert_eq!(q3(10_050), 10_000); // 100.5 -> 100 (even quotient stays)
        assert_eq!(q3(10_150), 10_200); // 101.5 -> 102 (odd quotient bumps)
    }

    #[test]
    fn chain_collapse_joins_unary_paths() {
        // a -> b -> c unary chain; c holds only a shared file (size 0), so the
        // chain terminates at c with no residue row.
        let con = new_db();
        ins(&con, 1, None, "a", 1, 0, None, 1);
        ins(&con, 2, Some(1), "b", 1, 0, None, 1);
        ins(&con, 3, Some(2), "c", 1, 0, None, 1);
        ins(&con, 4, Some(3), "d1", 0, MB, Some(99), 1); // shared → 0

        let mut fm = HashMap::new();
        fm.insert(1, MB as u64);
        fm.insert(2, MB as u64);
        fm.insert(3, MB as u64);
        let mc: HashSet<i64> = [99].into_iter().collect();

        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        let res = build_share(&inp, 1, "a", "2026-07-15", 10_000)
            .unwrap()
            .unwrap();
        let doc = decode(&res.fragment);
        let n = &doc["n"];
        assert_eq!(n[0].as_str(), Some("a/b/c"));
        assert_eq!(node_size(n), q3(MB as u64));
        assert!(node_kids(n).is_empty(), "collapsed chain leaf has no children");
    }

    #[test]
    fn chain_collapse_residue_capped_across_chain() {
        // A 7-level unary chain of dirs, each ~99.5% of its parent's size, so
        // every single hop sits right at the existing per-hop 0.5% residue
        // threshold and would collapse on its own. Naively collapsing the
        // whole chain would compound to several percent of dropped residue —
        // comfortably past the 2% slop the viewer allows (`max(2%, 2048)`,
        // mirrored by `sum_invariant_holds_everywhere` below). The residue
        // cap must stop the collapse once the running total would exceed
        // ~1% of the chain-root size, so no encoded node's size-vs-children
        // gap ever approaches the viewer's threshold.
        const LEVELS: i64 = 7;
        let con = new_db();
        let mut sizes = vec![10_000_000u64];
        for _ in 1..LEVELS {
            let prev = *sizes.last().unwrap();
            sizes.push(prev - prev / 200); // drop ~0.5% each level
        }

        ins(&con, 1, None, "d0", 1, 0, None, 1);
        for lvl in 1..LEVELS {
            ins(&con, lvl + 1, Some(lvl), &format!("d{lvl}"), 1, 0, None, 1);
        }
        // Terminal unique file under the last dir, sized to match it exactly
        // (no extra residue at the final hop).
        let leaf_size = *sizes.last().unwrap() as i64;
        ins(&con, LEVELS + 1, Some(LEVELS), "leaf", 0, leaf_size, None, 1);

        let mut fm = HashMap::new();
        for (i, &s) in sizes.iter().enumerate() {
            fm.insert(i as i64 + 1, s);
        }
        let mc: HashSet<i64> = HashSet::new();

        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        let res = build_share(&inp, 1, "d0", "2026-07-15", 100_000)
            .unwrap()
            .unwrap();
        let doc = decode(&res.fragment);

        // Same sum-invariant every other test in this module holds encoded
        // trees to: at every node, |children-sum - node-size| <= max(2%, 2048).
        fn check(v: &Value) {
            let kids = node_kids(v);
            if !kids.is_empty() {
                let size = node_size(v) as f64;
                let sum: f64 = kids.iter().map(|k| node_size(k) as f64).sum();
                let slop = (0.02 * size).max(2048.0);
                assert!(
                    (sum - size).abs() <= slop,
                    "node {:?}: |{sum} - {size}| > {slop}",
                    v[0]
                );
            }
            for k in kids {
                check(k);
            }
        }
        check(&doc["n"]);

        // Sanity: the residue cap actually bit — the chain did not fully
        // collapse into a single "d0/d1/.../d6/leaf" node. A full collapse
        // would join all LEVELS+1 segments with LEVELS slashes.
        let name = doc["n"][0].as_str().unwrap();
        assert!(
            name.matches('/').count() < LEVELS as usize,
            "chain fully collapsed despite residue cap: {name}"
        );
    }

    #[test]
    fn unknown_node_returns_none() {
        let (con, fm, mc) = fixture();
        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        assert!(build_share(&inp, 999, "?", "2026-07-15", 100_000)
            .unwrap()
            .is_none());
    }

    #[test]
    fn fanout_capped_to_top_three() {
        // Root has 5 unique, unshared files of distinct decreasing sizes.
        // Even with a budget large enough to reveal everything, only the top
        // REVEAL_FANOUT=3 may ever enter the reveal queue; the rest fold into
        // the root's `*` residue.
        let con = new_db();
        ins(&con, 1, None, "root", 1, 0, None, 1);
        let sizes: [i64; 5] = [100 * MB, 90 * MB, 80 * MB, 70 * MB, 60 * MB];
        for (i, &sz) in sizes.iter().enumerate() {
            ins(&con, 10 + i as i64, Some(1), &format!("f{i}"), 0, sz, None, 1);
        }
        let mut fm = HashMap::new();
        fm.insert(1, sizes.iter().sum::<i64>() as u64);
        let mc: HashSet<i64> = HashSet::new();

        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        let res = build_share(&inp, 1, "root", "2026-07-15", 1_000_000)
            .unwrap()
            .unwrap();
        let doc = decode(&res.fragment);
        let n = &doc["n"];
        let kids = node_kids(n);

        let non_star: Vec<&Value> = kids.iter().filter(|k| k[0].as_str() != Some("*")).collect();
        assert_eq!(non_star.len(), 3, "expected exactly top-3 fanout: {kids:?}");

        let mut revealed: Vec<u64> = non_star.iter().map(|k| node_size(k)).collect();
        revealed.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            revealed,
            vec![q3((100 * MB) as u64), q3((90 * MB) as u64), q3((80 * MB) as u64)],
            "the three revealed children must be the three largest"
        );

        let star = kids
            .iter()
            .find(|k| k[0].as_str() == Some("*"))
            .expect("hidden children (beyond fanout cap) must fold into `*` residue");
        let expected_hidden = q3((70 * MB + 60 * MB) as u64);
        let got = node_size(star) as f64;
        let slop = (0.02 * expected_hidden as f64).max(2048.0);
        assert!(
            (got - expected_hidden as f64).abs() <= slop,
            "* residue {got} should equal hidden sum {expected_hidden} within slop {slop}"
        );
    }

    #[test]
    fn fanout_shows_all_children_when_at_or_below_cap() {
        // Exactly REVEAL_FANOUT=3 children, sized to sum exactly to the
        // parent: all three must reveal and there must be no spurious `*`
        // row (residue is 0, well under the 0.5% threshold).
        let con = new_db();
        ins(&con, 1, None, "root", 1, 0, None, 1);
        let sizes: [i64; 3] = [50 * MB, 30 * MB, 20 * MB];
        for (i, &sz) in sizes.iter().enumerate() {
            ins(&con, 10 + i as i64, Some(1), &format!("f{i}"), 0, sz, None, 1);
        }
        let mut fm = HashMap::new();
        fm.insert(1, sizes.iter().sum::<i64>() as u64);
        let mc: HashSet<i64> = HashSet::new();

        let inp = ShareInput {
            con: &con,
            freeable_map: &fm,
            multi_clone: &mc,
        };
        let res = build_share(&inp, 1, "root", "2026-07-15", 1_000_000)
            .unwrap()
            .unwrap();
        let doc = decode(&res.fragment);
        let n = &doc["n"];
        let kids = node_kids(n);

        assert_eq!(kids.len(), 3, "all three children should reveal: {kids:?}");
        assert!(
            kids.iter().all(|k| k[0].as_str() != Some("*")),
            "no spurious `*` residue row expected: {kids:?}"
        );
    }

    #[test]
    fn multi_clone_set_qualifies_families() {
        let (con, _fm, _mc) = fixture();
        let mc = multi_clone_set(&con).unwrap();
        // 77 (cross-dir) and 88 (intra-dir) each have 2 members.
        assert!(mc.contains(&77));
        assert!(mc.contains(&88));
        assert_eq!(mc.len(), 2);
    }
}
