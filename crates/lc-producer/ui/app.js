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
  holdN: 0,             // a whole show we sent (New, Open) as edit n: ignore shows until it is in
  server: null,         // the show as the Director last sent it (the shared truth)
  pending: [],          // our edits sent but not yet seen back: [{ n, ops }]
  replaceNext: false,   // the next send replaces the show for everyone (New, Open)
  deferredShow: false,  // a show arrived during a drag: apply it when the drag ends
  user: loadUser(),     // { name, color }: who this is, for the other users
  others: new Map(),    // node id → presence of the other Producers' users
  sel: null,            // { type: 'cue'|'display'|'virtual'|'control'|'track'|'task', id }; several
                        // cues: { type: 'cue', id, ids } (id = the one clicked last, also in ids)
  tl: MAIN,             // the timeline open in the Timeline window
  transports: {},       // timeline id → transport states known, oldest first (≤ 2)
  frameRate: { num: 60, den: 1 },
  clockOffsetNs: null,  // cluster time − performance.now(), null until known
  status: null,         // last cluster status
  library: [],          // items on the Asset Manager (null: not reachable)
  uploads: [],          // files this window is sending to it, in order (see uploadFiles)
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
  // Virtual displays: canvas areas captured at a resolution, shown by cues
  // whose source is { type: 'virtual', virtual: id } (see lc-proto show.rs).
  st.virtuals ??= [];
  for (const v of st.virtuals) {
    v.id ||= uid();
    v.name ??= '';
    v.canvas_rect = Object.assign({ x: 0, y: 0, w: 1920, h: 1080 }, v.canvas_rect);
    v.width ??= 1920; v.height ??= 1080;
    v.keep_aspect ??= false;
  }
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
          x: 0, y: 0, w: 640, h: 360, rotation: 0, opacity: 1, fade_in_s: 0, fade_out_s: 0, keep_aspect: false,
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
const findVirtual = (id) => S.doc.stage.virtuals.find((v) => v.id === id) || null;
/** Every cue (of every timeline) that shows virtual display `id`. */
const cuesShowing = (id) => tlIds().flatMap((tl) => tlObj(tl).tracks.flatMap((t) => t.cues))
  .filter((c) => c.source.type === 'virtual' && c.source.virtual === id);
const assetOf = (cue) => (cue.source.type === 'asset' ? S.doc.assets.find((a) => a.id === cue.source.asset) : null);
/** Ids of the selected cues: none, one, or several (box selection or
 *  Shift/Ctrl-click in the timeline). */
const selCueIds = () => (S.sel?.type !== 'cue' ? [] : S.sel.ids?.length ? S.sel.ids : [S.sel.id]);
const isCueSelected = (id) => selCueIds().includes(id);
/** The selected cues themselves (those that still exist). */
const selectedCues = () => selCueIds().map(findCue).filter(Boolean).map((h) => h.cue);
/** The selected cue when exactly one is selected: the stage's resize
 *  handles and the single-cue inspector work on it. */
const selectedCue = () => (selCueIds().length === 1 ? findCue(S.sel.id)?.cue : null) || null;
/** A cue selection from a list of ids: null, a plain one-cue selection, or a
 *  multiple one whose primary cue is `primary` (when it is in the list). */
function cueSelection(ids, primary) {
  ids = [...new Set(ids)];
  if (!ids.length) return null;
  const id = ids.includes(primary) ? primary : ids[ids.length - 1];
  return ids.length === 1 ? { type: 'cue', id } : { type: 'cue', id, ids };
}

/** Does the current selection still exist? */
function selectionExists() {
  if (!S.sel) return true;
  const { type, id } = S.sel;
  return !!({ cue: findCue, display: findDisplay, virtual: findVirtual, control: findControl, track: findTrack, task: (i) => tlObj(i) }[type]?.(id));
}
/** Drop from the selection what no longer exists (deleted here, by undo, or
 *  by another user): a multiple cue selection keeps the cues that are left. */
function pruneSelection() {
  if (S.sel?.type === 'cue' && S.sel.ids) S.sel = cueSelection(S.sel.ids.filter((id) => findCue(id)), S.sel.id);
  if (!selectionExists()) S.sel = null;
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
  send({ type: 'presence', name: S.user.name, color: S.user.color, selection: presenceSelection(), dragging: !!(drag?.undone || tlDrag?.undone) });
}

/** The Director relays a presence only if its selection is at most this
 *  many bytes of JSON (lc-director: MAX_PRESENCE_SELECTION_BYTES). */
const MAX_SELECTION_BYTES = 4096;
/** The selection as sent to the others. A very large multiple selection is
 *  cut down to what the Director relays (the primary cue always kept): the
 *  others then see part of it outlined, instead of a stale one. */
function presenceSelection() {
  const sel = S.sel;
  if (!sel?.ids) return sel;
  const bytes = (v) => new TextEncoder().encode(JSON.stringify(v)).length;
  const ids = [sel.id];
  let size = bytes({ ...sel, ids }) + 8; // + 8: margin for rounding of the estimate below
  for (const id of sel.ids) {
    if (id === sel.id) continue;
    size += bytes(id) + 1; // the id and its comma
    if (size > MAX_SELECTION_BYTES) break;
    ids.push(id);
  }
  return { ...sel, ids };
}

/** A colour from another user, safe to put in a style attribute. */
const safeColor = (c) => (/^#[0-9a-fA-F]{3,8}$/.test(c || '') ? c : '#888888');

/** Other users who have the object `type`/`id` selected (alone, or as one
 *  of several cues). */
function othersOn(type, id) {
  return [...S.others.values()].filter((o) => o.selection?.type === type
    && (o.selection.id === id || (Array.isArray(o.selection.ids) && o.selection.ids.includes(id))));
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
const KEYED = new Set(['tracks', 'cues', 'displays', 'virtuals', 'tasks', 'control_cues', 'assets']);
const isObj = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const keyedList = (list) => list.every((e) => isObj(e) && typeof e.id === 'string' && e.id);

/** The operations that turn document `a` into document `b`. Removals come
 *  first: a cue moved to another track is a removal and an insertion, and
 *  the insertion of an id that still exists elsewhere would be skipped. */
function diffDoc(a, b) {
  const ops = diffInto(a, b, [], []);
  return [...ops.filter((o) => o.op === 'remove'), ...ops.filter((o) => o.op !== 'remove')];
}

function diffInto(a, b, path, out) {
  if (a === b) return out;
  if (isObj(a) && isObj(b)) {
    for (const k of Object.keys(b)) {
      if (b[k] === undefined) continue;
      if (a[k] === undefined) out.push({ op: 'set', path: [...path, k], value: clone(b[k]) });
      else diffInto(a[k], b[k], [...path, k], out);
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
    for (const e of b) if (byId.has(e.id)) diffInto(byId.get(e.id), e, [...path, { id: e.id }], out);
    return out;
  }
  if (JSON.stringify(a) !== JSON.stringify(b)) out.push({ op: 'set', path, value: clone(b) });
  return out;
}

/** Apply operations to `doc` in place, skipping those whose target is gone
 *  (the same rules as the Director's). Returns the document. */
function applyOps(doc, ops) {
  const find = (list, id) => (Array.isArray(list) && typeof id === 'string' && id ? list.findIndex((e) => isObj(e) && e.id === id) : -1);
  const own = (o, k) => isObj(o) && Object.hasOwn(o, k);
  // Ids are unique among all lists of a kind (edit.rs: id_exists).
  const exists = (v, kind, id) => (Array.isArray(v) ? v.some((e) => exists(e, kind, id))
    : isObj(v) ? Object.entries(v).some(([k, x]) => (k === kind && Array.isArray(x) && x.some((e) => isObj(e) && e.id === id)) || exists(x, kind, id))
    : false);
  const walk = (path, create) => {
    let here = doc;
    for (let n = 0; n < path.length; n++) {
      const step = path[n];
      if (typeof step === 'string') {
        if (!isObj(here)) return undefined;
        if (create && !own(here, step)) {
          const next = n + 1 < path.length ? path[n + 1] : create;
          here[step] = typeof next === 'string' ? {} : [];
        }
        if (!own(here, step)) return undefined;
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
    if (path[0] === '_edit' || path[0] === '_applied') continue; // the Director's records
    if (op.op === 'set') {
      if (!path.length) { doc = clone(op.value); continue; }
      const p = walk(parent, last);
      if (typeof last === 'string' && isObj(p)) p[last] = clone(op.value);
      else if (last && typeof last === 'object' && op.value?.id === last.id) { const i = find(p, last.id); if (i >= 0) p[i] = clone(op.value); }
    } else if (op.op === 'unset') {
      const p = walk(parent);
      if (own(p, last)) delete p[last];
    } else if (op.op === 'insert') {
      const kind = [...path].reverse().find((p) => typeof p === 'string');
      if (exists(doc, kind, op.value?.id)) continue;
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

/** Make `old` equal to `next`, keeping the identity of every object (and
 *  of every list element with the same id) that exists in both. Returns
 *  what should replace `old` (itself, when the kinds match). */
function reconcile(old, next, kind = null, index = null) {
  // Every list element of the old show by kind and id, wherever it is: an
  // object someone moved to another list (a cue to another track) keeps its
  // identity too.
  if (!index) {
    index = new Map();
    const walk = (v, k) => {
      if (Array.isArray(v)) { for (const e of v) { if (KEYED.has(k) && isObj(e) && e.id) index.set(`${k}\u0000${e.id}`, e); walk(e, null); } }
      else if (isObj(v)) for (const [kk, x] of Object.entries(v)) walk(x, kk);
    };
    walk(old, null);
  }
  if (isObj(old) && isObj(next)) {
    for (const k of Object.keys(old)) if (!Object.hasOwn(next, k)) delete old[k];
    for (const [k, v] of Object.entries(next)) old[k] = Object.hasOwn(old, k) ? reconcile(old[k], v, k, index) : reconcile(undefined, v, k, index);
    return old;
  }
  if (Array.isArray(next) && KEYED.has(kind) && keyedList(next)) {
    const merged = next.map((e) => {
      const was = index.get(`${kind}\u0000${e.id}`);
      return was ? reconcile(was, e, null, index) : e;
    });
    if (!Array.isArray(old)) return merged;
    old.splice(0, old.length, ...merged);
    return old;
  }
  return next;
}

/** Show the Director's copy with our pending edits on top. Called when a
 *  show arrives, and after a drag that held one back. */
function rebase() {
  if (!S.server) return;
  // Mid-drag nothing may move under the pointer: after the drag.
  if (drag || tlDrag) { S.deferredShow = true; return; }
  // Local changes not yet turned into an edit go first, so they are kept.
  // If they cannot be sent (no connection), keep everything as it is until
  // the connection is back: replacing the show now would lose them.
  if (S.doc && JSON.stringify(S.doc) !== S.lastSent) sendDoc(true);
  if (S.dirty) { S.deferredShow = true; return; }
  let doc = clone(S.server);
  for (const p of S.pending) doc = applyOps(doc, p.ops);
  const next = normalize(doc);
  const changed = JSON.stringify(next) !== JSON.stringify(S.doc);
  S.deferredShow = false;
  if (!changed) return;
  // Merge into the objects already on screen rather than replacing them:
  // the inspector's fields and a drag hold references to them, and must
  // keep writing into the live show.
  S.doc = reconcile(S.doc, next);
  S.lastSent = JSON.stringify(S.doc);
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
  ws.onclose = (ev) => {
    if (S.connected) {
      // Was connected: say why it was lost. A freeze of this window just
      // before is the usual cause (the Director drops a client that sends
      // nothing, not even its pings, for 6 s), not the Director or the
      // network.
      const stall = S.lastStall && performance.now() - S.lastStall.at < 15000 ? S.lastStall : null;
      console.warn(`Director connection lost (code ${ev.code})`, stall ? `after a ${Math.round(stall.ms)} ms freeze` : '');
      toast(stall
        ? `Director connection lost after this window was unresponsive for ${(stall.ms / 1000).toFixed(1)} s — reconnecting.`
        : `Director connection lost (code ${ev.code}) — reconnecting.`);
    }
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
        if (msg.show_revision == null || S.holdN) {
          // It has no show, or the whole show we sent (New, Open) may have
          // been lost on the way: send ours, whole.
          S.pending = [];
          S.replaceNext = true;
          sendDoc(true);
        } else {
          // Edits that were on their way when the link dropped may not have
          // arrived: send them again. The Director applies each edit number
          // only once, so one that did arrive is ignored. Then any made while
          // offline.
          for (const p of S.pending) command({ type: 'edit_show', ops: p.ops, tag: { by: S.clientId, n: p.n } });
          if (S.dirty || (S.doc && JSON.stringify(S.doc) !== S.lastSent)) sendDoc(true);
          if (S.deferredShow) rebase();
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
      // it. It records the number of our last edit it has applied
      // (`_applied`): those edits are no longer pending; the rest are
      // re-applied on top.
      const applied = msg.document._applied?.[S.clientId]?.[0] ?? 0;
      delete msg.document._applied;
      delete msg.document._edit; // older Directors
      if (S.holdN) {
        if (applied < S.holdN) break; // our whole show is not in yet
        S.holdN = 0;
      }
      S.pending = S.pending.filter((p) => p.n > applied);
      S.server = msg.document;
      if (!S.doc) { S.doc = normalize(clone(S.server)); S.lastSent = JSON.stringify(S.doc); refreshAll(); reconcileAssets(); break; }
      rebase();
      break;
    }
    case 'presence':
      if (msg.node_id === S.clientId) break; // our own, relayed back
      if (msg.presence) S.others.set(msg.node_id, { ...msg.presence, color: safeColor(msg.presence.color) });
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
      if (msg.edit?.by === S.clientId) {
        // One of our edits (or our whole show) was refused: drop it — only
        // it — and show the shared show with the rest on top.
        if (msg.edit.n === S.holdN) S.holdN = 0;
        S.pending = S.pending.filter((p) => p.n !== msg.edit.n);
        rebase();
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
  pruneSelection();
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
  if (S.replaceNext || !S.server || !S.lastSent) {
    const tag = { by: S.clientId, n: ++S.editN };
    // A whole new show (New, Open, or the Director has none): replaces the
    // show for everyone.
    S.replaceNext = false;
    S.pending = [];
    S.holdN = tag.n;
    command({ type: 'load_show', document: S.doc, tag });
  } else {
    // Only what changed since the last send, as operations.
    const ops = diffDoc(JSON.parse(S.lastSent), S.doc);
    if (!ops.length) { S.dirty = false; S.lastSent = json; return; }
    const tag = { by: S.clientId, n: ++S.editN };
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
  pruneSelection();
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
  const cueIds = selCueIds();
  // A whole timeline is too much to lose to a stray Delete key.
  if (type === 'task' && !(await ask(`Delete the task "${tlName(id)}" with everything on it?`))) return;
  // A cue cannot show a virtual display that is gone (the Director would
  // refuse the show): its cues go with it, after asking.
  const showing = type === 'virtual' ? cuesShowing(id).length : 0;
  if (showing && !(await ask(`${showing} cue${showing > 1 ? 's show' : ' shows'} this virtual display. Delete ${showing > 1 ? 'them' : 'it'} too?`))) return;
  edit(() => {
    if (type === 'cue') for (const cid of cueIds) { const hit = findCue(cid); if (hit) hit.track.cues.splice(hit.ci, 1); }
    else if (type === 'control') { const hit = findControl(id); if (hit) hit.list.splice(hit.i, 1); }
    else if (type === 'track') { const hit = findTrack(id); if (hit) tlObj(hit.tl).tracks.splice(hit.ti, 1); }
    else if (type === 'task') {
      S.doc.tasks = S.doc.tasks.filter((t) => t.id !== id);
      // Control cues that acted on the task have nothing left to act on (and
      // the Director would refuse a show that still refers to it).
      for (const tl of tlIds()) { const o = tlObj(tl); o.control_cues = o.control_cues.filter((cc) => cc.target !== id); }
    }
    else if (type === 'display') S.doc.stage.displays = S.doc.stage.displays.filter((d) => d.id !== id);
    else if (type === 'virtual') {
      for (const tl of tlIds()) for (const t of tlObj(tl).tracks) t.cues = t.cues.filter((c) => !(c.source.type === 'virtual' && c.source.virtual === id));
      S.doc.stage.virtuals = S.doc.stage.virtuals.filter((v) => v.id !== id);
    }
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
  const entry = { id: item.id, name: item.name, kind, ...assetFacts(item) };
  const cue = {
    name: item.name, source: { type: 'asset', asset: item.id },
    duration_s: kind === 'video' && item.duration_s > 0 ? round2(item.duration_s) : 10,
    ...fitRect(item.width || 1920, item.height || 1080, false),
  };
  // Upsert before the cue is added: validation requires the asset to exist.
  S.doc.assets = S.doc.assets.filter((a) => a.id !== item.id).concat(entry);
  addCue(cue, tl, ti, t);
}

/** A cue showing virtual display `v`, at the open timeline's playhead, on
 *  track `ti` (the top one by default): at the virtual display's resolution
 *  (as in WATCHOUT), made smaller only if it would not fit the displays. */
function addVirtualCue(v, tl = S.tl, ti = 0, t = showSeconds(tl)) {
  addCue({ name: v.name || 'Virtual', source: { type: 'virtual', virtual: v.id }, duration_s: 10, ...fitRect(v.width, v.height, false) }, tl, ti, t);
}

/** A new virtual display: over the first display's area (or the canvas
 *  origin), at that area's size in pixels. */
function addVirtual() {
  const st = S.doc.stage, n = st.virtuals.length + 1;
  const r = st.displays[0]?.canvas_rect || { x: 0, y: 0, w: 1920, h: 1080 };
  edit(() => {
    const v = { id: uid(), name: `Virtual ${n}`, canvas_rect: { ...r }, width: Math.round(r.w), height: Math.round(r.h) };
    st.virtuals.push(v);
    S.sel = { type: 'virtual', id: v.id };
  });
}

/** A new task, on top: the first task is the front-most (as a new track is
 *  added above the others). */
function addTask() {
  edit(() => {
    const task = { id: 'task-' + uid(), name: `Task ${S.doc.tasks.length + 1}`, duration_s: 30, tracks: [{ id: uid(), name: 'Track 1', cues: [] }], control_cues: [] };
    S.doc.tasks.unshift(task);
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
    const was = S.library;
    S.library = null;
    if (was !== null) renderAssets();
  }
}

/** What the show records about a library item: the file Runners play — the
 *  optimised version once the Asset Manager has made it, the original until
 *  then — and the media's dimensions and duration. */
function assetFacts(item) {
  return {
    url: S.assetsUrl + (item.play_url || `/stream/${item.id}`),
    size: item.play_size ?? item.size,
    path: item.play_path || item.path,
    width: item.width || 0, height: item.height || 0,
    duration_s: item.duration_s || 0,
  };
}

/** A file replaced on the Asset Manager keeps its id (ids come from the
 *  name) but not its size, dimensions or duration; an optimised version, when
 *  it is ready, has another URL, size and path. The show records those — URL
 *  and size are every Runner's cache key — so bring the show's copy up to
 *  date; Runners then fetch the new file. */
function reconcileAssets() {
  if (!S.doc || !S.library) return;
  const keys = ['url', 'size', 'path', 'width', 'height', 'duration_s'];
  const facts = (x) => keys.map((k) => x[k] ?? '').join('|');
  const stale = S.doc.assets
    .map((a) => [a, S.library.find((i) => i.id === a.id)])
    .filter(([a, item]) => item && facts(assetFacts(item)) !== facts(a));
  if (!stale.length) return;
  // Not an undo step: it is the Asset Manager's change, not the user's —
  // undoing it would point Runners back at a file that is gone, and
  // recording it would wipe the user's redo history every time a file is
  // optimised. Undo steps hold operations, so the user's own steps still
  // undo correctly around it.
  for (const [a, item] of stale) Object.assign(a, assetFacts(item));
  afterChange(false);
  toast(`${stale.map(([a]) => a.name).join(', ')}: new version on the Asset Manager — show updated.`, true);
}

/** One line on where the optimiser is with an item, for the asset list. */
function optimizeBadge(item) {
  const o = item.optimized;
  if (!o || o.state === 'original') return null;
  const label = { hap: 'HAP', hap_alpha: 'HAP Alpha', hap_q: 'HAP Q', rechunk: 're-chunked', keep: 'original' };
  const how = o.rule ? `${label[o.rule.output] || o.rule.output}, ${o.rule.chunks} chunks${o.rule.snappy ? '' : ', no Snappy'}` : '';
  const text = {
    pending: 'waiting to optimise',
    optimizing: `optimising ${Math.round(100 * (o.progress || 0))} %`,
    done: o.output ? `optimised: ${how}` : (o.rule?.output === 'keep' ? 'played as it is' : `already ${how}`),
    failed: 'optimisation failed',
  }[o.state] || o.state;
  return el('span', { class: `opt opt-${o.state}`, title: o.error || '' }, text);
}

async function reoptimize(item) {
  try {
    const res = await fetch(`${S.assetsUrl}/api/items/${item.id}/optimize`, { method: 'POST' });
    if (!res.ok) toast(`Could not re-optimise ${item.name}: ${await res.text()}`);
    else toast(`${item.name} will be optimised again`, true);
  } catch {
    toast(`Could not re-optimise ${item.name}: Asset Manager not reachable`);
  }
  loadLibrary();
}

// ───────────────────── optimisation settings (codec map) ─────────────────────

const SOURCES = [
  ['h264', 'H.264'], ['hevc', 'HEVC / H.265'], ['prores', 'ProRes'], ['prores_alpha', 'ProRes 4444 with alpha'],
  ['hap', 'HAP'], ['hap_alpha', 'HAP Alpha'], ['hap_q', 'HAP Q'], ['hap_r', 'HAP R (BC7)'], ['hap_q_alpha', 'HAP Q Alpha'],
  ['notchlc', 'NotchLC'], ['dxv', 'DXV'], ['other', 'Anything else'],
];
const HAP_SOURCES = ['hap', 'hap_alpha', 'hap_q', 'hap_r'];

/** The Asset Manager's codec map, as a table: for each source codec, what it
 *  becomes, in how many chunks, Snappy or not. */
async function openOptimizeSettings() {
  let settings;
  try {
    const res = await fetch(`${S.assetsUrl}/api/optimize`);
    if (!res.ok) throw new Error(await res.text());
    settings = await res.json();
  } catch (e) {
    return toast(`Cannot read the optimisation settings: ${e.message || 'Asset Manager not reachable'}`);
  }
  const body = $('#opt-rows');
  body.replaceChildren();
  for (const [src, label] of SOURCES) {
    const rule = settings.rules[src];
    const outputs = [['keep', 'Keep as it is']];
    // Lossless re-chunking needs a single-texture HAP file.
    if (HAP_SOURCES.includes(src)) outputs.push(['rechunk', 'Re-chunk (lossless)']);
    // HAP Q Alpha cannot be written by ffmpeg, nor safely converted.
    if (src !== 'hap_q_alpha') outputs.push(['hap', 'HAP'], ['hap_alpha', 'HAP Alpha'], ['hap_q', 'HAP Q']);
    const output = el('select', {}, ...outputs.map(([v, t]) => el('option', { value: v }, t)));
    output.value = rule.output;
    const chunks = el('input', { type: 'number', min: '1', max: '64', step: '1', value: String(rule.chunks) });
    const snappy = el('input', { type: 'checkbox' });
    snappy.checked = rule.snappy;
    const sync = () => { chunks.disabled = snappy.disabled = output.value === 'keep'; };
    output.onchange = sync;
    sync();
    const row = el('tr', {}, el('td', {}, label), el('td', {}, output), el('td', {}, chunks), el('td', {}, snappy));
    row.read = () => ({ output: output.value, chunks: Math.max(1, Math.min(64, parseInt(chunks.value, 10) || 1)), snappy: snappy.checked });
    row.dataset.src = src;
    body.append(row);
  }
  $('#opt-enabled').checked = settings.enabled;
  $('#opt-dialog').hidden = false;

  const save = async (apply) => {
    const rules = {};
    for (const row of body.children) rules[row.dataset.src] = row.read();
    try {
      const res = await fetch(`${S.assetsUrl}/api/optimize`, {
        method: 'PUT', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ enabled: $('#opt-enabled').checked, rules }),
      });
      if (!res.ok) throw new Error(await res.text());
      if (apply) {
        const r = await fetch(`${S.assetsUrl}/api/optimize/apply`, { method: 'POST' });
        const { queued } = await r.json();
        toast(`Settings saved; ${queued} file${queued === 1 ? '' : 's'} will be optimised again.`, true);
      } else {
        toast('Settings saved: they apply to files optimised from now on.', true);
      }
      $('#opt-dialog').hidden = true;
      loadLibrary();
    } catch (e) {
      toast(`Could not save the optimisation settings: ${e.message || 'Asset Manager not reachable'}`);
    }
  };
  $('#opt-save').onclick = () => save(false);
  $('#opt-apply').onclick = () => save(true);
  $('#opt-cancel').onclick = () => ($('#opt-dialog').hidden = true);
}

/** The asset list: files being uploaded from this window first, greyed out
 *  with a bar under each, then the Asset Manager's library. */
function renderAssets() {
  const list = $('#asset-list');
  list.replaceChildren(...S.uploads.map(uploadRow));
  if (S.library === null) return list.append(el('div', { class: 'empty' }, `Asset Manager not reachable at ${S.assetsUrl}`));
  if (!S.library.length && !S.uploads.length) return list.append(el('div', { class: 'empty' }, 'No media yet. Upload files, or drop them here.'));
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
      el('div', {}, el('div', { class: 'name' }, item.name), el('div', { class: 'meta' }, meta, optimizeBadge(item))),
      el('div', { class: 'btns' },
        item.kind === 'video' ? el('button', { title: 'Optimise again, with the current settings', onclick: (e) => { e.stopPropagation(); reoptimize(item); } }, '↻') : null,
        el('button', { class: 'danger', title: 'Delete from the Asset Manager', onclick: (e) => { e.stopPropagation(); deleteAsset(item); } }, '✕')),
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

// ───────────────────────────── uploads ─────────────────────────────
// Every file given to this window (Upload… or dropped on the list) joins
// one queue, S.uploads, and is listed at once at the top of the asset list:
// greyed out (it cannot be cued yet) with a bar under it. Files go one at a
// time, in the order given. On one link, several at once would not finish
// the batch sooner; one at a time gets the first file stored, and its
// optimisation started, as soon as possible.
//
// An upload is one of:
//   waiting  — queued behind another;         ✕ removes it
//   sending  — bar = bytes sent;               ✕ stops it (the Asset Manager
//              deletes what it had received:  it stores under a hidden
//              temporary name until the last byte)
//   stored   — the Asset Manager has the file; the row goes as soon as the
//              library lists it as a normal item (where optimisation shows)
//   failed   — the row stays, in red, with the reason: ↻ sends it again
//              (from the start), ✕ removes it
// A cancelled upload is not kept: the operator asked for it.
//
// XMLHttpRequest rather than fetch because it reports upload progress.

/** "640 KB", "2.9 MB", "850 MB", "3.40 GB". */
const mb = (b) => (b >= 1e9 ? `${(b / 1e9).toFixed(2)} GB` : b >= 1e6 ? `${(b / 1e6).toFixed(b < 1e8 ? 1 : 0)} MB` : `${Math.round(b / 1e3)} KB`);

function uploadFiles(files) {
  for (const file of files) S.uploads.push({ key: uid(), file, name: file.name, size: file.size, sent: 0, state: 'waiting' });
  renderAssets();
  pumpUploads();
}

/** Start the next waiting upload, unless one is being sent. */
function pumpUploads() {
  if (S.uploads.some((u) => u.state === 'sending')) return;
  const u = S.uploads.find((x) => x.state === 'waiting');
  if (u) sendUpload(u);
}

function sendUpload(u) {
  const xhr = new XMLHttpRequest();
  Object.assign(u, { xhr, state: 'sending', sent: 0, error: '', started: performance.now() });
  xhr.open('POST', `${S.assetsUrl}/api/upload?name=${encodeURIComponent(u.name)}`);
  xhr.upload.onprogress = (e) => { u.sent = e.loaded; updateUploadRow(u); };
  xhr.onload = async () => {
    if (xhr.status >= 300) return uploadFailed(u, errorText(xhr));
    u.state = 'stored';
    u.sent = u.size;
    updateUploadRow(u);
    pumpUploads();
    let kind = '';
    try { kind = JSON.parse(xhr.responseText).kind; } catch { /* only used for the message */ }
    // The library now has the file: list it, then drop the upload row, so
    // the row is replaced rather than disappearing and coming back.
    await loadLibrary();
    dropUpload(u);
    toast(kind === 'audio' ? `Uploaded ${u.name}: audio files are stored but cannot be cued yet, so they are not listed.` : `Uploaded ${u.name}`, true);
  };
  // No response at all: the Asset Manager went away, or closed the
  // connection (it may refuse a file before reading it).
  xhr.onerror = () => uploadFailed(u, 'the connection to the Asset Manager was lost');
  xhr.send(u.file);
  updateUploadRow(u);
}

/** The Asset Manager's reason for refusing a request: its JSON `error`, or
 *  the body as it is. */
function errorText(xhr) {
  try { return JSON.parse(xhr.responseText).error || xhr.responseText; } catch { return xhr.responseText || `HTTP ${xhr.status}`; }
}

function uploadFailed(u, reason) {
  Object.assign(u, { state: 'failed', error: reason, xhr: null });
  updateUploadRow(u);
  toast(`Upload of ${u.name} failed: ${reason}`);
  pumpUploads();
}

/** ✕ on an upload row: stop it if it is being sent, forget it either way. */
function cancelUpload(u) {
  if (u.state === 'stored') return; // already on the Asset Manager; the row is about to go
  if (u.state === 'sending') {
    u.xhr.onerror = null;
    u.xhr.abort();
    toast(`Upload of ${u.name} cancelled`, true);
  }
  dropUpload(u);
  pumpUploads();
}

function retryUpload(u) {
  Object.assign(u, { state: 'waiting', sent: 0, error: '' });
  updateUploadRow(u);
  pumpUploads();
}

function dropUpload(u) {
  S.uploads = S.uploads.filter((x) => x !== u);
  renderAssets();
}

/** An upload's row in the asset list. The parts that change while bytes go
 *  out are kept on the upload (u.ui) and updated in place: progress events
 *  come many times a second and must not rebuild the list. */
function uploadRow(u) {
  const ext = (u.name.match(/\.([^.]+)$/)?.[1] || '?').slice(0, 4).toUpperCase();
  const replaces = S.library?.some((i) => i.name === u.name);
  const status = el('span', { class: 'up-status' });
  const fill = el('div', { class: 'fill' });
  const retry = el('button', { title: 'Upload again, from the start', onclick: () => retryUpload(u) }, '↻');
  const cancel = el('button', { class: 'danger', onclick: () => cancelUpload(u) }, '✕');
  const row = el('div', { class: 'asset upload', dataset: { upload: u.key } },
    el('div', { class: 'up-thumb' }, ext),
    el('div', {}, el('div', { class: 'name' }, u.name),
      el('div', { class: 'meta' }, status, replaces ? el('span', { class: 'up-note' }, 'replaces the file of the same name') : null)),
    el('div', { class: 'btns' }, retry, cancel),
    el('div', { class: 'bar' }, fill));
  u.ui = { row, status, fill, retry, cancel };
  updateUploadRow(u);
  return row;
}

function updateUploadRow(u) {
  if (!u.ui) return;
  const { row, status, fill, retry, cancel } = u.ui;
  const frac = u.size ? Math.min(1, u.sent / u.size) : u.state === 'stored' ? 1 : 0;
  // The status is a few parts joined by " · ". In the (narrow) Assets
  // window it wraps between parts, never inside one: no-break spaces.
  let parts;
  if (u.state === 'waiting') parts = ['waiting to upload', mb(u.size)];
  else if (u.state === 'stored') parts = ['uploaded', 'adding to the list'];
  else if (u.state === 'failed') parts = [];
  else {
    const secs = (performance.now() - u.started) / 1000;
    const rate = secs > 0.5 ? u.sent / secs : 0;           // average since the start: steady on a LAN
    const left = rate > 0 ? (u.size - u.sent) / rate : null;
    parts = [`uploading ${Math.floor(frac * 100)} %`, `${mb(u.sent)} / ${mb(u.size)}`];
    if (rate) parts.push(`${mb(rate)}/s`);
    if (left != null) parts.push(left >= 90 ? `${Math.round(left / 60)} min left` : `${Math.ceil(left)} s left`);
  }
  // A failure's reason is a sentence: it wraps normally.
  const text = u.state === 'failed' ? `upload failed: ${u.error}` : parts.map((x) => x.replace(/ /g, '\u00a0')).join(' · ');
  row.className = `asset upload up-${u.state}`;
  row.title = u.name;
  status.textContent = text;
  fill.style.width = `${(frac * 100).toFixed(1)}%`;
  retry.hidden = u.state !== 'failed';
  cancel.hidden = u.state === 'stored';
  cancel.title = u.state === 'sending' ? 'Stop this upload' : 'Remove from the list';
}

// Leaving the page stops the uploads: ask first.
window.addEventListener('beforeunload', (e) => {
  if (S.uploads.some((u) => u.state === 'waiting' || u.state === 'sending')) { e.preventDefault(); e.returnValue = ''; }
});

// ───────────────────────────── preview media ─────────────────────────────
// What the stage draws for a cue. This is a PREVIEW of the content, played
// by the browser — not the Runner's picture. It is kept in step with the
// timeline the same way a Runner is: nothing "plays along"; every frame we
// compute where the media should be and correct the element if it is not.
//
//   image cue → the image itself (its thumbnail until it has loaded)
//   video cue → a muted <video>, one per cue, steered to the right time
//
// Videos are previewed from the Asset Manager's preview proxy (a small
// H.264 copy), so HAP and ProRes play here too; until the proxy is ready,
// or if it cannot be made, the thumbnail is shown.

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
      // The Asset Manager's preview proxy (a small H.264 copy, made once)
      // plays in any web view whatever the original's codec (HAP, ProRes)
      // and is far lighter to decode than a 4K original. Its address is
      // given to the <video> only once it exists (see proxySource): until
      // then the thumbnail shows, and no connection waits on an encode.
      p.node = Object.assign(document.createElement('video'), { muted: true, playsInline: true, preload: 'auto' });
      p.proxy = asset.url.replace('/stream/', '/proxy/');
      p.nextCheck = 0;
      // No proxy after all: the original, which plays if it is H.264.
      p.node.addEventListener('error', () => {
        if (p.node.src.includes('/proxy/')) p.node.src = asset.url;
        else p.failed = true;
      });
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

/** Give a preview video its source once the Asset Manager has its proxy
 *  (asking every few seconds; each question is answered at once). */
function proxySource(p, asset) {
  if (p.node.getAttribute('src') || p.checking || performance.now() < p.nextCheck) return;
  p.checking = true;
  fetch(`${p.proxy}?status=1`)
    .then((r) => (r.ok ? r.json() : { state: 'failed' }))
    .then(({ state }) => {
      if (state === 'ready') p.node.src = p.proxy;
      else if (state === 'failed') p.node.src = asset.url;
      else p.nextCheck = performance.now() + 3000;
    })
    .catch(() => { p.nextCheck = performance.now() + 5000; })
    .finally(() => { p.checking = false; });
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
  if (asset.kind === 'video') proxySource(p, asset);
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
function drawSyncTest(g, cue, t) {
  const fps = cue.source.fps || 60;
  const frame = Math.floor(Math.max(0, cue.in_s + t - cue.start_s) * fps + 1e-6);
  const flash = frame % Math.max(1, Math.round(fps)) === 0;
  g.fillStyle = flash ? '#fff' : '#000';
  g.fillRect(-cue.w / 2, -cue.h / 2, cue.w, cue.h);
  g.fillStyle = flash ? '#000' : '#fff';
  g.font = `bold ${Math.max(8, cue.h / 4)}px ui-monospace, monospace`;
  g.textAlign = 'center';
  g.textBaseline = 'middle';
  g.fillText(String(frame), 0, 0);
  g.font = `${Math.max(6, cue.h / 14)}px system-ui, sans-serif`;
  g.fillText('SYNC TEST', 0, -cue.h / 3);
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
/** The theme colours the stage draws with, read from style.css (the
 *  stylesheet loads before this script), so the canvas follows the CSS. */
const THEME = (() => {
  const css = getComputedStyle(document.documentElement);
  const v = (name, fallback) => css.getPropertyValue(name).trim() || fallback;
  return { accent: v('--accent', '#0066ff'), accentLine: v('--accent-line', '#3385ff'), onAccent: v('--on-accent', '#fff'), hi: v('--hi', '#0ff'), onHi: v('--on-hi', '#002626') };
})();
let view = { k: 1, ox: 0, oy: 0 };  // canvas px → screen px: s = c·k + o
let displayTabs = [];               // screen rects of display (and virtual display) name tabs, for hit-testing
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

  virtualPictures.clear(); // drawn afresh this frame, when first needed
  drawCues(ctx, k);
  ctx.restore();

  // Displays: outlines and name tabs, drawn in screen space so line widths
  // and text stay readable at any zoom.
  displayTabs = [];
  ctx.font = '11px system-ui, sans-serif';
  for (const d of st.displays) {
    const r = d.canvas_rect;
    const x = view.ox + r.x * k, y = view.oy + r.y * k;
    const selected = S.sel?.type === 'display' && S.sel.id === d.id;
    ctx.strokeStyle = selected ? THEME.hi : THEME.accentLine;
    ctx.lineWidth = selected ? 2 : 1;
    ctx.setLineDash(selected ? [] : [6, 4]);
    ctx.strokeRect(x + 0.5, y + 0.5, r.w * k, r.h * k);
    ctx.setLineDash([]);
    const label = (d.name || 'display') + (d.runner ? ` @${d.runner}` : '') + (d.test_pattern ? ` · TEST ${d.test_pattern.fps} fps` : '');
    const tw = ctx.measureText(label).width + 10;
    ctx.fillStyle = selected ? THEME.hi : THEME.accent;
    ctx.fillRect(x, y - 16, tw, 16);
    ctx.fillStyle = selected ? THEME.onHi : THEME.onAccent;
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

  // Virtual displays: dashed green outlines, with a tab (name, resolution)
  // to select and move them by, like a display's.
  for (const v of st.virtuals) {
    const r = v.canvas_rect;
    const x = view.ox + r.x * k, y = view.oy + r.y * k;
    const selected = S.sel?.type === 'virtual' && S.sel.id === v.id;
    const colour = selected ? THEME.hi : '#3ddc97';
    ctx.strokeStyle = colour;
    ctx.lineWidth = selected ? 2 : 1;
    ctx.setLineDash([3, 3]);
    ctx.strokeRect(x + 0.5, y + 0.5, r.w * k, r.h * k);
    ctx.setLineDash([]);
    const label = `⧉ ${v.name || 'virtual'} · ${v.width}×${v.height}`;
    const tw = ctx.measureText(label).width + 10;
    ctx.fillStyle = colour;
    ctx.fillRect(x, y + r.h * k, tw, 16); // below the area: a display's tab may be above it
    ctx.fillStyle = THEME.onHi; // dark text: on the green tab and on the cyan one
    ctx.fillText(label, x + 5, y + r.h * k + 12);
    displayTabs.push({ id: v.id, kind: 'virtual', x, y: y + r.h * k, w: tw, h: 16 });
    othersOn('virtual', v.id).forEach((o, i) => {
      const pad = 3 + 3 * i;
      ctx.strokeStyle = o.color;
      ctx.lineWidth = 2;
      ctx.strokeRect(x - pad, y - pad, r.w * k + 2 * pad, r.h * k + 2 * pad);
    });
  }

  // Resize handles of the selected cue (only unrotated: resizing a rotated
  // box by its corners needs more maths than a first version deserves).
  const sel = selectedCue();
  if (sel && !sel.rotation) {
    ctx.fillStyle = THEME.hi;
    for (const [hx, hy] of cueCorners(sel)) ctx.fillRect(hx - HANDLE, hy - HANDLE, HANDLE * 2, HANDLE * 2);
  }
}

/** Draw the cues of every timeline, back to front, on `g`, which is set up
 *  in canvas coordinates (`k` screen px per canvas px). The main timeline
 *  first, then the tasks (the first task is front-most); within a timeline
 *  the last track is at the back.
 *
 *  On the stage (`visiting` null) selected cues are also shown when off
 *  air, and selections are outlined. Into a virtual display's picture
 *  (`visiting`: the virtual displays being drawn, innermost last), only the
 *  cues on air in its `area`, as a Runner draws them. */
function drawCues(g, k, { area = null, visiting = null } = {}) {
  const onStage = !visiting;
  const sels = new Set(onStage ? selectedCues() : []);
  for (const tl of tlIds().reverse()) {
    const t = showSeconds(tl), active = isActive(tl);
    for (const track of [...tlObj(tl).tracks].reverse()) {
      for (const cue of track.cues) {
        const onAir = active && t >= cue.start_s && t < cue.start_s + cue.duration_s;
        // Selected cues are always shown (ghosted when not on air), so they
        // can be positioned without scrubbing to them first.
        if (onStage && !onAir) S.onsets.delete(cue.id); // on air again later: afresh
        const others = onStage ? othersOn('cue', cue.id) : [];
        if (!onAir && !sels.has(cue) && !others.length) continue;
        if (area && !cueReaches(cue, area)) continue;
        // In a virtual display's picture, a cue showing a virtual display
        // being drawn is left out (a loop; the Runners do the same).
        const picture = cue.source.type === 'asset' ? cuePicture(cue, track, onAir ? t : cue.start_s, tl, onAir)
          : cue.source.type === 'virtual' ? virtualPicture(cue.source.virtual, visiting) : null;
        if (!onStage && cue.source.type === 'virtual' && !picture) continue;
        const fade = clamp(cue.fade_in_s > 0 ? (t - cue.start_s) / cue.fade_in_s : 1, 0, 1)
                   * clamp(cue.fade_out_s > 0 ? (cue.start_s + cue.duration_s - t) / cue.fade_out_s : 1, 0, 1);
        g.save();
        g.translate(cue.x + cue.w / 2, cue.y + cue.h / 2);
        g.rotate((cue.rotation * Math.PI) / 180);
        g.globalAlpha = onAir ? cue.opacity * fade : 0.3;
        if (picture) g.drawImage(picture, -cue.w / 2, -cue.h / 2, cue.w, cue.h);
        else if (cue.source.type === 'sync_test') drawSyncTest(g, cue, onAir ? t : cue.start_s);
        else {
          g.fillStyle = cue.source.type === 'color' ? rgbaCss(cue.source.rgba) : '#444';
          g.fillRect(-cue.w / 2, -cue.h / 2, cue.w, cue.h);
        }
        if (sels.has(cue)) {
          g.globalAlpha = 1;
          g.strokeStyle = THEME.hi;
          g.lineWidth = 2 / k;
          g.strokeRect(-cue.w / 2, -cue.h / 2, cue.w, cue.h);
        }
        // Another user's selection: their colour, their name; dashed while
        // they are dragging it.
        others.forEach((o, i) => {
          g.globalAlpha = 1;
          g.strokeStyle = o.color;
          g.lineWidth = 2 / k;
          g.setLineDash(o.dragging ? [6 / k, 4 / k] : []);
          const pad = (3 + 3 * i) / k;
          g.strokeRect(-cue.w / 2 - pad, -cue.h / 2 - pad, cue.w + 2 * pad, cue.h + 2 * pad);
          g.setLineDash([]);
          g.font = `${12 / k}px system-ui, sans-serif`;
          g.fillStyle = o.color;
          g.fillText(o.name, -cue.w / 2, -cue.h / 2 - pad - 4 / k - i * 14 / k);
        });
        g.restore();
      }
    }
  }
}

/** Whether a cue may cover part of rect `r` (taken as a circle as wide as
 *  its diagonal around its centre, which covers any rotation — the Runner's
 *  rule too). */
function cueReaches(c, r) {
  const reach = Math.hypot(c.w, c.h), cx = c.x + c.w / 2, cy = c.y + c.h / 2;
  return cx + reach >= r.x && cx - reach <= r.x + r.w && cy + reach >= r.y && cy - reach <= r.y + r.h;
}

// Virtual displays in the stage preview: each is drawn, when a cue on the
// stage first needs it in a frame, into a canvas of its own (at most
// VIRTUAL_PREVIEW px wide or high: a sketch, the Runners render the real
// picture at its resolution), transparent where nothing is on air.
const VIRTUAL_PREVIEW = 640;
const virtualCanvases = new Map(); // virtual id → <canvas>, reused
const virtualPictures = new Map(); // virtual id → <canvas> drawn this frame

/** The picture of virtual display `id` this frame, or null if it is one of
 *  those being drawn (`visiting`): a loop. */
function virtualPicture(id, visiting) {
  if (visiting?.includes(id)) return null;
  if (virtualPictures.has(id)) return virtualPictures.get(id);
  const v = findVirtual(id);
  if (!v) return null;
  const scale = Math.min(1, VIRTUAL_PREVIEW / v.width, VIRTUAL_PREVIEW / v.height);
  const w = Math.max(1, Math.round(v.width * scale)), h = Math.max(1, Math.round(v.height * scale));
  let c = virtualCanvases.get(id);
  if (!c) { c = document.createElement('canvas'); virtualCanvases.set(id, c); }
  if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
  const g = c.getContext('2d');
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.clearRect(0, 0, w, h);
  const r = v.canvas_rect, kx = w / r.w, ky = h / r.h;
  g.setTransform(kx, 0, 0, ky, -r.x * kx, -r.y * ky);
  drawCues(g, Math.min(kx, ky), { area: r, visiting: [...(visiting || []), id] });
  virtualPictures.set(id, c);
  return c;
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
  const sel = selectedCue(), sels = new Set(selectedCues());
  if (sel && !sel.rotation) {
    const corner = cueCorners(sel).findIndex(([hx, hy]) => Math.abs(hx - sx) <= HANDLE + 2 && Math.abs(hy - sy) <= HANDLE + 2);
    if (corner >= 0) { drag = { ...start, kind: 'resize', corner, target: sel, orig: { ...sel } }; return; }
  }
  // 2. A display's name tab? (Displays are picked by their tab, so that
  //    clicking inside one still reaches the cues it shows.)
  const tab = displayTabs.find((t) => sx >= t.x && sx <= t.x + t.w && sy >= t.y && sy <= t.y + t.h);
  if (tab) {
    // A display or a virtual display: both are moved by their canvas_rect.
    const d = tab.kind === 'virtual' ? findVirtual(tab.id) : findDisplay(tab.id);
    select({ type: tab.kind === 'virtual' ? 'virtual' : 'display', id: d.id });
    drag = { ...start, kind: 'display', target: d, orig: { ...d.canvas_rect } };
    return;
  }
  // 3. The front-most cue under the pointer that is on air (or selected).
  for (const tl of tlIds()) {
    const t = showSeconds(tl), active = isActive(tl);
    for (const track of tlObj(tl).tracks) {
      for (const cue of [...track.cues].reverse()) {
        const visible = (active && t >= cue.start_s && t < cue.start_s + cue.duration_s) || sels.has(cue);
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
    if (e.shiftKey || c.keep_aspect) { // keep the aspect ratio (Shift, or the cue's own setting)
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
  // The stacking of the Tasks window, read left to right from the back:
  // Main (always the lowest) on the left, the front-most task on the right.
  for (const id of [MAIN, ...S.doc.tasks.map((t) => t.id).reverse()]) {
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
        const at = (e.clientX - lane.getBoundingClientRect().left) / pps;
        const item = S.library?.find((i) => i.id === e.dataTransfer.getData('text/lcplay-asset'));
        if (item) addAssetCue(item, S.tl, ti, at);
        const v = findVirtual(e.dataTransfer.getData('text/lcplay-virtual'));
        if (v) addVirtualCue(v, S.tl, ti, at);
      },
    });
    for (const cue of track.cues) {
      const selected = isCueSelected(cue.id);
      lane.append(el('div', {
        class: `cue ${cue.source.type}${cue.free_run ? ' free' : ''}${selected ? ' sel' : ''}`, dataset: { id: cue.id },
        style: `left:${cue.start_s * pps}px;width:${cue.duration_s * pps}px`, title: cue.name,
      }, cue.name || (cue.source.type === 'virtual' ? findVirtual(cue.source.virtual)?.name : null)
        || ({ color: 'colour', sync_test: 'sync test', virtual: 'virtual' }[cue.source.type] || 'cue'),
        el('div', { class: 'h l' }), el('div', { class: 'h r' })));
    }
    const selected = S.sel?.type === 'track' && S.sel.id === track.id;
    tracks.append(el('div', { class: 'track' },
      el('div', { class: `track-head${selected ? ' sel' : ''}`, title: 'Click to select the track', onclick: () => select({ type: 'track', id: track.id }) },
        el('span', { class: 'name' }, track.name || `Track ${ti + 1}`)),
      lane));
  });
}

/** preventDefault() on a mousedown (needed so a drag does not select page
 *  text) also stops it from taking the focus: leave a field being typed in
 *  ourselves, so its value is committed before the selection changes and
 *  the inspector is rebuilt. */
function leaveField() {
  const f = document.activeElement;
  if (f && ['INPUT', 'SELECT', 'TEXTAREA'].includes(f.tagName)) f.blur();
}

// One mousedown handler for all cues (they are rebuilt constantly, so
// per-element listeners would be lost mid-drag).
//
// Selecting several cues:
//   - drag a box on an empty part of the tracks: the cues it touches;
//   - Shift- or Ctrl-click a cue: add it to the selection, or take it out;
//   - Shift- or Ctrl-drag a box: add the cues it touches.
// Dragging any selected cue (or one of its ends) moves (trims, lengthens)
// all of them by the same amount; a plain click on one of them, without
// moving, selects that cue alone.
$('#tracks').addEventListener('mousedown', (e) => {
  if (e.button !== 0) return;
  const additive = e.shiftKey || e.ctrlKey || e.metaKey;
  const node = e.target.closest('.cue');
  if (!node) {
    if (e.target.classList.contains('lane')) {
      // Box selection. A plain click on an empty lane still deselects (on
      // mouseup, when the pointer has not moved).
      tlDrag = { kind: 'box', mx: e.clientX, my: e.clientY, additive, base: additive ? selCueIds() : [], moved: false, key: '', undone: false };
      leaveField();
      e.preventDefault();
    }
    return;
  }
  const hit = findCue(node.dataset.id);
  if (!hit) return;
  leaveField();
  e.preventDefault();
  const id = hit.cue.id;
  if (additive) {
    // Toggle this cue in (or out of) the cue selection. No drag.
    const ids = selCueIds();
    select(cueSelection(ids.includes(id) ? ids.filter((i) => i !== id) : [...ids, id], id));
    return;
  }
  const handle = e.target.classList.contains('h') ? (e.target.classList.contains('l') ? 'trim' : 'length') : 'move';
  // Pressing on a cue of a multiple selection keeps the selection (so the
  // whole group can be dragged); otherwise this cue alone is selected.
  const ids = isCueSelected(id) ? selCueIds() : [id];
  select(cueSelection(ids, id));
  const orig = new Map();
  for (const cid of ids) {
    const h = findCue(cid);
    if (h) orig.set(cid, { start_s: h.cue.start_s, duration_s: h.cue.duration_s, in_s: h.cue.in_s, ti: h.ti, tl: h.tl });
  }
  tlDrag = { kind: handle, id, ids: [...orig.keys()], mx: e.clientX, orig, narrow: ids.length > 1, undone: false };
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

  if (tlDrag.kind === 'box') return boxSelectMove(e);
  if (tlDrag.kind === 'control') {
    const hit = findControl(tlDrag.id);
    if (!hit) return;
    takeUndo();
    hit.cc.at_s = round2(clamp(o.at_s + dt, 0, tlObj(hit.tl).duration_s));
  } else {
    // Every cue of the selection gets the same change. `tlDrag.orig` holds
    // each cue's values from the moment the drag started.
    const hits = tlDrag.ids.map(findCue).filter(Boolean);
    const lead = findCue(tlDrag.id);
    if (!lead || !hits.length) return;
    takeUndo();
    const origOf = (h) => tlDrag.orig.get(h.cue.id);
    if (tlDrag.kind === 'move') {
      // The group moves as a block: it stops when its earliest cue reaches 0
      // (clamping each cue on its own would squash their spacing).
      const d = Math.max(dt, -Math.min(...hits.map((h) => origOf(h).start_s)));
      for (const h of hits) h.cue.start_s = round2(Math.max(0, origOf(h).start_s + d));
      // Dragging up/down moves the cues by as many tracks as the pointer
      // has moved from the cue it holds — only if every cue on that
      // timeline still lands on a track.
      const lane = document.elementFromPoint(e.clientX, e.clientY)?.closest('#tracks .lane');
      const step = lane && lead.tl === S.tl ? Number(lane.dataset.ti) - origOf(lead).ti : null;
      const movable = hits.filter((h) => h.tl === lead.tl);
      const tracks = tlObj(lead.tl).tracks;
      if (step != null && movable.every((h) => tracks[origOf(h).ti + step])) {
        for (const h of movable) {
          const target = tracks[origOf(h).ti + step];
          const now = findCue(h.cue.id); // indices change as cues move
          if (now.track !== target) { now.track.cues.splice(now.ci, 1); target.cues.push(now.cue); }
        }
      }
    } else if (tlDrag.kind === 'length') {
      for (const h of hits) h.cue.duration_s = round2(Math.max(0.1, origOf(h).duration_s + dt));
    } else {
      // Trim the start: the end stays where it is, and a video's in-point
      // moves with it so the picture at any given time does not change.
      for (const h of hits) {
        const o1 = origOf(h), c = h.cue;
        const d = clamp(dt, -o1.start_s, o1.duration_s - 0.1);
        c.start_s = round2(o1.start_s + d);
        c.duration_s = round2(o1.duration_s - d);
        if (assetOf(c)?.kind === 'video') c.in_s = round2(Math.max(0, o1.in_s + d));
      }
    }
    // A cue that now ends after its timeline makes the timeline longer.
    for (const h of hits) {
      const obj = tlObj(h.tl), grown = Math.max(obj.duration_s, Math.ceil(h.cue.start_s + h.cue.duration_s));
      if (grown !== obj.duration_s) { obj.duration_s = grown; if (h.tl === MAIN) syncLoop(); }
    }
  }
  renderTimeline();
  renderInspector();
  scheduleSend();
}

/** Box selection in progress: draw the box, and select the cues it touches
 *  (plus, with Shift/Ctrl, those that were selected before). */
function boxSelectMove(e) {
  const d = tlDrag;
  if (!d.moved && Math.abs(e.clientX - d.mx) + Math.abs(e.clientY - d.my) < 4) return; // still a click
  d.moved = true;
  const x0 = Math.min(d.mx, e.clientX), x1 = Math.max(d.mx, e.clientX);
  const y0 = Math.min(d.my, e.clientY), y1 = Math.max(d.my, e.clientY);
  const body = $('#tl-body'), at = body.getBoundingClientRect();
  let box = $('#tl-box');
  if (!box) { box = el('div', { id: 'tl-box' }); body.append(box); }
  Object.assign(box.style, { left: `${x0 - at.left}px`, top: `${y0 - at.top}px`, width: `${x1 - x0}px`, height: `${y1 - y0}px` });
  const hits = [...document.querySelectorAll('#tracks .cue')].filter((n) => {
    const r = n.getBoundingClientRect();
    return r.right > x0 && r.left < x1 && r.bottom > y0 && r.top < y1;
  }).map((n) => n.dataset.id);
  const ids = [...new Set([...d.base, ...hits])];
  // Re-render (and tell the other users) only when the set changes.
  const key = ids.join();
  if (key !== d.key) { d.key = key; select(cueSelection(ids, S.sel?.id)); }
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
    if (S.deferredShow) rebase();
    return;
  }
  if (winDrag) saveLayout();
  if (scrubbing) { lastScrub = 0; scrub(e); }
  if (tlDrag?.kind === 'box') {
    $('#tl-box')?.remove();
    if (!tlDrag.moved && !tlDrag.additive) select(null); // a plain click on an empty lane
  } else if (tlDrag?.narrow && !tlDrag.undone) {
    select({ type: 'cue', id: tlDrag.id }); // clicked one cue of several, without moving it
  }
  const finished = drag?.undone ? drag : tlDrag?.undone ? tlDrag : null;
  if (finished) { record(finished.before); sendDoc(); } // final position, immediately
  winDrag = drag = tlDrag = null;
  scrubbing = false;
  if (finished) sendPresence();
  if (S.deferredShow) rebase();
});

$('#tl-play').onclick = () => command({ type: currentTransport(S.tl)?.playing ? 'pause' : 'play' }, S.tl);
$('#tl-stop').onclick = () => command({ type: 'stop' }, S.tl);
// A new track goes on top: the first track is the front-most layer, so a
// new one covers what is already there (as a new layer does in a stack).
$('#btn-track').onclick = () => edit(() => { const o = tlObj(S.tl); o.tracks.unshift({ id: uid(), name: `Track ${o.tracks.length + 1}`, cues: [] }); });
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
  // Top to bottom is front to back, as for tracks: drag a task up or down to
  // change what covers what. The main timeline is always at the back.
  for (const task of S.doc.tasks) {
    const tr = currentTransport(task.id);
    const selected = (S.sel?.type === 'task' && S.sel.id === task.id) || S.tl === task.id;
    const row = el('div', {
      class: `task${selected ? ' sel' : ''}`, draggable: 'true', dataset: { id: task.id },
      title: 'Click to select and open; drag up or down to bring it in front of / behind the other tasks',
      onclick: (e) => { if (!e.target.closest('button')) { S.tl = task.id; select({ type: 'task', id: task.id }); } },
      ondragstart: (e) => { e.dataTransfer.setData('text/lcplay-task', task.id); e.dataTransfer.effectAllowed = 'move'; },
      ondragover: (e) => {
        if (!e.dataTransfer.types.includes('text/lcplay-task')) return;
        e.preventDefault();
        const below = e.offsetY > row.offsetHeight / 2;
        row.classList.toggle('drop-above', !below);
        row.classList.toggle('drop-below', below);
      },
      ondragleave: () => row.classList.remove('drop-above', 'drop-below'),
      ondrop: (e) => {
        const below = row.classList.contains('drop-below');
        row.classList.remove('drop-above', 'drop-below');
        const id = e.dataTransfer.getData('text/lcplay-task');
        if (!id) return;
        e.preventDefault();
        const to = S.doc.tasks.findIndex((t) => t.id === task.id) + (below ? 1 : 0);
        moveTask(id, to);
      },
    });
    box.append(row);
    row.append(
      el('span', { class: `dot ${!tr?.active ? '' : tr.playing ? 'ok' : 'warn'}` }),
      el('span', { class: 'name', title: 'Open in the Timeline window' }, task.name || 'Task'),
      el('span', { class: 'state dim mono', dataset: { tl: task.id } }, taskStateText(task.id)),
      el('button', { class: 'small', title: 'Play', onclick: () => command({ type: 'play' }, task.id) }, '▶'),
      el('button', { class: 'small', title: 'Pause (a stopped task: bring it on stage, paused at the start)', onclick: () => command({ type: 'pause' }, task.id) }, '⏸'),
      el('button', { class: 'small', title: 'Stop (take off stage; click its ruler to bring it back, paused)', onclick: () => command({ type: 'stop' }, task.id) }, '⏹'));
  }
  box.append(el('div', { class: 'task main dim', title: 'The main timeline is always behind the tasks' }, el('span', { class: 'name' }, 'Main timeline (always at the back)')));
}
$('#btn-task').onclick = addTask;

/** Move task `id` so that it ends up at index `to` of the task list as it is
 *  now (0 = top, the front-most). */
function moveTask(id, to) {
  const from = S.doc.tasks.findIndex((t) => t.id === id);
  if (from < 0) return;
  const at = from < to ? to - 1 : to;
  if (at === from) return;
  edit(() => {
    const [task] = S.doc.tasks.splice(from, 1);
    S.doc.tasks.splice(clamp(at, 0, S.doc.tasks.length), 0, task);
    S.sel = { type: 'task', id };
  });
}

// ───────────────────────────── clipboard ─────────────────────────────
// Ctrl+C / Ctrl+X / Ctrl+V for cues (one or several, on any timeline) and
// for whole tasks. The clipboard is this page's: it holds copies, so editing
// or deleting the originals afterwards does not change what is pasted.
//
// - Cues paste into the timeline open in the Timeline window, the earliest
//   at its playhead, keeping their spacing in time and between tracks; the
//   top one goes on the track of the selected cue (or track), otherwise on
//   the top track. Missing tracks are added below.
// - A task pastes above the selected task (or the one open), otherwise on
//   top, and opens.
// - Cut, then paste, is a move: ids still free are kept (so is a control cue
//   elsewhere that started a cut task). A second paste gets new ids, and a
//   pasted task whose name is in use is called "… copy".

let clipboard = null; // { kind: 'cues', cues: [{ cue, ti }], assets } | { kind: 'task', task, targeting, assets }

/** Every id in the show (ids are unique across all its lists). */
function allIds() {
  const ids = new Set();
  const walk = (v) => {
    if (Array.isArray(v)) v.forEach(walk);
    else if (isObj(v)) { if (typeof v.id === 'string') ids.add(v.id); Object.values(v).forEach(walk); }
  };
  walk(S.doc);
  return ids;
}

/** The asset entries the cues use (pasted into a show that lacks them). */
const assetsOf = (cues) => clone(S.doc.assets.filter((a) => cues.some((c) => c.source.type === 'asset' && c.source.asset === a.id)));

function copySelection(cut = false) {
  if (S.sel?.type === 'cue') {
    const hits = selCueIds().map(findCue).filter(Boolean);
    if (!hits.length) return;
    clipboard = { kind: 'cues', cues: hits.map((h) => ({ cue: clone(h.cue), ti: h.ti })), assets: assetsOf(hits.map((h) => h.cue)) };
    if (cut) {
      edit(() => {
        for (const h of hits) { const now = findCue(h.cue.id); if (now) now.track.cues.splice(now.ci, 1); }
        S.sel = null;
      });
    }
    toast(`${cut ? 'Cut' : 'Copied'} ${hits.length} cue${hits.length > 1 ? 's' : ''}: Ctrl+V pastes at the playhead of the open timeline.`, true);
  } else if (S.sel?.type === 'task') {
    const task = tlObj(S.sel.id);
    if (!task) return;
    // Control cues elsewhere that act on it: they come back if it is cut and
    // pasted (the Director refuses one that names a task that is not there).
    const targeting = [];
    for (const tl of tlIds()) for (const cc of tlObj(tl).control_cues) if (tl !== task.id && cc.target === task.id) targeting.push({ tl, cc: clone(cc) });
    clipboard = { kind: 'task', task: clone(task), targeting, assets: assetsOf(task.tracks.flatMap((t) => t.cues)) };
    if (cut) {
      edit(() => {
        S.doc.tasks = S.doc.tasks.filter((t) => t.id !== task.id);
        for (const tl of tlIds()) { const o = tlObj(tl); o.control_cues = o.control_cues.filter((cc) => cc.target !== task.id); }
        S.sel = null;
      });
    }
    toast(`${cut ? 'Cut' : 'Copied'} the task "${task.name || 'Task'}": Ctrl+V pastes it above the selected task.`, true);
  }
}

/** A cue from the clipboard can go into the show: what it shows exists. */
const pastable = (cue) => cue.source.type !== 'virtual' || !!findVirtual(cue.source.virtual);

function paste() {
  if (clipboard?.kind === 'cues') pasteCues();
  else if (clipboard?.kind === 'task') pasteTask();
}

function pasteCues() {
  const items = clipboard.cues.filter((i) => pastable(i.cue));
  if (!items.length) { toast('Nothing to paste: the virtual displays those cues showed are gone.'); return; }
  const tl = S.tl, at = round2(Math.max(0, showSeconds(tl)));
  const t0 = Math.min(...items.map((i) => i.cue.start_s)), ti0 = Math.min(...items.map((i) => i.ti));
  const lead = S.sel?.type === 'cue' ? findCue(S.sel.id) : S.sel?.type === 'track' ? findTrack(S.sel.id) : null;
  const base = lead?.tl === tl ? lead.ti : 0;
  edit(() => {
    const obj = tlObj(tl), ids = allIds(), pasted = [];
    for (const a of clipboard.assets) if (!S.doc.assets.some((x) => x.id === a.id)) S.doc.assets.push(clone(a));
    for (const { cue, ti } of items) {
      const c = clone(cue);
      if (ids.has(c.id)) c.id = uid();
      ids.add(c.id);
      c.start_s = round2(at + (cue.start_s - t0));
      const target = base + (ti - ti0);
      while (obj.tracks.length <= target) obj.tracks.push({ id: uid(), name: `Track ${obj.tracks.length + 1}`, cues: [] });
      obj.tracks[target].cues.push(c);
      obj.duration_s = Math.max(obj.duration_s, Math.ceil(c.start_s + c.duration_s));
      pasted.push(c.id);
    }
    normalize(S.doc);
    S.sel = cueSelection(pasted, pasted[0]);
  });
  if (tl === MAIN) syncLoop();
}

function pasteTask() {
  const src = clipboard.task, ids = allIds();
  const task = clone(src);
  const keep = !ids.has(task.id);
  if (!keep) task.id = 'task-' + uid();
  ids.add(task.id);
  if (!keep || S.doc.tasks.some((t) => t.name === task.name)) task.name = `${src.name || 'Task'} copy`;
  // Fresh ids for whatever inside is taken; nothing that refers to what is
  // not there (the Director would refuse the show).
  const known = new Set([MAIN, task.id, ...S.doc.tasks.map((t) => t.id)]);
  for (const t of task.tracks) {
    if (ids.has(t.id)) t.id = uid();
    ids.add(t.id);
    t.cues = t.cues.filter(pastable);
    for (const c of t.cues) { if (ids.has(c.id)) c.id = uid(); ids.add(c.id); }
  }
  task.control_cues = task.control_cues.filter((cc) => {
    if (ids.has(cc.id)) cc.id = uid();
    ids.add(cc.id);
    if (cc.target === src.id) cc.target = '';
    return !cc.target || known.has(cc.target);
  });
  // Above the selected (or open) task, else on top.
  const anchor = S.sel?.type === 'task' ? S.sel.id : S.tl;
  const index = Math.max(0, S.doc.tasks.findIndex((t) => t.id === anchor));
  edit(() => {
    for (const a of clipboard.assets) if (!S.doc.assets.some((x) => x.id === a.id)) S.doc.assets.push(clone(a));
    S.doc.tasks.splice(index, 0, task);
    if (keep) {
      for (const { tl, cc } of clipboard.targeting) {
        const o = tlObj(tl);
        if (o && !ids.has(cc.id)) { o.control_cues.push(clone(cc)); ids.add(cc.id); }
      }
    }
    normalize(S.doc);
    S.tl = task.id;
    S.sel = { type: 'task', id: task.id };
  });
}

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
        (h?.output_health || []).map((o) => [o.presenter, o.barrier, o.missed, o.late_decodes, o.split_frames, o.latency_frames, o.grid_offset_ns == null ? null : Math.round(o.grid_offset_ns / 100000),
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
        act('Quit', machine?.supervised ? 'Close the Runner program; Start (here) opens it again' : 'Close the Runner program (it runs without a supervisor: it can then only be started on its computer)',
          () => runnerAction(name, 'quit', machine?.supervised
            ? `Close the Runner program on ${name}? It can be started again from here with Start.`
            : `Close the Runner program on ${name}? It runs without a supervisor, so it can then only be started on that computer.`)),
        act('Exit', 'Close the Runner program for good: it can then only be started on its computer', () => runnerAction(name, 'exit', `Close the Runner program on ${name} for good? It can then only be started again on that computer${machine?.allow_power ? ' or by rebooting it' : ''}.`)),
        act('Reboot', power ? 'Restart the computer' : noPower,
          () => runnerAction(name, 'reboot', `Restart the computer ${machine?.hostname || name}?`), { disabled: !power }),
        act('Power off', power ? 'Shut the computer down' : noPower,
          () => runnerAction(name, 'power_off', `Shut down the computer ${machine?.hostname || name}? It can be woken again only if Wake-on-LAN is enabled on it.`), { disabled: !power }),
      );
    } else {
      const startable = machine?.ip && machine.supervised;
      actions.append(act('Start', startable
        ? `Start the Runner program again on ${machine.hostname || machine.ip} (its supervisor waits for this after the Runner was closed; the computer must be on)`
        : !machine?.ip ? 'Its network address is not known yet' : 'It ran without a supervisor (--no-supervise): start it on its computer',
        () => { command({ type: 'start_runner', runner: name }); toast(`Asked ${name} to start.`, true); }, { disabled: !startable }));
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
        el('button', {
          class: `small${d.test_pattern ? ' on' : ''}`,
          title: d.test_pattern ? 'Test pattern on: click to turn it off' : 'Show moving test lines on this display (rate and kind in the Inspector)',
          onclick: () => edit(() => { const live = findDisplay(d.id); if (live) live.test_pattern = live.test_pattern ? null : { kind: 'lines', fps: 60 }; }),
        }, 'Test'),
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
      // Only Runners with several cards can split a frame between them.
      o.split_frames ? `split ${o.split_frames}` : null,
      o.frame_counter != null ? `counter ${o.frame_counter}` : null,
    ];
    const bad = o.missed > 0 || o.late_decodes > 0 || o.split_frames > 0;
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

/** Call `commit` when a typed value is entered: on Enter, and on leaving
 *  the field (the 'change' event), but only once per value — browsers also
 *  fire 'change' on Enter, and leaving the field after Enter must not make a
 *  second undo step. `commit` may rewrite the field (a refused value goes
 *  back); that is the new baseline. (Checkboxes, colours and dropdowns
 *  commit on 'change' directly.)
 *
 *  Enter also moves to the next typed field of the same panel, with its
 *  text selected, so several values can be typed in a row (Shift+Enter: the
 *  previous one). On the last field the cursor stays, its text selected. */
function onCommit(input, commit, { step = null } = {}) {
  let last = input.value;
  // The field was rewritten from the show (e.g. H after W, aspect kept):
  // that is the new baseline, so typing the old value again counts.
  input.rebase = () => { last = input.value; };
  const fire = () => {
    if (input.value === last) return;
    commit();
    last = input.value;
  };
  input.addEventListener('change', fire);
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      fire();
      focusNextField(input, e.shiftKey ? -1 : 1);
    } else if (step && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
      // A number field: the arrows step its value (Shift: ten steps), at
      // once — as typing "+step" and Enter would, but staying in the field.
      e.preventDefault();
      const d = step * (e.shiftKey ? 10 : 1) * (e.key === 'ArrowUp' ? 1 : -1);
      input.value = `+${d}`;
      fire();
      input.select();
    }
  });
}

/** Text and number inputs, in page order, that Enter moves between. */
const TYPED_FIELD = 'input:not([type]), input[type=text], input[type=number]';

/** Put the cursor in the typed field `step` places after `input` in its
 *  panel (the window it is in), with that field's text selected; stay on
 *  `input` if there is none. */
function focusNextField(input, step) {
  const scope = input.closest('.win') || document;
  const fields = [...scope.querySelectorAll(TYPED_FIELD)].filter((n) => !n.disabled && n.offsetParent !== null);
  const next = fields[fields.indexOf(input) + step] || input;
  next.focus();
  next.select();
}

/** Evaluate what was typed in a number field: a number, or arithmetic
 *  with + - * / and parentheses ("1920/2", "(1080 - 40) / 2", "3*1.5").
 *  A comma is a decimal point. Starting with +, * or / it applies to
 *  `current`, the field's value: "*2" doubles it, "/3" divides it, "+10"
 *  adds 10, "+-10" takes 10 off (a leading "-" is a negative number).
 *  Returns null for an empty field, NaN for anything else it cannot read
 *  (or a relative entry with no current value). Rounded to 6 decimals, so
 *  0.1+0.2 is 0.3. No eval: a small parser of its own. */
function evalNumber(text, current) {
  let src = String(text).trim().replace(/,/g, '.');
  if (src === '') return null;
  if (/^[+*/]/.test(src)) {
    if (typeof current !== 'number' || !Number.isFinite(current)) return NaN;
    src = `(${current})${src}`;
  }
  const tokens = src.match(/(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?|\S/gi) || [];
  let i = 0;
  const peek = () => tokens[i];
  // expr := term (('+' | '-') term)* ; term := factor (('*' | '/') factor)* ;
  // factor := ('+' | '-') factor | number | '(' expr ')'
  const expr = () => {
    let v = term();
    while (peek() === '+' || peek() === '-') v = tokens[i++] === '+' ? v + term() : v - term();
    return v;
  };
  const term = () => {
    let v = factor();
    while (peek() === '*' || peek() === '/') v = tokens[i++] === '*' ? v * factor() : v / factor();
    return v;
  };
  const factor = () => {
    const t = tokens[i++];
    if (t === '-') return -factor();
    if (t === '+') return factor();
    if (t === '(') { const v = expr(); return tokens[i++] === ')' ? v : NaN; }
    return t !== undefined && /^[\d.]/.test(t) ? Number(t) : NaN;
  };
  const v = expr();
  if (i !== tokens.length || !Number.isFinite(v)) return NaN;
  return Math.round(v * 1e6) / 1e6;
}

/** A field's value as text: numbers without float noise. */
const fmtValue = (v) => (typeof v === 'number' ? String(Math.round(v * 1e6) / 1e6) : v ?? '');

/** An input for a number. A text field, not type=number, so that
 *  arithmetic can be typed in it (see `evalNumber`); the arrow keys step it. */
const numberInput = (attrs = {}) => el('input', { type: 'text', inputmode: 'decimal', class: 'num', spellcheck: 'false', autocomplete: 'off', ...attrs });

/** A labelled input bound to obj[key]. Changes are committed on Enter or on
 *  leaving the field (not per keystroke), each as one undo step. A number
 *  field takes arithmetic (see `evalNumber`).
 *
 *  `linked` (for W/H pairs): { key, when(obj), fit(raw), refresh() } — while
 *  `when(obj)` holds, changing this value scales obj[key of the other] in
 *  proportion (keeping the aspect ratio), through `fit`. */
function field(label, obj, key, opts = {}) {
  const { type = 'number', step = 1, min, get = (v) => v, set = (v) => v, after, optional = false, linked } = opts;
  if (type === 'checkbox') {
    const input = el('input', { type });
    input.checked = !!obj[key];
    input.addEventListener('change', () => edit(() => { obj[key] = set(input.checked); after?.(); }, { inspector: false }));
    return el('div', { class: 'row' }, el('label', {}, label), input);
  }
  const input = type === 'number' ? numberInput() : el('input', { type });
  const show = () => { input.value = fmtValue(get(obj[key])); input.rebase?.(); };
  input.refresh = show;
  show();
  onCommit(input, () => {
    if (type !== 'number') { edit(() => { obj[key] = set(input.value); after?.(); }, { inspector: false }); return; }
    const v = evalNumber(input.value, get(obj[key]));
    // A number field that is emptied, or holds something unusable, goes back
    // to its value: writing "no value" into the show would make the Director
    // refuse every edit from then on. Only fields that are genuinely
    // optional (the monitor number) may be cleared.
    const unusable = v == null ? !optional : Number.isNaN(v) || (min != null && v < min);
    if (unusable) { show(); return; }
    edit(() => {
      const old = obj[key];
      obj[key] = set(v);
      if (linked?.when(obj) && old > 0 && obj[key] > 0) obj[linked.key] = linked.fit((obj[linked.key] * obj[key]) / old);
      after?.();
    }, { inspector: false });
    show(); // the result, not the expression
    linked?.refresh();
  }, { step: type === 'number' ? step : null });
  return el('div', { class: 'row' }, el('label', {}, label), input);
}

/** A labelled input bound to `key` of several objects at once (the cues of a
 *  multiple selection). `objs()` finds them afresh when a value is entered.
 *  Where they all hold the same value it is shown; otherwise the field is
 *  empty ("mixed"; a checkbox shows a dash) and leaving it so changes
 *  nothing. A value entered is set on all of them, as one undo step; an
 *  entry relative to the value ("*2", "+10") applies to each one's own.
 *  `linked`: as for `field`, per object. */
function multiField(label, objs, key, opts = {}) {
  const { type = 'number', step = 1, min, get = (v) => v, set = (v) => v, linked } = opts;
  // The value they share, or `mixed` (asked afresh: after Enter the field
  // stays open while the values change underneath).
  const mixed = Symbol('mixed');
  const common = () => {
    const values = objs().map((o) => o[key]);
    return values.every((v) => JSON.stringify(v) === JSON.stringify(values[0])) ? values[0] : mixed;
  };
  if (type === 'checkbox') {
    const input = el('input', { type });
    const v = common();
    input.checked = v !== mixed && !!v;
    input.indeterminate = v === mixed;
    input.addEventListener('change', () => edit(() => { for (const o of objs()) o[key] = set(input.checked); }, { inspector: false }));
    return el('div', { class: 'row' }, el('label', {}, label), input);
  }
  const input = type === 'number' ? numberInput() : el('input', { type });
  const shown = () => { const v = common(); return v === mixed ? '' : fmtValue(get(v)); };
  input.refresh = () => { input.value = shown(); input.rebase?.(); };
  input.refresh();
  if (common() === mixed) input.placeholder = 'mixed';
  onCommit(input, () => {
    // Empty: leave every cue as it is.
    if (input.value.trim() === '') { input.refresh(); return; }
    const list = objs();
    if (type !== 'number') { edit(() => { for (const o of list) o[key] = set(input.value); }, { inspector: false }); return; }
    // Each one's own result (they differ for "*2"); one unusable: go back.
    const results = list.map((o) => evalNumber(input.value, get(o[key])));
    if (results.some((v) => v == null || Number.isNaN(v) || (min != null && v < min))) { input.refresh(); return; }
    edit(() => list.forEach((o, i) => {
      const old = o[key];
      o[key] = set(results[i]);
      if (linked?.when(o) && old > 0 && o[key] > 0) o[linked.key] = linked.fit((o[linked.key] * o[key]) / old);
    }), { inspector: false });
    input.refresh();
    linked?.refresh();
  }, { step: type === 'number' ? step : null });
  return el('div', { class: 'row' }, el('label', {}, label), input);
}

/** Two fields on one row (x/y, w/h), built by `make` (`field` or
 *  `multiField`). `keepAspect(obj)`: while it holds, changing one of the two
 *  scales the other in proportion (whole pixels, at least 1). */
function twoFields(make, label, target, a, b, opts) {
  const { keepAspect, ...rest } = opts;
  const row = el('div', { class: 'row two' }, el('label', {}, label));
  const inputs = {};
  for (const key of [a, b]) {
    const other = key === a ? b : a;
    const linked = keepAspect && {
      key: other, when: keepAspect,
      fit: (raw) => (rest.set || ((x) => x))(Math.max(1, Math.round(raw))),
      refresh: () => inputs[other].refresh(),
    };
    inputs[key] = make('', target, key, { ...rest, linked }).lastChild;
    row.append(inputs[key]);
  }
  return row;
}
/** Two multiFields on one row (x/y, w/h). */
const multiPair = (label, objs, a, b, opts = {}) => twoFields(multiField, label, objs, a, b, opts);
/** Two numbers on one row (x/y, w/h). */
const pair = (label, obj, a, b, opts = {}) => twoFields(field, label, obj, a, b, opts);

/** An optional [a, b] array property (window position/size): empty = unset. */
function optionalPair(label, obj, key, min) {
  const row = el('div', { class: 'row two' }, el('label', {}, label));
  const inputs = [0, 1].map(() => numberInput({ placeholder: 'auto' }));
  const show = () => inputs.forEach((n, i) => { n.value = fmtValue(obj[key]?.[i]); n.rebase?.(); });
  show();
  for (const input of inputs) {
    onCommit(input, () => {
      const vals = inputs.map((n, i) => evalNumber(n.value, obj[key]?.[i]));
      if (vals.some((v) => Number.isNaN(v) || (v != null && min != null && v < min))) { show(); return; }
      edit(() => (obj[key] = vals.every((v) => v != null) ? vals : null), { inspector: false });
      // Leave a half-filled pair as typed: the other value may come next.
      if (vals.every((v) => v != null)) show();
    }, { step: 1 });
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

/** A display's test pattern: drawn by its Runner over the content, fitted
 *  to the display, at a chosen rate, on the cluster clock (it moves whether
 *  the show plays or not, and every display is on the same step). */
function testPatternFields(d) {
  const tp = d.test_pattern;
  const kind = el('select', { class: 'wide' },
    el('option', { value: '' }, 'Off'),
    el('option', { value: 'lines' }, 'Moving lines'),
    el('option', { value: 'counter' }, 'Frame counter'));
  kind.value = tp?.kind || '';
  kind.onchange = () => {
    kind.blur();
    edit(() => {
      const live = findDisplay(d.id);
      if (!live) return;
      live.test_pattern = kind.value ? { kind: kind.value, fps: live.test_pattern?.fps || 60 } : null;
    });
  };
  const rows = [el('h4', {}, 'Test pattern'), el('div', { class: 'row' }, el('label', {}, 'Pattern'), kind)];
  if (tp) {
    rows.push(field('Rate (frames/s)', tp, 'fps', { step: 0.001, min: 1 }),
      note(tp.kind === 'lines'
        ? 'Two lines sweep across this display, one step per frame: smooth motion means every frame reached the screen once. Set the rate to the display refresh (or the content\'s frame rate) to judge judder.'
        : 'The frame number, in digits and binary cells, with a white flash every second: film several displays together to read any offset in frames.'));
  }
  return rows;
}
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

  if (type === 'cue' && selCueIds().length > 1) {
    // Several cues: the properties they can share. A value entered here is
    // set on all of them.
    const cues = selectedCues, videos = () => selectedCues().filter((c) => assetOf(c)?.kind === 'video');
    const n = cues().length, nv = videos().length;
    box.append(
      el('h3', {}, `${n} cues`),
      note('A value entered here is set on every selected cue. An empty field (mixed) means the cues differ: leave it empty to keep their own values. Shift- or Ctrl-click a cue in the timeline to add or remove it.'),
      el('h4', {}, 'Time'),
      multiField('Start (s)', cues, 'start_s', sec),
      multiField('Duration (s)', cues, 'duration_s', { step: 0.1, min: 0.1 }),
      multiField('Fade in (s)', cues, 'fade_in_s', sec),
      multiField('Fade out (s)', cues, 'fade_out_s', sec),
    );
    if (nv) {
      box.append(
        el('h4', {}, nv === n ? 'Video' : `Video (the ${nv} video cues)`),
        multiField('In point (s)', videos, 'in_s', sec),
        multiField('Loop video', videos, 'loop', { type: 'checkbox' }),
        multiField('Free run', videos, 'free_run', { type: 'checkbox' }));
    }
    box.append(
      el('h4', {}, 'Position on stage'),
      multiPair('X / Y', cues, 'x', 'y'),
      multiPair('W / H', cues, 'w', 'h', { min: 1, keepAspect: (o) => !!o.keep_aspect }),
      multiField('Keep aspect ratio', cues, 'keep_aspect', { type: 'checkbox' }),
      multiField('Rotation (°)', cues, 'rotation'),
      multiField('Opacity (%)', cues, 'opacity', { min: 0, get: (v) => Math.round(v * 100), set: (v) => clamp(v / 100, 0, 1) }),
      el('div', { class: 'btns' }, el('button', { class: 'danger', onclick: deleteSelection }, `Delete ${n} cues`)));
    return;
  }

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
      pair('W / H', c, 'w', 'h', { min: 1, keepAspect: (o) => !!o.keep_aspect }),
      field('Keep aspect ratio', c, 'keep_aspect', { type: 'checkbox' }),
      field('Rotation (°)', c, 'rotation'),
      field('Opacity (%)', c, 'opacity', { min: 0, get: (v) => Math.round(v * 100), set: (v) => clamp(v / 100, 0, 1) }),
    );
    if (c.source.type === 'color') box.append(colorField('Colour', c.source, 'rgba'));
    if (c.source.type === 'virtual') {
      box.append(
        el('h4', {}, 'Source'),
        choice('Virtual display', c.source, 'virtual', S.doc.stage.virtuals.map((v) => [v.id, v.name || v.id])),
        note('Live: what the virtual display captures, on this frame. In point, loop and free run do not apply.'));
    }
    if (c.source.type === 'sync_test') {
      box.append(
        field('Frame rate', c.source, 'fps', { min: 1 }),
        note('Drawn by each Runner: the frame number (digits and binary blocks), a marker stepping one place per frame, and a white flash on the first frame of every second. Film two outputs, or two machines, with a fast camera: any difference in the numbers is the offset in frames. Use the displays\' refresh rate as frame rate.'));
    }
    // A picture's own size: the asset's, or the virtual display's resolution.
    const vd = c.source.type === 'virtual' ? findVirtual(c.source.virtual) : null;
    const own = asset?.width ? [asset.width, asset.height] : vd ? [vd.width, vd.height] : null;
    const mw = own?.[0] || extent().w, mh = own?.[1] || extent().h;
    box.append(el('div', { class: 'btns' },
      el('button', { onclick: () => edit(() => Object.assign(c, fitRect(mw, mh, true))) }, 'Fit to stage'),
      own ? el('button', { onclick: () => edit(() => Object.assign(c, { w: own[0], h: own[1] })) }, 'Original size') : null,
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
      note(`An auxiliary timeline with its own transport. It is off stage until played — or paused (on stage, at the start), or positioned: clicking or dragging on its ruler, or a control cue's jump, brings it on stage, paused on that frame — and is drawn in front of the main timeline; among the tasks, the order in the Tasks window is the stacking (the top one in front — drag to reorder; Ctrl+C / Ctrl+X / Ctrl+V copy, cut and paste a task). Start it from the Tasks window, from a control cue, or over OSC: /lcplay/task/${task.id}/play (also pause, stop, seek).`),
      deleteButton('Delete task'));
    return;
  }

  if (type === 'virtual') {
    const v = findVirtual(S.sel.id), shownBy = cuesShowing(v.id).length;
    const px = { min: 1, set: (n) => clamp(Math.round(n), 1, 16384) };
    box.append(
      el('h3', {}, 'Virtual display'),
      field('Name', v, 'name', { type: 'text' }),
      note('A camera on the canvas: what is on air in its area, rendered at its resolution into a picture that cues can show — scaled, moved, rotated, faded, on any display or inside another virtual display. Not sent to any screen itself.'),
      el('h4', {}, 'Area of the canvas it captures'),
      pair('X / Y', v.canvas_rect, 'x', 'y'),
      pair('W / H', v.canvas_rect, 'w', 'h', { min: 1, keepAspect: () => !!v.keep_aspect }),
      el('h4', {}, 'Resolution (pixels)'),
      pair('W / H', v, 'width', 'height', { ...px, keepAspect: () => !!v.keep_aspect }),
      field('Keep aspect ratio', v, 'keep_aspect', { type: 'checkbox' }),
      el('div', { class: 'btns' },
        el('button', { title: 'One pixel of the picture per canvas pixel', onclick: () => edit(() => { v.width = clamp(Math.round(v.canvas_rect.w), 1, 16384); v.height = clamp(Math.round(v.canvas_rect.h), 1, 16384); }) }, 'Resolution = area'),
        el('button', { title: 'A cue showing this virtual display, at the playhead of the open timeline (or drag it from the Show inspector onto a track)', onclick: () => addVirtualCue(v) }, '+ Cue showing it')),
      note(`Shown by ${shownBy} cue${shownBy === 1 ? '' : 's'}. It captures over a transparent background, so a cue showing it keeps what is behind it wherever nothing is on air. A cue that would show it inside its own picture (directly or through other virtual displays) is left out of that picture: no feedback.`),
      deleteButton('Delete virtual display'));
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
      ...testPatternFields(d),
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
  box.append(el('h4', {}, 'Virtual displays'));
  for (const v of st.virtuals) {
    box.append(el('div', {
      class: 'list-item', draggable: 'true', title: 'Click to edit; drag onto a track for a cue showing it',
      onclick: () => select({ type: 'virtual', id: v.id }),
      ondragstart: (e) => e.dataTransfer.setData('text/lcplay-virtual', v.id),
    },
    el('span', {}, `⧉ ${v.name || 'virtual'}`),
    el('span', { class: 'dim' }, `${v.width}×${v.height}`)));
  }
  box.append(el('div', { class: 'btns' },
    el('button', { onclick: addVirtual, title: 'A camera on part of the canvas whose picture cues can show' }, '+ Virtual display')));
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
    delete doc._applied;
    if (S.others.size && !(await ask(`Open ${file.name}? It replaces the show for every user working on it.`))) return;
    S.replaceNext = true;
    edit(() => { S.doc = doc; S.sel = null; S.tl = MAIN; });
  } catch (err) { toast(`Cannot open ${file.name}: ${err.message}`); }
};
$('#btn-upload').onclick = () => $('#file-upload').click();
$('#btn-optimize').onclick = () => openOptimizeSettings();
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
  // Copy / cut / paste: cues and tasks (anything else: the browser's own).
  else if (mod && e.key.toLowerCase() === 'c' && ['cue', 'task'].includes(S.sel?.type)) { e.preventDefault(); copySelection(); }
  else if (mod && e.key.toLowerCase() === 'x' && ['cue', 'task'].includes(S.sel?.type)) { e.preventDefault(); copySelection(true); }
  else if (mod && e.key.toLowerCase() === 'v') { if (clipboard) { e.preventDefault(); paste(); } }
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

/** Notice when this window's script was blocked (a timer that fires late
 *  by more than a second): kept in `S.lastStall` and logged, so that a lost
 *  Director connection can be told apart from a frozen interface. A hidden
 *  window's timers are slowed down on purpose by the browser: not counted. */
function watchFreezes() {
  const PERIOD = 250;
  let last = performance.now(), wasVisible = !document.hidden;
  setInterval(() => {
    const now = performance.now(), late = now - last - PERIOD, visible = !document.hidden;
    if (late > 1000 && visible && wasVisible) {
      S.lastStall = { at: now, ms: late + PERIOD };
      console.warn(`interface unresponsive for ${Math.round(late + PERIOD)} ms`);
    }
    last = now;
    wasVisible = visible;
  }, PERIOD);
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
  watchFreezes();
  loadLibrary();
  setInterval(loadLibrary, 5000);                   // picks up files copied in by hand
  renderCluster();
  requestAnimationFrame(tick);
})();
