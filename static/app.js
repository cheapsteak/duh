// ---- global state ----
const state = {
  currentId: null,
  breadcrumb: [],
  children: [],
  nodeInfo: null,
  mode: 'freeable',
};

let chart = null;

// ---- init ----
window.addEventListener('DOMContentLoaded', async () => {
  chart = echarts.init(document.getElementById('treemap'));
  chart.on('click', (params) => {
    if (params && params.data && params.data.id) {
      navigateTo(params.data.id);
    }
  });
  window.addEventListener('resize', () => chart && chart.resize());

  const copyBtn = document.getElementById('copy-path');
  copyBtn.addEventListener('click', async () => {
    if (!state.nodeInfo) return;
    try {
      await navigator.clipboard.writeText(state.nodeInfo.path);
      copyBtn.textContent = '✓';
      copyBtn.classList.add('copied');
      setTimeout(() => {
        copyBtn.textContent = '⧉';
        copyBtn.classList.remove('copied');
      }, 1200);
    } catch (e) {
      showError('Copy failed: ' + e.message);
    }
  });

  // Check URL for deep-link
  const params = new URLSearchParams(window.location.search);
  const startId = params.get('id') ? parseInt(params.get('id')) : null;

  try {
    const root = await apiFetch('/api/root');
    if (root.error) {
      showError(root.error);
      return;
    }
    if (startId) {
      // Load breadcrumb then navigate
      const crumbs = await apiFetch('/api/breadcrumb/' + startId);
      if (crumbs && crumbs.length) {
        await navigateTo(startId, crumbs);
        return;
      }
    }
    await navigateTo(root.id);
  } catch(e) {
    showError('Failed to connect: ' + e.message);
  }
});

// ---- navigation ----
async function navigateTo(id, existingBreadcrumb) {
  setLoading(true);
  try {
    const data = await apiFetch('/api/node/' + id);
    if (data.error) { showError(data.error); return; }

    state.currentId = id;
    state.nodeInfo = data.node;
    state.children = data.children || [];

    // Breadcrumb
    if (existingBreadcrumb) {
      state.breadcrumb = existingBreadcrumb;
    } else {
      const crumbs = await apiFetch('/api/breadcrumb/' + id);
      state.breadcrumb = crumbs || [];
    }

    // Update URL
    const url = new URL(window.location);
    url.searchParams.set('id', id);
    window.history.pushState({id}, '', url);

    render();
  } catch(e) {
    showError('Navigation failed: ' + e.message);
  } finally {
    setLoading(false);
  }
}

window.addEventListener('popstate', (e) => {
  if (e.state && e.state.id) navigateTo(e.state.id);
});

// ---- rendering ----
function render(opts) {
  renderBreadcrumb();
  renderTable();
  renderTreemap(opts && opts.animate);
}

function renderBreadcrumb() {
  const el = document.getElementById('breadcrumb');
  // The copy button is a persistent node (it carries the click listener);
  // detach it before clearing and re-append after the last crumb.
  const copyBtn = document.getElementById('copy-path');
  el.innerHTML = '';
  state.breadcrumb.forEach((crumb, i) => {
    if (i > 0) {
      const sep = document.createElement('span');
      sep.className = 'crumb-sep';
      sep.textContent = ' / ';
      el.appendChild(sep);
    }
    const span = document.createElement('span');
    const isLast = i === state.breadcrumb.length - 1;
    span.className = isLast ? 'crumb crumb-cur' : 'crumb';
    span.textContent = crumb.name;
    span.title = crumb.name;
    if (!isLast) {
      span.onclick = () => navigateTo(crumb.id);
    }
    el.appendChild(span);
  });
  if (copyBtn) el.appendChild(copyBtn);
}

function sizeField(child) {
  if (state.mode === 'logical') return child.total_logical;
  if (state.mode === 'blocks') return child.total_blocks;
  // freeable (default)
  return child.freeable || 0;
}

function renderTable() {
  const tbody = document.getElementById('files-tbody');
  tbody.innerHTML = '';

  const children = [...state.children];
  // sort by current size field
  children.sort((a, b) => sizeField(b) - sizeField(a));

  const maxSize = children.reduce((m, c) => Math.max(m, sizeField(c)), 1);

  // Locked-here banner: if current node has significant locked_here, show at top
  const LOCKED_THRESHOLD = 100 * 1024 * 1024;
  if (state.mode === 'freeable' && state.nodeInfo && (state.nodeInfo.locked_here || 0) > LOCKED_THRESHOLD) {
    const trLock = document.createElement('tr');
    trLock.style.cssText = 'background:#2a1a40;color:#bb99ff;';
    const tdLock = document.createElement('td');
    tdLock.colSpan = 4;
    tdLock.style.cssText = 'padding:6px 10px;font-style:italic;';
    tdLock.textContent = '⛓ locked across children — freed only if all deleted: ' + fmtBytes(state.nodeInfo.locked_here);
    trLock.appendChild(tdLock);
    tbody.appendChild(trLock);
  }

  children.forEach(child => {
    const sz = sizeField(child);
    const frac = maxSize > 0 ? sz / maxSize : 0;
    // Color: blue (small) → red (big)
    const r = Math.round(frac * 220);
    const g = Math.round(60 + (1 - frac) * 100);
    const b = Math.round((1 - frac) * 220 + 30);
    const barColor = `rgb(${r},${g},${b})`;

    const tr = document.createElement('tr');
    if (child.is_excluded) tr.classList.add('excluded');
    if (child.is_dir) {
      tr.classList.add('clickable');
      tr.title = child.name;
      tr.onclick = () => navigateTo(child.id);
    }

    // Name cell
    const tdName = document.createElement('td');
    tdName.className = 'name';
    const nameSpan = document.createElement('span');
    nameSpan.textContent = child.name;
    tdName.appendChild(nameSpan);
    if (child.is_excluded) {
      const tag = document.createElement('span');
      tag.className = 'tag-excl';
      tag.textContent = 'excl';
      tdName.appendChild(tag);
    }
    if (child.shared) {
      const tag = document.createElement('span');
      tag.className = 'tag-shared';
      tag.textContent = 'shared';
      tag.title = 'Clone or hardlink: these blocks are also referenced from another path — deleting this alone frees 0 B (size: ' + fmtBytes(child.total_blocks) + ')';
      tdName.appendChild(tag);
    }
    tr.appendChild(tdName);

    // Size
    const tdSize = document.createElement('td');
    tdSize.className = 'size';
    tdSize.textContent = fmtBytes(sz);
    tr.appendChild(tdSize);

    // Files
    const tdFiles = document.createElement('td');
    tdFiles.className = 'files';
    tdFiles.textContent = fmtCount(child.total_files);
    tr.appendChild(tdFiles);

    // Bar
    const tdBar = document.createElement('td');
    tdBar.className = 'bar';
    const barDiv = document.createElement('div');
    barDiv.className = 'bar-inner';
    barDiv.style.width = Math.max(2, Math.round(frac * 100)) + '%';
    barDiv.style.background = barColor;
    tdBar.appendChild(barDiv);
    tr.appendChild(tdBar);

    tbody.appendChild(tr);
  });

  // Total row
  const tfoot = document.getElementById('files-tfoot');
  tfoot.innerHTML = '';
  if (children.length > 0) {
    const totalSize = children.reduce((s, c) => s + sizeField(c), 0);
    const totalFiles = children.reduce((s, c) => s + (c.total_files || 0), 0);

    const tr = document.createElement('tr');

    const tdName = document.createElement('td');
    tdName.className = 'name';
    tdName.textContent = `Total (${children.length} items)`;
    tr.appendChild(tdName);

    const tdSize = document.createElement('td');
    tdSize.className = 'size';
    tdSize.textContent = fmtBytes(totalSize);
    if (state.mode === 'freeable' && state.nodeInfo) {
      const own = state.nodeInfo.freeable || 0;
      if (own > totalSize * 1.05) {
        tdSize.title = 'Rows sum to ' + fmtBytes(totalSize) +
          ', but deleting this whole directory frees ' + fmtBytes(own) +
          ' — the difference is space shared across children (freed only together).';
        tdSize.textContent = fmtBytes(totalSize) + ' †';
      }
    }
    tr.appendChild(tdSize);

    const tdFiles = document.createElement('td');
    tdFiles.className = 'files';
    tdFiles.textContent = fmtCount(totalFiles);
    tr.appendChild(tdFiles);

    tr.appendChild(document.createElement('td')); // empty bar cell

    tfoot.appendChild(tr);
  }
}

// Categorical palette validated for the dark surface (#1a1a1a): lightness band,
// chroma floor, adjacent-pair CVD separation, >=3:1 vs surface all pass.
// INK[i] is the WCAG-preferred text color on PALETTE[i] (all pairs >=4.8:1) —
// computed, not eyeballed. Excluded dirs use the amber EXCL pair.
const PALETTE = ['#3987e5', '#199e70', '#c98500', '#008300', '#9085e9', '#e66767', '#d55181', '#d95926'];
const INK     = ['#111111', '#111111', '#111111', '#ffffff', '#111111', '#111111', '#111111', '#111111'];
const EXCL_FILL = '#c8a000', EXCL_INK = '#111111';

function renderTreemap(animate) {
  if (!chart) return;
  const children = state.children;
  // Sort by value so palette index matches layout adjacency (treemap lays out desc).
  const sorted = children
    .map(c => ({ c, value: sizeField(c) || 0 }))
    .filter(d => d.value > 0)
    .sort((a, b) => b.value - a.value);
  const total = sorted.reduce((s, d) => s + d.value, 0);

  const data = sorted.map((d, i) => {
    const fill = d.c.is_excluded ? EXCL_FILL : PALETTE[i % PALETTE.length];
    const ink = d.c.is_excluded ? EXCL_INK : INK[i % INK.length];
    return {
      name: d.c.name,
      value: d.value,
      id: d.c.id,
      itemStyle: { color: fill, opacity: d.c.is_excluded ? 0.9 : 1 },
      label: { color: ink },
      emphasis: { label: { color: ink, fontWeight: 'bold' } },
    };
  });

  // Navigation swaps in an unrelated node set — tweening between the two reads
  // as a nonsensical shuffle, so navigation renders instantly. Mode toggles
  // keep the morph: the same tiles meaningfully resize.
  // NOTE: instant means notMerge + animation:false (a from-scratch render,
  // same code path as initial load). A merged update with duration 0 hits an
  // ECharts treemap bug where the layout tween never fires and nothing paints.
  const option = {
    backgroundColor: '#1a1a1a',
    animation: !!animate,
    series: [{
      type: 'treemap',
      animationDurationUpdate: animate ? 400 : 0,
      roam: false,
      nodeClick: false,
      breadcrumb: { show: false },
      width: '100%',
      height: '100%',
      label: {
        show: true,
        // Slivers (<1% of view) render only truncated garbage — show nothing;
        // the hover tooltip and the table still carry them.
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
  chart.setOption(option, { notMerge: !animate });
}

// ---- mode toggle ----
function setMode(mode) {
  state.mode = mode;
  document.querySelectorAll('.mode-btn').forEach(b => {
    b.classList.toggle('active', b.dataset.mode === mode);
  });
  render({ animate: true });
}

// ---- helpers ----
function setLoading(on) {
  document.getElementById('loading').classList.toggle('hidden', !on);
}

function showError(msg) {
  const el = document.getElementById('error-banner');
  el.textContent = msg;
  el.style.display = 'block';
}

async function apiFetch(path) {
  const res = await fetch(path);
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(`HTTP ${res.status}: ${txt}`);
  }
  return res.json();
}

function fmtBytes(n) {
  if (n === null || n === undefined) return '—';
  if (n < 0) return '-' + fmtBytes(-n);
  const GiB = 1 << 30, MiB = 1 << 20, KiB = 1 << 10;
  if (n >= GiB) return (n / GiB).toFixed(1) + ' GiB';
  if (n >= MiB) return (n / MiB).toFixed(1) + ' MiB';
  if (n >= KiB) return (n / KiB).toFixed(1) + ' KiB';
  return n + ' B';
}

function fmtCount(n) {
  if (!n) return '0';
  if (n >= 1e6) return (n/1e6).toFixed(1) + 'M';
  if (n >= 1e3) return (n/1e3).toFixed(1) + 'k';
  return String(n);
}
