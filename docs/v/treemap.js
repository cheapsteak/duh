// Shared treemap rendering — consumed by the local UI (`app.js`) and the
// static share viewer (Task 4). Pure functions over an `items` array; no DOM
// or app-state dependencies.
//
// items: [{name, value, id?, excluded?}] — need not be pre-sorted; buildOption
// sorts desc by value, assigns palette+ink by sorted index, and hides labels
// for tiles under 1% of the total.

// Categorical palette validated for the dark surface (#1a1a1a): lightness band,
// chroma floor, adjacent-pair CVD separation, >=3:1 vs surface all pass.
// INK[i] is the WCAG-preferred text color on PALETTE[i] (all pairs >=4.8:1) —
// computed, not eyeballed. Excluded dirs use the amber EXCL pair.
const PALETTE = ['#3987e5', '#199e70', '#c98500', '#008300', '#9085e9', '#e66767', '#d55181', '#d95926'];
const INK     = ['#111111', '#111111', '#111111', '#ffffff', '#111111', '#111111', '#111111', '#111111'];
const EXCL_FILL = '#c8a000', EXCL_INK = '#111111';

function fmtBytes(n) {
  if (n === null || n === undefined) return '—';
  if (n < 0) return '-' + fmtBytes(-n);
  const GiB = 1 << 30, MiB = 1 << 20, KiB = 1 << 10;
  if (n >= GiB) return (n / GiB).toFixed(1) + ' GiB';
  if (n >= MiB) return (n / MiB).toFixed(1) + ' MiB';
  if (n >= KiB) return (n / KiB).toFixed(1) + ' KiB';
  return n + ' B';
}

function buildOption(items) {
  // Sort by value so palette index matches layout adjacency (treemap lays out desc).
  const sorted = items
    .map(it => ({ it, value: it.value || 0 }))
    .filter(d => d.value > 0)
    .sort((a, b) => b.value - a.value);
  const total = sorted.reduce((s, d) => s + d.value, 0);

  const data = sorted.map((d, i) => {
    const fill = d.it.excluded ? EXCL_FILL : PALETTE[i % PALETTE.length];
    const ink = d.it.excluded ? EXCL_INK : INK[i % INK.length];
    return {
      name: d.it.name,
      value: d.value,
      id: d.it.id,
      itemStyle: { color: fill, opacity: d.it.excluded ? 0.9 : 1 },
      label: { color: ink },
      emphasis: { label: { color: ink, fontWeight: 'bold' } },
    };
  });

  return {
    backgroundColor: '#1a1a1a',
    series: [{
      type: 'treemap',
      roam: false,
      nodeClick: false,
      breadcrumb: { show: false },
      width: '100%',
      height: '100%',
      label: {
        show: true,
        // Slivers (<1% of view) render only truncated garbage — show nothing.
        // No ECharts tooltip picks up the slack: an HTML tooltip would echo
        // the fragment's file/dir names into the DOM unescaped, an XSS sink
        // for a name crafted by whoever built the shared link. Deliberately
        // not enabled.
        formatter: (p) => (p.data.value / total < 0.01)
          ? ''
          : p.data.name + '\n' + fmtBytes(p.data.value),
        fontSize: 12,
        fontWeight: 500,
        lineHeight: 16,
        overflow: 'truncate',
      },
      upperLabel: { show: false },
      itemStyle: {
        gapWidth: 2,
        borderColor: '#1a1a1a',
      },
      emphasis: {
        itemStyle: { borderColor: '#ffffff', borderWidth: 2 }
      },
      data: data,
    }]
  };
}

window.DuhTreemap = { PALETTE, INK, EXCL_FILL, EXCL_INK, fmtBytes, buildOption };
