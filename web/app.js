// Recall web UI. No framework: one canvas, a few lists, plain DOM.
// Memory text is only ever written with textContent, never as HTML.

// ---- theme (runs before first paint) ---------------------------------------
const THEME_KEY = 'recall-theme';
const media = matchMedia('(prefers-color-scheme: dark)');
function savedTheme() {
  try { return localStorage.getItem(THEME_KEY) || 'system'; } catch { return 'system'; }
}
function applyTheme() {
  const t = savedTheme();
  document.documentElement.classList.toggle('dark', t === 'dark' || (t === 'system' && media.matches));
}
applyTheme();
media.addEventListener('change', () => { applyTheme(); graph?.draw(); });

document.addEventListener('DOMContentLoaded', () => { main(); });

// ---- helpers -----------------------------------------------------------------
const $ = (id) => document.getElementById(id);
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}
async function api(path, opts = {}) {
  const res = await fetch(path, {
    ...opts,
    credentials: 'same-origin',
    headers: { 'X-Requested-With': 'recall', 'Content-Type': 'application/json' },
  });
  const body = await res.json().catch(() => ({}));
  if (res.status === 401 && path !== '/api/login') { showLogin(); throw new Error('signed out'); }
  if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
  return body;
}
let toastTimer;
function toast(msg) {
  const t = $('toast');
  t.textContent = msg;
  t.dataset.show = '';
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => delete t.dataset.show, 2600);
}

// Cluster colours (Tailwind 500 / 400 shades for light / dark).
const PALETTE = [
  ['#6366f1', '#818cf8'], ['#0ea5e9', '#38bdf8'], ['#10b981', '#34d399'], ['#f59e0b', '#fbbf24'],
  ['#f43f5e', '#fb7185'], ['#8b5cf6', '#a78bfa'], ['#14b8a6', '#2dd4bf'], ['#f97316', '#fb923c'],
  ['#d946ef', '#e879f9'], ['#84cc16', '#a3e635'], ['#06b6d4', '#22d3ee'], ['#ec4899', '#f472b6'],
];
const isDark = () => document.documentElement.classList.contains('dark');
const LONE = 9999;
function colour(cluster) {
  if (cluster === LONE) return isDark() ? '#64748b' : '#94a3b8';
  return PALETTE[cluster % PALETTE.length][isDark() ? 1 : 0];
}

// ---- state ---------------------------------------------------------------------
let data = { nodes: [], edges: [], clusters: [] };
let byId = new Map();
let neighbours = [];        // node index -> [[index, weight]]
let selected = null;        // node index
let matches = null;         // Set of node indexes, or null when not searching
let clusterFilter = null;   // cluster id or null
let graph = null;

// ---- boot ------------------------------------------------------------------------
async function main() {
  wireStatic();
  const s = await api('/api/session').catch(() => ({ authed: false, passwordSet: true }));
  if (s.authed) showApp(); else showLogin(s.passwordSet);
}

function showLogin(passwordSet = true) {
  $('app').classList.add('hidden'); $('app').classList.remove('grid');
  $('login').classList.remove('hidden'); $('login').classList.add('flex');
  $('login-hint').classList.toggle('hidden', passwordSet);
  $('password').focus();
}

async function showApp() {
  $('login').classList.add('hidden'); $('login').classList.remove('flex');
  $('app').classList.remove('hidden'); $('app').classList.add('grid');
  if (!graph) graph = new Graph($('graph'));
  await load();
}

async function load(selectId) {
  data = await api('/api/graph');
  // Singletons share one grey "unlinked" cluster so the legend stays short.
  const sizes = new Map();
  data.nodes.forEach((n) => sizes.set(n.cluster, (sizes.get(n.cluster) || 0) + 1));
  data.nodes.forEach((n) => { if (sizes.get(n.cluster) === 1) n.cluster = LONE; });
  byId = new Map(data.nodes.map((n, i) => [n.id, i]));
  neighbours = data.nodes.map(() => []);
  for (const [a, b, w] of data.edges) { neighbours[a].push([b, w]); neighbours[b].push([a, w]); }
  neighbours.forEach((l) => l.sort((x, y) => y[1] - x[1]));
  $('count').textContent = data.total;
  $('empty').classList.toggle('hidden', data.nodes.length > 0);
  $('empty').classList.toggle('flex', data.nodes.length === 0);
  renderLegend();
  graph.setData(data.nodes, data.edges);
  select(selectId != null && byId.has(selectId) ? byId.get(selectId) : null, false);
  runSearch();
}

// ---- static wiring -----------------------------------------------------------------
function wireStatic() {
  $('login-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const btn = e.submitter; btn.disabled = true; $('login-error').textContent = '';
    try {
      await api('/api/login', { method: 'POST', body: JSON.stringify({ password: $('password').value }) });
      $('password').value = '';
      showApp();
    } catch (err) { $('login-error').textContent = err.message; }
    btn.disabled = false;
  });
  $('logout').addEventListener('click', async () => { await api('/api/logout', { method: 'POST' }).catch(() => {}); showLogin(); });

  const icons = {
    system: '<svg viewBox="0 0 20 20" fill="currentColor" class="size-4"><path fill-rule="evenodd" d="M2 4.25A2.25 2.25 0 0 1 4.25 2h11.5A2.25 2.25 0 0 1 18 4.25v8.5A2.25 2.25 0 0 1 15.75 15h-3.105a3.501 3.501 0 0 0 1.1 1.677A.75.75 0 0 1 13.26 18H6.74a.75.75 0 0 1-.484-1.323A3.501 3.501 0 0 0 7.355 15H4.25A2.25 2.25 0 0 1 2 12.75v-8.5Zm1.5 0a.75.75 0 0 1 .75-.75h11.5a.75.75 0 0 1 .75.75v7.5a.75.75 0 0 1-.75.75H4.25a.75.75 0 0 1-.75-.75v-7.5Z" clip-rule="evenodd"/></svg>',
    light: '<svg viewBox="0 0 20 20" fill="currentColor" class="size-4"><path d="M10 2a.75.75 0 0 1 .75.75v1.5a.75.75 0 0 1-1.5 0v-1.5A.75.75 0 0 1 10 2ZM10 15a.75.75 0 0 1 .75.75v1.5a.75.75 0 0 1-1.5 0v-1.5A.75.75 0 0 1 10 15ZM10 7a3 3 0 1 0 0 6 3 3 0 0 0 0-6ZM15.657 5.404a.75.75 0 1 0-1.06-1.06l-1.061 1.06a.75.75 0 0 0 1.06 1.06l1.06-1.06ZM6.464 14.596a.75.75 0 1 0-1.06-1.06l-1.06 1.06a.75.75 0 0 0 1.06 1.06l1.06-1.06ZM18 10a.75.75 0 0 1-.75.75h-1.5a.75.75 0 0 1 0-1.5h1.5A.75.75 0 0 1 18 10ZM5 10a.75.75 0 0 1-.75.75h-1.5a.75.75 0 0 1 0-1.5h1.5A.75.75 0 0 1 5 10ZM14.596 15.657a.75.75 0 0 0 1.06-1.06l-1.06-1.061a.75.75 0 1 0-1.06 1.06l1.06 1.06ZM5.404 6.464a.75.75 0 0 0 1.06-1.06l-1.06-1.06a.75.75 0 1 0-1.061 1.06l1.06 1.06Z"/></svg>',
    dark: '<svg viewBox="0 0 20 20" fill="currentColor" class="size-4"><path fill-rule="evenodd" d="M7.455 2.004a.75.75 0 0 1 .26.77 7 7 0 0 0 9.958 7.967.75.75 0 0 1 1.067.853A8.5 8.5 0 1 1 6.647 1.921a.75.75 0 0 1 .808.083Z" clip-rule="evenodd"/></svg>',
  };
  const order = ['system', 'light', 'dark'];
  const paintThemeButton = () => {
    const t = savedTheme();
    $('theme').innerHTML = icons[t];   // static SVG only
    $('theme').title = `Theme: ${t}`;
  };
  paintThemeButton();
  $('theme').addEventListener('click', () => {
    const next = order[(order.indexOf(savedTheme()) + 1) % order.length];
    try { localStorage.setItem(THEME_KEY, next); } catch {}
    applyTheme(); paintThemeButton(); renderLegend(); renderDetail(); graph?.draw();
  });

  let timer;
  $('search').addEventListener('input', () => { clearTimeout(timer); runSearch(true); timer = setTimeout(() => runSearch(false), 120); });
  $('menu').addEventListener('click', () => {
    const s = $('sidebar');
    if ('open' in s.dataset) delete s.dataset.open; else s.dataset.open = '';
  });
  document.querySelectorAll('[data-zoom]').forEach((b) => b.addEventListener('click', () => graph?.zoomBy(b.dataset.zoom)));

  $('new').addEventListener('click', openNew);
  $('new-cancel').addEventListener('click', () => $('new-dialog').close());
  $('new-form').addEventListener('submit', saveNew);
  $('d-close').addEventListener('click', () => select(null));
  $('d-content').addEventListener('input', () => {
    $('d-save').disabled = selected == null || $('d-content').value.trim() === data.nodes[selected].content;
  });
  $('d-save').addEventListener('click', saveEdit);
  $('d-forget').addEventListener('click', forget);

  addEventListener('keydown', (e) => {
    const typing = /INPUT|TEXTAREA/.test(document.activeElement?.tagName);
    if ((e.key === '/' && !typing) || (e.key === 'k' && (e.metaKey || e.ctrlKey))) { e.preventDefault(); $('search').focus(); $('search').select(); }
    else if (e.key === 'n' && !typing && !$('app').classList.contains('hidden')) { e.preventDefault(); openNew(); }
    else if (e.key === 'Escape' && !$('new-dialog').open) {
      if (document.activeElement === $('search') && $('search').value) { $('search').value = ''; runSearch(false); }
      else select(null);
    }
  });
}

// ---- search ----------------------------------------------------------------------
let searchSeq = 0;
async function runSearch(localOnly) {
  const q = $('search').value.trim();
  if (!q) { matches = null; renderList(); graph?.draw(); return; }
  const lower = q.toLowerCase();
  const local = data.nodes.map((n, i) => (n.content.toLowerCase().includes(lower) ? i : -1)).filter((i) => i >= 0);
  matches = new Set(local);
  let order = local;
  if (!localOnly) {
    const seq = ++searchSeq;
    try {
      const r = await api(`/api/search?q=${encodeURIComponent(q)}`);
      if (seq !== searchSeq) return;
      // Fuzzy/semantic hits first (server order), then plain substring hits.
      const remote = r.ids.map((id) => byId.get(id)).filter((i) => i != null);
      order = [...new Set([...remote, ...local])];
      matches = new Set(order);
    } catch { /* keep local results */ }
  }
  renderList(order);
  graph?.draw();
}

// ---- list ------------------------------------------------------------------------
function renderList(order) {
  const list = $('list');
  list.replaceChildren();
  let idx = order ?? data.nodes.map((_, i) => i);
  if (clusterFilter != null) idx = idx.filter((i) => data.nodes[i].cluster === clusterFilter);
  $('results').textContent = matches ? `${idx.length} result${idx.length === 1 ? '' : 's'}` : clusterFilter != null ? clusterName(clusterFilter) : 'Most recalled';
  const frag = document.createDocumentFragment();
  for (const i of idx.slice(0, 400)) {
    const n = data.nodes[i];
    const li = el('li');
    const b = el('button', 'group flex w-full items-start gap-2.5 rounded-lg px-2 py-2 text-left hover:bg-slate-200/60 dark:hover:bg-slate-800/70');
    if (i === selected) b.classList.add('bg-indigo-50', 'dark:bg-indigo-500/10');
    const dot = el('span', 'mt-1.5 size-2 shrink-0 rounded-full');
    dot.style.background = colour(n.cluster);
    const body = el('span', 'min-w-0');
    body.append(el('span', 'line-clamp-2 text-[13px] leading-snug text-slate-700 dark:text-slate-300', n.content));
    body.append(el('span', 'mt-0.5 block text-[11px] tabular-nums text-slate-500 dark:text-slate-400', n.date));
    b.append(dot, body);
    b.addEventListener('click', () => { select(i); graph.focus(i); delete $('sidebar').dataset.open; });
    li.append(b);
    frag.append(li);
  }
  list.append(frag);
}

function clusterName(c) {
  if (c === LONE) return 'unlinked';
  return data.clusters[c] || `cluster ${c + 1}`;
}

function renderLegend() {
  const legend = $('legend');
  legend.replaceChildren();
  const counts = new Map();
  data.nodes.forEach((n) => counts.set(n.cluster, (counts.get(n.cluster) || 0) + 1));
  const top = [...counts.entries()].filter(([c]) => c !== LONE).sort((a, b) => b[1] - a[1]).slice(0, 8);
  for (const [c, count] of top) {
    const b = el('button', 'pointer-events-auto flex items-center gap-1.5 rounded-full border border-slate-200 bg-white/80 px-2.5 py-1 text-xs text-slate-600 backdrop-blur hover:border-slate-300 dark:border-slate-800 dark:bg-slate-900/80 dark:text-slate-300 dark:hover:border-slate-700');
    if (clusterFilter === c) b.classList.add('ring-2', 'ring-indigo-500/40');
    const dot = el('span', 'size-2 rounded-full');
    dot.style.background = colour(c);
    b.append(dot, el('span', null, clusterName(c)), el('span', 'tabular-nums text-slate-500 dark:text-slate-400', String(count)));
    b.addEventListener('click', () => { clusterFilter = clusterFilter === c ? null : c; renderLegend(); runSearch(true); graph.draw(); });
    legend.append(b);
  }
}

// ---- detail ----------------------------------------------------------------------
function select(i, redraw = true) {
  selected = i;
  renderDetail();
  renderList(matches ? [...matches] : undefined);
  if (redraw) graph?.draw();
}

function renderDetail() {
  const empty = selected == null;
  if (empty) $('detail').dataset.empty = ''; else delete $('detail').dataset.empty;
  $('detail-empty').classList.toggle('hidden', !empty);
  $('detail-empty').classList.toggle('flex', empty);
  $('detail-body').classList.toggle('hidden', empty);
  $('detail-body').classList.toggle('flex', !empty);
  if (empty) {
    const stats = $('stats');
    stats.replaceChildren();
    const clusters = new Set(data.nodes.map((n) => n.cluster).filter((c) => c !== LONE)).size;
    for (const [label, value] of [['memories', data.total], ['links', data.edges.length], ['clusters', clusters]]) {
      const d = el('div', 'rounded-lg bg-white px-3 py-3 dark:bg-slate-950');
      d.append(el('dd', 'text-lg font-semibold tabular-nums', String(value ?? 0)), el('dt', 'text-[11px] text-slate-500 dark:text-slate-400', label));
      stats.append(d);
    }
    return;
  }
  const n = data.nodes[selected];
  $('d-dot').style.background = colour(n.cluster);
  $('d-cluster').textContent = clusterName(n.cluster);
  $('d-id').textContent = n.id;
  $('d-content').value = n.content;
  $('d-save').disabled = true;
  $('d-date').textContent = n.date;
  $('d-uses').textContent = `${n.uses} time${n.uses === 1 ? '' : 's'}`;
  const links = $('d-links');
  links.replaceChildren();
  if (!neighbours[selected].length) links.append(el('li', 'text-sm text-slate-500 dark:text-slate-400', 'Nothing closely related yet.'));
  for (const [j, w] of neighbours[selected].slice(0, 12)) {
    const m = data.nodes[j];
    const b = el('button', 'flex w-full items-start gap-2 rounded-lg px-2 py-1.5 text-left text-[13px] leading-snug hover:bg-slate-200/60 dark:hover:bg-slate-800/70');
    const dot = el('span', 'mt-1.5 size-2 shrink-0 rounded-full');
    dot.style.background = colour(m.cluster);
    b.append(dot, el('span', 'min-w-0 flex-1 text-slate-700 dark:text-slate-300', m.content), el('span', 'shrink-0 text-[11px] tabular-nums text-slate-500 dark:text-slate-400', `${Math.round(w * 100)}%`));
    b.addEventListener('click', () => { select(j); graph.focus(j); });
    const li = el('li');
    li.append(b);
    links.append(li);
  }
}

async function saveEdit() {
  const n = data.nodes[selected];
  try {
    const r = await api('/api/memories', { method: 'POST', body: JSON.stringify({ content: $('d-content').value, date: n.date, replaces: n.id }) });
    toast('Saved. The old version can be restored for 30 days.');
    await load(r.id);
  } catch (e) { toast(e.message); }
}

async function forget() {
  const n = data.nodes[selected];
  if (!confirm(`Forget this memory?\n\n${n.content}\n\nIt can be restored for 30 days with: recall restore ${n.id}`)) return;
  try { await api(`/api/memories/${encodeURIComponent(n.id)}`, { method: 'DELETE' }); toast('Forgotten.'); await load(); } catch (e) { toast(e.message); }
}

function openNew() {
  $('new-error').textContent = '';
  $('new-content').value = '';
  $('new-date').value = '';
  $('new-dialog').showModal();
  $('new-content').focus();
}

async function saveNew(e) {
  e.preventDefault();
  $('new-error').textContent = '';
  try {
    const r = await api('/api/memories', { method: 'POST', body: JSON.stringify({ content: $('new-content').value, date: $('new-date').value }) });
    $('new-dialog').close();
    toast(r.duplicate ? 'Already known.' : r.related?.length ? 'Saved. Check the linked memories in case one is now outdated.' : 'Saved.');
    await load(r.id);
    if (byId.has(r.id)) graph.focus(byId.get(r.id));
  } catch (err) { $('new-error').textContent = err.message; }
}

// ---- graph -------------------------------------------------------------------------
/** Deterministic pseudo-random number in [0, 1) from a string and a salt. */
function hash01(str, salt) {
  let h = 2166136261 ^ salt;
  for (let i = 0; i < str.length; i++) h = Math.imul(h ^ str.charCodeAt(i), 16777619);
  return ((h >>> 0) % 100000) / 100000;
}

class Graph {
  constructor(canvas) {
    this.c = canvas;
    this.ctx = canvas.getContext('2d');
    this.nodes = [];
    this.edges = [];
    this.view = { x: 0, y: 0, k: 1 };
    this.alpha = 0;
    this.hover = null;
    this.drag = null;
    this.raf = 0;
    new ResizeObserver(() => this.resize()).observe(canvas);
    this.bind();
  }

  setData(nodes, edges) {
    const prev = new Map(this.nodes.map((p) => [p.id, p]));
    // Seed new nodes on a ring per cluster so groups start apart.
    const clusters = [...new Set(nodes.map((n) => n.cluster))];
    const angle = new Map(clusters.map((c, i) => [c, (i / clusters.length) * Math.PI * 2]));
    this.nodes = nodes.map((n, i) => {
      const old = prev.get(n.id);
      if (old) return { ...old, id: n.id, i };
      // Seeded by the id, so the map looks the same on every visit.
      const a = angle.get(n.cluster) + (hash01(n.id, 1) - 0.5) * 0.8;
      const r = 220 + hash01(n.id, 2) * 60;
      return { id: n.id, i, x: Math.cos(a) * r, y: Math.sin(a) * r, vx: 0, vy: 0 };
    });
    this.edges = edges;
    this.degree = nodes.map(() => 0);
    edges.forEach(([a, b]) => { this.degree[a]++; this.degree[b]++; });
    this.radius = nodes.map((n, i) => 4 + Math.sqrt(n.uses + this.degree[i]) * 1.8);
    const fresh = prev.size === 0;
    if (fresh) { for (let t = 0; t < 300; t++) this.tick(1 - t / 300); }
    this.alpha = fresh ? 0.15 : 0.6;
    if (fresh) this.fit();
    this.kick();
  }

  // One step of a simple force simulation: grid-bucketed repulsion, edge
  // springs (stronger links pull closer) and weak gravity to the centre.
  tick(alpha) {
    const N = this.nodes, cell = 90, grid = new Map();
    for (const p of N) {
      const k = `${Math.floor(p.x / cell)},${Math.floor(p.y / cell)}`;
      (grid.get(k) || grid.set(k, []).get(k)).push(p);
    }
    for (const p of N) {
      const cx = Math.floor(p.x / cell), cy = Math.floor(p.y / cell);
      for (let dx = -2; dx <= 2; dx++) for (let dy = -2; dy <= 2; dy++) {
        const bucket = grid.get(`${cx + dx},${cy + dy}`);
        if (!bucket) continue;
        for (const q of bucket) {
          if (q === p) continue;
          let x = p.x - q.x, y = p.y - q.y, d2 = x * x + y * y;
          if (d2 === 0) { x = hash01(p.id, 3) - 0.5; y = hash01(q.id, 4) - 0.5; d2 = 0.01; }
          if (d2 > cell * cell * 4) continue;
          const f = (900 * alpha) / d2;
          p.vx += x * f; p.vy += y * f;
        }
      }
    }
    // Springs: tight inside a topic, long and weak between topics.
    const cl = (i) => data.nodes[i].cluster;
    for (const [a, b, w] of this.edges) {
      const p = N[a], q = N[b];
      const x = q.x - p.x, y = q.y - p.y, d = Math.hypot(x, y) || 1;
      const same = cl(a) === cl(b) && cl(a) !== LONE;
      const target = same ? 26 + (1 - w) * 40 : 150;
      const f = ((d - target) / d) * (same ? 0.08 : 0.008) * alpha;
      p.vx += x * f; p.vy += y * f; q.vx -= x * f; q.vy -= y * f;
    }
    // Topics: pull each memory toward its cluster's centre, and push cluster
    // centres apart, so the map reads as islands rather than a hairball.
    const centre = new Map();
    for (const p of N) {
      const m = centre.get(cl(p.i)) || centre.set(cl(p.i), { x: 0, y: 0, n: 0 }).get(cl(p.i));
      m.x += p.x; m.y += p.y; m.n++;
    }
    for (const m of centre.values()) { m.x /= m.n; m.y /= m.n; m.r = 30 + Math.sqrt(m.n) * 26; }
    const groups = [...centre.entries()].filter(([c]) => c !== LONE);
    for (const p of N) {
      const c = cl(p.i);
      if (c !== LONE) {
        const m = centre.get(c);
        p.vx += (m.x - p.x) * 0.06 * alpha; p.vy += (m.y - p.y) * 0.06 * alpha;
      }
      for (const [o, m] of groups) {
        if (o === c) continue;
        const x = p.x - m.x, y = p.y - m.y, d = Math.hypot(x, y) || 1;
        const reach = m.r + (c === LONE ? 30 : centre.get(c).r) + 70;
        if (d < reach) { const f = ((reach - d) / d) * 0.05 * alpha; p.vx += x * f; p.vy += y * f; }
      }
    }
    for (const p of N) {
      p.vx -= p.x * 0.006 * alpha; p.vy -= p.y * 0.006 * alpha;
      if (p === this.drag?.node) { p.vx = p.vy = 0; continue; }
      p.vx *= 0.82; p.vy *= 0.82;
      p.x += Math.max(-30, Math.min(30, p.vx));
      p.y += Math.max(-30, Math.min(30, p.vy));
    }
  }

  kick() {
    if (this.raf) return;
    const loop = () => {
      if (this.alpha > 0.02 || this.drag?.node) {
        this.tick(this.alpha);
        this.alpha *= 0.985;
        this.draw();
        this.raf = requestAnimationFrame(loop);
      } else { this.raf = 0; this.draw(); }
    };
    this.raf = requestAnimationFrame(loop);
  }

  resize() {
    const r = this.c.getBoundingClientRect();
    this.w = r.width; this.h = r.height;
    this.dpr = devicePixelRatio || 1;
    this.c.width = Math.round(r.width * this.dpr);
    this.c.height = Math.round(r.height * this.dpr);
    this.draw();
  }

  fit() {
    if (!this.nodes.length || !this.w) return;
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const p of this.nodes) { x0 = Math.min(x0, p.x); y0 = Math.min(y0, p.y); x1 = Math.max(x1, p.x); y1 = Math.max(y1, p.y); }
    // Extra horizontal room: node labels extend sideways from the dots.
    const padX = Math.min(320, this.w * 0.25), padY = Math.min(140, this.h * 0.15);
    const k = Math.min(2.2, Math.min((this.w - padX) / (x1 - x0 || 1), (this.h - padY) / (y1 - y0 || 1)));
    this.view = { k, x: this.w / 2 - ((x0 + x1) / 2) * k, y: this.h / 2 + 10 - ((y0 + y1) / 2) * k };
    this.draw();
  }

  zoomBy(what) {
    if (what === 'fit') return this.fit();
    this.zoomAt(this.w / 2, this.h / 2, what === 'in' ? 1.3 : 1 / 1.3);
  }

  zoomAt(sx, sy, f) {
    const v = this.view, k = Math.max(0.15, Math.min(6, v.k * f));
    v.x = sx - ((sx - v.x) * k) / v.k; v.y = sy - ((sy - v.y) * k) / v.k; v.k = k;
    this.draw();
  }

  focus(i) {
    const p = this.nodes[i];
    if (!p) return;
    const k = Math.max(this.view.k, 1.4);
    this.view = { k, x: this.w / 2 - p.x * k, y: this.h / 2 - p.y * k };
    this.draw();
  }

  toWorld(sx, sy) { return [(sx - this.view.x) / this.view.k, (sy - this.view.y) / this.view.k]; }

  hit(sx, sy) {
    const [x, y] = this.toWorld(sx, sy);
    let best = null, bestD = Infinity;
    for (const p of this.nodes) {
      const d = Math.hypot(p.x - x, p.y - y), r = this.radius[p.i] + 4 / this.view.k;
      if (d < r && d < bestD) { best = p.i; bestD = d; }
    }
    return best;
  }

  bind() {
    const c = this.c;
    const pos = (e) => { const r = c.getBoundingClientRect(); return [e.clientX - r.left, e.clientY - r.top]; };
    c.addEventListener('wheel', (e) => { e.preventDefault(); const [x, y] = pos(e); this.zoomAt(x, y, Math.exp(-e.deltaY * 0.0015)); }, { passive: false });
    c.addEventListener('pointerdown', (e) => {
      c.setPointerCapture(e.pointerId);
      const [x, y] = pos(e), i = this.hit(x, y);
      this.drag = { x, y, moved: false, node: i != null ? this.nodes[i] : null, vx: this.view.x, vy: this.view.y };
    });
    c.addEventListener('pointermove', (e) => {
      const [x, y] = pos(e);
      if (this.drag) {
        if (Math.hypot(x - this.drag.x, y - this.drag.y) > 3) this.drag.moved = true;
        if (this.drag.node) {
          [this.drag.node.x, this.drag.node.y] = this.toWorld(x, y);
          this.alpha = Math.max(this.alpha, 0.3); this.kick();
        } else { this.view.x = this.drag.vx + x - this.drag.x; this.view.y = this.drag.vy + y - this.drag.y; this.draw(); }
        return;
      }
      const h = this.hit(x, y);
      if (h !== this.hover) { this.hover = h; c.style.cursor = h != null ? 'pointer' : 'grab'; this.draw(); }
    });
    const up = (e) => {
      const d = this.drag; this.drag = null;
      if (d && !d.moved) { const [x, y] = pos(e); select(this.hit(x, y)); }
    };
    c.addEventListener('pointerup', up);
    c.addEventListener('pointercancel', () => { this.drag = null; });
    c.addEventListener('pointerleave', () => { if (this.hover != null) { this.hover = null; this.draw(); } });
    c.addEventListener('dblclick', () => this.fit());
  }

  draw() {
    const ctx = this.ctx;
    if (!ctx || !this.w) return;
    const dark = isDark(), v = this.view, dpr = this.dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, this.w, this.h);
    if (!this.nodes.length) return;

    const focusNode = this.hover ?? selected;
    const lit = new Set();
    if (focusNode != null) { lit.add(focusNode); neighbours[focusNode]?.forEach(([j]) => lit.add(j)); }
    const visible = (i) => {
      if (clusterFilter != null && data.nodes[i].cluster !== clusterFilter) return false;
      if (matches && !matches.has(i)) return false;
      return true;
    };
    const dim = (i) => (focusNode != null ? !lit.has(i) : !visible(i));

    ctx.save();
    ctx.translate(v.x, v.y);
    ctx.scale(v.k, v.k);

    // Edges
    ctx.lineCap = 'round';
    for (const [a, b, w] of this.edges) {
      const p = this.nodes[a], q = this.nodes[b];
      const hot = focusNode != null && (a === focusNode || b === focusNode);
      const faded = dim(a) || dim(b);
      const ca = data.nodes[a].cluster, same = ca === data.nodes[b].cluster && ca !== LONE;
      ctx.strokeStyle = hot ? colour(data.nodes[focusNode].cluster) : same ? colour(ca) : dark ? '#94a3b8' : '#64748b';
      ctx.globalAlpha = hot ? 0.9 : faded ? 0.04 : same ? 0.25 + w * 0.35 : 0.07;
      ctx.lineWidth = (hot ? 1.8 : same ? 0.8 + w * 1.4 : 0.6) / Math.sqrt(v.k);
      ctx.beginPath(); ctx.moveTo(p.x, p.y); ctx.lineTo(q.x, q.y); ctx.stroke();
    }

    // Nodes
    for (const p of this.nodes) {
      const n = data.nodes[p.i], r = this.radius[p.i];
      ctx.globalAlpha = dim(p.i) ? 0.15 : 1;
      ctx.fillStyle = colour(n.cluster);
      ctx.beginPath(); ctx.arc(p.x, p.y, r, 0, Math.PI * 2); ctx.fill();
      if (p.i === selected || p.i === this.hover) {
        ctx.globalAlpha = 1;
        ctx.lineWidth = 2.5 / v.k;
        ctx.strokeStyle = dark ? '#f8fafc' : '#0f172a';
        ctx.beginPath(); ctx.arc(p.x, p.y, r + 3 / v.k, 0, Math.PI * 2); ctx.stroke();
      }
    }

    // Node labels: when zoomed in, or for the focused node and its links.
    const font = '-apple-system, BlinkMacSystemFont, "Segoe UI", Inter, system-ui, sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'top';
    const showAll = v.k >= 1.25;
    for (const p of this.nodes) {
      const focused = focusNode != null && lit.has(p.i);
      const searchHit = matches && matches.size <= 25 && matches.has(p.i);
      if (!showAll && !focused && !searchHit) continue;
      if (dim(p.i) && !focused) continue;
      const n = data.nodes[p.i];
      const text = n.content.length > 42 ? `${n.content.slice(0, 40).trimEnd()}…` : n.content;
      const size = 11 / Math.min(v.k, 1.6);
      ctx.font = `${p.i === focusNode ? 600 : 400} ${size}px ${font}`;
      ctx.globalAlpha = 1;
      ctx.lineWidth = 3 / v.k;
      ctx.strokeStyle = dark ? '#020617' : '#ffffff';
      ctx.fillStyle = dark ? '#e2e8f0' : '#334155';
      const y = p.y + this.radius[p.i] + 3 / v.k;
      ctx.strokeText(text, p.x, y);
      ctx.fillText(text, p.x, y);
    }

    // Cluster names at their centroids (zoomed out).
    // Hidden while searching or focusing, so they never cover result labels.
    if (v.k < 1.25 && focusNode == null && !matches) {
      const sum = new Map();
      for (const p of this.nodes) {
        const c = data.nodes[p.i].cluster;
        if (c === LONE) continue;
        const s = sum.get(c) || sum.set(c, { x: 0, y: 0, n: 0, top: Infinity }).get(c);
        s.x += p.x; s.y += p.y; s.n++; s.top = Math.min(s.top, p.y);
      }
      ctx.textBaseline = 'bottom';
      for (const [c, s] of sum) {
        if (s.n < 3) continue;
        if (clusterFilter != null && c !== clusterFilter) continue;
        ctx.font = `600 ${13 / v.k}px ${font}`;
        ctx.globalAlpha = 0.9;
        ctx.lineWidth = 4 / v.k;
        ctx.strokeStyle = dark ? '#020617' : '#ffffff';
        ctx.fillStyle = colour(c);
        const x = s.x / s.n, y = s.top - 10 / v.k;
        ctx.strokeText(clusterName(c), x, y);
        ctx.fillText(clusterName(c), x, y);
      }
    }
    ctx.restore();
  }
}
