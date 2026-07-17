// fmtBytes is a global provided by treemap.js (loaded before this script);
// don't redeclare it here — a `const fmtBytes` in this shared global scope
// collides with treemap.js's `function fmtBytes` ("already declared").

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

  initShareDialog();

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
  renderDiskFree();
  renderTable();
  renderTreemap();
}

function renderDiskFree() {
  const el = document.getElementById('disk-free');
  const n = state.nodeInfo;
  if (n && n.disk_free != null && n.disk_total != null) {
    el.textContent = fmtBytes(n.disk_free) + ' free of ' + fmtBytes(n.disk_total);
  } else {
    el.textContent = '';
  }
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

function renderTreemap() {
  if (!chart) return;
  const items = state.children.map(c => ({
    name: c.name,
    value: sizeField(c),
    id: c.id,
    excluded: !!c.is_excluded,
  }));
  chart.setOption(DuhTreemap.buildOption(items));
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

async function apiFetch(path, options) {
  const res = await fetch(path, options);
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(`HTTP ${res.status}: ${txt}`);
  }
  return res.json();
}

// ---- share dialog ----
const SHARE_CONSENT_KEY = 'duh-share-consented';

function initShareDialog() {
  const shareBtn = document.getElementById('share-btn');
  const dialog = document.getElementById('share-dialog');
  const backdrop = document.getElementById('share-dialog-backdrop');
  const cancelBtn = document.getElementById('share-cancel');
  const copyBtn = document.getElementById('share-copy');

  function openShareDialog() {
    // Relies on closeShareDialog() having already hidden the share-error element
    // from any previous attempt (dialogs are re-opened, not recreated).
    if (!state.nodeInfo) {
      showError('Nothing to share yet.');
      return;
    }
    renderShareConsent();
    dialog.classList.remove('hidden');
  }

  function closeShareDialog() {
    dialog.classList.add('hidden');
    hideShareError();
  }

  shareBtn.addEventListener('click', openShareDialog);
  cancelBtn.addEventListener('click', closeShareDialog);
  backdrop.addEventListener('click', closeShareDialog);
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !dialog.classList.contains('hidden')) closeShareDialog();
  });

  copyBtn.addEventListener('click', async () => {
    // Guard re-entrancy: a second click while a request is in flight (or
    // during the 1.2s "Copied ✓" flash) would otherwise create a second gist,
    // or capture the flashed text as `original` and get stuck showing it
    // forever. Disabling the button for both windows closes that off.
    if (!state.currentId || copyBtn.disabled) return;
    hideShareError();
    const original = copyBtn.textContent;
    copyBtn.disabled = true;
    try {
      // /api/share has a side effect (a gist upload), so it's CSRF-guarded on
      // the server: this custom header can't be set by a cross-site fetch
      // without a CORS preflight, which this same-origin request never needs
      // but a cross-site one would fail (see src/serve.rs share_csrf_guard).
      const resp = await apiFetch('/api/share/' + state.currentId, {headers: {'X-Duh-Share': '1'}});
      await navigator.clipboard.writeText(resp.url);
      localStorage.setItem(SHARE_CONSENT_KEY, '1');
      copyBtn.textContent = 'Copied ✓';
      setTimeout(() => {
        copyBtn.textContent = original;
        copyBtn.disabled = false;
        closeShareDialog();
      }, 1200);
    } catch (e) {
      copyBtn.textContent = original;
      copyBtn.disabled = false;
      showShareError(shareErrorMessage(e));
    }
  });
}

// apiFetch throws `Error("HTTP <status>: <raw body text>")` on non-2xx. The
// share endpoint's error body is JSON `{"error": "<message>"}` — pull that
// message out for display; fall back to the raw exception text if the body
// isn't the expected shape (defensive, shouldn't happen against this server).
function shareErrorMessage(e) {
  const msg = e && e.message || String(e);
  const i = msg.indexOf(': ');
  const body = i === -1 ? msg : msg.slice(i + 2);
  try {
    const parsed = JSON.parse(body);
    if (parsed && typeof parsed.error === 'string') return parsed.error;
  } catch (_) {
    // not JSON — fall through to raw text
  }
  return body;
}

function showShareError(msg) {
  const el = document.getElementById('share-error');
  el.textContent = msg;
  el.classList.remove('hidden');
}

function hideShareError() {
  const el = document.getElementById('share-error');
  el.textContent = '';
  el.classList.add('hidden');
}

// SECURITY: state.nodeInfo.path is the real filesystem path being shared —
// insert it via textContent on a dedicated <b>, never innerHTML/string concat,
// so a maliciously-named directory can't inject markup into this dialog.
function renderShareConsent() {
  const el = document.getElementById('share-consent');
  el.textContent = '';
  const consented = localStorage.getItem(SHARE_CONSENT_KEY) === '1';
  if (consented) {
    el.appendChild(document.createTextNode(
      'Uploads a snapshot to a secret gist on your GitHub.'
    ));
    return;
  }
  const path = (state.nodeInfo && state.nodeInfo.path) || '';
  el.appendChild(document.createTextNode('This uploads a snapshot of everything below '));
  const b = document.createElement('b');
  b.textContent = path;
  el.appendChild(b);
  el.appendChild(document.createTextNode(
    ' — real directory and file names — to a secret (unlisted) gist on your GitHub account. ' +
    'Anyone with the link can open it; it is not searchable, and you can delete it anytime at ' +
    'gist.github.com. Nothing is access-controlled: treat the link as the secret.'
  ));
}

function fmtCount(n) {
  if (!n) return '0';
  if (n >= 1e6) return (n/1e6).toFixed(1) + 'M';
  if (n >= 1e3) return (n/1e3).toFixed(1) + 'k';
  return String(n);
}
