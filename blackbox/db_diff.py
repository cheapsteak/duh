"""Compare two duh databases by PATH (ids differ across implementations).
Clone/family comparison is structural: clone_id VALUES may legitimately match
(they come from the fs), but we compare family *partitions* not raw ids."""
import sqlite3
import sys
from collections import defaultdict

FIELDS = ("is_dir", "is_symlink", "is_excluded", "ino", "nlinks",
          "size_logical", "size_blocks", "excluded_file_count",
          "dev", "mtime")


def load(db):
    con = sqlite3.connect(db)
    con.row_factory = sqlite3.Row
    rows = {r["id"]: r for r in con.execute("SELECT * FROM files")}
    paths, families = {}, defaultdict(set)

    def path_of(nid, memo={}):
        if nid in memo: return memo[nid]
        r = rows[nid]
        p = r["name"] if r["parent_id"] is None else path_of(r["parent_id"]) + "/" + r["name"]
        memo[nid] = p
        return p

    for nid, r in rows.items():
        p = path_of(nid)
        paths[p] = {f: r[f] for f in FIELDS}
        if r["clone_id"] is not None and not r["is_dir"]:
            families[r["clone_id"]].add(p)
    partition = {frozenset(v) for v in families.values() if len(v) > 1}
    return paths, partition


def diff(db_a, db_b):
    pa, fa = load(db_a)
    pb, fb = load(db_b)
    out = []
    out += [f"only in A: {p}" for p in sorted(pa.keys() - pb.keys())]
    out += [f"only in B: {p}" for p in sorted(pb.keys() - pa.keys())]
    for p in sorted(pa.keys() & pb.keys()):
        for f in FIELDS:
            if pa[p][f] != pb[p][f]:
                out.append(f"{p}: {f} {pa[p][f]!r} != {pb[p][f]!r}")
    if fa != fb:
        out.append(f"family partitions differ: {len(fa ^ fb)} families")
    return out


if __name__ == "__main__":
    problems = diff(sys.argv[1], sys.argv[2])
    print("\n".join(problems) or "IDENTICAL")
    sys.exit(1 if problems else 0)
