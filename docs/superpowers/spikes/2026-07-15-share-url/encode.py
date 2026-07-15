#!/usr/bin/env python3
"""Spike: budget-driven greedy-reveal snapshot encoder for duh share URLs.

Validates the algorithm + codec end to end against the real scan DB:
  global priority-queue reveal -> per-dir *other rows -> unary chain collapse
  -> 3-sig-fig quantization -> compact JSON -> deflate -> base64url fragment.
Binary-searches the reveal-sequence prefix so the final URL fits the budget.
"""
import base64
import heapq
import json
import math
import sqlite3

DB = "/Users/chang/.local/share/duh/scan.db"
VIEWER = "http://127.0.0.1:8899/viewer.html"
TIERS = {"compact": 1900, "standard": 8000, "deep": 32000}

con = sqlite3.connect(DB)
con.row_factory = sqlite3.Row
FREEABLE = {r[0]: r[1] for r in con.execute("SELECT node_id, freeable FROM freeable_cache")}

# Multi-member clone families (files ∪ excluded aggregates): a file whose
# clone_id is in this set — or with nlinks>1 — has its blocks held elsewhere
# too, so its own freeable is 0 (same rule as the serve layer's file rows).
MULTI_CLONE = {
    r[0]
    for r in con.execute(
        "SELECT clone_id FROM ("
        "  SELECT clone_id FROM files WHERE clone_id IS NOT NULL AND is_dir=0"
        "  UNION ALL SELECT clone_id FROM excluded_families"
        ") GROUP BY clone_id HAVING COUNT(*) > 1"
    )
}


def children_of(pid):
    return con.execute(
        "SELECT id, name, is_dir, size_blocks, clone_id, nlinks FROM files WHERE parent_id=?",
        (pid,),
    ).fetchall()


def size_of(row):
    if row["is_dir"]:
        return FREEABLE.get(row["id"], 0)
    if row["nlinks"] > 1 or (row["clone_id"] is not None and row["clone_id"] in MULTI_CLONE):
        return 0  # shared blocks: deleting this path alone frees nothing
    return row["size_blocks"]


def q3(n):
    """Quantize to 3 significant figures (display is 1-decimal IEC anyway)."""
    if n < 1000:
        return n
    d = 10 ** (int(math.log10(n)) - 2)
    return round(n / d) * d


class Node:
    __slots__ = ("id", "name", "is_dir", "size", "kids", "revealed", "hidden_sum", "hidden_cnt")

    def __init__(self, id, name, is_dir, size):
        self.id, self.name, self.is_dir, self.size = id, name, is_dir, size
        self.kids = []          # revealed children, in reveal order
        self.revealed = False
        self.hidden_sum = 0     # accumulated *other
        self.hidden_cnt = 0


def build_reveal_sequence(root_id, root_name, max_reveals=20000):
    """Return (root_node, seq) where seq[i] applies the i-th reveal."""
    root = Node(root_id, root_name, True, FREEABLE.get(root_id, 0))
    root.revealed = True
    seq = []  # (parent_node, child_node) in reveal order
    pq = []   # (-size, tiebreak, parent_node, row)
    tie = 0

    def push_children(node):
        nonlocal tie
        for r in children_of(node.id):
            sz = size_of(r)
            if sz <= 0:
                continue
            tie += 1
            heapq.heappush(pq, (-sz, tie, node, r))

    push_children(root)
    while pq and len(seq) < max_reveals:
        negsz, _, parent, row = heapq.heappop(pq)
        child = Node(row["id"], row["name"], bool(row["is_dir"]), -negsz)
        child.revealed = True
        parent.kids.append(child)
        seq.append((parent, child))
        if child.is_dir:
            push_children(child)
    return root, seq


def encode_tree(node, cutoff_set):
    """Encode revealed subtree using only children in cutoff_set; collapse unary chains."""
    name = node.name
    # unary chain collapse: single revealed child, no hidden residue
    cur = node
    while True:
        vis = [k for k in cur.kids if id(k) in cutoff_set]
        residue = cur.size - sum(k.size for k in vis)
        if cur.is_dir and len(vis) == 1 and residue <= cur.size * 0.005:
            name = name + "/" + vis[0].name
            cur = vis[0]
        else:
            break
    out = [name, q3(cur.size)]
    vis = [k for k in cur.kids if id(k) in cutoff_set]
    if vis:
        enc_kids = [encode_tree(k, cutoff_set) for k in vis]
        hidden = cur.size - sum(k.size for k in vis)
        if hidden > cur.size * 0.005:
            enc_kids.append(["*", q3(hidden)])
        out.append(enc_kids)
    return out


def encode_prefix(root, seq, k, meta):
    cutoff = {id(child) for _, child in seq[:k]}
    tree = encode_tree(root, cutoff)
    doc = {"v": 1, **meta, "n": tree}
    raw = json.dumps(doc, separators=(",", ":")).encode()
    comp = zlib_compress(raw)
    return "1." + base64.urlsafe_b64encode(comp).decode().rstrip("=")


def zlib_compress(b):
    import zlib
    return zlib.compress(b, 9)


def fit_budget(root, seq, budget, meta):
    lo, hi, best = 0, len(seq), None
    while lo <= hi:
        mid = (lo + hi) // 2
        frag = encode_prefix(root, seq, mid, meta)
        if len(frag) <= budget:
            best = (mid, frag)
            lo = mid + 1
        else:
            hi = mid - 1
    return best


def depth_stats(root, cutoff_set, d=0):
    vis = [k for k in root.kids if id(k) in cutoff_set]
    if not vis:
        return d, 1
    mx, cnt = d, 1
    for k in vis:
        md, mc = depth_stats(k, cutoff_set, d + 1)
        mx = max(mx, md)
        cnt += mc
    return mx, cnt


root_row = con.execute("SELECT id, name FROM files WHERE parent_id IS NULL").fetchone()
meta = {"t": root_row["name"], "d": "2026-07-15", "tot": q3(FREEABLE.get(root_row["id"], 0))}
root, seq = build_reveal_sequence(root_row["id"], root_row["name"])
print(f"reveal sequence: {len(seq)} candidates materialized")

for tier, budget in TIERS.items():
    fitted = fit_budget(root, seq, budget, meta)
    assert fitted is not None, f"{tier}: even an empty snapshot exceeds {budget} chars"
    k, frag = fitted
    cutoff = {id(c) for _, c in seq[:k]}
    mx, cnt = depth_stats(root, cutoff)
    url = f"{VIEWER}#{frag}"
    open(f"/tmp/duh-share-{tier}.url", "w").write(url)
    print(f"{tier:9} budget={budget:6}: fragment={len(frag):6} chars  reveals={k:5}  "
          f"encoded_nodes={cnt:5}  max_depth={mx}")
print("\nURLs written to /tmp/duh-share-{compact,standard,deep}.url")
