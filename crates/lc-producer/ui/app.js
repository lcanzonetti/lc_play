'use strict';
/*
 * LC Play Producer — the operator's interface.
 *
 * One file, no framework, no build step: open it in a browser and it works;
 * edit it and reload. It talks to two services directly:
 *
 *   Director       WebSocket  — receives the show + transport + cluster status,
 *                               sends show edits and transport commands
 *   Asset Manager  HTTP       — media list, upload, thumbnails
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
 * - Show time is never "pushed" to us: like a Runner, we hold the transport
 *   state and compute the position ourselves each animation frame from the
 *   cluster clock (estimated from the Director's status messages).
 *
 * Sections: helpers · state · network · time · editing · assets · stage ·
 * timeline · inspector · top bar · start-up.
 */

// ───────────────────────────── helpers ─────────────────────────────

const $ = (sel) => document.querySelector(sel);
const NS = 1e9; // nanoseconds per second
const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const round2 = (v) => Math.round(v * 100) / 100;
// Short random id. (crypto.randomUUID needs HTTPS or localhost; the Producer
// is often opened over plain HTTP from another machine.)
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

// ───────────────────────────── state ─────────────────────────────

const S = {
  assetsUrl: '',        // base URL of the Asset Manager
  ws: null,
  connected: false,
  doc: null,            // the show document being edited
  lastSent: '',         // JSON of the document as last sent/received (echo filter)
  sel: null,            // { type: 'cue' | 'display', id }
  transports: [],       // transport states known, oldest first (at most 2)
  frameRate: { num: 60, den: 1 },
  clockOffsetNs: null,  // cluster time − performance.now(), null until known
  status: null,         // last cluster status
  library: [],          // items on the Asset Manager
  pxPerSec: 40,         // timeline zoom
  undo: [],
  redo: [],
  thumbs: new Map(),    // asset id → Image
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
  for (const t of doc.tracks) {
    t.id ||= uid(); t.name ??= ''; t.cues ??= [];
    for (const c of t.cues) {
      c.id ||= uid();
      Object.assign(c, Object.assign({
        name: '', source: { type: 'color', rgba: [0.5, 0.5, 0.5, 1] },
        start_s: 0, duration_s: 10, in_s: 0, loop: false,
        x: 0, y: 0, w: 640, h: 360, rotation: 0, opacity: 1, fade_in_s: 0, fade_out_s: 0,
      }, c));
    }
  }
  return doc;
}

function findCue(id) {
  for (const [ti, track] of S.doc.tracks.entries()) {
    const ci = track.cues.findIndex((c) => c.id === id);
    if (ci >= 0) return { cue: track.cues[ci], track, ti, ci };
  }
  return null;
}
const findDisplay = (id) => S.doc.stage.displays.find((d) => d.id === id) || null;
const assetOf = (cue) => (cue.source.type === 'asset' ? S.doc.assets.find((a) => a.id === cue.source.asset) : null);
const selectedCue = () => (S.sel?.type === 'cue' ? findCue(S.sel.id)?.cue : null) || null;

// ───────────────────────────── network ─────────────────────────────

function connect(url, protocol) {
  const ws = new WebSocket(url);
  S.ws = ws;
  ws.onopen = () => {
    S.connected = true;
    ws.send(JSON.stringify({ type: 'hello', protocol, node_id: 'producer-' + uid(), role: 'producer', name: 'Producer' }));
  };
  ws.onmessage = (ev) => onMessage(JSON.parse(ev.data));
  ws.onclose = () => {
    S.connected = false;
    S.status = null;
    renderCluster();
    // The Director may simply be restarting: keep trying.
    setTimeout(() => connect(url, protocol), 1000);
  };
}

function send(message) {
  if (S.ws?.readyState === WebSocket.OPEN) S.ws.send(JSON.stringify(message));
}
const command = (cmd) => send({ type: 'command', command: cmd });

function onMessage(msg) {
  switch (msg.type) {
    case 'welcome':
      S.frameRate = msg.frame_rate;
      pushTransport(msg.transport);
      command({ type: 'status' }); // gives us the cluster clock right away
      if (msg.show_revision == null) {
        // A Director with no show yet: start it off with a usable one.
        S.doc = defaultDoc();
        sendDoc();
        refreshAll();
      }
      break;
    case 'transport':
      pushTransport(msg.state);
      break;
    case 'show': {
      const json = JSON.stringify(msg.document);
      if (json === S.lastSent) break; // our own edit coming back
      S.lastSent = json;
      S.doc = normalize(msg.document);
      refreshAll();
      break;
    }
    case 'cluster':
      S.status = msg;
      // Our estimate of the cluster clock: accurate to the network delay of
      // this message, which is ample for moving a playhead on screen.
      S.clockOffsetNs = msg.cluster_ns - performance.now() * 1e6;
      pushTransport(msg.transport);
      renderCluster();
      break;
    case 'error':
      toast(msg.message);
      break;
  }
}

// ───────────────────────────── time ─────────────────────────────
// The same arithmetic a Runner does (lc-proto/src/transport.rs), in floating
// point — good to well under a frame, which is all a UI needs.

function pushTransport(state) {
  const newest = S.transports[S.transports.length - 1];
  if (newest && state.epoch <= newest.epoch) return;
  S.transports.push(state);
  if (S.transports.length > 2) S.transports.shift();
}

const clusterNow = () => (S.clockOffsetNs == null ? null : performance.now() * 1e6 + S.clockOffsetNs);

/** The transport state in force now: the newest whose anchor has passed. */
function currentTransport() {
  const now = clusterNow();
  const list = S.transports;
  if (!list.length) return null;
  for (let i = list.length - 1; i >= 0; i--) if (now == null || list[i].anchor_cluster_ns <= now) return list[i];
  return list[0];
}

/** Show time in seconds, now. */
function showSeconds() {
  const t = currentTransport();
  if (!t) return 0;
  const now = clusterNow();
  const elapsed = t.playing && now != null ? Math.max(0, now - t.anchor_cluster_ns) : 0;
  let ns = t.anchor_show_ns + (elapsed * t.rate_permille) / 1000;
  ns = t.loop_ns > 0 ? ((ns % t.loop_ns) + t.loop_ns) % t.loop_ns : Math.max(0, ns);
  return ns / NS;
}

const seek = (seconds) => command({ type: 'seek', show_ns: Math.round(Math.max(0, seconds) * NS) });

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
  renderTimeline();
  if (inspector) renderInspector();
  scheduleSend();
}

let sendTimer = 0;
/** Send soon, but not on every mouse-move: at most ~7 documents a second
 *  while dragging, which the outputs follow live. */
function scheduleSend() {
  if (!sendTimer) sendTimer = setTimeout(sendDoc, 150);
}
function sendDoc() {
  clearTimeout(sendTimer);
  sendTimer = 0;
  const json = JSON.stringify(S.doc);
  if (json === S.lastSent) return;
  S.lastSent = json;
  command({ type: 'load_show', document: S.doc });
}

function stepHistory(from, to) {
  if (!from.length) return;
  to.push(JSON.stringify(S.doc));
  S.doc = JSON.parse(from.pop());
  refreshAll();
  scheduleSend();
}

function refreshAll() {
  // The selection may refer to something that no longer exists.
  if (S.sel && !(S.sel.type === 'cue' ? findCue(S.sel.id) : findDisplay(S.sel.id))) S.sel = null;
  renderTimeline();
  renderInspector();
}

function select(sel) {
  S.sel = sel;
  renderTimeline();
  renderInspector();
}

function deleteSelection() {
  if (!S.sel) return;
  const { type, id } = S.sel;
  edit(() => {
    if (type === 'cue') {
      const hit = findCue(id);
      if (hit) hit.track.cues.splice(hit.ci, 1);
    } else {
      S.doc.stage.displays = S.doc.stage.displays.filter((d) => d.id !== id);
    }
    S.sel = null;
  });
}

/** Add a cue to track `ti` at time `t`, select it, and grow the timeline if
 *  the cue ends after it. */
function addCue(cue, ti, t) {
  edit(() => {
    if (!S.doc.tracks.length) S.doc.tracks.push({ id: uid(), name: 'Track 1', cues: [] });
    const track = S.doc.tracks[clamp(ti, 0, S.doc.tracks.length - 1)];
    cue.id = uid();
    cue.start_s = round2(Math.max(0, t));
    track.cues.push(cue);
    S.doc.duration_s = Math.max(S.doc.duration_s, Math.ceil(cue.start_s + cue.duration_s));
    normalize(S.doc);
    S.sel = { type: 'cue', id: cue.id };
  });
}

/** Largest rectangle with the media's aspect that fits the stage, centred. */
function fitRect(mediaW, mediaH, enlarge) {
  const { width: sw, height: sh } = S.doc.stage;
  const k = Math.min(sw / mediaW, sh / mediaH, enlarge ? Infinity : 1);
  const w = Math.round(mediaW * k), h = Math.round(mediaH * k);
  return { x: Math.round((sw - w) / 2), y: Math.round((sh - h) / 2), w, h };
}

function addAssetCue(item, ti, t) {
  const kind = item.kind; // 'video' | 'image'
  // Record the asset in the show, with the URL Runners will fetch it from.
  const entry = {
    id: item.id, name: item.name, kind,
    url: `${S.assetsUrl}/stream/${item.id}`,
    size: item.size, width: item.width || 0, height: item.height || 0,
    duration_s: item.duration_s || 0,
  };
  const rect = fitRect(item.width || 1920, item.height || 1080, false);
  const cue = {
    name: item.name, source: { type: 'asset', asset: item.id },
    duration_s: kind === 'video' && item.duration_s > 0 ? round2(item.duration_s) : 10,
    ...rect,
  };
  // Upsert before the cue is added: validation requires the asset to exist.
  S.doc.assets = S.doc.assets.filter((a) => a.id !== item.id).concat(entry);
  addCue(cue, ti, t);
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
  } catch {
    if (S.library !== null) renderAssets('Asset Manager not reachable at ' + S.assetsUrl);
    S.library = null;
  }
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
      ondblclick: () => addAssetCue(item, 0, showSeconds()),
    },
      el('img', { src: `${S.assetsUrl}/thumb/${item.id}`, loading: 'lazy', draggable: 'false' }),
      el('div', {}, el('div', { class: 'name' }, item.name), el('div', { class: 'meta' }, meta)),
      el('button', { class: 'danger', title: 'Delete from the Asset Manager', onclick: (e) => { e.stopPropagation(); deleteAsset(item); } }, '✕'),
    ));
  }
}

async function deleteAsset(item) {
  const used = S.doc.tracks.some((t) => t.cues.some((c) => c.source.asset === item.id));
  if (!confirm(`Delete "${item.name}" from the Asset Manager?` + (used ? '\n\nIt is used in this show; those cues will show a placeholder.' : ''))) return;
  await fetch(`${S.assetsUrl}/api/items/${item.id}`, { method: 'DELETE' });
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

/** Thumbnail image for an asset of the show (loaded once, then cached). */
function thumbFor(asset) {
  let img = S.thumbs.get(asset.id);
  if (!img) {
    img = new Image();
    img.src = asset.url.replace('/stream/', '/thumb/');
    S.thumbs.set(asset.id, img);
  }
  return img.complete && img.naturalWidth ? img : null;
}

// ───────────────────────────── stage ─────────────────────────────
// A 2D-canvas preview of the stage at the current show time: the canvas, the
// displays cut from it, and the cues active now (as thumbnails — this is a
// layout view, not a video monitor). Redrawn every animation frame.

const stage = $('#stage');
const ctx = stage.getContext('2d');
let view = { k: 1, ox: 0, oy: 0 };  // canvas px → screen px: s = c·k + o
let displayTabs = [];               // screen rects of display name tabs, for hit-testing
let drag = null;                    // the stage drag in progress
const HANDLE = 5;                   // half-size of a resize handle, screen px

function drawStage(t) {
  const wrap = $('#stage-wrap');
  const dpr = window.devicePixelRatio || 1;
  const W = wrap.clientWidth, H = wrap.clientHeight;
  if (stage.width !== W * dpr || stage.height !== H * dpr) { stage.width = W * dpr; stage.height = H * dpr; }
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

  // Cues, back to front: the last track is at the back.
  const sel = selectedCue();
  for (const track of [...S.doc.tracks].reverse()) {
    for (const cue of track.cues) {
      const active = t >= cue.start_s && t < cue.start_s + cue.duration_s;
      // The selected cue is always shown (ghosted when not on air), so it
      // can be positioned without scrubbing to it first.
      if (!active && cue !== sel) continue;
      const fade = clamp(cue.fade_in_s > 0 ? (t - cue.start_s) / cue.fade_in_s : 1, 0, 1)
                 * clamp(cue.fade_out_s > 0 ? (cue.start_s + cue.duration_s - t) / cue.fade_out_s : 1, 0, 1);
      ctx.save();
      ctx.translate(cue.x + cue.w / 2, cue.y + cue.h / 2);
      ctx.rotate((cue.rotation * Math.PI) / 180);
      ctx.globalAlpha = active ? cue.opacity * fade : 0.25;
      const asset = assetOf(cue);
      const img = asset && thumbFor(asset);
      if (img) ctx.drawImage(img, -cue.w / 2, -cue.h / 2, cue.w, cue.h);
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
  const t = showSeconds();
  for (const track of S.doc.tracks) {
    for (const cue of [...track.cues].reverse()) {
      const visible = (t >= cue.start_s && t < cue.start_s + cue.duration_s) || cue === sel;
      if (visible && cueContains(cue, cx, cy)) {
        select({ type: 'cue', id: cue.id });
        drag = { ...start, kind: 'move', target: cue, orig: { ...cue } };
        return;
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
// Tracks top to bottom (the first is in front), time left to right. Built
// from plain DOM elements; rebuilt whenever the document changes.

const HEAD_W = 120; // width of the track-name column; matches --head-w in CSS
let tlDrag = null;  // the timeline drag in progress

function renderTimeline() {
  if (!S.doc) return;
  const pps = S.pxPerSec, width = S.doc.duration_s * pps;
  $('#tl-body').style.width = HEAD_W + width + 40 + 'px';

  // Ruler: pick the smallest "nice" step that leaves labels ≥ 60 px apart.
  const ruler = $('#ruler');
  ruler.style.width = width + 'px';
  ruler.replaceChildren();
  const step = [0.5, 1, 2, 5, 10, 30, 60, 120, 300, 600].find((s) => s * pps >= 60) || 600;
  for (let t = 0; t <= S.doc.duration_s; t += step) {
    const label = t >= 60 ? `${Math.floor(t / 60)}:${String(Math.round(t % 60)).padStart(2, '0')}` : `${t}s`;
    ruler.append(el('div', { class: 'tick', style: `left:${t * pps}px` }, label));
  }

  const tracks = $('#tracks');
  tracks.replaceChildren();
  S.doc.tracks.forEach((track, ti) => {
    const lane = el('div', {
      class: 'lane', style: `width:${width}px`, dataset: { ti },
      ondragover: (e) => { e.preventDefault(); lane.classList.add('drop'); },
      ondragleave: () => lane.classList.remove('drop'),
      ondrop: (e) => {
        e.preventDefault();
        lane.classList.remove('drop');
        const item = S.library?.find((i) => i.id === e.dataTransfer.getData('text/lcplay-asset'));
        if (item) addAssetCue(item, ti, (e.clientX - lane.getBoundingClientRect().left) / pps);
      },
    });
    for (const cue of track.cues) {
      const selected = S.sel?.type === 'cue' && S.sel.id === cue.id;
      lane.append(el('div', {
        class: `cue ${cue.source.type}${selected ? ' sel' : ''}`, dataset: { id: cue.id },
        style: `left:${cue.start_s * pps}px;width:${cue.duration_s * pps}px`, title: cue.name,
      }, cue.name || (cue.source.type === 'color' ? 'colour' : 'cue'),
        el('div', { class: 'h l' }), el('div', { class: 'h r' })));
    }
    tracks.append(el('div', { class: 'track' },
      el('div', { class: 'track-head' },
        el('span', {
          class: 'name', title: 'Double-click to rename',
          ondblclick: () => { const n = prompt('Track name', track.name); if (n != null) edit(() => (track.name = n)); },
        }, track.name || `Track ${ti + 1}`),
        el('button', {
          class: 'danger', title: 'Delete track',
          onclick: () => { if (!track.cues.length || confirm(`Delete "${track.name}" and its ${track.cues.length} cue(s)?`)) edit(() => S.doc.tracks.splice(ti, 1)); },
        }, '✕')),
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

function timelineDragMove(e) {
  const hit = findCue(tlDrag.id);
  if (!hit) return;
  if (!tlDrag.undone) { S.undo.push(JSON.stringify(S.doc)); S.redo = []; tlDrag.undone = true; }
  const dt = (e.clientX - tlDrag.mx) / S.pxPerSec, o = tlDrag.orig, c = hit.cue;
  if (tlDrag.kind === 'move') {
    c.start_s = round2(Math.max(0, o.start_s + dt));
    // Dragging up/down moves the cue to the track under the pointer.
    const lane = document.elementFromPoint(e.clientX, e.clientY)?.closest('.lane');
    const ti = lane ? Number(lane.dataset.ti) : hit.ti;
    if (ti !== hit.ti && S.doc.tracks[ti]) {
      hit.track.cues.splice(hit.ci, 1);
      S.doc.tracks[ti].cues.push(c);
    }
  } else if (tlDrag.kind === 'length') {
    c.duration_s = round2(Math.max(0.1, o.duration_s + dt));
  } else {
    // Trim the start: the end stays where it is, and a video's in-point
    // moves with it so the picture at any given show time does not change.
    const d = clamp(dt, -o.start_s, o.duration_s - 0.1);
    c.start_s = round2(o.start_s + d);
    c.duration_s = round2(o.duration_s - d);
    if (assetOf(c)?.kind === 'video') c.in_s = round2(Math.max(0, o.in_s + d));
  }
  S.doc.duration_s = Math.max(S.doc.duration_s, Math.ceil(c.start_s + c.duration_s));
  renderTimeline();
  renderInspector();
  scheduleSend();
}

// Scrubbing: press or drag on the ruler to move the playhead.
let scrubbing = false, lastScrub = 0;
function scrub(e) {
  const t = clamp((e.clientX - $('#ruler').getBoundingClientRect().left) / S.pxPerSec, 0, S.doc.duration_s);
  // Each seek is a transport change for the whole cluster: do not flood it.
  if (performance.now() - lastScrub > 60) { seek(t); lastScrub = performance.now(); }
}
$('#ruler').addEventListener('mousedown', (e) => { scrubbing = true; lastScrub = 0; scrub(e); });

// Drags are followed on the window, so they continue outside their panel.
window.addEventListener('mousemove', (e) => {
  if (drag) stageDragMove(e);
  else if (tlDrag) timelineDragMove(e);
  else if (scrubbing) scrub(e);
});
window.addEventListener('mouseup', (e) => {
  if (scrubbing) { lastScrub = 0; scrub(e); }
  if ((drag?.undone) || (tlDrag?.undone)) sendDoc(); // final position, immediately
  drag = tlDrag = null;
  scrubbing = false;
});

// ───────────────────────────── inspector ─────────────────────────────
// Properties of whatever is selected: a cue, a display, or — with nothing
// selected — the show itself.

/** A labelled input bound to obj[key]. Changes are committed on Enter/blur
 *  (not per keystroke), each as one undo step. */
function field(label, obj, key, opts = {}) {
  const { type = 'number', step = 1, min, get = (v) => v, set = (v) => v, after } = opts;
  const input = el('input', { type, step, min });
  if (type === 'checkbox') input.checked = !!obj[key];
  else input.value = get(obj[key]) ?? '';
  input.addEventListener('change', () => {
    let v = type === 'checkbox' ? input.checked : type === 'number' ? (input.value === '' ? null : Number(input.value)) : input.value;
    if (type === 'number' && v != null && (Number.isNaN(v) || (min != null && v < min))) { input.value = get(obj[key]) ?? ''; return; }
    edit(() => { obj[key] = set(v); after?.(); }, { inspector: false });
  });
  return el('div', { class: 'row' }, el('label', {}, label), input);
}

/** Two numbers on one row (x/y, w/h): [label, obj, keyA, keyB, opts]. */
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

function colorField(label, obj, key) {
  const input = el('input', { type: 'color', value: rgbaToHex(obj[key]) });
  input.addEventListener('change', () => edit(() => (obj[key] = [...hexToRgb(input.value), obj[key][3] ?? 1]), { inspector: false }));
  return el('div', { class: 'row' }, el('label', {}, label), input);
}

function renderInspector() {
  const box = $('#inspector');
  // Do not rebuild under the operator's cursor while they are typing here.
  if (box.contains(document.activeElement) && document.activeElement.tagName === 'INPUT' && (drag || tlDrag) === null && box.dataset.sel === JSON.stringify(S.sel)) {
    return;
  }
  box.dataset.sel = JSON.stringify(S.sel);
  box.replaceChildren();
  if (!S.doc) return;
  const sec = { step: 0.1, min: 0 };

  if (S.sel?.type === 'cue') {
    const c = findCue(S.sel.id).cue, asset = assetOf(c);
    box.append(
      el('h3', {}, 'Cue'),
      field('Name', c, 'name', { type: 'text' }),
      el('h4', {}, 'Time'),
      field('Start (s)', c, 'start_s', sec),
      field('Duration (s)', c, 'duration_s', { step: 0.1, min: 0.1 }),
      field('Fade in (s)', c, 'fade_in_s', sec),
      field('Fade out (s)', c, 'fade_out_s', sec),
    );
    if (asset?.kind === 'video') box.append(field('In point (s)', c, 'in_s', sec), field('Loop video', c, 'loop', { type: 'checkbox' }));
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
      el('button', { onclick: () => seek(c.start_s) }, 'Go to cue'),
      el('button', { class: 'danger', onclick: deleteSelection }, 'Delete')));
    return;
  }

  if (S.sel?.type === 'display') {
    const d = findDisplay(S.sel.id);
    const frac = { step: 0.01, min: 0 };
    box.append(
      el('h3', {}, 'Display'),
      field('Name', d, 'name', { type: 'text' }),
      field('Runner', d, 'runner', { type: 'text' }),
      el('div', { class: 'dim' }, "The Runner's --name; empty = any Runner."),
      el('h4', {}, 'Area of the stage it shows'),
      pair('X / Y', d.canvas_rect, 'x', 'y'),
      pair('W / H', d.canvas_rect, 'w', 'h', { min: 1 }),
      el('h4', {}, "Window on the Runner"),
      field('Fullscreen', d.window, 'fullscreen', { type: 'checkbox' }),
      field('Monitor #', d.window, 'monitor', { min: 0 }),
      optionalPair('Position', d.window, 'position'),
      optionalPair('Size', d.window, 'size', 1),
      el('div', { class: 'dim' }, 'Windowed: position and size in desktop pixels. Fullscreen: uses the monitor number the Runner logs at start-up.'),
      el('h4', {}, 'Output'),
      field('Rotation (°)', d, 'rotation_degrees', { step: 90, min: 0, set: (v) => (((v % 360) / 90) | 0) * 90 }),
      el('h4', {}, 'Edge blend (fraction of the output)'),
      pair('Left / Right', d.blend, 'left', 'right', frac),
      pair('Top / Bottom', d.blend, 'top', 'bottom', frac),
      field('Gamma', d.blend, 'gamma', { step: 0.1, min: 0.1 }),
      el('div', { class: 'btns' }, el('button', { class: 'danger', onclick: deleteSelection }, 'Delete display')),
    );
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
  box.append(el('div', { class: 'btns' }, el('button', {
    onclick: () => edit(() => {
      const n = st.displays.length + 1;
      const d = { id: uid(), name: `Display ${n}`, canvas_rect: { x: 0, y: 0, w: st.width, h: st.height },
        window: { fullscreen: false, position: [60 + 40 * n, 60 + 40 * n], size: [960, Math.round((960 * st.height) / st.width)] } };
      st.displays.push(d);
      normalize(S.doc);
      S.sel = { type: 'display', id: d.id };
    }),
  }, '+ Display')));
}

// ───────────────────────────── top bar ─────────────────────────────

/** Looping wraps at the timeline length; keep the two in step. */
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
$('#btn-stop').onclick = () => { command({ type: 'pause' }); seek(0); };
$('#chk-loop').onchange = (e) => command({ type: 'set_loop', loop_ns: e.target.checked ? Math.round(S.doc.duration_s * NS) : null });
$('#sel-rate').onchange = (e) => command({ type: 'set_rate', rate_permille: Number(e.target.value) });
$('#btn-undo').onclick = () => stepHistory(S.undo, S.redo);
$('#btn-redo').onclick = () => stepHistory(S.redo, S.undo);
$('#btn-track').onclick = () => edit(() => S.doc.tracks.push({ id: uid(), name: `Track ${S.doc.tracks.length + 1}`, cues: [] }));
$('#btn-color').onclick = () => addCue({ name: 'Colour', source: { type: 'color', rgba: [0.1, 0.4, 0.8, 1] }, duration_s: 10, ...fitRect(640, 360, false) }, 0, showSeconds());
$('#zoom').oninput = (e) => { S.pxPerSec = Number(e.target.value); renderTimeline(); };
$('#btn-new').onclick = () => { if (confirm('Start a new, empty show? The current one is replaced on the Director.')) edit(() => { S.doc = defaultDoc(); S.sel = null; }); };
$('#btn-save').onclick = () => {
  const a = el('a', { href: URL.createObjectURL(new Blob([JSON.stringify(S.doc, null, 2)], { type: 'application/json' })), download: `${S.doc.name || 'show'}.lcplay.json` });
  a.click();
  URL.revokeObjectURL(a.href);
};
$('#btn-open').onclick = () => $('#file-open').click();
$('#file-open').onchange = async (e) => {
  const file = e.target.files[0];
  e.target.value = '';
  if (!file) return;
  try {
    const doc = normalize(JSON.parse(await file.text()));
    edit(() => { S.doc = doc; S.sel = null; });
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

/** Per-frame update of everything that moves with time. */
function tick() {
  const t = showSeconds(), tr = currentTransport();
  const hh = Math.floor(t / 3600), mm = Math.floor(t / 60) % 60, ss = (t % 60).toFixed(2).padStart(5, '0');
  $('#time').textContent = `${String(hh).padStart(2, '0')}:${String(mm).padStart(2, '0')}:${ss}`;
  $('#frame').textContent = 'f ' + Math.floor((t * S.frameRate.num) / S.frameRate.den);
  $('#btn-play').textContent = tr?.playing ? '⏸' : '▶';
  // Loop and rate reflect the cluster's state (another controller — OSC,
  // lc-ctl — may have changed them), unless the operator is on the control.
  if (tr && document.activeElement !== $('#chk-loop')) $('#chk-loop').checked = tr.loop_ns > 0;
  if (tr && document.activeElement !== $('#sel-rate')) $('#sel-rate').value = String(tr.rate_permille);
  $('#playhead').style.left = HEAD_W + t * S.pxPerSec + 'px';
  drawStage(t);
  requestAnimationFrame(tick);
}

// ───────────────────────────── start-up ─────────────────────────────

(async function start() {
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
