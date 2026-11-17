'use strict';
/*
 * LC Play Producer — the operator's interface.
 *
 * One file, no framework, no build step. It talks to two services directly:
 *
 *   Director       WebSocket  — receives the show + transports + cluster
 *                               status, sends show edits and commands
 *   Asset Manager  HTTP       — media list, upload, thumbnails, media files
 *
 * How the pieces fit:
 *
 *   S (state) ── edit() ──▶ S.doc changes ──▶ sendDoc() ──▶ Director ──▶ Runners
 *        ▲                                                     │
 *        └──────────── "show" / "transport" / "cluster" ◀──────┘
 *
 * - The show document (S.doc) is the single source of truth for everything
 *   drawn here. Every edit goes through edit() (undo + redraw + send).
 * - The whole document is sent on every change. It is small, and it means
 *   the Director and Runners never hold a half-applied edit.
 * - A show has several TIMELINES: the main one, plus any number of auxiliary
 *   timelines ("tasks"). Each has its own transport. Time is never pushed to
 *   us: like a Runner, we hold each timeline's transport state and compute
 *   its position ourselves, every animation frame, from the cluster clock.
 *
 * Sections: helpers · state · window manager · network · time · editing ·
 * assets · preview media · stage · timeline · tasks · runners · inspector ·
 * top bar · start-up.
 */

// ───────────────────────────── helpers ─────────────────────────────

const $ = (sel) => document.querySelector(sel);
const NS = 1e9;            // nanoseconds per second
const MAIN = 'main';       // id of the main timeline
const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const round2 = (v) => Math.round(v * 100) / 100;
// Short random id. (crypto.randomUUID needs HTTPS or localhost; the Producer
// may be opened over plain HTTP from another machine.)
const uid = () => Math.random().toString(36).slice(2, 10);

/** Create an element: el('div', {class:'x', onclick: fn}, child, 'text', …). */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
    else if (k === 'dataset') Object.assign(node.dataset, v);
    else if (v === true) node.setAttribute(k, '');
    else if (v !== false && v != null) node.setAttribute(k, v);
  }
  for (const c of children) if (c != null) node.append(c);
  return node;
}

// Colours in the show are linear-light RGBA 0..1 (what the renderer computes
// in); the browser's colour picker and canvas use sRGB-encoded values.
const lin2srgb = (c) => (c <= 0.0031308 ? 12.92 * c : 1.055 * Math.pow(c, 1 / 2.4) - 0.055);
const srgb2lin = (c) => (c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4));
const to255 = (c) => Math.round(clamp(lin2srgb(c), 0, 1) * 255);
const rgbaCss = ([r, g, b, a = 1]) => `rgba(${to255(r)},${to255(g)},${to255(b)},${a})`;
const rgbaToHex = ([r, g, b]) => '#' + [r, g, b].map((c) => to255(c).toString(16).padStart(2, '0')).join('');
const hexToRgb = (hex) => [1, 3, 5].map((i) => srgb2lin(parseInt(hex.slice(i, i + 2), 16) / 255));

let toastTimer = 0;
function toast(message, info = false) {
  const t = $('#toast');
  t.textContent = message;
  t.className = info ? 'info' : '';
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), info ? 2500 : 6000);
}

/** "Are you sure?" as a promise. Our own dialog rather than confirm(): an
 *  embedded web view (the stand-alone app) does not always provide one. */
function ask(message) {
  return new Promise((resolve) => {
    $('#modal-text').textContent = message;
    $('#modal').hidden = false;
    const done = (answer) => { $('#modal').hidden = true; resolve(answer); };
    $('#modal-yes').onclick = () => done(true);
    $('#modal-no').onclick = () => done(false);
  });
}

// ───────────────────────────── state ─────────────────────────────

const S = {
  assetsUrl: '',        // base URL of the Asset Manager
  ws: null,
  connected: false,
  clientId: 'producer-' + uid(), // identifies this page's edits when they come back
  editN: 0,             // number of the last document this page sent
  doc: null,            // the show document being edited
  lastSent: '',         // JSON of the document as last sent/received: what the next edit is computed against
  dirty: false,         // there are edits the Director has not received
  holdForeign: false,   // a whole show we sent is on its way: ignore older ones until it is back
  server: null,         // the show as the Director last sent it (the shared truth)
  pending: [],          // our edits sent but not yet seen back: [{ n, ops }]
  replaceNext: false,   // the next send replaces the show for everyone (New, Open)
  deferredShow: false,  // a show arrived during a drag: apply it when the drag ends
  user: loadUser(),     // { name, color }: who this is, for the other users
  others: new Map(),    // node id → presence of the other Producers' users
  sel: null,            // { type: 'cue'|'display'|'control'|'track'|'task', id }
  tl: MAIN,             // the timeline open in the Timeline window
  transports: {},       // timeline id → transport states known, oldest first (≤ 2)
  frameRate: { num: 60, den: 1 },
  clockOffsetNs: null,  // cluster time − performance.now(), null until known
  status: null,         // last cluster status
  library: [],          // items on the Asset Manager
  pxPerSec: 40,         // timeline zoom
  view: { k: null, cx: 0, cy: 0 }, // stage view: screen px per canvas px (null = fit), canvas point at the centre
  onsets: new Map(),    // free-running cue id → cluster ns it went on air (preview)
  previewOn: new Set(), // runners whose output pictures are being shown
  previews: {},         // runner name → [{ name, width, height, png_base64 }]
  logRunner: '',        // runner shown in the log window
  logs: {},             // runner name → log lines
  undo: [],
  redo: [],
};

/** A new, empty show that already puts something on a laptop screen: one
 *  1920×1080 canvas, shown in one half-size window. */
function defaultDoc() {
  return normalize({
    name: 'Untitled',
    stage: {
      width: 1920, height: 1080,
      displays: [{
        id: uid(), name: 'Display 1', runner: '',
        canvas_rect: { x: 0, y: 0, w: 1920, h: 1080 },
        window: { fullscreen: false, position: [60, 60], size: [960, 540] },
      }],
    },
    tracks: [{ id: uid(), name: 'Track 1', cues: [] }],
  });
}

/** Fill in every field this interface reads, so the rest of the code never
 *  has to check. Unknown fields are kept untouched (a newer tool may have
 *  written them). Mirrors the defaults in lc-proto/src/show.rs. */
function normalize(doc) {
  doc.name ??= 'Untitled';
  doc.duration_s ??= 60;
  doc.assets ??= [];
  doc.tracks ??= [];
  doc.control_cues ??= [];
  doc.tasks ??= [];
  const st = (doc.stage ??= {});
  st.width ??= 1920; st.height ??= 1080;
  st.background ??= [0, 0, 0, 1];
  st.displays ??= [];
  for (const d of st.displays) {
    d.id ||= uid();
    d.name ??= ''; d.runner ??= '';
    d.canvas_rect = Object.assign({ x: 0, y: 0, w: 1920, h: 1080 }, d.canvas_rect);
    d.window = Object.assign({ fullscreen: false, monitor: null, position: null, size: null }, d.window);
    d.rotation_degrees ??= 0;
    d.warp_corners ??= null;
    d.blend = Object.assign({ left: 0, right: 0, top: 0, bottom: 0, gamma: 2.2 }, d.blend);
  }
  for (const task of doc.tasks) {
    task.id ||= uid(); task.name ??= ''; task.duration_s ??= 30;
    task.tracks ??= []; task.control_cues ??= [];
  }
  // Every timeline — the document itself is the main one.
  for (const tl of [doc, ...doc.tasks]) {
    for (const cc of tl.control_cues) {
      cc.id ||= uid();
      Object.assign(cc, Object.assign({ name: '', at_s: 0, action: 'pause', target: '', to_s: 0 }, cc));
    }
    for (const t of tl.tracks) {
      t.id ||= uid(); t.name ??= ''; t.cues ??= [];
      // Free-run used to be a track setting; it is now per cue.
      // (A free-running track always looped, so its cues loop too.)
      if (t.free_run) for (const c of t.cues) { c.free_run = true; c.loop = true; }
      t.free_run = false;
      for (const c of t.cues) {
        c.id ||= uid();
        Object.assign(c, Object.assign({
          name: '', source: { type: 'color', rgba: [0.5, 0.5, 0.5, 1] },
          start_s: 0, duration_s: 10, in_s: 0, loop: false, free_run: false,
          x: 0, y: 0, w: 640, h: 360, rotation: 0, opacity: 1, fade_in_s: 0, fade_out_s: 0,
        }, c));
      }
    }
  }
  return doc;
}

// A "timeline object" is anything with tracks, duration_s and control_cues:
// the document (main timeline) or one of its tasks.
const tlObj = (id) => (id === MAIN ? S.doc : S.doc.tasks.find((t) => t.id === id)) || null;
const tlName = (id) => (id === MAIN ? 'Main' : tlObj(id)?.name || 'Task');
/** All timeline ids, front-most first (tasks render in front of main). */
const tlIds = () => [...S.doc.tasks.map((t) => t.id), MAIN];

function findCue(id) {
  for (const tl of tlIds()) {
    for (const [ti, track] of tlObj(tl).tracks.entries()) {
      const ci = track.cues.findIndex((c) => c.id === id);
      if (ci >= 0) return { cue: track.cues[ci], track, ti, ci, tl };
    }
  }
  return null;
}
function findTrack(id) {
  for (const tl of tlIds()) {
    const ti = tlObj(tl).tracks.findIndex((t) => t.id === id);
    if (ti >= 0) return { track: tlObj(tl).tracks[ti], ti, tl };
  }
  return null;
}
function findControl(id) {
  for (const tl of tlIds()) {
    const list = tlObj(tl).control_cues, i = list.findIndex((c) => c.id === id);
    if (i >= 0) return { cc: list[i], list, i, tl };
  }
  return null;
}
const findDisplay = (id) => S.doc.stage.displays.find((d) => d.id === id) || null;
const assetOf = (cue) => (cue.source.type === 'asset' ? S.doc.assets.find((a) => a.id === cue.source.asset) : null);
const selectedCue = () => (S.sel?.type === 'cue' ? findCue(S.sel.id)?.cue : null) || null;

/** Does the current selection still exist? */
function selectionExists() {
  if (!S.sel) return true;
  const { type, id } = S.sel;
  return !!({ cue: findCue, display: findDisplay, control: findControl, track: findTrack, task: (i) => tlObj(i) }[type]?.(id));
}

// ───────────────────────────── window manager ─────────────────────────────
// Every panel is a floating window: drag its title bar to move it, its
// bottom-right corner to resize it, the ✕ to hide it (the "Windows" menu
// brings it back). The layout is remembered in this browser.

const LAYOUT_KEY = 'lcplay.layout.v2';
const desktop = $('#desktop');
const wins = [...document.querySelectorAll('.win')];
let layout = {};   // window id → { x, y, w, h, visible, z }
let zTop = 10;

/** The layout a new user starts with: stage in the middle, assets left,
 *  inspector right, timeline along the bottom, tasks and runners beside it. */
function defaultLayout() {
  const W = desktop.clientWidth, H = desktop.clientHeight;
  const top = Math.round(H * 0.57), side = 250, insp = 300, right = Math.min(400, Math.round(W * 0.3));
  return {
    'win-assets':    { x: 0, y: 0, w: side, h: top },
    'win-stage':     { x: side, y: 0, w: W - side - insp, h: top },
    'win-inspector': { x: W - insp, y: 0, w: insp, h: top },
    'win-timeline':  { x: 0, y: top, w: W - right, h: H - top },
    'win-tasks':     { x: W - right, y: top, w: right, h: Math.round((H - top) * 0.4) },
    'win-runners':   { x: W - right, y: top + Math.round((H - top) * 0.4), w: right, h: H - top - Math.round((H - top) * 0.4) },
    'win-log':       { x: Math.round(W * 0.25), y: Math.round(H * 0.15), w: Math.round(W * 0.5), h: Math.round(H * 0.5), visible: false },
  };
}

function applyLayout() {
  const W = desktop.clientWidth, H = desktop.clientHeight;
  for (const win of wins) {
    const r = layout[win.id];
    // Keep every window reachable: inside the desktop, title bar visible.
    r.w = clamp(r.w, 180, Math.max(180, W)); r.h = clamp(r.h, 90, Math.max(90, H));
    r.x = clamp(r.x, 0, Math.max(0, W - 60)); r.y = clamp(r.y, 0, Math.max(0, H - 30));
    Object.assign(win.style, { left: r.x + 'px', top: r.y + 'px', width: r.w + 'px', height: r.h + 'px', zIndex: r.z });
    win.hidden = !r.visible;
  }
  renderWindowMenu();
}

function saveLayout() {
  try { localStorage.setItem(LAYOUT_KEY, JSON.stringify(layout)); } catch { /* private mode: not remembered */ }
}

function loadLayout(reset = false) {
  let saved = {};
  if (!reset) { try { saved = JSON.parse(localStorage.getItem(LAYOUT_KEY)) || {}; } catch { /* corrupt: start over */ } }
  const def = defaultLayout();
  layout = {};
  wins.forEach((win, i) => { layout[win.id] = Object.assign({ visible: true, z: 10 + i }, def[win.id], saved[win.id]); });
  zTop = Math.max(...Object.values(layout).map((r) => r.z));
  applyLayout();
}

function focusWindow(win) {
  layout[win.id].z = ++zTop;
  win.style.zIndex = zTop;
  wins.forEach((w) => w.classList.toggle('front', w === win));
}

function renderWindowMenu() {
  const list = $('#win-menu-list');
  list.replaceChildren();
  for (const win of wins) {
    const box = el('input', { type: 'checkbox' });
    box.checked = layout[win.id].visible;
    box.onchange = () => {
      layout[win.id].visible = box.checked;
      if (box.checked) focusWindow(win);
      applyLayout(); saveLayout(); refreshAll();
    };
    list.append(el('label', {}, box, ' ' + win.dataset.title));
  }
  list.append(el('button', { onclick: () => { loadLayout(true); saveLayout(); refreshAll(); } }, 'Reset layout'));
}

let winDrag = null; // { win, kind: 'move'|'size', mx, my, orig }
for (const win of wins) {
  win.addEventListener('mousedown', () => focusWindow(win), true);
  win.querySelector('.win-head').addEventListener('mousedown', (e) => {
    if (e.target.closest('button, input, select, .tab')) return; // a control in the title bar
    winDrag = { win, kind: 'move', mx: e.clientX, my: e.clientY, orig: { ...layout[win.id] } };
    e.preventDefault();
  });
  win.querySelector('.win-grip').addEventListener('mousedown', (e) => {
    winDrag = { win, kind: 'size', mx: e.clientX, my: e.clientY, orig: { ...layout[win.id] } };
    e.preventDefault();
  });
  win.querySelector('.win-close').addEventListener('click', () => {
    layout[win.id].visible = false;
    applyLayout(); saveLayout();
  });
}
function windowDragMove(e) {
  const r = layout[winDrag.win.id], o = winDrag.orig, dx = e.clientX - winDrag.mx, dy = e.clientY - winDrag.my;
  if (winDrag.kind === 'move') { r.x = o.x + dx; r.y = o.y + dy; } else { r.w = o.w + dx; r.h = o.h + dy; }
  applyLayout();
}
window.addEventListener('resize', applyLayout);

// ───────────────────────────── users ─────────────────────────────
// Several people can edit the same show at once, each in their own
// Producer page (on this computer or any other that can reach the Producer's
// address). Each has a name and a colour; what they have selected is
// outlined in their colour on the others' screens.

// (A function declaration, not a constant: `S` calls it before this line.)
function userKey() { return 'lcplay.user'; }
function loadUser() {
  let u = null;
  try { u = JSON.parse(localStorage.getItem(userKey())); } catch { /* none yet */ }
  const palette = ['#ff6b6b', '#ffa94d', '#ffd43b', '#69db7c', '#38d9a9', '#4dabf7', '#9775fa', '#f783ac'];
  if (!u?.name) u = { name: `User ${Math.floor(Math.random() * 90 + 10)}`, color: palette[Math.floor(Math.random() * palette.length)] };
  return u;
}
function saveUser() {
  try { localStorage.setItem(userKey(), JSON.stringify(S.user)); } catch { /* not remembered */ }
}

/** Tell the other Producers who we are and what we are working on. */
function sendPresence() {
  send({ type: 'presence', name: S.user.name, color: S.user.color, selection: S.sel, dragging: !!(drag?.undone || tlDrag?.undone) });
}

/** Other users who have `sel` (or the object `type`/`id`) selected. */
function othersOn(type, id) {
  return [...S.others.values()].filter((o) => o.selection?.type === type && o.selection?.id === id);
}

function renderUsers() {
  const box = $('#users');
  if (!box) return;
  const me = el('span', { class: 'user me', style: `--c:${S.user.color}`, title: 'You — click to change your name' }, S.user.name);
  me.onclick = () => {
    const name = prompt('Your name, as the other users see it:', S.user.name);
    if (name?.trim()) { S.user.name = name.trim().slice(0, 40); saveUser(); renderUsers(); sendPresence(); }
  };
  box.replaceChildren(me, ...[...S.others.values()].map((o) =>
    el('span', { class: 'user', style: `--c:${o.color}`, title: o.selection ? `${o.name}: ${o.dragging ? 'moving' : 'has selected'} a ${o.selection.type}` : o.name }, o.name)));
}

/** Outline what the others have selected in the timeline (the stage draws
 *  its own outlines every frame). */
function markOthers() {
  document.querySelectorAll('#tracks .cue').forEach((n) => {
    const o = othersOn('cue', n.dataset.id)[0];
    n.style.boxShadow = o ? `0 0 0 2px ${o.color}` : '';
    n.title = o ? `${n.title.split(' — ')[0]} — ${o.name}${o.dragging ? ' is moving it' : ' has it selected'}` : n.title.split(' — ')[0];
  });
}

// ───────────────────────────── shared editing ─────────────────────────────
// Edits travel as small operations, not whole shows (lc-proto/src/edit.rs
// has the same rules, and the Director applies them): objects in lists are
// addressed by id, so two people's simultaneous edits both survive. The
// Director's copy is the truth; this page shows it with its own edits that
// are still on their way applied on top.

/** Lists whose elements are addressed by id (as in edit.rs). */
const KEYED = new Set(['tracks', 'cues', 'displays', 'tasks', 'control_cues', 'assets']);
const isObj = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const keyedList = (list) => list.every((e) => isObj(e) && typeof e.id === 'string' && e.id);

/** The operations that turn document `a` into document `b`. */
function diffDoc(a, b, path = [], out = []) {
  if (a === b) return out;
  if (isObj(a) && isObj(b)) {
    for (const k of Object.keys(b)) {
      if (b[k] === undefined) continue;
      if (a[k] === undefined) out.push({ op: 'set', path: [...path, k], value: clone(b[k]) });
      else diffDoc(a[k], b[k], [...path, k], out);
    }
    for (const k of Object.keys(a)) if (a[k] !== undefined && b[k] === undefined) out.push({ op: 'unset', path: [...path, k] });
    return out;
  }
  const last = path[path.length - 1];
  if (Array.isArray(a) && Array.isArray(b) && KEYED.has(last) && keyedList(a) && keyedList(b)) {
    const inB = new Set(b.map((e) => e.id)), byId = new Map(a.map((e) => [e.id, e]));
    for (const e of a) if (!inB.has(e.id)) out.push({ op: 'remove', path: [...path, { id: e.id }] });
    // Replay b's order on a copy of what survives, inserting and moving as
    // needed: the fewest operations for the usual edits (one added, one
    // dragged up or down).
    const sim = a.filter((e) => inB.has(e.id)).map((e) => e.id);
    let prev = null;
    for (const e of b) {
      const at = sim.indexOf(e.id);
      const want = prev === null ? 0 : sim.indexOf(prev) + 1;
      if (at < 0) {
        out.push({ op: 'insert', path, after: prev, value: clone(e) });
        sim.splice(want, 0, e.id);
      } else if (at !== want) {
        out.push({ op: 'move', path: [...path, { id: e.id }], after: prev });
        sim.splice(at, 1);
        sim.splice(prev === null ? 0 : sim.indexOf(prev) + 1, 0, e.id);
      }
      prev = e.id;
    }
    for (const e of b) if (byId.has(e.id)) diffDoc(byId.get(e.id), e, [...path, { id: e.id }], out);
    return out;
  }
  if (JSON.stringify(a) !== JSON.stringify(b)) out.push({ op: 'set', path, value: clone(b) });
  return out;
}

/** Apply operations to `doc` in place, skipping those whose target is gone
 *  (the same rules as the Director's). Returns the document. */
function applyOps(doc, ops) {
  const find = (list, id) => (Array.isArray(list) ? list.findIndex((e) => isObj(e) && e.id === id) : -1);
  const walk = (path, create) => {
    let here = doc;
    for (let n = 0; n < path.length; n++) {
      const step = path[n];
      if (typeof step === 'string') {
        if (!isObj(here)) return undefined;
        if (create && here[step] === undefined) {
          const next = n + 1 < path.length ? path[n + 1] : create;
          here[step] = typeof next === 'string' ? {} : [];
        }
        here = here[step];
      } else {
        const i = find(here, step.id);
        if (i < 0) return undefined;
        here = here[i];
      }
      if (here === undefined) return undefined;
    }
    return here;
  };
  const placeAfter = (list, after) => (after == null ? 0 : (find(list, after) < 0 ? list.length : find(list, after) + 1));
  for (const op of ops) {
    const path = op.path || [], last = path[path.length - 1], parent = path.slice(0, -1);
    if (op.op === 'set') {
      if (!path.length) { doc = clone(op.value); continue; }
      const p = walk(parent, last);
      if (typeof last === 'string' && isObj(p)) p[last] = clone(op.value);
      else if (last && typeof last === 'object') { const i = find(p, last.id); if (i >= 0) p[i] = clone(op.value); }
    } else if (op.op === 'unset') {
      const p = walk(parent);
      if (isObj(p)) delete p[last];
    } else if (op.op === 'insert') {
      let list = walk(path);
      if (list === undefined && typeof last === 'string') { const p = walk(parent, last); if (isObj(p)) list = p[last] = []; }
      if (!Array.isArray(list) || find(list, op.value?.id) >= 0) continue;
      list.splice(placeAfter(list, op.after), 0, clone(op.value));
    } else if (op.op === 'remove' || op.op === 'move') {
      const list = walk(parent), i = last && typeof last === 'object' ? find(list, last.id) : -1;
      if (i < 0) continue;
      const [e] = list.splice(i, 1);
      if (op.op === 'move') list.splice(placeAfter(list, op.after), 0, e);
    }
  }
  return doc;
}

/** Show the Director's copy with our pending edits on top. Called when a
 *  show arrives, and after a drag that held one back. */
function rebase() {
  if (!S.server) return;
  // Local changes not yet turned into an edit go first, so they are kept.
  if (S.doc && JSON.stringify(S.doc) !== S.lastSent) sendDoc(true);
  let doc = clone(S.server);
  for (const p of S.pending) doc = applyOps(doc, p.ops);
  const next = normalize(doc);
  const changed = JSON.stringify(next) !== JSON.stringify(S.doc);
  S.doc = next;
  S.lastSent = JSON.stringify(S.doc);
  S.deferredShow = false;
  if (!changed) return;
  refreshAll();
  markOthers();
  reconcileAssets();
}

// ───────────────────────────── network ─────────────────────────────

function connect(url, protocol) {
  const ws = new WebSocket(url);
  S.ws = ws;
  ws.onopen = () => {
    S.connected = true;
    ws.send(JSON.stringify({ type: 'hello', protocol, node_id: S.clientId, role: 'producer', name: S.user.name }));
    sendPresence();
  };
  ws.onmessage = (ev) => onMessage(JSON.parse(ev.data));
  ws.onclose = () => {
    S.connected = false;
    S.status = null;
    renderCluster(); renderRunners();
    // The Director may simply be restarting: keep trying.
    setTimeout(() => connect(url, protocol), 1000);
  };
}

function send(message) {
  if (S.ws?.readyState === WebSocket.OPEN) S.ws.send(JSON.stringify(message));
}
/** Send a command; `tl` names the timeline a transport command is for. */
const command = (cmd, tl = MAIN) => send({ type: 'command', command: cmd, timeline: tl === MAIN ? null : tl });

function onMessage(msg) {
  switch (msg.type) {
    case 'welcome':
      S.frameRate = msg.frame_rate;
      // A new connection may be to a restarted Director, whose transport
      // epochs and clock are unrelated to the old ones: start afresh.
      S.transports = {};
      S.clockOffsetNs = null;
      pushTransport(msg.transport, MAIN);
      for (const [id, state] of Object.entries(msg.tasks || {})) pushTransport(state, id);
      command({ type: 'status' }); // gives us the cluster clock right away
      if (S.doc) {
        // Reconnected while holding a show. If we have edits the Director
        // never received, or it came back with no show at all (restarted
        // without saved state), ours is the one to keep: send it, and do not
        // let the Director's older copy overwrite it in the meantime.
        if (msg.show_revision == null) {
          S.replaceNext = true; // it has nothing: ours becomes the show
          sendDoc(true);
        } else if (S.dirty) {
          sendDoc(true); // our edits, as operations, on top of its show
        }
      } else if (msg.show_revision == null) {
        // First contact with a Director that has no show: start it off with
        // a usable one.
        S.doc = defaultDoc();
        sendDoc(true);
        refreshAll();
      }
      break;
    case 'transport':
      pushTransport(msg.state, msg.timeline || MAIN);
      break;
    case 'show': {
      // Every show the Director sends is the shared truth, whoever changed
      // it. Our own edits come back tagged ({by, n}): those that have
      // arrived are no longer pending. The rest are re-applied on top.
      const tag = msg.document._edit;
      delete msg.document._edit;
      if (S.holdForeign && tag?.by !== S.clientId) break; // a whole show of ours is on its way
      if (tag?.by === S.clientId) {
        S.pending = S.pending.filter((p) => p.n > tag.n);
        if (tag.n >= S.editN) S.holdForeign = false;
      }
      S.server = msg.document;
      // Mid-drag the dragged object must not jump: apply after the drag.
      if (drag?.undone || tlDrag?.undone) { S.deferredShow = true; break; }
      if (!S.doc) { S.doc = normalize(clone(S.server)); S.lastSent = JSON.stringify(S.doc); refreshAll(); reconcileAssets(); break; }
      rebase();
      break;
    }
    case 'presence':
      if (msg.node_id === S.clientId) break; // our own, relayed back
      if (msg.presence) S.others.set(msg.node_id, msg.presence);
      else S.others.delete(msg.node_id);
      renderUsers(); markOthers();
      if (S.sel) {
        const key = JSON.stringify(othersOn(S.sel.type, S.sel.id).map((o) => [o.name, o.dragging]));
        if (key !== S.selOthersKey) renderInspector();
      }
      break;
    case 'cluster':
      S.status = msg;
      for (const id of S.others.keys()) {
        if (!(msg.nodes || []).some((n) => n.node_id === id)) { S.others.delete(id); renderUsers(); markOthers(); }
      }
      // Our estimate of the cluster clock: accurate to the network delay of
      // this message, which is ample for moving a playhead on screen.
      S.clockOffsetNs = msg.cluster_ns - performance.now() * 1e6;
      pushTransport(msg.transport, MAIN);
      for (const [id, state] of Object.entries(msg.tasks || {})) pushTransport(state, id);
      renderCluster(); renderRunners();
      break;
    case 'runner_preview':
      S.previews[msg.name] = msg.outputs;
      showPreviews(msg.name);
      break;
    case 'runner_logs':
      S.logs[msg.name] = msg.lines;
      if (msg.name === S.logRunner) renderLog();
      break;
    case 'error':
      // If it was our document that was refused, no echo will come.
      S.holdForeign = false;
      if (msg.message.startsWith('edit_show')) {
        // Refused (it would have made the show invalid): back to the shared
        // show, without our unconfirmed edits.
        S.pending = [];
        if (S.server) { S.doc = null; S.doc = normalize(clone(S.server)); S.lastSent = JSON.stringify(S.doc); refreshAll(); }
      }
      toast(msg.message);
      break;
  }
}

// ───────────────────────────── time ─────────────────────────────
// The same arithmetic a Runner does (lc-proto/src/transport.rs), in floating
// point — good to well under a frame, which is all a UI needs.

function pushTransport(state, tl) {
  const list = (S.transports[tl] ??= []);
  const newest = list[list.length - 1];
  if (newest && state.epoch <= newest.epoch) return;
  list.push(state);
  if (list.length > 2) list.shift();
}

const clusterNow = () => (S.clockOffsetNs == null ? null : performance.now() * 1e6 + S.clockOffsetNs);

/** The transport state of timeline `tl` in force now: the newest whose
 *  anchor has passed. Null for a task that was never started. */
function currentTransport(tl = MAIN) {
  const list = S.transports[tl];
  if (!list?.length) return null;
  const now = clusterNow();
  for (let i = list.length - 1; i >= 0; i--) if (now == null || list[i].anchor_cluster_ns <= now) return list[i];
  return list[0];
}

/** Is the timeline on stage? The main one always is; a task only between
 *  play and stop. */
const isActive = (tl) => tl === MAIN || !!currentTransport(tl)?.active;

/** Timeline time in seconds, now. */
function showSeconds(tl = MAIN) {
  const t = currentTransport(tl);
  if (!t) return 0;
  const now = clusterNow();
  const elapsed = t.playing && now != null ? Math.max(0, now - t.anchor_cluster_ns) : 0;
  let ns = t.anchor_show_ns + (elapsed * t.rate_permille) / 1000;
  ns = t.loop_ns > 0 ? ((ns % t.loop_ns) + t.loop_ns) % t.loop_ns : Math.max(0, ns);
  return ns / NS;
}

const seek = (seconds, tl = MAIN) => command({ type: 'seek', show_ns: Math.round(Math.max(0, seconds) * NS) }, tl);

// ───────────────────────────── editing ─────────────────────────────

/** Make a change to the show: undo point, apply, redraw, send. */
function edit(change, { inspector = true } = {}) {
  const before = clone(S.doc);
  change();
  record(before);
  afterChange(inspector);
}

/** Add an undo step for the change from `before` to the current show. The
 *  step holds operations, not a copy of the show: undoing reverts only what
 *  *this user* changed, never what others did in the meantime. */
function record(before) {
  const fwd = diffDoc(before, S.doc);
  if (!fwd.length) return;
  S.undo.push({ back: diffDoc(S.doc, before), fwd });
  if (S.undo.length > 200) S.undo.shift();
  S.redo = [];
}

function afterChange(inspector = true) {
  if (!tlObj(S.tl)) S.tl = MAIN;      // the open timeline was deleted
  if (!selectionExists()) S.sel = null;
  renderTimeline(); renderTasks(); renderRunners(true);
  if (inspector) renderInspector();
  scheduleSend();
}

let sendTimer = 0;
/** Send soon, but not on every mouse-move: at most ~7 documents a second
 *  while dragging, which the outputs follow live. */
function scheduleSend() {
  if (!sendTimer) sendTimer = setTimeout(sendDoc, 150);
}
function sendDoc(force = false) {
  clearTimeout(sendTimer);
  sendTimer = 0;
  delete S.doc._edit; // e.g. from an opened file; the tag is added per send
  syncStageSize();
  const json = JSON.stringify(S.doc);
  if (!force && !S.dirty && json === S.lastSent) return;
  if (S.ws?.readyState !== WebSocket.OPEN) {
    // Not lost: kept here and sent as soon as the Director is back (see the
    // 'welcome' handler). Say so once.
    if (!S.dirty) toast('Director offline — edits are kept here and sent when it is back.', true);
    S.dirty = true;
    return;
  }
  const tag = { by: S.clientId, n: ++S.editN };
  if (S.replaceNext || !S.server || !S.lastSent) {
    // A whole new show (New, Open, or the Director has none): replaces the
    // show for everyone.
    S.replaceNext = false;
    S.pending = [];
    S.holdForeign = true;
    command({ type: 'load_show', document: { ...S.doc, _edit: tag } });
  } else {
    // Only what changed since the last send, as operations.
    const ops = diffDoc(JSON.parse(S.lastSent), S.doc);
    if (!ops.length) { S.dirty = false; return; }
    S.pending.push({ n: tag.n, ops });
    command({ type: 'edit_show', ops, tag });
  }
  S.lastSent = json;
  S.dirty = false;
}

function stepHistory(from, to) {
  if (!from.length) return;
  const step = from.pop();
  S.doc = normalize(applyOps(S.doc, from === S.undo ? step.back : step.fwd));
  to.push(step);
  refreshAll();
  scheduleSend();
}

function refreshAll() {
  if (!S.doc) return;
  if (!tlObj(S.tl)) S.tl = MAIN;
  if (!selectionExists()) S.sel = null;
  renderTimeline(); renderTasks(); renderRunners(true); renderInspector();
}

function select(sel) {
  S.sel = sel;
  renderTimeline(); renderTasks(); renderRunners(true); renderInspector();
  markOthers();
  sendPresence();
}

async function deleteSelection() {
  if (!S.sel) return;
  const { type, id } = S.sel;
  // A whole timeline is too much to lose to a stray Delete key.
  if (type === 'task' && !(await ask(`Delete the task "${tlName(id)}" with everything on it?`))) return;
  edit(() => {
    if (type === 'cue') { const hit = findCue(id); if (hit) hit.track.cues.splice(hit.ci, 1); }
    else if (type === 'control') { const hit = findControl(id); if (hit) hit.list.splice(hit.i, 1); }
    else if (type === 'track') { const hit = findTrack(id); if (hit) tlObj(hit.tl).tracks.splice(hit.ti, 1); }
    else if (type === 'task') {
      S.doc.tasks = S.doc.tasks.filter((t) => t.id !== id);
      // Control cues that acted on the task have nothing left to act on (and
      // the Director would refuse a show that still refers to it).
      for (const tl of tlIds()) { const o = tlObj(tl); o.control_cues = o.control_cues.filter((cc) => cc.target !== id); }
    }
    else if (type === 'display') S.doc.stage.displays = S.doc.stage.displays.filter((d) => d.id !== id);
    S.sel = null;
  });
}

/** Add a cue to track `ti` of timeline `tl` at time `t`, select it, and
 *  grow the timeline if the cue ends after it. */
function addCue(cue, tl, ti, t) {
  edit(() => {
    const obj = tlObj(tl);
    if (!obj.tracks.length) obj.tracks.push({ id: uid(), name: 'Track 1', cues: [] });
    const track = obj.tracks[clamp(ti, 0, obj.tracks.length - 1)];
    cue.id = uid();
    cue.start_s = round2(Math.max(0, t));
    track.cues.push(cue);
    obj.duration_s = Math.max(obj.duration_s, Math.ceil(cue.start_s + cue.duration_s));
    normalize(S.doc);
    S.sel = { type: 'cue', id: cue.id };
  });
  syncLoop(); // the main timeline may have grown
}

/** The area the displays cover on the canvas (the canvas itself is
 *  unbounded). With no displays: the cues, or a plain HD frame. */
function extent() {
  const rects = S.doc.stage.displays.map((d) => d.canvas_rect);
  if (!rects.length) for (const tl of tlIds()) for (const t of tlObj(tl).tracks) rects.push(...t.cues);
  if (!rects.length) return { x: 0, y: 0, w: 1920, h: 1080 };
  const x = Math.min(...rects.map((r) => r.x)), y = Math.min(...rects.map((r) => r.y));
  const r = Math.max(...rects.map((r) => r.x + r.w)), b = Math.max(...rects.map((r) => r.y + r.h));
  return { x, y, w: Math.max(1, r - x), h: Math.max(1, b - y) };
}

/** The document still carries a stage size (older Runners read it). It is
 *  no longer edited: it simply follows the area the displays cover. */
function syncStageSize() {
  const e = extent();
  S.doc.stage.width = Math.max(1, Math.ceil(e.x + e.w));
  S.doc.stage.height = Math.max(1, Math.ceil(e.y + e.h));
}

/** Largest rectangle with the media's aspect that fits the displays' area,
 *  centred in it. */
function fitRect(mediaW, mediaH, enlarge) {
  const a = extent();
  const k = Math.min(a.w / mediaW, a.h / mediaH, enlarge ? Infinity : 1);
  const w = Math.round(mediaW * k), h = Math.round(mediaH * k);
  return { x: Math.round(a.x + (a.w - w) / 2), y: Math.round(a.y + (a.h - h) / 2), w, h };
}

function addAssetCue(item, tl, ti, t) {
  const kind = item.kind; // 'video' | 'image'
  // Record the asset in the show, with the URL Runners will fetch it from.
  const entry = {
    id: item.id, name: item.name, kind,
    url: `${S.assetsUrl}/stream/${item.id}`,
    size: item.size, width: item.width || 0, height: item.height || 0,
    duration_s: item.duration_s || 0,
  };
  const cue = {
    name: item.name, source: { type: 'asset', asset: item.id },
    duration_s: kind === 'video' && item.duration_s > 0 ? round2(item.duration_s) : 10,
    ...fitRect(item.width || 1920, item.height || 1080, false),
  };
  // Upsert before the cue is added: validation requires the asset to exist.
  S.doc.assets = S.doc.assets.filter((a) => a.id !== item.id).concat(entry);
  addCue(cue, tl, ti, t);
}

function addTask() {
  edit(() => {
    const task = { id: 'task-' + uid(), name: `Task ${S.doc.tasks.length + 1}`, duration_s: 30, tracks: [{ id: uid(), name: 'Track 1', cues: [] }], control_cues: [] };
    S.doc.tasks.push(task);
    normalize(S.doc);
    S.tl = task.id;
    S.sel = { type: 'task', id: task.id };
  });
}

// ───────────────────────────── assets ─────────────────────────────

async function loadLibrary() {
  try {
    const res = await fetch(`${S.assetsUrl}/api/library`);
    const { items } = await res.json();
    // Audio-only files are listed by the Asset Manager but cannot be cued yet.
    const usable = items.filter((i) => i.kind === 'video' || i.kind === 'image');
    if (JSON.stringify(usable) !== JSON.stringify(S.library)) {
      S.library = usable;
      renderAssets();
    }
    reconcileAssets();
  } catch {
    if (S.library !== null) renderAssets('Asset Manager not reachable at ' + S.assetsUrl);
    S.library = null;
  }
}

/** A file replaced on the Asset Manager keeps its id (ids come from the
 *  name) but not its size, dimensions or duration. The show records those —
 *  the size is part of every Runner's cache key — so bring the show's copy up
 *  to date; Runners then fetch the new file. */
function reconcileAssets() {
  if (!S.doc || !S.library) return;
  const facts = (x) => [x.size, x.width || 0, x.height || 0, x.duration_s || 0].join();
  const stale = S.doc.assets
    .map((a) => [a, S.library.find((i) => i.id === a.id)])
    .filter(([a, item]) => item && facts(item) !== facts(a));
  if (!stale.length) return;
  edit(() => {
    for (const [a, item] of stale) {
      Object.assign(a, { size: item.size, width: item.width || 0, height: item.height || 0, duration_s: item.duration_s || 0 });
    }
  });
  toast(`${stale.map(([a]) => a.name).join(', ')}: file changed on the Asset Manager — show updated.`, true);
}

function renderAssets(problem) {
  const list = $('#asset-list');
  list.replaceChildren();
  if (problem) return list.append(el('div', { class: 'empty' }, problem));
  if (!S.library?.length) return list.append(el('div', { class: 'empty' }, 'No media yet. Upload files, or drop them here.'));
  for (const item of S.library) {
    const meta = [item.width ? `${item.width}×${item.height}` : item.kind,
      item.duration_s ? `${item.duration_s.toFixed(1)} s` : null].filter(Boolean).join(' · ');
    list.append(el('div', {
      class: 'asset', draggable: 'true', title: item.name,
      ondragstart: (e) => e.dataTransfer.setData('text/lcplay-asset', item.id),
      ondblclick: () => addAssetCue(item, S.tl, 0, showSeconds(S.tl)),
    },
      // `?v=size`: a replaced file gets a new URL, so no stale cached thumbnail.
      el('img', { src: `${S.assetsUrl}/thumb/${item.id}?v=${item.size}`, loading: 'lazy', draggable: 'false' }),
      el('div', {}, el('div', { class: 'name' }, item.name), el('div', { class: 'meta' }, meta)),
      el('button', { class: 'danger', title: 'Delete from the Asset Manager', onclick: (e) => { e.stopPropagation(); deleteAsset(item); } }, '✕'),
    ));
  }
}

async function deleteAsset(item) {
  const used = tlIds().some((tl) => tlObj(tl).tracks.some((t) => t.cues.some((c) => c.source.asset === item.id)));
  if (!(await ask(`Delete "${item.name}" from the Asset Manager?` + (used ? '\n\nIt is used in this show; those cues will show a placeholder.' : '')))) return;
  try {
    const res = await fetch(`${S.assetsUrl}/api/items/${item.id}`, { method: 'DELETE' });
    if (!res.ok) toast(`Could not delete ${item.name}: ${await res.text()}`);
  } catch {
    toast(`Could not delete ${item.name}: Asset Manager not reachable`);
  }
  loadLibrary();
}

/** Upload files one after another. XMLHttpRequest rather than fetch because
 *  it reports upload progress, which matters for multi-gigabyte media. */
async function uploadFiles(files) {
  for (const file of files) {
    await new Promise((resolve) => {
      const xhr = new XMLHttpRequest();
      xhr.open('POST', `${S.assetsUrl}/api/upload?name=${encodeURIComponent(file.name)}`);
      xhr.upload.onprogress = (e) => toast(`Uploading ${file.name} — ${Math.round((100 * e.loaded) / e.total)} %`, true);
      xhr.onload = () => {
        if (xhr.status >= 300) toast(`Upload of ${file.name} failed: ${xhr.responseText}`);
        else toast(`Uploaded ${file.name}`, true);
        resolve();
      };
      xhr.onerror = () => { toast(`Upload of ${file.name} failed: Asset Manager not reachable`); resolve(); };
      xhr.send(file);
    });
  }
  loadLibrary();
}

// ───────────────────────────── preview media ─────────────────────────────
// What the stage draws for a cue. This is a PREVIEW of the content, played
// by the browser — not the Runner's picture. It is kept in step with the
// timeline the same way a Runner is: nothing "plays along"; every frame we
// compute where the media should be and correct the element if it is not.
//
//   image cue → the image itself (its thumbnail until it has loaded)
//   video cue → a muted <video>, one per cue, steered to the right time
//
// A video the browser cannot decode (ProRes, HAP, …) falls back to its
// thumbnail; the Runner, which decodes with ffmpeg, still plays it.

const previews = new Map(); // key → { kind, node, thumb, failed }

// Live video in the stage preview can be turned off on this computer: video
// cues then show their still thumbnail and no <video> element exists, so the
// browser decodes nothing. Images are drawn either way (decoded once).
const LIVE_VIDEO_KEY = 'lcplay.liveVideo';
let liveVideo = (() => {
  try { return localStorage.getItem(LIVE_VIDEO_KEY) !== 'off'; } catch { return true; }
})();
const stills = new Map(); // asset id:size → thumbnail <img> (still mode)

function setLiveVideo(on) {
  liveVideo = on;
  try { localStorage.setItem(LIVE_VIDEO_KEY, on ? 'on' : 'off'); } catch { /* not remembered */ }
  if (!on) {
    // Release every video decoder now, not when the previews time out.
    for (const [key, p] of previews) {
      if (p.kind !== 'video') continue;
      p.node.pause();
      p.node.removeAttribute('src');
      p.node.load();
      previews.delete(key);
    }
  }
}

/** The still picture of a video asset (its thumbnail), or null while loading. */
function stillFor(asset) {
  const key = `${asset.id}:${asset.size}`;
  let img = stills.get(key);
  if (!img) {
    img = new Image();
    img.src = asset.url.replace('/stream/', '/thumb/') + `?v=${asset.size}`;
    stills.set(key, img);
  }
  return drawable(img);
}

function previewFor(cue, asset) {
  // One element per video cue (each has its own position); images are
  // shared per file. The size is in the key so a replaced file reloads.
  const key = asset.kind === 'video' ? `v:${cue.id}:${asset.id}:${asset.size}` : `i:${asset.id}:${asset.size}`;
  let p = previews.get(key);
  if (!p) {
    const thumb = new Image();
    thumb.src = asset.url.replace('/stream/', '/thumb/') + `?v=${asset.size}`;
    p = { kind: asset.kind, thumb, failed: false };
    if (asset.kind === 'video') {
      p.node = Object.assign(document.createElement('video'), { muted: true, playsInline: true, preload: 'auto', src: asset.url });
      p.node.addEventListener('error', () => (p.failed = true));
    } else {
      p.node = new Image();
      p.node.onerror = () => (p.failed = true);
      p.node.src = asset.url;
    }
    previews.set(key, p);
  }
  p.used = performance.now();
  return p;
}

/** Steer a preview video to `target` seconds; return something drawable. */
function steerVideo(p, target, playing, rate) {
  const v = p.node;
  if (p.failed || v.readyState < 1) return drawable(p.thumb);
  if (Number.isFinite(v.duration) && v.duration > 0) target = clamp(target, 0, Math.max(0, v.duration - 0.05));
  const off = Math.abs(v.currentTime - target);
  if (playing && rate > 0) {
    // Let it run, and only jump when it has drifted visibly: constant
    // seeking would make it stutter.
    const r = clamp(rate, 0.1, 8);
    if (v.playbackRate !== r) v.playbackRate = r;
    if (v.paused) v.play().catch(() => {});
    if (off > 0.3) v.currentTime = target;
  } else {
    // Paused, or playing backwards (which a <video> cannot do): follow the
    // position by seeking.
    if (!v.paused) v.pause();
    if (off > 0.04 && !v.seeking) v.currentTime = target;
  }
  return v.readyState >= 2 ? v : drawable(p.thumb);
}

const drawable = (img) => (img.complete && img.naturalWidth ? img : null);

/** Pause videos whose cue is off air and forget previews nothing has asked
 *  for in a while (deleted cues, replaced files). */
function tidyPreviews(now) {
  for (const [key, p] of previews) {
    const idle = now - (p.used || 0);
    if (p.kind === 'video' && idle > 300 && !p.node.paused) p.node.pause();
    if (idle > 30000) {
      if (p.kind === 'video') { p.node.removeAttribute('src'); p.node.load(); }
      previews.delete(key);
    }
  }
}

/** The picture for `cue` at timeline time `t` (or null: draw a placeholder). */
function cuePicture(cue, track, t, tl, onAir) {
  const asset = assetOf(cue);
  if (!asset) return null;
  if (asset.kind === 'video' && !liveVideo) return stillFor(asset);
  const p = previewFor(cue, asset);
  if (asset.kind !== 'video') return (!p.failed && drawable(p.node)) || drawable(p.thumb);
  const tr = currentTransport(tl);
  const length = asset.duration_s || p.node.duration || 0;
  if (cue.free_run) {
    // Free-running: real time since the cue went on air, whatever the
    // timeline does; held at the end unless it loops. (The same rule as the
    // Runner's `on_air_since`.)
    if (!onAir) { S.onsets.delete(cue.id); return steerVideo(p, cue.in_s, false, 1); }
    const now = clusterNow() ?? 0;
    if (!S.onsets.has(cue.id)) S.onsets.set(cue.id, tr ? onAirSince(tr, cue.start_s, now) : now);
    let media = cue.in_s + (now - S.onsets.get(cue.id)) / NS;
    if (length > cue.in_s) {
      media = cue.loop ? cue.in_s + ((media - cue.in_s) % (length - cue.in_s)) : Math.min(media, length - 0.05);
    }
    return steerVideo(p, media, true, 1);
  }
  let media = cue.in_s + Math.max(0, t - cue.start_s);
  if (cue.loop && length > cue.in_s) media = cue.in_s + ((media - cue.in_s) % (length - cue.in_s));
  return steerVideo(p, media, onAir && !!tr?.playing, (tr?.rate_permille ?? 1000) / 1000);
}

/** The stage's sketch of a sync test pattern (the Runners draw the real
 *  one): its frame number at timeline time `t`, as the Runner computes it. */
function drawSyncTest(cue, t) {
  const fps = cue.source.fps || 60;
  const frame = Math.floor(Math.max(0, cue.in_s + t - cue.start_s) * fps + 1e-6);
  const flash = frame % Math.max(1, Math.round(fps)) === 0;
  ctx.fillStyle = flash ? '#fff' : '#000';
  ctx.fillRect(-cue.w / 2, -cue.h / 2, cue.w, cue.h);
  ctx.fillStyle = flash ? '#000' : '#fff';
  ctx.font = `bold ${Math.max(8, cue.h / 4)}px ui-monospace, monospace`;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'middle';
  ctx.fillText(String(frame), 0, 0);
  ctx.font = `${Math.max(6, cue.h / 14)}px system-ui, sans-serif`;
  ctx.fillText('SYNC TEST', 0, -cue.h / 3);
}

/** When a cue starting at `startS` went on air under transport `tr`, at
 *  cluster time `now` (ns): when the playhead crossed its start, or when the
 *  state took effect if it did not get there by playing. */
function onAirSince(tr, startS, now) {
  const start = startS * NS;
  if (tr.playing && tr.rate_permille > 0) {
    const toCluster = (show) => (show * 1000) / tr.rate_permille;
    const len = tr.loop_ns > 0 ? tr.loop_ns : 0;
    const ahead = len ? (((start - (tr.anchor_show_ns % len)) % len) + len) % len : start - tr.anchor_show_ns;
    if (ahead >= 0) {
      let crossing = tr.anchor_cluster_ns + toCluster(ahead);
      if (len && crossing <= now) { const lap = toCluster(len); crossing += Math.floor((now - crossing) / lap) * lap; }
      if (crossing <= now) return crossing;
    }
  }
  return Math.min(tr.anchor_cluster_ns, now);
}

// ───────────────────────────── stage ─────────────────────────────
// A 2D-canvas view of the stage: the canvas, the displays cut from it, and
// every cue that is on air on any timeline, with its content. Redrawn every
// animation frame.

const stage = $('#stage');
const ctx = stage.getContext('2d');
let view = { k: 1, ox: 0, oy: 0 };  // canvas px → screen px: s = c·k + o
let displayTabs = [];               // screen rects of display name tabs, for hit-testing
let drag = null;                    // the stage drag in progress
const HANDLE = 5;                   // half-size of a resize handle, screen px

function drawStage() {
  const wrap = $('#stage-wrap');
  const dpr = window.devicePixelRatio || 1;
  const W = wrap.clientWidth, H = wrap.clientHeight;
  if (W < 2 || H < 2) return; // window hidden
  if (stage.width !== Math.round(W * dpr) || stage.height !== Math.round(H * dpr)) { stage.width = Math.round(W * dpr); stage.height = Math.round(H * dpr); }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, W, H);
  if (!S.doc) return;

  const st = S.doc.stage;
  // The whole window is canvas. The view is a scale and the canvas point
  // shown at the window's centre; it is fitted to the displays once (and on
  // "Fit"), then left where the operator puts it.
  if (S.view.k == null) {
    const e = extent();
    S.view = { k: Math.min((W - 48) / e.w, (H - 48) / e.h), cx: e.x + e.w / 2, cy: e.y + e.h / 2 };
  }
  const k = S.view.k;
  view = { k, W, H, ox: W / 2 - S.view.cx * k, oy: H / 2 - S.view.cy * k };
  $('#stage-zoom').textContent = `${Math.round(k * 100)}%`;

  // Background colour everywhere, and a grid in canvas pixels (lines at
  // least ~60 screen px apart) to place things by; the canvas origin is
  // marked more strongly.
  ctx.fillStyle = rgbaCss(st.background);
  ctx.fillRect(0, 0, W, H);
  const step = [10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000, 50000].find((g) => g * k >= 60) || 100000;
  const x0 = Math.floor(-view.ox / k / step) * step, y0 = Math.floor(-view.oy / k / step) * step;
  ctx.lineWidth = 1;
  ctx.strokeStyle = 'rgba(255,255,255,0.06)';
  ctx.beginPath();
  for (let x = x0; x * k + view.ox < W; x += step) { const sx = Math.round(x * k + view.ox) + 0.5; ctx.moveTo(sx, 0); ctx.lineTo(sx, H); }
  for (let y = y0; y * k + view.oy < H; y += step) { const sy = Math.round(y * k + view.oy) + 0.5; ctx.moveTo(0, sy); ctx.lineTo(W, sy); }
  ctx.stroke();
  ctx.strokeStyle = 'rgba(255,255,255,0.18)';
  ctx.beginPath();
  ctx.moveTo(Math.round(view.ox) + 0.5, 0); ctx.lineTo(Math.round(view.ox) + 0.5, H);
  ctx.moveTo(0, Math.round(view.oy) + 0.5); ctx.lineTo(W, Math.round(view.oy) + 0.5);
  ctx.stroke();

  ctx.save();
  ctx.translate(view.ox, view.oy);
  ctx.scale(k, k);

  // Back to front: the main timeline first, then the tasks (the first task
  // is front-most); within a timeline the last track is at the back.
  const sel = selectedCue();
  for (const tl of tlIds().reverse()) {
    const t = showSeconds(tl), active = isActive(tl);
    for (const track of [...tlObj(tl).tracks].reverse()) {
      for (const cue of track.cues) {
        const onAir = active && t >= cue.start_s && t < cue.start_s + cue.duration_s;
        // The selected cue is always shown (ghosted when not on air), so it
        // can be positioned without scrubbing to it first.
        if (!onAir) S.onsets.delete(cue.id); // on air again later: afresh
        const others = othersOn('cue', cue.id);
        if (!onAir && cue !== sel && !others.length) continue;
        const fade = clamp(cue.fade_in_s > 0 ? (t - cue.start_s) / cue.fade_in_s : 1, 0, 1)
                   * clamp(cue.fade_out_s > 0 ? (cue.start_s + cue.duration_s - t) / cue.fade_out_s : 1, 0, 1);
        ctx.save();
        ctx.translate(cue.x + cue.w / 2, cue.y + cue.h / 2);
        ctx.rotate((cue.rotation * Math.PI) / 180);
        ctx.globalAlpha = onAir ? cue.opacity * fade : 0.3;
        const picture = cue.source.type === 'asset' ? cuePicture(cue, track, onAir ? t : cue.start_s, tl, onAir) : null;
        if (picture) ctx.drawImage(picture, -cue.w / 2, -cue.h / 2, cue.w, cue.h);
        else if (cue.source.type === 'sync_test') drawSyncTest(cue, onAir ? t : cue.start_s);
        else {
          ctx.fillStyle = cue.source.type === 'color' ? rgbaCss(cue.source.rgba) : '#444';
          ctx.fillRect(-cue.w / 2, -cue.h / 2, cue.w, cue.h);
        }
        if (cue === sel) {
          ctx.globalAlpha = 1;
          ctx.strokeStyle = '#fff';
          ctx.lineWidth = 2 / k;
          ctx.strokeRect(-cue.w / 2, -cue.h / 2, cue.w, cue.h);
        }
        // Another user's selection: their colour, their name; dashed while
        // they are dragging it.
        others.forEach((o, i) => {
          ctx.globalAlpha = 1;
          ctx.strokeStyle = o.color;
          ctx.lineWidth = 2 / k;
          ctx.setLineDash(o.dragging ? [6 / k, 4 / k] : []);
          const pad = (3 + 3 * i) / k;
          ctx.strokeRect(-cue.w / 2 - pad, -cue.h / 2 - pad, cue.w + 2 * pad, cue.h + 2 * pad);
          ctx.setLineDash([]);
          ctx.font = `${12 / k}px system-ui, sans-serif`;
          ctx.fillStyle = o.color;
          ctx.fillText(o.name, -cue.w / 2, -cue.h / 2 - pad - 4 / k - i * 14 / k);
        });
        ctx.restore();
      }
    }
  }
  ctx.restore();

  // Displays: outlines and name tabs, drawn in screen space so line widths
  // and text stay readable at any zoom.
  displayTabs = [];
  ctx.font = '11px system-ui, sans-serif';
  for (const d of st.displays) {
    const r = d.canvas_rect;
    const x = view.ox + r.x * k, y = view.oy + r.y * k;
    const selected = S.sel?.type === 'display' && S.sel.id === d.id;
    ctx.strokeStyle = selected ? '#fff' : '#4ea1ff';
    ctx.lineWidth = selected ? 2 : 1;
    ctx.setLineDash(selected ? [] : [6, 4]);
    ctx.strokeRect(x + 0.5, y + 0.5, r.w * k, r.h * k);
    ctx.setLineDash([]);
    const label = (d.name || 'display') + (d.runner ? ` @${d.runner}` : '');
    const tw = ctx.measureText(label).width + 10;
    ctx.fillStyle = selected ? '#fff' : '#4ea1ff';
    ctx.fillRect(x, y - 16, tw, 16);
    ctx.fillStyle = '#06121f';
    ctx.fillText(label, x + 5, y - 4);
    displayTabs.push({ id: d.id, x, y: y - 16, w: tw, h: 16 });
    othersOn('display', d.id).forEach((o, i) => {
      const pad = 3 + 3 * i;
      ctx.strokeStyle = o.color;
      ctx.lineWidth = 2;
      ctx.setLineDash(o.dragging ? [6, 4] : []);
      ctx.strokeRect(x - pad, y - pad, r.w * k + 2 * pad, r.h * k + 2 * pad);
      ctx.setLineDash([]);
      ctx.fillStyle = o.color;
      ctx.fillText(o.name, x + tw + 6 + i * 60, y - 4);
    });
  }

  // Resize handles of the selected cue (only unrotated: resizing a rotated
  // box by its corners needs more maths than a first version deserves).
  if (sel && !sel.rotation) {
    ctx.fillStyle = '#fff';
    for (const [hx, hy] of cueCorners(sel)) ctx.fillRect(hx - HANDLE, hy - HANDLE, HANDLE * 2, HANDLE * 2);
  }
}

/** Screen positions of a cue's corners: TL, TR, BR, BL. */
function cueCorners(c) {
  const { k, ox, oy } = view;
  return [[c.x, c.y], [c.x + c.w, c.y], [c.x + c.w, c.y + c.h], [c.x, c.y + c.h]].map(([x, y]) => [ox + x * k, oy + y * k]);
}

/** Is canvas point (px, py) inside the cue, taking its rotation into account? */
function cueContains(c, px, py) {
  const a = (-c.rotation * Math.PI) / 180;
  const dx = px - (c.x + c.w / 2), dy = py - (c.y + c.h / 2);
  const lx = dx * Math.cos(a) - dy * Math.sin(a), ly = dx * Math.sin(a) + dy * Math.cos(a);
  return Math.abs(lx) <= c.w / 2 && Math.abs(ly) <= c.h / 2;
}

stage.addEventListener('mousedown', (e) => {
  if (!S.doc) return;
  const box = stage.getBoundingClientRect();
  const sx = e.clientX - box.left, sy = e.clientY - box.top;        // screen
  const cx = (sx - view.ox) / view.k, cy = (sy - view.oy) / view.k; // canvas
  const start = { mx: e.clientX, my: e.clientY, undone: false };
  // Middle button: pan, whatever is under the pointer.
  if (e.button === 1) {
    e.preventDefault();
    drag = { ...start, kind: 'pan', orig: { ...S.view }, moved: false, deselect: false };
    $('#stage-wrap').classList.add('panning');
    return;
  }
  if (e.button !== 0) return;

  // 1. A resize handle of the selected cue?
  const sel = selectedCue();
  if (sel && !sel.rotation) {
    const corner = cueCorners(sel).findIndex(([hx, hy]) => Math.abs(hx - sx) <= HANDLE + 2 && Math.abs(hy - sy) <= HANDLE + 2);
    if (corner >= 0) { drag = { ...start, kind: 'resize', corner, target: sel, orig: { ...sel } }; return; }
  }
  // 2. A display's name tab? (Displays are picked by their tab, so that
  //    clicking inside one still reaches the cues it shows.)
  const tab = displayTabs.find((t) => sx >= t.x && sx <= t.x + t.w && sy >= t.y && sy <= t.y + t.h);
  if (tab) {
    const d = findDisplay(tab.id);
    select({ type: 'display', id: d.id });
    drag = { ...start, kind: 'display', target: d, orig: { ...d.canvas_rect } };
    return;
  }
  // 3. The front-most cue under the pointer that is on air (or selected).
  for (const tl of tlIds()) {
    const t = showSeconds(tl), active = isActive(tl);
    for (const track of tlObj(tl).tracks) {
      for (const cue of [...track.cues].reverse()) {
        const visible = (active && t >= cue.start_s && t < cue.start_s + cue.duration_s) || cue === sel;
        if (visible && cueContains(cue, cx, cy)) {
          if (S.tl !== tl) S.tl = tl; // open the timeline the cue is on
          select({ type: 'cue', id: cue.id });
          drag = { ...start, kind: 'move', target: cue, orig: { ...cue } };
          return;
        }
      }
    }
  }
  // Empty area: dragging pans the view; a plain click deselects.
  drag = { ...start, kind: 'pan', orig: { ...S.view }, moved: false, deselect: true };
});

/** Zoom by `factor`, keeping the canvas point under screen point (sx, sy)
 *  where it is. */
function zoomAt(factor, sx, sy) {
  const { k, W, H, ox, oy } = view;
  const k2 = clamp(k * factor, 0.0005, 50);
  const px = (sx - ox) / k, py = (sy - oy) / k; // canvas point under the pointer
  S.view = { k: k2, cx: px - (sx - W / 2) / k2, cy: py - (sy - H / 2) / k2 };
}
/** Show all the displays (and, without displays, all the content). */
const fitStage = () => { S.view = { k: null, cx: 0, cy: 0 }; };

stage.addEventListener('wheel', (e) => {
  if (!S.doc) return;
  e.preventDefault();
  const box = stage.getBoundingClientRect();
  // Trackpads send many small deltas, mouse wheels a few large ones.
  zoomAt(Math.exp(-e.deltaY * (e.deltaMode === 1 ? 0.05 : 0.0015)), e.clientX - box.left, e.clientY - box.top);
}, { passive: false });
stage.addEventListener('dblclick', (e) => {
  // Double-click on nothing in particular: back to the whole stage.
  if (!S.sel) fitStage();
});
$('#stage-fit').onclick = fitStage;
$('#stage-actual').onclick = () => S.doc && view.W && zoomAt(1 / view.k, view.W / 2, view.H / 2);
$('#stage-zoom-in').onclick = () => S.doc && view.W && zoomAt(1.25, view.W / 2, view.H / 2);
$('#stage-zoom-out').onclick = () => S.doc && view.W && zoomAt(0.8, view.W / 2, view.H / 2);

/** Move `pos` (with extent `size`) so that one of its edges lands on a
 *  candidate line, if one is within `tol`. */
function snapAxis(pos, size, candidates, tol) {
  let best = null;
  for (const c of candidates) {
    for (const edge of [pos, pos + size]) {
      const d = c - edge;
      if (Math.abs(d) <= tol && (best === null || Math.abs(d) < Math.abs(best))) best = d;
    }
  }
  return best === null ? pos : pos + best;
}

/** Lines to snap to: the canvas origin and the edges of every display except
 *  `skip`; for cues, also the displays' centres. */
function snapLines(skip, centres) {
  // The canvas origin, and the edges (for cues also the centres) of the
  // displays.
  const st = S.doc.stage, xs = [0], ys = [0];
  for (const d of st.displays) {
    if (d === skip) continue;
    const r = d.canvas_rect;
    xs.push(r.x, r.x + r.w); ys.push(r.y, r.y + r.h);
    if (centres) { xs.push(r.x + r.w / 2); ys.push(r.y + r.h / 2); }
  }
  return { xs, ys };
}

function stageDragMove(e) {
  if (drag.kind === 'pan') {
    const dx = e.clientX - drag.mx, dy = e.clientY - drag.my;
    if (Math.abs(dx) + Math.abs(dy) > 3) { drag.moved = true; $('#stage-wrap').classList.add('panning'); }
    S.view = { k: drag.orig.k, cx: drag.orig.cx - dx / drag.orig.k, cy: drag.orig.cy - dy / drag.orig.k };
    return;
  }
  // The undo point is taken on the first real movement, so a plain click
  // does not leave an empty step in the history.
  if (!drag.undone) { drag.before = clone(S.doc); drag.undone = true; sendPresence(); }
  const dx = (e.clientX - drag.mx) / view.k, dy = (e.clientY - drag.my) / view.k;
  const o = drag.orig, c = drag.target;
  const snapping = $('#stage-snap').checked && !e.altKey, tol = 8 / view.k;
  if (drag.kind === 'move') {
    let x = o.x + dx, y = o.y + dy;
    if (snapping) {
      const { xs, ys } = snapLines(null, true);
      x = snapAxis(x, c.w, xs, tol); y = snapAxis(y, c.h, ys, tol);
    }
    c.x = Math.round(x); c.y = Math.round(y);
  } else if (drag.kind === 'display') {
    let x = o.x + dx, y = o.y + dy;
    if (snapping) {
      const { xs, ys } = snapLines(c, false);
      x = snapAxis(x, o.w, xs, tol); y = snapAxis(y, o.h, ys, tol);
    }
    c.canvas_rect.x = Math.round(x); c.canvas_rect.y = Math.round(y);
  } else {
    // Resize: the dragged corner follows the pointer, the opposite one stays.
    const left = drag.corner === 0 || drag.corner === 3, top = drag.corner < 2;
    let w = Math.max(8, o.w + (left ? -dx : dx)), h = Math.max(8, o.h + (top ? -dy : dy));
    if (e.shiftKey) { // keep the aspect ratio
      const k = Math.max(w / o.w, h / o.h);
      w = o.w * k; h = o.h * k;
    }
    c.w = Math.round(w); c.h = Math.round(h);
    c.x = Math.round(left ? o.x + o.w - w : o.x);
    c.y = Math.round(top ? o.y + o.h - h : o.y);
  }
  renderInspector();
  scheduleSend();
}

// ───────────────────────────── timeline ─────────────────────────────
// One timeline at a time is open in the Timeline window; the tabs in its
// title bar switch between the main timeline and the tasks. Tracks top to
// bottom (the first is in front), time left to right, control cues in a row
// of their own above the tracks.

const HEAD_W = 130; // width of the track-name column; matches --head-w in CSS
const ACTION_ICON = { pause: '⏸', play: '▶', stop: '⏹', jump: '↩' };
let tlDrag = null;  // the timeline drag in progress

function renderTabs() {
  const tabs = $('#tl-tabs');
  tabs.replaceChildren();
  for (const id of [MAIN, ...S.doc.tasks.map((t) => t.id)]) {
    const tr = currentTransport(id);
    const state = id === MAIN ? null : !tr?.active ? '' : tr.playing ? 'ok' : 'warn';
    tabs.append(el('button', { class: `tab${id === S.tl ? ' on' : ''}`, onclick: () => { S.tl = id; refreshAll(); } },
      state != null ? el('span', { class: `dot ${state}` }) : null, tlName(id)));
  }
  tabs.append(el('button', { class: 'tab', title: 'New auxiliary timeline', onclick: addTask }, '+'));
}

function renderTimeline() {
  if (!S.doc) return;
  queueMicrotask(markOthers); // once the cues below exist
  renderTabs();
  const obj = tlObj(S.tl), pps = S.pxPerSec, width = obj.duration_s * pps;
  $('#tl-body').style.width = HEAD_W + width + 40 + 'px';

  // Ruler: pick the smallest "nice" step that leaves labels ≥ 60 px apart.
  const ruler = $('#ruler');
  ruler.style.width = width + 'px';
  ruler.replaceChildren();
  const step = [0.5, 1, 2, 5, 10, 30, 60, 120, 300, 600].find((s) => s * pps >= 60) || 600;
  for (let t = 0; t <= obj.duration_s; t += step) {
    const label = t >= 60 ? `${Math.floor(t / 60)}:${String(Math.round(t % 60)).padStart(2, '0')}` : `${t}s`;
    ruler.append(el('div', { class: 'tick', style: `left:${t * pps}px` }, label));
  }

  // Control cues: flags standing on their point in time.
  const cuelane = $('#cuelane');
  cuelane.style.width = width + 'px';
  cuelane.replaceChildren();
  for (const cc of obj.control_cues) {
    const selected = S.sel?.type === 'control' && S.sel.id === cc.id;
    const target = cc.target ? ` ${tlName(cc.target)}` : '';
    const text = `${ACTION_ICON[cc.action] || '?'}${target}${cc.action === 'jump' ? ` → ${cc.to_s}s` : ''}`;
    cuelane.append(el('div', { class: `ctl${selected ? ' sel' : ''}`, dataset: { id: cc.id }, style: `left:${cc.at_s * pps}px`, title: cc.name || `${cc.action} at ${cc.at_s} s` }, text));
  }

  const tracks = $('#tracks');
  tracks.replaceChildren();
  obj.tracks.forEach((track, ti) => {
    const lane = el('div', {
      class: 'lane', style: `width:${width}px`, dataset: { ti },
      ondragover: (e) => { e.preventDefault(); lane.classList.add('drop'); },
      ondragleave: () => lane.classList.remove('drop'),
      ondrop: (e) => {
        e.preventDefault();
        lane.classList.remove('drop');
        const item = S.library?.find((i) => i.id === e.dataTransfer.getData('text/lcplay-asset'));
        if (item) addAssetCue(item, S.tl, ti, (e.clientX - lane.getBoundingClientRect().left) / pps);
      },
    });
    for (const cue of track.cues) {
      const selected = S.sel?.type === 'cue' && S.sel.id === cue.id;
      lane.append(el('div', {
        class: `cue ${cue.source.type}${cue.free_run ? ' free' : ''}${selected ? ' sel' : ''}`, dataset: { id: cue.id },
        style: `left:${cue.start_s * pps}px;width:${cue.duration_s * pps}px`, title: cue.name,
      }, cue.name || ({ color: 'colour', sync_test: 'sync test' }[cue.source.type] || 'cue'),
        el('div', { class: 'h l' }), el('div', { class: 'h r' })));
    }
    const selected = S.sel?.type === 'track' && S.sel.id === track.id;
    tracks.append(el('div', { class: 'track' },
      el('div', { class: `track-head${selected ? ' sel' : ''}`, title: 'Click to select the track', onclick: () => select({ type: 'track', id: track.id }) },
        el('span', { class: 'name' }, track.name || `Track ${ti + 1}`)),
      lane));
  });
}

// One mousedown handler for all cues (they are rebuilt constantly, so
// per-element listeners would be lost mid-drag).
$('#tracks').addEventListener('mousedown', (e) => {
  const node = e.target.closest('.cue');
  if (!node) { if (e.target.classList.contains('lane')) select(null); return; }
  const hit = findCue(node.dataset.id);
  if (!hit) return;
  const handle = e.target.classList.contains('h') ? (e.target.classList.contains('l') ? 'trim' : 'length') : 'move';
  select({ type: 'cue', id: hit.cue.id });
  tlDrag = { kind: handle, id: hit.cue.id, mx: e.clientX, orig: { ...hit.cue }, undone: false };
  e.preventDefault();
});
$('#cuelane').addEventListener('mousedown', (e) => {
  const node = e.target.closest('.ctl');
  if (!node) return select(null);
  const hit = findControl(node.dataset.id);
  if (!hit) return;
  select({ type: 'control', id: hit.cc.id });
  tlDrag = { kind: 'control', id: hit.cc.id, mx: e.clientX, orig: { ...hit.cc }, undone: false };
  e.preventDefault();
});

function timelineDragMove(e) {
  const dt = (e.clientX - tlDrag.mx) / S.pxPerSec, o = tlDrag.orig;
  const takeUndo = () => { if (!tlDrag.undone) { tlDrag.before = clone(S.doc); tlDrag.undone = true; sendPresence(); } };

  if (tlDrag.kind === 'control') {
    const hit = findControl(tlDrag.id);
    if (!hit) return;
    takeUndo();
    hit.cc.at_s = round2(clamp(o.at_s + dt, 0, tlObj(hit.tl).duration_s));
  } else {
    const hit = findCue(tlDrag.id);
    if (!hit) return;
    takeUndo();
    const c = hit.cue, obj = tlObj(hit.tl);
    if (tlDrag.kind === 'move') {
      c.start_s = round2(Math.max(0, o.start_s + dt));
      // Dragging up/down moves the cue to the track under the pointer.
      const lane = document.elementFromPoint(e.clientX, e.clientY)?.closest('#tracks .lane');
      const ti = lane ? Number(lane.dataset.ti) : hit.ti;
      if (ti !== hit.ti && obj.tracks[ti]) {
        hit.track.cues.splice(hit.ci, 1);
        obj.tracks[ti].cues.push(c);
      }
    } else if (tlDrag.kind === 'length') {
      c.duration_s = round2(Math.max(0.1, o.duration_s + dt));
    } else {
      // Trim the start: the end stays where it is, and a video's in-point
      // moves with it so the picture at any given time does not change.
      const d = clamp(dt, -o.start_s, o.duration_s - 0.1);
      c.start_s = round2(o.start_s + d);
      c.duration_s = round2(o.duration_s - d);
      if (assetOf(c)?.kind === 'video') c.in_s = round2(Math.max(0, o.in_s + d));
    }
    const grown = Math.max(obj.duration_s, Math.ceil(c.start_s + c.duration_s));
    if (grown !== obj.duration_s) { obj.duration_s = grown; syncLoop(); }
  }
  renderTimeline();
  renderInspector();
  scheduleSend();
}

// Scrubbing: press or drag on the ruler to move the open timeline's playhead.
let scrubbing = false, lastScrub = 0;
function scrub(e) {
  const t = clamp((e.clientX - $('#ruler').getBoundingClientRect().left) / S.pxPerSec, 0, tlObj(S.tl).duration_s);
  // Each seek is a transport change for the whole cluster: do not flood it.
  if (performance.now() - lastScrub > 60) { seek(t, S.tl); lastScrub = performance.now(); }
}
$('#ruler').addEventListener('mousedown', (e) => { scrubbing = true; lastScrub = 0; scrub(e); });

// Drags are followed on the window, so they continue outside their panel.
window.addEventListener('mousemove', (e) => {
  if (winDrag) windowDragMove(e);
  else if (drag) stageDragMove(e);
  else if (tlDrag) timelineDragMove(e);
  else if (scrubbing) scrub(e);
});
window.addEventListener('mouseup', (e) => {
  if (drag?.kind === 'pan') {
    $('#stage-wrap').classList.remove('panning');
    if (!drag.moved && drag.deselect) select(null);
    drag = null;
    return;
  }
  if (winDrag) saveLayout();
  if (scrubbing) { lastScrub = 0; scrub(e); }
  const finished = drag?.undone ? drag : tlDrag?.undone ? tlDrag : null;
  if (finished) { record(finished.before); sendDoc(); } // final position, immediately
  winDrag = drag = tlDrag = null;
  scrubbing = false;
  if (finished) sendPresence();
  if (S.deferredShow) rebase();
});

$('#tl-play').onclick = () => command({ type: currentTransport(S.tl)?.playing ? 'pause' : 'play' }, S.tl);
$('#tl-stop').onclick = () => command({ type: 'stop' }, S.tl);
$('#btn-track').onclick = () => edit(() => { const o = tlObj(S.tl); o.tracks.push({ id: uid(), name: `Track ${o.tracks.length + 1}`, cues: [] }); });
$('#btn-control').onclick = () => edit(() => {
  const cc = { id: uid(), name: '', at_s: round2(showSeconds(S.tl)), action: 'pause', target: '', to_s: 0 };
  tlObj(S.tl).control_cues.push(cc);
  S.sel = { type: 'control', id: cc.id };
});
$('#zoom').oninput = (e) => { S.pxPerSec = Number(e.target.value); renderTimeline(); };

// ───────────────────────────── tasks ─────────────────────────────
// The auxiliary timelines and their transports: what is running, and the
// buttons to start, pause and stop each.

const taskStateText = (id) => {
  const tr = currentTransport(id);
  if (!tr?.active) return 'stopped';
  return `${tr.playing ? 'playing' : 'paused'} · ${showSeconds(id).toFixed(1)} s`;
};

function renderTasks() {
  const box = $('#tasks');
  box.replaceChildren();
  if (!S.doc) return;
  if (!S.doc.tasks.length) {
    box.append(el('div', { class: 'empty' }, 'No tasks. A task is an extra timeline that runs independently of the main one — started by you, by a control cue, or by show control (OSC).'));
    return;
  }
  for (const task of S.doc.tasks) {
    const tr = currentTransport(task.id);
    const selected = (S.sel?.type === 'task' && S.sel.id === task.id) || S.tl === task.id;
    box.append(el('div', { class: `task${selected ? ' sel' : ''}` },
      el('span', { class: `dot ${!tr?.active ? '' : tr.playing ? 'ok' : 'warn'}` }),
      el('span', { class: 'name', title: 'Open in the Timeline window', onclick: () => { S.tl = task.id; select({ type: 'task', id: task.id }); } }, task.name || 'Task'),
      el('span', { class: 'state dim mono', dataset: { tl: task.id } }, taskStateText(task.id)),
      el('button', { class: 'small', title: 'Play', onclick: () => command({ type: 'play' }, task.id) }, '▶'),
      el('button', { class: 'small', title: 'Pause', onclick: () => command({ type: 'pause' }, task.id) }, '⏸'),
      el('button', { class: 'small', title: 'Stop (take off stage)', onclick: () => command({ type: 'stop' }, task.id) }, '⏹')));
  }
}
$('#btn-task').onclick = addTask;

// ───────────────────────────── runners ─────────────────────────────
// One card per Runner — connected, remembered from earlier, or named by a
// display: its health and machine, what its outputs show (live pictures),
// the displays it drives and where each one goes (a window, or fullscreen
// on one of its monitors), and the actions an operator needs: reload
// outputs, standby, restart, quit, reboot / power off the computer, wake it,
// read its log, open a remote desktop on it.

let runnersKey = ''; // what the window was last built from

/** Everything known about a Runner, by name. */
function runnerInfo(name) {
  const node = (S.status?.nodes || []).find((n) => n.role === 'runner' && n.name === name) || null;
  const known = (S.status?.known_runners || []).find((k) => k.name === name) || null;
  const h = node?.health || null;
  return { node, known, h, machine: h?.machine || known?.machine || null, online: !!node };
}

/** Names of all Runners worth listing. */
function runnerNames() {
  const names = [
    ...(S.status?.nodes || []).filter((n) => n.role === 'runner').map((n) => n.name),
    ...(S.status?.known_runners || []).map((k) => k.name),
    ...(S.doc?.stage.displays || []).map((d) => d.runner).filter(Boolean),
  ];
  return [...new Set(names)];
}

/** The monitor a Runner treats as primary, as it reports it (older Runners
 *  do not: then the one at the desktop origin). */
const primaryMonitor = (mons) => mons.find((m) => m.primary) || mons.find((m) => m.x === 0 && m.y === 0) || mons[0] || null;

/** Is this Runner on the operator's own computer (the one running the
 *  Producer)? Compared by host name, which the Producer service reports. */
function isLocal(node) {
  const runnerHost = node?.health?.machine?.hostname;
  if (runnerHost && S.hostname) return runnerHost.toLowerCase() === S.hostname.toLowerCase();
  return false;
}

function runnerAction(name, action, confirmText) {
  const go = () => {
    command({ type: 'runner_action', runner: name, action });
    // Pictures and logs are asked for continually: no message for those.
    if (!action.startsWith('send_')) toast(`${name}: ${action.replace('_', ' ')} requested.`, true);
  };
  if (!confirmText) return go();
  ask(confirmText).then((yes) => yes && go());
}

async function remoteDesktop(host, kind) {
  try {
    // The header proves to the Producer service that the request comes from
    // its own page (see remote_desktop in lc-producer/src/main.rs).
    const res = await fetch(`local/remote-desktop?host=${encodeURIComponent(host)}&kind=${kind}`, { method: 'POST', headers: { 'X-LCPlay': '1' } });
    if (!res.ok) toast(await res.text());
  } catch {
    toast('Cannot reach the Producer service to open the remote desktop.');
  }
}

function renderRunners(force = false) {
  const box = $('#runners');
  if (!S.doc) return;
  const names = runnerNames();
  // Rebuilding the list under the operator's cursor would close an open
  // dropdown or steal a click: only rebuild when something shown changed.
  const key = JSON.stringify([
    names.map((n) => {
      const { node, h, machine, online } = runnerInfo(n);
      return [n, online, node?.addr, node?.frame_error, h?.sync.locked, Math.round(h?.fps || 0), h?.monitors, h?.gpu, h?.outputs, h?.standby, machine,
        (h?.output_health || []).map((o) => [o.presenter, o.barrier, o.missed, o.late_decodes, o.latency_frames, o.grid_offset_ns == null ? null : Math.round(o.grid_offset_ns / 100000),
          o.refresh_hz == null ? null : Math.round(o.refresh_hz * 10), o.frame_counter == null]),
        (h?.downloads || []).map((d) => [d.name, d.total ? Math.floor((d.done / d.total) * 200) : Math.round(d.done / 1e7), !!d.error, d.retry_in_s]),
        S.status?.grid_source === node?.node_id];
    }),
    S.doc.stage.displays, S.sel, S.connected, [...S.previewOn],
  ]);
  if (key === runnersKey && !force) return;
  if (box.contains(document.activeElement) && ['SELECT', 'INPUT'].includes(document.activeElement.tagName) && !force) return;
  runnersKey = key;
  box.replaceChildren();
  if (!names.length) box.append(el('div', { class: 'empty' }, S.connected ? 'No Runner is connected.' : 'Not connected to the Director.'));

  for (const name of names) {
    const { node, h, machine, online } = runnerInfo(name);
    const state = !online ? 'bad' : !h ? 'warn' : node.frame_error ? 'bad' : h.standby ? 'warn' : h.sync.locked ? 'ok' : 'warn';
    const host = machine?.ip || (node?.addr || '').replace(/:\d+$/, '');
    const card = el('div', { class: `runner${online ? '' : ' offline'}`, dataset: { runner: name } },
      el('div', { class: 'runner-head' },
        el('span', { class: `dot ${state}` }),
        el('span', { class: 'name' }, name),
        el('span', { class: 'dim' }, online ? [machine?.hostname, host].filter(Boolean).join(' · ') : 'offline')));

    // Actions.
    const act = (label, title, onclick, opts = {}) => el('button', { title, onclick, disabled: opts.disabled, class: opts.on ? 'on' : null }, label);
    const actions = el('div', { class: 'actions' });
    if (online) {
      const previewing = S.previewOn.has(name);
      const power = machine?.allow_power;
      const noPower = 'Start this runner with --allow-power to allow this';
      actions.append(
        act('Preview', 'Show what its outputs show, refreshed every second or two', () => {
          previewing ? S.previewOn.delete(name) : S.previewOn.add(name);
          if (!previewing) runnerAction(name, 'send_preview');
          renderRunners(true);
        }, { on: previewing }),
        act('Log', 'Read its recent log', () => openLog(name)),
        act('Reload outputs', 'Close and rebuild its output windows and reload its media', () => runnerAction(name, 'reload_outputs')),
        h?.standby
          ? act('Resume', 'Open its outputs again', () => runnerAction(name, 'resume'))
          : act('Standby', 'Close its outputs; it stays connected', () => runnerAction(name, 'standby', `Put ${name} in standby? Its outputs close until resumed.`)),
        act('Restart', machine?.supervised ? 'Restart the Runner program' : 'Not supervised: Restart will only stop it',
          () => runnerAction(name, 'restart', `Restart the Runner program on ${name}? Its outputs go dark for a few seconds.`)),
        act('Quit', 'Stop the Runner program', () => runnerAction(name, 'quit', `Stop the Runner program on ${name}? It can only be started again on that computer${machine?.allow_power ? ' or by rebooting it' : ''}.`)),
        act('Reboot', power ? 'Restart the computer' : noPower,
          () => runnerAction(name, 'reboot', `Restart the computer ${machine?.hostname || name}?`), { disabled: !power }),
        act('Power off', power ? 'Shut the computer down' : noPower,
          () => runnerAction(name, 'power_off', `Shut down the computer ${machine?.hostname || name}? It can be woken again only if Wake-on-LAN is enabled on it.`), { disabled: !power }),
      );
    } else {
      actions.append(act('Wake', machine?.mac ? `Send a Wake-on-LAN packet to ${machine.mac}` : 'Its network card address is not known yet',
        () => { command({ type: 'wake_runner', runner: name }); toast(`Wake-on-LAN sent to ${name}.`, true); }, { disabled: !machine?.mac }));
    }
    if (host) {
      actions.append(
        act('RDP', `Open Windows Remote Desktop on ${host} (the remote computer must allow it)`, () => remoteDesktop(host, 'rdp')),
        act('VNC', `Open a VNC viewer on ${host} (the remote computer must run a VNC server)`, () => remoteDesktop(host, 'vnc')));
    }
    card.append(actions);

    if (h) {
      card.append(el('div', { class: 'kv' },
        el('span', {}, 'Status'), el('span', {}, [
          h.standby ? 'STANDBY' : null,
          h.fps ? `${h.fps.toFixed(0)} fps` : 'not rendering',
          h.sync.locked ? `clock ±${Math.max(1, Math.round(h.sync.error_ns / 1000))} µs` : 'clock syncing',
          node.frame_error ? `${node.frame_error > 0 ? '+' : ''}${node.frame_error} frames off` : null,
          `${h.outputs ?? 0} output(s)`,
        ].filter(Boolean).join(' · ')),
        el('span', {}, 'GPU'), el('span', {}, h.gpu || '—'),
        ...outputSyncRows(h, node),
        el('span', {}, 'Monitors'), el('span', {}, h.monitors?.length
          ? h.monitors.map((m) => `#${m.index} ${m.width}×${m.height}${m === primaryMonitor(h.monitors) ? ' (primary)' : ''}`).join('   ')
          : '—'),
        el('span', {}, 'Machine'), el('span', {}, machine
          ? [machine.os, `LC Play ${machine.version}`, machine.mac ? `MAC ${machine.mac}` : null, machine.allow_power ? 'power control allowed' : null].filter(Boolean).join(' · ')
          : '—')));
    }
    if (h?.downloads?.length) card.append(downloadBars(h.downloads));
    if (S.previewOn.has(name)) card.append(el('div', { class: 'previews', dataset: { runner: name } }));

    // The displays this Runner drives: its own, and those assigned to none.
    card.append(el('h4', {}, 'Displays'));
    const displays = S.doc.stage.displays.filter((d) => d.runner === name || d.runner === '');
    if (!displays.length) card.append(el('div', { class: 'note' }, 'None.'));
    for (const d of displays) {
      const selected = S.sel?.type === 'display' && S.sel.id === d.id;
      card.append(el('div', { class: `disp${selected ? ' sel' : ''}` },
        el('span', { class: 'name', title: 'Edit in the Inspector', onclick: () => select({ type: 'display', id: d.id }) },
          `${d.name || 'display'} — ${d.canvas_rect.w}×${d.canvas_rect.h}${d.runner ? '' : ' (any runner)'}`),
        outputPicker(d, h?.monitors || []),
        el('span', {}),
        el('button', { class: 'small', title: 'Edit position, size, blend…', onclick: () => select({ type: 'display', id: d.id }) }, '✎')));
    }
    card.append(el('div', { class: 'btns' }, el('button', { onclick: () => addDisplay(name) }, '+ Display on this runner')));
    box.append(card);
    showPreviews(name);
  }
}

/** Media being copied into a Runner's cache: one bar each. Until a file is
 *  complete, the cues that use it show the black-and-magenta placeholder on
 *  that Runner's outputs. */
function downloadBars(downloads) {
  const mb = (b) => (b >= 1e9 ? `${(b / 1e9).toFixed(2)} GB` : `${(b / 1e6).toFixed(0)} MB`);
  const box = el('div', { class: 'downloads' }, el('h4', {}, 'Loading media'));
  for (const d of downloads) {
    const frac = d.total ? Math.min(1, d.done / d.total) : 0;
    const left = d.total && d.bytes_per_s > 0 ? Math.max(0, (d.total - d.done) / d.bytes_per_s) : null;
    const eta = left == null ? '' : left >= 90 ? ` · ${Math.round(left / 60)} min left` : ` · ${Math.ceil(left)} s left`;
    const text = d.error
      ? `failed — retrying in ${d.retry_in_s ?? 0} s`
      : `${d.total ? Math.floor(frac * 100) + ' % · ' : ''}${mb(d.done)}${d.total ? ' / ' + mb(d.total) : ''} · ${mb(d.bytes_per_s)}/s${eta}`;
    box.append(el('div', { class: `dl${d.error ? ' failed' : ''}`, title: d.error || d.name },
      el('div', { class: 'dl-head' }, el('span', { class: 'dl-name' }, d.name), el('span', { class: 'dim' }, text)),
      el('div', { class: 'bar' }, el('div', { class: 'fill', style: `width:${(frac * 100).toFixed(1)}%` }))));
  }
  return box;
}

/** One "Sync" row per output of a Runner: how its frames reach the
 *  display, and whether they are where they should be.
 *
 *  - presenter: `dxgi` = LC Play's own swap chain (exact refresh timing,
 *    NVIDIA present barrier); `wgpu` = estimated from the render loop.
 *  - barrier: NVIDIA present barrier — `cluster` (synchronized across
 *    machines through frame lock) is the goal on a frame-locked rig.
 *  - grid: distance from the Director's refresh grid; within a millisecond
 *    or so when frame-locked. "off grid" = this output picks its frames on
 *    its own clock (no frame lock, or another refresh rate).
 *  - missed: frames that reached the screen later than planned; late: frames
 *    where a video was not decoded in time. Both should stay at 0. */
function outputSyncRows(h, node) {
  const rows = [];
  const isRef = node && S.status?.grid_source === node.node_id;
  for (const o of h.output_health || []) {
    const parts = [
      o.presenter,
      o.refresh_hz ? `${o.refresh_hz.toFixed(2)} Hz` : 'measuring',
      o.barrier && o.barrier !== 'off' ? `barrier ${o.barrier}` : null,
      o.grid_offset_ns == null ? 'off grid' : `grid ${o.grid_offset_ns >= 0 ? '+' : ''}${(o.grid_offset_ns / 1e6).toFixed(2)} ms`,
      o.latency_frames != null ? `${o.latency_frames} frame(s) ahead` : null,
      `missed ${o.missed}`,
      `late ${o.late_decodes}`,
      o.frame_counter != null ? `counter ${o.frame_counter}` : null,
    ];
    const bad = o.missed > 0 || o.late_decodes > 0;
    rows.push(el('span', {}, `Sync ${o.id}`), el('span', { class: bad ? 'warn-text' : null, title: 'Frames presented: ' + o.presents }, parts.filter(Boolean).join(' · ')));
  }
  if (isRef) rows.push(el('span', {}, 'Grid'), el('span', {}, 'This Runner\'s refresh is the reference for the cluster\'s refresh grid.'));
  return rows;
}

/** Where a display's picture goes: a window (a preview on the operator's
 *  screen), or fullscreen on one of the Runner's monitors. */
function outputPicker(d, monitors) {
  const sel = el('select', { class: 'out', title: 'Where this display appears on its Runner' },
    el('option', { value: 'w' }, 'Window (preview)'),
    el('option', { value: 'p' }, 'Fullscreen · primary monitor'));
  for (const m of monitors) sel.append(el('option', { value: `m${m.index}` }, `Fullscreen · #${m.index} ${m.width}×${m.height}`));
  const current = !d.window.fullscreen ? 'w' : d.window.monitor == null ? 'p' : `m${d.window.monitor}`;
  // A monitor the Runner has not reported (yet) must stay selectable.
  if (![...sel.options].some((o) => o.value === current)) sel.append(el('option', { value: current }, `Fullscreen · #${d.window.monitor} (not connected)`));
  sel.value = current;
  sel.onchange = () => edit(() => {
    if (sel.value === 'w') {
      d.window.fullscreen = false;
      d.window.size ??= [960, Math.max(1, Math.round((960 * d.canvas_rect.h) / d.canvas_rect.w))];
    } else {
      d.window.fullscreen = true;
      d.window.monitor = sel.value === 'p' ? null : Number(sel.value.slice(1));
    }
  });
  return sel;
}

/** Put the latest pictures of a Runner's outputs into its card. */
function showPreviews(name) {
  const holder = [...document.querySelectorAll('#runners .previews')].find((n) => n.dataset.runner === name);
  if (!holder) return;
  const outputs = S.previews[name];
  holder.replaceChildren(...(outputs?.length
    ? outputs.map((o) => el('figure', {}, el('img', { src: `data:image/png;base64,${o.png_base64}`, alt: o.name }), el('figcaption', {}, o.name)))
    : [el('div', { class: 'note' }, outputs ? 'No outputs open.' : 'Waiting for pictures…')]));
}

function openLog(name) {
  S.logRunner = name;
  const win = $('#win-log');
  layout[win.id].visible = true;
  focusWindow(win);
  applyLayout(); saveLayout();
  renderLogChoices();
  runnerAction(name, 'send_logs');
  renderLog();
}

function renderLogChoices() {
  const sel = $('#log-runner');
  const names = runnerNames().filter((n) => runnerInfo(n).online);
  if (S.logRunner && !names.includes(S.logRunner)) names.push(S.logRunner);
  sel.replaceChildren(...names.map((n) => el('option', { value: n }, n)));
  sel.value = S.logRunner;
}

function renderLog() {
  const pre = $('#log-text');
  const lines = S.logs[S.logRunner];
  const atBottom = pre.parentElement.scrollTop + pre.parentElement.clientHeight >= pre.parentElement.scrollHeight - 20;
  pre.textContent = !S.logRunner ? 'Choose a runner.' : lines ? lines.join('\n') : 'Waiting for the log…';
  if (atBottom || $('#log-follow').checked) pre.parentElement.scrollTop = pre.parentElement.scrollHeight;
}
$('#log-runner').onchange = (e) => { S.logRunner = e.target.value; runnerAction(S.logRunner, 'send_logs'); renderLog(); };
$('#log-refresh').onclick = () => S.logRunner && runnerAction(S.logRunner, 'send_logs');

// Live pictures and a following log are refreshed while their windows are
// open, and only then (each picture is a few tens of kilobytes).
setInterval(() => {
  if (!S.connected) return;
  if (layout['win-runners']?.visible) {
    for (const name of S.previewOn) if (runnerInfo(name).online) command({ type: 'runner_action', runner: name, action: 'send_preview' });
  }
  if (layout['win-log']?.visible && $('#log-follow').checked && S.logRunner && runnerInfo(S.logRunner).online) {
    command({ type: 'runner_action', runner: S.logRunner, action: 'send_logs' });
  }
}, 1500);

/** A monitor of `runner` that no display uses yet, for a new display to go
 *  fullscreen on. On the operator's own computer the primary monitor is
 *  left alone: covering it would hide the Producer. */
function freeMonitor(runner) {
  const nodes = (S.status?.nodes || []).filter((n) => n.role === 'runner');
  const node = runner ? nodes.find((n) => n.name === runner) : nodes.length === 1 ? nodes[0] : null;
  const mons = node?.health?.monitors || [];
  if (!mons.length) return null;
  const primary = primaryMonitor(mons);
  const used = new Set(S.doc.stage.displays
    .filter((d) => d.window.fullscreen && (d.runner === runner || d.runner === '' || runner === ''))
    .map((d) => d.window.monitor ?? primary.index));
  return mons.find((m) => !used.has(m.index) && !(isLocal(node) && m === primary)) || null;
}

/** A new display: placed to the right of the existing ones on the canvas,
 *  fullscreen on a free monitor of its Runner if there is one, otherwise a
 *  preview window. */
function addDisplay(runner = '') {
  const st = S.doc.stage, n = st.displays.length + 1;
  const mon = freeMonitor(runner);
  const [w, h] = mon ? [mon.width, mon.height] : [1920, 1080];
  const x = st.displays.reduce((right, d) => Math.max(right, d.canvas_rect.x + d.canvas_rect.w), 0);
  edit(() => {
    const d = {
      id: uid(), name: `Display ${n}`, runner, canvas_rect: { x, y: 0, w, h },
      window: mon
        ? { fullscreen: true, monitor: mon.index, position: null, size: null }
        : { fullscreen: false, position: [60 + 40 * n, 60 + 40 * n], size: [960, Math.round((960 * h) / w)] },
    };
    st.displays.push(d);
    normalize(S.doc);
    S.sel = { type: 'display', id: d.id };
  });
  fitStage();
  toast(mon
    ? `Display ${n}: fullscreen on monitor #${mon.index} (${mon.width}×${mon.height}).`
    : `Display ${n}: shown in a window — ${runner || 'the runner'} has no free monitor${runner === '' ? '' : ''}. Choose fullscreen in the Runners window once a monitor is connected.`, true);
}

/** Line the displays up left to right, in their current order. */
function arrangeSideBySide() {
  edit(() => {
    const st = S.doc.stage;
    let x = 0, h = 0;
    for (const d of [...st.displays].sort((a, b) => a.canvas_rect.x - b.canvas_rect.x)) {
      d.canvas_rect.x = x; d.canvas_rect.y = 0;
      x += d.canvas_rect.w; h = Math.max(h, d.canvas_rect.h);
    }
  });
  fitStage();
}

// ───────────────────────────── inspector ─────────────────────────────
// Properties of whatever is selected — a cue, a control cue, a track, a
// task, a display — or, with nothing selected, of the show itself.

/** A labelled input bound to obj[key]. Changes are committed on Enter/blur
 *  (not per keystroke), each as one undo step. */
function field(label, obj, key, opts = {}) {
  const { type = 'number', step = 1, min, get = (v) => v, set = (v) => v, after, optional = false } = opts;
  const input = el('input', { type, step, min });
  if (type === 'checkbox') input.checked = !!obj[key];
  else input.value = get(obj[key]) ?? '';
  input.addEventListener('change', () => {
    let v = type === 'checkbox' ? input.checked : type === 'number' ? (input.value === '' ? null : Number(input.value)) : input.value;
    // A number field that is emptied, or holds something unusable, goes back
    // to its value: writing "no value" into the show would make the Director
    // refuse every edit from then on. Only fields that are genuinely
    // optional (the monitor number) may be cleared.
    const unusable = v == null ? !optional : Number.isNaN(v) || (min != null && v < min);
    if (type === 'number' && unusable) { input.value = get(obj[key]) ?? ''; return; }
    edit(() => { obj[key] = set(v); after?.(); }, { inspector: false });
  });
  return el('div', { class: 'row' }, el('label', {}, label), input);
}

/** Two numbers on one row (x/y, w/h). */
function pair(label, obj, a, b, opts = {}) {
  const row = el('div', { class: 'row two' }, el('label', {}, label));
  for (const key of [a, b]) row.append(field('', obj, key, opts).lastChild);
  return row;
}

/** An optional [a, b] array property (window position/size): empty = unset. */
function optionalPair(label, obj, key, min) {
  const row = el('div', { class: 'row two' }, el('label', {}, label));
  const inputs = [0, 1].map((i) => el('input', { type: 'number', step: 1, min, placeholder: 'auto', value: obj[key]?.[i] ?? '' }));
  for (const input of inputs) {
    input.addEventListener('change', () => {
      const vals = inputs.map((n) => (n.value === '' ? null : Number(n.value)));
      edit(() => (obj[key] = vals.every((v) => v != null) ? vals : null), { inspector: false });
    });
    row.append(input);
  }
  return row;
}

/** A dropdown bound to obj[key]; options are [value, label] pairs. Rebuilds
 *  the inspector afterwards, since a choice may show or hide other fields. */
function choice(label, obj, key, options) {
  const sel = el('select', { class: 'wide' });
  for (const [value, text] of options) sel.append(el('option', { value }, text));
  sel.value = obj[key];
  sel.onchange = () => { sel.blur(); edit(() => (obj[key] = sel.value)); };
  return el('div', { class: 'row' }, el('label', {}, label), sel);
}

function colorField(label, obj, key) {
  const input = el('input', { type: 'color', value: rgbaToHex(obj[key]) });
  input.addEventListener('change', () => edit(() => (obj[key] = [...hexToRgb(input.value), obj[key][3] ?? 1]), { inspector: false }));
  return el('div', { class: 'row' }, el('label', {}, label), input);
}

const note = (text) => el('div', { class: 'note' }, text);
const deleteButton = (text) => el('div', { class: 'btns' }, el('button', { class: 'danger', onclick: deleteSelection }, text));

function renderInspector() {
  const box = $('#inspector');
  // Do not rebuild under the operator's cursor while they are typing here.
  if (box.contains(document.activeElement) && document.activeElement.tagName === 'INPUT' && !drag && !tlDrag && box.dataset.sel === JSON.stringify(S.sel)) return;
  box.dataset.sel = JSON.stringify(S.sel);
  box.replaceChildren();
  if (!S.doc) return;
  // Someone else on the same object: say so (a warning, not a lock — the
  // last change to a field wins).
  const together = S.sel ? othersOn(S.sel.type, S.sel.id) : [];
  S.selOthersKey = JSON.stringify(together.map((o) => [o.name, o.dragging]));
  if (together.length) {
    box.append(el('div', { class: 'together', style: `--c:${together[0].color}` },
      `${together.map((o) => o.name).join(', ')} ${together.some((o) => o.dragging) ? 'is moving' : together.length > 1 ? 'also have' : 'also has'} this selected.`));
  }
  const sec = { step: 0.1, min: 0 };
  const type = S.sel?.type;

  if (type === 'cue') {
    const { cue: c, track } = findCue(S.sel.id), asset = assetOf(c);
    box.append(
      el('h3', {}, 'Cue'),
      field('Name', c, 'name', { type: 'text' }),
      el('h4', {}, 'Time'),
      field('Start (s)', c, 'start_s', sec),
      field('Duration (s)', c, 'duration_s', { step: 0.1, min: 0.1 }),
      field('Fade in (s)', c, 'fade_in_s', sec),
      field('Fade out (s)', c, 'fade_out_s', sec),
    );
    if (asset?.kind === 'video') {
      box.append(
        field('In point (s)', c, 'in_s', sec),
        field('Loop video', c, 'loop', { type: 'checkbox' }),
        field('Free run', c, 'free_run', { type: 'checkbox' }),
        note('Free run: the video plays at normal speed from the moment the cue goes on air and keeps playing while the timeline is paused; it stops on its last frame unless Loop is on. Start and duration still decide when it is on air.'));
    }
    box.append(
      el('h4', {}, 'Position on stage'),
      pair('X / Y', c, 'x', 'y'),
      pair('W / H', c, 'w', 'h', { min: 1 }),
      field('Rotation (°)', c, 'rotation'),
      field('Opacity (%)', c, 'opacity', { min: 0, get: (v) => Math.round(v * 100), set: (v) => clamp(v / 100, 0, 1) }),
    );
    if (c.source.type === 'color') box.append(colorField('Colour', c.source, 'rgba'));
    if (c.source.type === 'sync_test') {
      box.append(
        field('Frame rate', c.source, 'fps', { min: 1 }),
        note('Drawn by each Runner: the frame number (digits and binary blocks), a marker stepping one place per frame, and a white flash on the first frame of every second. Film two outputs, or two machines, with a fast camera: any difference in the numbers is the offset in frames. Use the displays\' refresh rate as frame rate.'));
    }
    const mw = asset?.width || extent().w, mh = asset?.height || extent().h;
    box.append(el('div', { class: 'btns' },
      el('button', { onclick: () => edit(() => Object.assign(c, fitRect(mw, mh, true))) }, 'Fit to stage'),
      asset?.width ? el('button', { onclick: () => edit(() => Object.assign(c, { w: asset.width, h: asset.height })) }, 'Original size') : null,
      el('button', { onclick: () => seek(c.start_s, findCue(c.id).tl) }, 'Go to cue'),
      el('button', { class: 'danger', onclick: deleteSelection }, 'Delete')));
    return;
  }

  if (type === 'control') {
    const { cc, tl } = findControl(S.sel.id);
    // "This timeline" plus every other timeline as a possible target.
    const targets = [['', `This timeline (${tlName(tl)})`], ...[MAIN, ...S.doc.tasks.map((t) => t.id)].filter((id) => id !== tl).map((id) => [id, tlName(id)])];
    box.append(
      el('h3', {}, 'Control cue'),
      field('Name', cc, 'name', { type: 'text' }),
      field('At (s)', cc, 'at_s', sec),
      choice('Action', cc, 'action', [['pause', 'Pause'], ['play', 'Play'], ['stop', 'Stop'], ['jump', 'Jump to…']]),
      choice('Timeline', cc, 'target', targets),
    );
    if (cc.action === 'jump') box.append(field('Jump to (s)', cc, 'to_s', sec));
    box.append(
      note('Fires when this timeline\'s playhead reaches it while playing forward, on the exact frame, on all Runners. Control cues are executed by the Director: they do not fire while it is offline.'),
      deleteButton('Delete control cue'));
    return;
  }

  if (type === 'track') {
    const { track } = findTrack(S.sel.id);
    box.append(
      el('h3', {}, 'Track'),
      field('Name', track, 'name', { type: 'text' }),
      deleteButton('Delete track'));
    return;
  }

  if (type === 'task') {
    const task = tlObj(S.sel.id);
    box.append(
      el('h3', {}, 'Task'),
      field('Name', task, 'name', { type: 'text' }),
      field('Length (s)', task, 'duration_s', { min: 1 }),
      note(`An auxiliary timeline with its own transport. It is off stage until played, and is drawn in front of the main timeline. Start it from the Tasks window, from a control cue, or over OSC: /lcplay/task/${task.id}/play (also pause, stop, seek).`),
      deleteButton('Delete task'));
    return;
  }

  if (type === 'display') {
    const d = findDisplay(S.sel.id);
    const frac = { step: 0.01, min: 0 };
    box.append(
      el('h3', {}, 'Display'),
      field('Name', d, 'name', { type: 'text' }),
      field('Runner', d, 'runner', { type: 'text' }),
      note("The Runner's --name; empty = any Runner."),
      el('h4', {}, 'Area of the canvas it shows'),
      pair('X / Y', d.canvas_rect, 'x', 'y'),
      pair('W / H', d.canvas_rect, 'w', 'h', { min: 1 }),
      el('h4', {}, 'Window on the Runner'),
      field('Fullscreen', d.window, 'fullscreen', { type: 'checkbox' }),
      field('Monitor #', d.window, 'monitor', { min: 0, optional: true }),
      optionalPair('Position', d.window, 'position'),
      optionalPair('Size', d.window, 'size', 1),
      note('Windowed: position and size in desktop pixels. Fullscreen: on the monitor with that number (see the Runners window).'),
      el('h4', {}, 'Output'),
      field('Rotation (°)', d, 'rotation_degrees', { step: 90, min: 0, set: (v) => (((v % 360) / 90) | 0) * 90 }),
      el('h4', {}, 'Edge blend (fraction of the output)'),
      pair('Left / Right', d.blend, 'left', 'right', frac),
      pair('Top / Bottom', d.blend, 'top', 'bottom', frac),
      field('Gamma', d.blend, 'gamma', { step: 0.1, min: 0.1 }),
      deleteButton('Delete display'));
    return;
  }

  // Nothing selected: the show and its stage.
  const st = S.doc.stage;
  box.append(
    el('h3', {}, 'Show'),
    field('Name', S.doc, 'name', { type: 'text' }),
    field('Length (s)', S.doc, 'duration_s', { min: 1, after: syncLoop }),
    el('h4', {}, 'Canvas'),
    colorField('Background', st, 'background'),
    note(`The canvas has no fixed size: displays and content can go anywhere on it. The displays cover ${extent().w}×${extent().h} px.`),
    el('h4', {}, 'Displays'),
  );
  for (const d of st.displays) {
    box.append(el('div', { class: 'list-item', onclick: () => select({ type: 'display', id: d.id }) },
      el('span', {}, d.name || 'display'),
      el('span', { class: 'dim' }, `${d.canvas_rect.w}×${d.canvas_rect.h}${d.runner ? ' @' + d.runner : ''}`)));
  }
  box.append(el('div', { class: 'btns' },
    el('button', { onclick: () => addDisplay(''), title: 'A new display, to the right of the others' }, '+ Display'),
    el('button', { onclick: arrangeSideBySide, title: 'Line the displays up left to right (content stays where it is on the canvas)' }, 'Side by side')));
}

// ───────────────────────────── top bar ─────────────────────────────

/** Looping wraps at the main timeline's length; keep the two in step. */
function syncLoop() {
  if ($('#chk-loop').checked) command({ type: 'set_loop', loop_ns: Math.round(S.doc.duration_s * NS) });
}

function renderCluster() {
  const box = $('#cluster');
  box.replaceChildren(el('div', { class: 'chip' }, el('span', { class: `dot ${S.connected ? 'ok' : 'bad'}` }), S.connected ? 'Director' : 'Director offline'));
  if (!S.status) return;
  const runners = S.status.nodes.filter((n) => n.role === 'runner');
  if (!runners.length) box.append(el('div', { class: 'chip' }, el('span', { class: 'dot warn' }), 'no runners'));
  for (const n of runners) {
    const h = n.health;
    // Green: locked to the cluster clock and on the prescribed frame.
    // Amber: not (yet) locked, or between transport states. Red: wrong frame.
    const state = !h ? 'warn' : n.frame_error ? 'bad' : h.sync.locked && n.frame_error === 0 ? 'ok' : 'warn';
    const detail = !h ? 'starting' : [
      h.fps ? `${h.fps.toFixed(0)} fps` : null,
      h.sync.locked ? `±${Math.max(1, Math.round(h.sync.error_ns / 1000))} µs` : 'syncing',
      n.frame_error ? `${n.frame_error > 0 ? '+' : ''}${n.frame_error} frames` : null,
    ].filter(Boolean).join(' · ');
    box.append(el('div', { class: 'chip', title: `${n.addr}\nround trip ${h ? Math.round(h.sync.rtt_ns / 1000) : '?'} µs, drift ${h ? h.sync.drift_ppm.toFixed(1) : '?'} ppm` },
      el('span', { class: `dot ${state}` }), n.name, el('span', { class: 'dim' }, detail)));
  }
}

$('#btn-play').onclick = () => command({ type: currentTransport()?.playing ? 'pause' : 'play' });
$('#btn-stop').onclick = () => command({ type: 'stop' });
$('#chk-loop').onchange = (e) => command({ type: 'set_loop', loop_ns: e.target.checked ? Math.round(S.doc.duration_s * NS) : null });
$('#sel-rate').onchange = (e) => command({ type: 'set_rate', rate_permille: Number(e.target.value) });
$('#btn-undo').onclick = () => stepHistory(S.undo, S.redo);
$('#btn-redo').onclick = () => stepHistory(S.redo, S.undo);
// Presence is also repeated, so a user who joins later sees everyone.
setInterval(sendPresence, 3000);
renderUsers();
$('#stage-video').checked = liveVideo;
$('#stage-video').onchange = (e) => setLiveVideo(e.target.checked);
$('#btn-synctest').onclick = () => {
  const e = extent();
  addCue({ name: 'Sync test', source: { type: 'sync_test', fps: 60 }, duration_s: 600, x: e.x, y: e.y, w: e.w, h: e.h }, S.tl, 0, showSeconds(S.tl));
};
$('#btn-color').onclick = () => addCue({ name: 'Colour', source: { type: 'color', rgba: [0.1, 0.4, 0.8, 1] }, duration_s: 10, ...fitRect(640, 360, false) }, S.tl, 0, showSeconds(S.tl));
$('#btn-new').onclick = async () => {
  if (await ask('Start a new, empty show? It replaces the current one on the Director — for every user working on it.')) {
    S.replaceNext = true;
    edit(() => { S.doc = defaultDoc(); S.sel = null; S.tl = MAIN; });
  }
};
$('#btn-save').onclick = () => {
  const a = el('a', { href: URL.createObjectURL(new Blob([JSON.stringify(S.doc, null, 2)], { type: 'application/json' })), download: `${S.doc.name || 'show'}.lcplay.json` });
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 10000); // after the download has started
};
$('#btn-open').onclick = () => $('#file-open').click();
$('#file-open').onchange = async (e) => {
  const file = e.target.files[0];
  e.target.value = '';
  if (!file) return;
  try {
    const doc = normalize(JSON.parse(await file.text()));
    delete doc._edit;
    if (S.others.size && !(await ask(`Open ${file.name}? It replaces the show for every user working on it.`))) return;
    S.replaceNext = true;
    edit(() => { S.doc = doc; S.sel = null; S.tl = MAIN; });
  } catch (err) { toast(`Cannot open ${file.name}: ${err.message}`); }
};
$('#btn-upload').onclick = () => $('#file-upload').click();
$('#file-upload').onchange = (e) => { uploadFiles([...e.target.files]); e.target.value = ''; };

// Files dropped on the asset panel are uploaded.
const assetList = $('#asset-list');
assetList.addEventListener('dragover', (e) => { if (e.dataTransfer.types.includes('Files')) { e.preventDefault(); assetList.classList.add('drop'); } });
assetList.addEventListener('dragleave', () => assetList.classList.remove('drop'));
assetList.addEventListener('drop', (e) => {
  assetList.classList.remove('drop');
  if (e.dataTransfer.files.length) { e.preventDefault(); uploadFiles([...e.dataTransfer.files]); }
});

window.addEventListener('keydown', (e) => {
  if (['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName)) return; // typing in a field
  const mod = e.ctrlKey || e.metaKey;
  // Space plays / pauses the timeline open in the Timeline window.
  if (e.code === 'Space') { e.preventDefault(); $('#tl-play').click(); }
  else if (e.key === 'Delete' || e.key === 'Backspace') { e.preventDefault(); deleteSelection(); }
  else if (mod && e.key.toLowerCase() === 'z') { e.preventDefault(); (e.shiftKey ? $('#btn-redo') : $('#btn-undo')).click(); }
  else if (mod && e.key.toLowerCase() === 'y') { e.preventDefault(); $('#btn-redo').click(); }
});
// Close the Windows menu when clicking elsewhere.
window.addEventListener('mousedown', (e) => { if (!e.target.closest('#win-menu')) $('#win-menu').open = false; });

let taskSignature = '';
/** Per-frame update of everything that moves with time. */
function tick(now) {
  if (S.doc) {
    const t = showSeconds(), tr = currentTransport();
    const hh = Math.floor(t / 3600), mm = Math.floor(t / 60) % 60, ss = (t % 60).toFixed(2).padStart(5, '0');
    $('#time').textContent = `${String(hh).padStart(2, '0')}:${String(mm).padStart(2, '0')}:${ss}`;
    $('#frame').textContent = 'f ' + Math.floor((t * S.frameRate.num) / S.frameRate.den);
    $('#btn-play').textContent = tr?.playing ? '⏸' : '▶';
    // Loop and rate reflect the cluster's state (another controller — OSC,
    // lc-ctl — may have changed them), unless the operator is on the control.
    if (tr && document.activeElement !== $('#chk-loop')) $('#chk-loop').checked = tr.loop_ns > 0;
    if (tr && document.activeElement !== $('#sel-rate')) {
      const sel = $('#sel-rate'), value = String(tr.rate_permille);
      // A rate set elsewhere (OSC, lc-ctl) may not be one of the presets.
      if (![...sel.options].some((o) => o.value === value)) sel.append(el('option', { value }, `${tr.rate_permille / 1000}×`));
      sel.value = value;
    }

    // The open timeline: its own transport buttons, time and playhead.
    const tlT = showSeconds(S.tl), tlTr = currentTransport(S.tl);
    $('#tl-play').textContent = tlTr?.playing ? '⏸' : '▶';
    $('#tl-time').textContent = isActive(S.tl) ? `${tlT.toFixed(2)} s` : 'stopped';
    $('#playhead').style.left = HEAD_W + tlT * S.pxPerSec + 'px';

    // Tasks: rebuild the lists when a state changes (stopped/paused/playing),
    // otherwise just update the running times in place.
    const signature = S.doc.tasks.map((task) => { const s = currentTransport(task.id); return `${task.id}:${s?.active}:${s?.playing}`; }).join();
    if (signature !== taskSignature) { taskSignature = signature; renderTasks(); renderTabs(); }
    for (const node of document.querySelectorAll('#tasks .state')) node.textContent = taskStateText(node.dataset.tl);

    drawStage();
    tidyPreviews(now);
  }
  requestAnimationFrame(tick);
}

// ───────────────────────────── start-up ─────────────────────────────

(async function start() {
  loadLayout();
  // The Producer service tells us where the other services are; anything it
  // leaves open means "same host as this page, default port".
  const cfg = await (await fetch('config.json')).json();
  const host = location.hostname || 'localhost';
  S.assetsUrl = cfg.assets || `http://${host}:${cfg.assets_port}`;
  S.hostname = cfg.hostname || '';
  connect(`ws://${cfg.director || `${host}:${cfg.director_port}`}/ws`, cfg.protocol);

  setInterval(() => send({ type: 'ping' }), 2000);  // the Director drops silent clients
  loadLibrary();
  setInterval(loadLibrary, 5000);                   // picks up files copied in by hand
  renderCluster();
  requestAnimationFrame(tick);
})();
