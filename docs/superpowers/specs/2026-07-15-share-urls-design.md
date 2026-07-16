# duh Share URLs — Design

Shareable, explorable treemap snapshots carried entirely in a URL fragment,
viewed on a static GitHub Pages page. No backend, no storage, links only.

## Decisions (ratified in brainstorming, 2026-07-15)

| Question | Decision |
|---|---|
| What is shared | Explorable treemap snapshot (not just a stats card) |
| Privacy model | Sharer's responsibility; one-time consent dialog; scope = the subtree currently viewed |
| Data storage | URL fragment only — never reaches any server, including ours |
| Community | Links only; no gallery |
| Trigger | Share button in the local `duh serve` UI, backed by `GET /api/share/{id}` (CLI command possible later on the same endpoint) |
| Metrics per node | Freeable only (deepest tree per byte; header carries totals for the story) |
| Viewer | Static page in `docs/v/` (GitHub Pages), reusing the local UI's treemap rendering |

## Validated by spike (`docs/superpowers/spikes/2026-07-15-share-url/`)

Run against a real 4.5M-file scan; decoded in Node 18+ and real Chrome via
native `DecompressionStream('deflate')`:

| Tier | Budget (chars) | Fits | Result on real data |
|---|---|---|---|
| compact | 1,900 | Discord messages (2,000 hard cap), anywhere | ~125 nodes, max depth 10 |
| standard (default) | 8,000 | Slack, iMessage, Reddit, HN | ~660 nodes, max depth 14 |
| deep | 32,000 | browser-to-browser | ~2,820 nodes, max depth 17 |

Key spike findings, now requirements:

1. **Budget-driven greedy reveal beats fixed depth ~3x** (compact tier reaches
   depth 10 where uniform depth-3 fits the same budget).
2. **Codec chain is fully standard**: zlib deflate ↔ browser-native
   `DecompressionStream('deflate')`. Zero JS dependencies in the viewer.
3. **File sizes MUST use the serve-layer freeable rule** (`size_blocks` unless
   the file's blocks are shared via clone twin — in files or excluded
   aggregates — or nlinks>1, then 0). The spike's first version mixed
   clone-aware directory sizes with raw file sizes and the viewer's sum
   invariant caught children summing to 2.6 GB inside a 1.4 GB directory
   (Postgres FILE_COPY clone farms). This exact scenario is a required test.
4. **3-significant-figure size quantization saves ~14%** compressed, invisible
   at display precision (1-decimal IEC).
5. Exact budget fitting works: binary-search the reveal-sequence prefix
   against the true encoded length (encode+deflate is sub-ms); lands within
   a few chars of budget.

## Snapshot format (fragment)

```
https://cheapsteak.github.io/duh/v/#1.<base64url(deflate(json))>
```

- Version prefix `1.` — future codecs (brotli-wasm, preset dictionary) bump it;
  old links keep decoding forever.
- JSON document: `{"v":1, "t":<root path or user title>, "d":"YYYY-MM-DD",
  "tot":<root freeable>, "n":<node>}`
- Node: `[name, size]` or `[name, size, [child...]]`. Sizes are freeable
  bytes quantized to 3 significant figures.
- Per-directory residue (unrevealed children + shared-file blocks) appears as
  a `["*", size]` child when > 0.5% of the parent; the viewer renders it as
  "… other".
- Unary chains with < 0.5% residue collapse into one node with `/`-joined
  names.
- base64url without padding.

## Encoder (Rust, `src/share.rs`)

`GET /api/share/{node_id}?budget=1900|8000|32000` (default 8000) returns
`{"fragment": "1.…", "url": "https://…#1.…", "nodes": N, "chars": M}`.

Algorithm:
1. Build the reveal sequence: priority queue of hidden children of revealed
   dirs, keyed by size (dirs: freeable_map; files: serve-layer file rule),
   popping the globally largest until exhausted or a hard cap (~20k reveals).
   Sizes ≤ 0 never enter the queue.
2. Binary-search the longest sequence prefix whose encoded fragment fits the
   budget. Encoding a prefix: revealed tree → chain collapse → `*` residue
   rows → quantize → compact JSON → deflate (level 9) → base64url.
3. Uses the in-memory `dir_agg`/`freeable_map` plus per-thread read-only DB
   connection for children (same data the node API uses). Target: < 1s for
   the deep tier on a 5M-file scan.

Invariant (tested): for every encoded directory, `Σ(children incl. *) - size`
is within quantization slop (max(2%, 2 KiB)).

## Share UI (local serve page)

- Ghost "Share" button in the header (next to the mode toggle).
- First use per browser (localStorage): consent dialog in plain words —
  "Everything below <path> — real directory and file names — is encoded into
  the link. Anyone with the link sees it. Nothing is uploaded; the data IS
  the link."
- Tier picker (compact / standard / deep) with the "fits where" hints above;
  standard preselected. Copies the URL, flashes ✓ (same pattern as copy-path).
- Shares the currently-viewed node's subtree: navigation is the scope control.

## Viewer (`docs/v/index.html`, GitHub Pages)

- Reuses the treemap rendering + palette by extracting it from `static/app.js`
  into a shared `static/treemap.js` consumed by both pages. A small script,
  `scripts/sync-viewer.sh`, copies `treemap.js` + `vendor/echarts.min.js`
  into `docs/v/` and is CI-checked (build fails if `docs/v/` is stale), so
  Pages stays a plain static checkout.
- View-only: no table pane, no API. Click-to-zoom within the snapshot,
  breadcrumb, "… other" tiles inert. Header: title, scan date, total
  freeable. Footer: "measured by duh" → repo.
- Decode: fragment version check → base64url → `DecompressionStream('deflate')`
  → JSON → sum-invariant check (belt and braces; a corrupt link fails here).
- Errors: no fragment → mini landing explaining what duh is; unknown version →
  "made with a newer duh"; decode/invariant failure → "link damaged".
- Vendored ECharts serves the treemap as on the local page (~1 MB, cached).

## Error handling

- `/api/share`: 404 unknown node, 400 bad budget (reuse existing error JSON).
- Fragment > budget never emitted; if even the bare root exceeds budget
  (pathological), 400 with a clear message.
- Viewer failures render the status banner, never a blank page.

## Testing

- Rust unit: reveal-sequence monotonicity (more budget ⇒ superset of nodes);
  residue accounting invariant on synthetic trees including intra-dir and
  cross-dir clone families (the Postgres-farm regression); quantization
  round-trip; chain collapse.
- Blackbox: `/api/share` on the fixture tree — fragment decodes (Node 18+
  `DecompressionStream`), root total matches the fixture's known freeable,
  budget respected for all three tiers.
- Viewer: decode+invariant logic exercised in Node; manual browser pass per
  release (the spike's PASS/FAIL banner pattern is kept in the viewer as a
  hidden `?selftest` mode).

## Out of scope (recorded levers, not v1)

- brotli-wasm codec (measured on real payloads: 13-17% smaller than deflate;
  version-prefix ready for it; costs a ~200KB wasm decoder in the viewer).
- lz-string: evaluated 2026-07-16 and REJECTED — measured 31-35% WORSE than
  deflate+base64url at every tier (LZ78-family, no entropy coding; its
  URI-safe output is 6 bits/char, the same density as base64, so no
  encoding-overhead win either), and it would add a viewer JS dep + a Rust
  port where deflate is native on both ends.
- Preset dictionary of common macOS path vocabulary (needs pako; helps small
  snapshots most).
- Binary node encoding (~10-20% pre-compression; hurts debuggability).
- `duh share PATH` CLI (trivial once `/api/share` exists; add on demand).
- Gallery/community index; multi-metric snapshots.
