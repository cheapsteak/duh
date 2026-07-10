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
function render() {
  renderBreadcrumb();
  renderTable();
  renderTreemap();
}

function renderBreadcrumb() {
  const el = document.getElementById('breadcrumb');
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
}

function renderTreemap() {
  if (!chart) return;
  const children = state.children;
  const data = children.map(c => ({
    name: c.name,
    value: sizeField(c) || 0,
    id: c.id,
    itemStyle: c.is_excluded ? { color: '#c8a000', opacity: 0.9 } : undefined,
  })).filter(d => d.value > 0);

  chart.setOption({
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
        formatter: (p) => p.data.name + '\n' + fmtBytes(p.data.value),
        color: '#e0e0e0',
        fontSize: 11,
        overflow: 'truncate',
      },
      upperLabel: { show: false },
      itemStyle: {
        gapWidth: 2,
        borderColor: '#1a1a1a',
        color: '#3a5a8a',
      },
      emphasis: {
        itemStyle: { borderColor: '#7eb8f7', borderWidth: 2 }
      },
      data: data,
    }]
  });
}

// ---- mode toggle ----
function setMode(mode) {
  state.mode = mode;
  document.querySelectorAll('.mode-btn').forEach(b => {
    b.classList.toggle('active', b.dataset.mode === mode);
  });
  render();
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
