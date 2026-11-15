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
  lastSent: '',         // JSON of the document as last sent/received (change detection)
  dirty: false,         // there are edits the Director has not received
  holdForeign: false,   // ignore other senders' shows until ours is confirmed
  sel: null,            // { type: 'cue'|'display'|'control'|'track'|'task', id }
  tl: MAIN,             // the timeline open in the Timeline window
  transports: {},       // timeline id → transport states known, oldest first (≤ 2)
  frameRate: { num: 60, den: 1 },
  clockOffsetNs: null,  // cluster time − performance.now(), null until known
  status: null,         // last cluster status
  library: [],          // items on the Asset Manager
  pxPerSec: 40,         // timeline zoom
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
      t.id ||= uid(); t.name ??= ''; t.cues ??= []; t.free_run ??= false;
      for (const c of t.cues) {
        c.id ||= uid();
        Object.assign(c, Object.assign({
          name: '', source: { type: 'color', rgba: [0.5, 0.5, 0.5, 1] },
          start_s: 0, duration_s: 10, in_s: 0, loop: false,
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

// ───────────────────────────── network ─────────────────────────────

function connect(url, protocol) {
  const ws = new WebSocket(url);
  S.ws = ws;
  ws.onopen = () => {
    S.connected = true;
    ws.send(JSON.stringify({ type: 'hello', protocol, node_id: S.clientId, role: 'producer', name: 'Producer' }));
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
        if (S.dirty || msg.show_revision == null) {
          S.holdForeign = true;
          sendDoc(true);
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
      // Every document this page sends carries a tag ({by, n}); the Director
      // stores and re-broadcasts documents verbatim, so the tag tells our own
      // edits coming back from somebody else's. (Comparing the JSON text
      // would not: the Director does not preserve key order.)
      const tag = msg.document._edit;
      if (tag?.by === S.clientId) {
        if (tag.n >= S.editN) S.holdForeign = false; // our latest has arrived
        break; // we already have this, or something newer
      }
      if (S.holdForeign || S.dirty) break; // ours is on its way, and wins
      // Somebody else's show (another Producer, lc-ctl, or the one stored on
      // the Director when this page was opened): adopt it. Anything being
      // dragged refers to the old document.
      drag = tlDrag = null;
      delete msg.document._edit;
      S.doc = normalize(msg.document);
      S.lastSent = JSON.stringify(S.doc);
      refreshAll();
      reconcileAssets();
      break;
    }
    case 'cluster':
      S.status = msg;
      // Our estimate of the cluster clock: accurate to the network delay of
      // this message, which is ample for moving a playhead on screen.
      S.clockOffsetNs = msg.cluster_ns - performance.now() * 1e6;
      pushTransport(msg.transport, MAIN);
      for (const [id, state] of Object.entries(msg.tasks || {})) pushTransport(state, id);
      renderCluster(); renderRunners();
      break;
    case 'error':
      // If it was our document that was refused, no echo will come.
      S.holdForeign = false;
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
  S.undo.push(JSON.stringify(S.doc));
  if (S.undo.length > 200) S.undo.shift();
  S.redo = [];
  change();
  afterChange(inspector);
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
  const json = JSON.stringify(S.doc);
  if (!force && !S.dirty && json === S.lastSent) return;
  if (S.ws?.readyState !== WebSocket.OPEN) {
    // Not lost: kept here and sent as soon as the Director is back (see the
    // 'welcome' handler). Say so once.
    if (!S.dirty) toast('Director offline — edits are kept here and sent when it is back.', true);
    S.dirty = true;
    return;
  }
  S.lastSent = json;
  S.dirty = false;
  command({ type: 'load_show', document: { ...S.doc, _edit: { by: S.clientId, n: ++S.editN } } });
}

function stepHistory(from, to) {
  if (!from.length) return;
  to.push(JSON.stringify(S.doc));
  S.doc = JSON.parse(from.pop());
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
}

function deleteSelection() {
  if (!S.sel) return;
  const { type, id } = S.sel;
  edit(() => {
    if (type === 'cue') { const hit = findCue(id); if (hit) hit.track.cues.splice(hit.ci, 1); }
    else if (type === 'control') { const hit = findControl(id); if (hit) hit.list.splice(hit.i, 1); }
    else if (type === 'track') { const hit = findTrack(id); if (hit) tlObj(hit.tl).tracks.splice(hit.ti, 1); }
    else if (type === 'task') S.doc.tasks = S.doc.tasks.filter((t) => t.id !== id);
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

/** Largest rectangle with the media's aspect that fits the stage, centred. */
function fitRect(mediaW, mediaH, enlarge) {
  const { width: sw, height: sh } = S.doc.stage;
  const k = Math.min(sw / mediaW, sh / mediaH, enlarge ? Infinity : 1);
  const w = Math.round(mediaW * k), h = Math.round(mediaH * k);
  return { x: Math.round((sw - w) / 2), y: Math.round((sh - h) / 2), w, h };
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
  const p = previewFor(cue, asset);
  if (asset.kind !== 'video') return (!p.failed && drawable(p.node)) || drawable(p.thumb);
  const tr = currentTransport(tl);
  const length = asset.duration_s || p.node.duration || 0;
  if (track.free_run) {
    // Free-running: loops on the cluster clock, whatever the timeline does.
    const now = (clusterNow() ?? 0) / NS, span = Math.max(0.1, length - cue.in_s);
    return steerVideo(p, cue.in_s + (now % span), onAir, 1);
  }
  let media = cue.in_s + Math.max(0, t - cue.start_s);
  if (cue.loop && length > cue.in_s) media = cue.in_s + ((media - cue.in_s) % (length - cue.in_s));
  return steerVideo(p, media, onAir && !!tr?.playing, (tr?.rate_permille ?? 1000) / 1000);
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
  const k = Math.min((W - 48) / st.width, (H - 48) / st.height);
  view = { k, ox: (W - st.width * k) / 2, oy: (H - st.height * k) / 2 };
  ctx.save();
  ctx.translate(view.ox, view.oy);
  ctx.scale(k, k);

  ctx.fillStyle = rgbaCss(st.background);
  ctx.fillRect(0, 0, st.width, st.height);

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
        if (!onAir && cue !== sel) continue;
        const fade = clamp(cue.fade_in_s > 0 ? (t - cue.start_s) / cue.fade_in_s : 1, 0, 1)
                   * clamp(cue.fade_out_s > 0 ? (cue.start_s + cue.duration_s - t) / cue.fade_out_s : 1, 0, 1);
        ctx.save();
        ctx.translate(cue.x + cue.w / 2, cue.y + cue.h / 2);
        ctx.rotate((cue.rotation * Math.PI) / 180);
        ctx.globalAlpha = onAir ? cue.opacity * fade : 0.3;
        const picture = cue.source.type === 'asset' ? cuePicture(cue, track, onAir ? t : cue.start_s, tl, onAir) : null;
        if (picture) ctx.drawImage(picture, -cue.w / 2, -cue.h / 2, cue.w, cue.h);
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
  select(null);
});

function stageDragMove(e) {
  // The undo point is taken on the first real movement, so a plain click
  // does not leave an empty step in the history.
  if (!drag.undone) { S.undo.push(JSON.stringify(S.doc)); S.redo = []; drag.undone = true; }
  const dx = (e.clientX - drag.mx) / view.k, dy = (e.clientY - drag.my) / view.k;
  const o = drag.orig, c = drag.target;
  if (drag.kind === 'move') {
    c.x = Math.round(o.x + dx); c.y = Math.round(o.y + dy);
  } else if (drag.kind === 'display') {
    c.canvas_rect.x = Math.round(o.x + dx); c.canvas_rect.y = Math.round(o.y + dy);
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
        class: `cue ${cue.source.type}${track.free_run ? ' free' : ''}${selected ? ' sel' : ''}`, dataset: { id: cue.id },
        style: `left:${cue.start_s * pps}px;width:${cue.duration_s * pps}px`, title: cue.name,
      }, cue.name || (cue.source.type === 'color' ? 'colour' : 'cue'),
        el('div', { class: 'h l' }), el('div', { class: 'h r' })));
    }
    const selected = S.sel?.type === 'track' && S.sel.id === track.id;
    tracks.append(el('div', { class: 'track' },
      el('div', { class: `track-head${selected ? ' sel' : ''}`, title: 'Click to select the track', onclick: () => select({ type: 'track', id: track.id }) },
        el('span', { class: 'name' }, track.name || `Track ${ti + 1}`),
        el('button', {
          class: `small${track.free_run ? ' on' : ''}`,
          title: 'Free run: video on this track loops on its own and keeps moving while the timeline is paused',
          onclick: (e) => { e.stopPropagation(); edit(() => (track.free_run = !track.free_run)); },
        }, '∞')),
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
  const takeUndo = () => { if (!tlDrag.undone) { S.undo.push(JSON.stringify(S.doc)); S.redo = []; tlDrag.undone = true; } };

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
  if (winDrag) saveLayout();
  if (scrubbing) { lastScrub = 0; scrub(e); }
  if ((drag?.undone) || (tlDrag?.undone)) sendDoc(); // final position, immediately
  winDrag = drag = tlDrag = null;
  scrubbing = false;
});

$('#tl-play').onclick = () => command({ type: currentTransport(S.tl)?.playing ? 'pause' : 'play' }, S.tl);
$('#tl-stop').onclick = () => command({ type: 'stop' }, S.tl);
$('#btn-track').onclick = () => edit(() => { const o = tlObj(S.tl); o.tracks.push({ id: uid(), name: `Track ${o.tracks.length + 1}`, cues: [], free_run: false }); });
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
// One card per Runner: is it healthy, what hardware does it have, which
// displays does it drive and where do their windows go — and a button to
// make it rebuild its outputs.

let runnersKey = ''; // what the window was last built from

function renderRunners(force = false) {
  const box = $('#runners');
  if (!S.doc) return;
  const nodes = (S.status?.nodes || []).filter((n) => n.role === 'runner');
  // Rebuilding the list under the operator's cursor would close an open
  // dropdown or steal a click: only rebuild when something shown changed.
  const key = JSON.stringify([
    nodes.map((n) => [n.name, n.addr, n.frame_error, n.health?.sync.locked, Math.round(n.health?.fps || 0), n.health?.monitors, n.health?.gpu, n.health?.outputs]),
    S.doc.stage.displays, S.sel, S.connected,
  ]);
  if (key === runnersKey && !force) return;
  if (box.contains(document.activeElement) && ['SELECT', 'INPUT'].includes(document.activeElement.tagName) && !force) return;
  runnersKey = key;
  box.replaceChildren();

  // Runners that are connected, plus any a display names that is not.
  const names = [...new Set([...nodes.map((n) => n.name), ...S.doc.stage.displays.map((d) => d.runner).filter(Boolean)])];
  if (!names.length) box.append(el('div', { class: 'empty' }, S.connected ? 'No Runner is connected.' : 'Not connected to the Director.'));

  for (const name of names) {
    const node = nodes.find((n) => n.name === name), h = node?.health;
    const state = !node ? 'bad' : !h ? 'warn' : node.frame_error ? 'bad' : h.sync.locked ? 'ok' : 'warn';
    const card = el('div', { class: 'runner' },
      el('div', { class: 'runner-head' },
        el('span', { class: `dot ${state}` }),
        el('span', { class: 'name' }, name),
        el('span', { class: 'dim' }, node ? node.addr : 'offline'),
        node ? el('button', {
          title: 'Close and rebuild all of this Runner\'s outputs and reload its media',
          onclick: () => { command({ type: 'runner_action', runner: name, action: 'reload_outputs' }); toast(`Asked ${name} to reload its outputs.`, true); },
        }, 'Reload outputs') : null));
    if (h) {
      card.append(el('div', { class: 'kv' },
        el('span', {}, 'Status'), el('span', {}, [
          h.fps ? `${h.fps.toFixed(0)} fps` : 'not rendering',
          h.sync.locked ? `clock ±${Math.max(1, Math.round(h.sync.error_ns / 1000))} µs` : 'clock syncing',
          node.frame_error ? `${node.frame_error > 0 ? '+' : ''}${node.frame_error} frames off` : null,
          `${h.outputs ?? 0} output(s)`,
        ].filter(Boolean).join(' · ')),
        el('span', {}, 'GPU'), el('span', {}, h.gpu || '—'),
        el('span', {}, 'Monitors'), el('span', {}, h.monitors?.length
          ? h.monitors.map((m) => `#${m.index} ${m.width}×${m.height} at ${m.x},${m.y}`).join('   ')
          : '—')));
    }

    // The displays this Runner drives: its own, and those assigned to none.
    card.append(el('h4', {}, 'Displays'));
    const displays = S.doc.stage.displays.filter((d) => d.runner === name || d.runner === '');
    for (const d of displays) {
      const selected = S.sel?.type === 'display' && S.sel.id === d.id;
      const full = el('input', { type: 'checkbox', title: 'Fullscreen' });
      full.checked = d.window.fullscreen;
      full.onchange = () => edit(() => (d.window.fullscreen = full.checked));
      const mon = el('select', { title: 'Monitor' }, el('option', { value: '' }, 'primary'));
      for (const m of h?.monitors || []) mon.append(el('option', { value: m.index }, `#${m.index} ${m.width}×${m.height}`));
      // A monitor number the Runner did not report (yet) must stay selectable.
      if (d.window.monitor != null && ![...mon.options].some((o) => o.value === String(d.window.monitor))) mon.append(el('option', { value: d.window.monitor }, `#${d.window.monitor}`));
      mon.value = d.window.monitor ?? '';
      mon.onchange = () => edit(() => (d.window.monitor = mon.value === '' ? null : Number(mon.value)));
      const where = d.window.fullscreen ? 'fullscreen' : `window ${d.window.size ? d.window.size.join('×') : 'auto'}${d.window.position ? ' at ' + d.window.position.join(',') : ''}`;
      card.append(el('div', { class: `disp${selected ? ' sel' : ''}` },
        el('span', { class: 'name', title: 'Edit in the Inspector', onclick: () => select({ type: 'display', id: d.id }) },
          `${d.name || 'display'} — ${d.canvas_rect.w}×${d.canvas_rect.h} · ${where}${d.runner ? '' : ' (any runner)'}`),
        el('label', { class: 'dim' }, full, ' full'), mon,
        el('button', { class: 'small', title: 'Edit position, size, blend…', onclick: () => select({ type: 'display', id: d.id }) }, '✎')));
    }
    card.append(el('div', { class: 'btns' }, el('button', { onclick: () => addDisplay(name) }, '+ Display on this runner')));
    box.append(card);
  }
}

function addDisplay(runner = '') {
  edit(() => {
    const st = S.doc.stage, n = st.displays.length + 1;
    const d = {
      id: uid(), name: `Display ${n}`, runner, canvas_rect: { x: 0, y: 0, w: st.width, h: st.height },
      window: { fullscreen: false, position: [60 + 40 * n, 60 + 40 * n], size: [960, Math.round((960 * st.height) / st.width)] },
    };
    st.displays.push(d);
    normalize(S.doc);
    S.sel = { type: 'display', id: d.id };
  });
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
      box.append(field('In point (s)', c, 'in_s', sec), field('Loop video', c, 'loop', { type: 'checkbox' }));
      if (track.free_run) box.append(note('This track is free-running: the video loops on its own clock; start and duration only decide when it is visible.'));
    }
    box.append(
      el('h4', {}, 'Position on stage'),
      pair('X / Y', c, 'x', 'y'),
      pair('W / H', c, 'w', 'h', { min: 1 }),
      field('Rotation (°)', c, 'rotation'),
      field('Opacity (%)', c, 'opacity', { min: 0, get: (v) => Math.round(v * 100), set: (v) => clamp(v / 100, 0, 1) }),
    );
    if (c.source.type === 'color') box.append(colorField('Colour', c.source, 'rgba'));
    const mw = asset?.width || S.doc.stage.width, mh = asset?.height || S.doc.stage.height;
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
      field('Free run', track, 'free_run', { type: 'checkbox' }),
      note('Free run: video on this track loops continuously and keeps moving while the timeline is paused or scrubbed — for backgrounds and ambient loops. Its cues still decide when it is visible. The loop is tied to the cluster clock, so all Runners show the same frame, but a clip does not necessarily start from its first frame.'),
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
      el('h4', {}, 'Area of the stage it shows'),
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
    el('h4', {}, 'Stage (canvas pixels)'),
    pair('W / H', st, 'width', 'height', { min: 1 }),
    colorField('Background', st, 'background'),
    el('h4', {}, 'Displays'),
  );
  for (const d of st.displays) {
    box.append(el('div', { class: 'list-item', onclick: () => select({ type: 'display', id: d.id }) },
      el('span', {}, d.name || 'display'),
      el('span', { class: 'dim' }, `${d.canvas_rect.w}×${d.canvas_rect.h}${d.runner ? ' @' + d.runner : ''}`)));
  }
  box.append(el('div', { class: 'btns' }, el('button', { onclick: () => addDisplay('') }, '+ Display')));
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
$('#btn-color').onclick = () => addCue({ name: 'Colour', source: { type: 'color', rgba: [0.1, 0.4, 0.8, 1] }, duration_s: 10, ...fitRect(640, 360, false) }, S.tl, 0, showSeconds(S.tl));
$('#btn-new').onclick = async () => {
  if (await ask('Start a new, empty show? The current one is replaced on the Director.')) edit(() => { S.doc = defaultDoc(); S.sel = null; S.tl = MAIN; });
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
  if (e.code === 'Space') { e.preventDefault(); $('#btn-play').click(); }
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
  connect(`ws://${cfg.director || `${host}:${cfg.director_port}`}/ws`, cfg.protocol);

  setInterval(() => send({ type: 'ping' }), 2000);  // the Director drops silent clients
  loadLibrary();
  setInterval(loadLibrary, 5000);                   // picks up files copied in by hand
  renderCluster();
  requestAnimationFrame(tick);
})();
