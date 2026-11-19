# LC Play wire protocol (version 2)

Source of truth: `crates/lc-proto` (messages) and `crates/lc-time/src/wire.rs`
(sync packet). This file is the human-readable summary. All times are signed
64-bit **nanoseconds**.

| Channel | Transport | Default | Encoding |
|---|---|---|---|
| Discovery | UDP multicast `239.255.76.67` | port 7400 | JSON datagram |
| Control | WebSocket `ws://director:7401/ws` | TCP 7401 | JSON text frames |
| Clock sync | UDP unicast | port 7402 | fixed 44-byte binary |
| HTTP helpers | `GET /status`, `POST /command` | TCP 7401 | JSON |
| OSC | UDP unicast | port 7405 | OSC 1.0 |
| Assets | HTTP | TCP 7403 | files, JSON |

## 1. Discovery beacon

Sent once a second by every node, TTL 1.

```json
{ "magic": "lcplay", "protocol": 1, "node_id": "…uuid…", "role": "director",
  "name": "main", "control_port": 7401, "sync_port": 7402, "clock_id": 1234 }
```

`role` is `director | runner | producer | assets | controller`. The node's IP
is the datagram's source address. Datagrams with another `magic` or `protocol`
are ignored.

## 2. Control channel

The client connects to the Director and sends `hello` first; the Director
answers `welcome` (or `error` and closes). A client must send something at
least every 6 s (`ping` or `health`), or it is dropped.

### Client → Director

```json
{ "type": "hello", "protocol": 1, "node_id": "…", "role": "runner", "name": "left",
  "runner": { "show_revision": 3, "show_id": "…uuid…", "clock_id": 1234,
              "cluster_now_ns": 98765432100, "transport": { … } } }
{ "type": "command", "command": { "type": "play" }, "timeline": null }
{ "type": "health", "cluster_ns": 0, "show_ns": 0, "frame": 0, "transport_epoch": 2,
  "show_revision": 3, "fps": 59.9,
  "sync": { "locked": true, "samples": 64, "offset_ns": 0, "drift_ppm": -3.1,
            "rtt_ns": 180000, "error_ns": 21000, "holdover_ns": 400000000 },
  "vblank": { "vblank_ns": 98765000000, "period_ps": 16683333333, "exact": true },
  "output_health": [ { "id": "d1", "gpu": "NVIDIA RTX A5000 (Dx12)", "presenter": "dxgi",
      "refresh_hz": 59.94, "presents": 35964, "missed": 0, "late_decodes": 0,
      "latency_frames": 2, "barrier": "cluster", "grid_offset_ns": -42000,
      "frame_counter": 1234567 } ] }
{ "type": "ping" }
{ "type": "logs", "lines": [ "…" ] }
{ "type": "preview", "outputs": [ { "name": "Left", "width": 480, "height": 270, "png_base64": "…" } ] }
```

`logs` and `preview` are only sent by Runners, in answer to `send_logs` /
`send_preview`.

`runner` is only sent by Runners. `show_id` identifies the cached show; the
Director sends the show unless it matches (revisions are for humans — they
restart if a Director loses its state, ids are unique per load). `clock_id`,
`cluster_now_ns` and `transport` are present only when the Runner is still
following a timebase (holdover); they allow a restarted Director to adopt it,
and a Director to pick up a transport state newer than its own.

Commands:

| `type` | Fields | Effect |
|---|---|---|
| `play`, `pause` | — | |
| `stop` | — | main timeline: pause and rewind; a task: take it off stage |
| `runner_action` | `runner` (name or node id), `action` | see below |
| `wake_runner` | `runner` | the Director sends a Wake-on-LAN packet to the MAC that Runner last reported |
| `start_runner` | `runner` | the Director sends `{ "start_runner": <node id> }` (UDP, to the address and `launcher_port` that Runner last reported, default 7406); its supervisor, waiting since the Runner was closed, starts it |
| `seek` | `show_ns ≥ 0` | jump; play state unchanged |
| `set_rate` | `rate_permille` (1000 = 1×, negative = reverse, ‖rate‖ ≤ 16000) | position stays continuous |
| `set_loop` | `loop_ns` or `null` | wrap show time into `0..loop_ns` |
| `load_show` | `document` (JSON object), optional `tag` | new show revision, distributed to Runners; replaces the show for everyone |
| `edit_show` | `ops` (list of edit operations), `tag` `{ "by": <editor id>, "n": <edit number> }` | applies the operations to the current show, then as `load_show`; see below |
| `status` | — | Director replies with `cluster` |

`edit_show` is how Producers edit, so that several can work on one show at
once (lc-proto `edit.rs`). An operation is one of

```json
{ "op": "set",    "path": ["stage", "displays", { "id": "d1" }, "warp_corners"], "value": [[0,0],[1,0],[1,1],[0,1]] }
{ "op": "unset",  "path": ["tracks", { "id": "t1" }, "name"] }
{ "op": "insert", "path": ["tracks", { "id": "t1" }, "cues"], "after": "c3", "value": { "id": "c9", … } }
{ "op": "remove", "path": ["tracks", { "id": "t1" }, "cues", { "id": "c3" }] }
{ "op": "move",   "path": ["tracks", { "id": "t2" }], "after": null }
```

A path step is a field name, or `{ "id": … }` for the element of a list
(`tracks`, `cues`, `displays`, `virtuals`, `tasks`, `control_cues`, `assets`) with that
id. `after: null` inserts or moves to the start; an `after` that no longer
exists, to the end. The Director applies edits in the order they arrive;
operations whose target someone else has deleted meanwhile are skipped, and
for the same field the later edit wins. Ids are unique among all lists of a
kind (all `cues` lists together, all `tracks`, …): an `insert` of an id that
exists in a list of that kind is skipped (so a cue moved to two tracks at
once is not duplicated), an edit that would still leave two elements of a
kind with one id is refused, a `set` of a list element must keep its id, and
nothing may write `_applied`. Edits are limited to 2000 operations. If the result is not a valid show, or is
nested deeper than 64 levels, the whole edit is refused: `{ "type": "error",
"message": "edit_show: …", "edit": <its tag> }`. Every element of those lists
gets an id on load if it has none (`auto-<n>`).

Each editor numbers its edits 1, 2, 3, …; the show records the last number
applied per editor in `_applied` (`{ "<editor id>": [n, revision] }`, the 64
most recent editors). An edit whose number is not above the recorded one is
ignored — it was already applied — so edits can safely be sent again after a
reconnect. A Producer drops its own edits up to its recorded number from
those it still has pending, and shows the Director's show with the rest
applied on top. When a show is loaded, a track-level `free_run` (older shows)
is converted to per-cue `free_run` + `loop`.

A Producer re-sends its unconfirmed edits after reconnecting (one that had
arrived is recognised by its number and ignored). Presence is relayed at
most five times a second per client.

Producers also send `{ "type": "presence", "name": "Alice", "color":
"#ff6b6b", "selection": { "type": "cue", "id": "c1" }, "dragging": false }`
(on selection changes and every 3 s; name ≤ 64 characters, colour `#rgb` to
`#rrggbbaa`, selection ≤ 4096 bytes of JSON, or it is dropped). Several
cues selected: `{ "type": "cue", "id": "c1", "ids": ["c1", "c2", …] }`
(`id`, the cue clicked last, is also in `ids`); the Producer trims `ids` to
stay within the limit. The Director relays it to the other
Producers and controllers — never to Runners — as `{ "type": "presence",
"node_id": "…", "presence": { … } }`, and with `"presence": null` when that
Producer disconnects.

Runner actions: `reload_outputs` (close and rebuild the outputs), `restart`
and `quit` (the Runner program; after `quit` its supervisor waits for
`start_runner`), `exit` (the Runner and its supervisor), `standby` / `resume` (close / reopen the
outputs, staying connected), `reboot` and `power_off` (the computer; only if
the Runner was started with `--allow-power`), `send_logs` (it replies with
`logs`), `send_preview` (it replies with `preview`).

Transport commands take effect `lead_ns` (default 100 ms) after the Director
receives them, simultaneously on all Runners. `timeline` on the `command`
message selects the transport they act on: `null`/absent/`"main"` is the main
timeline, otherwise the id of an auxiliary timeline (task) of the show.

### Director → client

```json
{ "type": "welcome", "protocol": 2, "director_id": "…", "name": "main",
  "clock_id": 1234, "sync_port": 7402, "lead_ns": 100000000,
  "frame_rate": { "num": 60, "den": 1 }, "transport": { … }, "show_revision": 3 }
{ "type": "transport", "clock_id": 1234, "state": { … }, "timeline": null }
{ "type": "runner_action", "node_id": "…", "action": "reload_outputs" }
{ "type": "runner_logs", "node_id": "…", "name": "left", "lines": [ "…" ] }
{ "type": "runner_preview", "node_id": "…", "name": "left", "outputs": [ … ] }
{ "type": "show", "revision": 3, "id": "…uuid…", "document": { … } }
{ "type": "cluster", "clock_id": 1234, "cluster_ns": 0, "frame_rate": { … },
  "transport": { … }, "show_ns": 0, "frame": 0, "show_revision": 3,
  "nodes": [ { "node_id": "…", "role": "runner", "name": "left", "addr": "10.0.0.11:51000",
               "health": { … }, "clock_skew_ns": -350000, "frame_error": 0 } ] }
{ "type": "grid", "grid": { "origin_ns": 1000000000000, "period_ps": 16683333333,
                             "valid_from_ns": 1000200000000, "generation": 3 } }
{ "type": "error", "message": "seek: show time must not be negative" }
{ "type": "pong" }
```

`cluster` is pushed every second to non-Runner clients, and on `status`. It
also carries `grid` and `grid_source` (the node id of the reference Runner).

`grid` is the refresh grid (see ARCHITECTURE §3.3), sent to everyone when it
changes and to a Runner right after its `welcome`. A Runner renders the frame
for refresh instant `t` as the frame for
`origin_ns + floor(k × period_ps / 1000)`, with `k` the grid line nearest to
`t`, when `t` is within a quarter period of it; lines at or after
`valid_from_ns` (decided on the previous grid's instants) use the new grid.

### Transport state

```json
{ "epoch": 7, "playing": true, "rate_permille": 1000,
  "anchor_cluster_ns": 123000000000, "anchor_show_ns": 42000000000, "loop_ns": null,
  "active": true }
```

From cluster time `anchor_cluster_ns` on:

```
show_ns = anchor_show_ns + (cluster_ns − anchor_cluster_ns) × rate_permille / 1000     (if playing)
show_ns = anchor_show_ns                                                               (if paused)
then: wrapped into 0..loop_ns if looping, else clamped at ≥ 0
frame   = floor(show_ns × num / (den × 1e9))
```

Every timeline has its own transport state and its own epochs. `welcome` and
`cluster` carry the tasks' states in `tasks` (id → state); a task with no
entry has never been started. `active: false` means the timeline is off
stage: nothing of it is drawn (a paused timeline, by contrast, holds its
picture). The main timeline is always active.

A Runner's `health` also reports `monitors` (index, name, size, position),
`gpu`, `outputs` (open windows), `standby`, `output_health` (per output:
presenter, refresh rate, presents, missed presents, late decodes, frames
queued, NVIDIA present-barrier state, distance from the refresh grid,
frame-lock counter), `vblank` (the first output's refresh, which the Director
builds the grid from), `downloads` (media being copied into its cache:
`name`, `done` and `total` bytes, `bytes_per_s`, and `error` / `retry_in_s`
while waiting to retry), and `machine`: `hostname`, `os`,
`version`, `ip`, `mac`, `allow_power`, `supervised`, `launcher_port`. `cluster` carries
`known_runners`: every Runner the Director has seen (`name`, `node_id`,
`machine`, `online`), so offline ones can be listed and woken.
`runner_logs` / `runner_preview` are relayed to Producers and controllers,
not to Runners.

A receiver keeps the state it has until the new one's anchor is reached, and
ignores any state whose `epoch` is not higher than the newest it knows (within
one `clock_id`). Integer arithmetic throughout, so every node gets the same
result.

## 3. Clock sync packet

Big-endian, 44 bytes, same layout for request and response:

| Offset | Size | Field | |
|---|---|---|---|
| 0 | 4 | magic | `"LCTS"` |
| 4 | 1 | version | 1 |
| 5 | 1 | kind | 0 = request, 1 = response |
| 6 | 2 | reserved | 0 |
| 8 | 4 | seq | echoed |
| 12 | 8 | clock_id | timebase of t2/t3 (0 in requests) |
| 20 | 8 | t1 | client clock at send (echoed) |
| 28 | 8 | t2 | cluster time at request receive |
| 36 | 8 | t3 | cluster time at response send |

The client notes `t4` (its clock at receive) and computes, as in NTP
(RFC 5905 §8):

```
offset = ((t2 − t1) + (t3 − t4)) / 2
delay  =  (t4 − t1) − (t3 − t2)
```

Responses are matched to requests by `seq` and the echoed `t1`, not by source
address (a multi-homed Director may answer from another address). Responses
carrying a `clock_id` other than the one announced in `welcome` are ignored:
which Director to follow is decided on the control channel only.

## 4. HTTP helpers

```bash
curl http://director:7401/status
curl -X POST http://director:7401/command -H 'content-type: application/json' \
     -d '{"type":"seek","show_ns":42000000000}'
```

## 5. Show document

The `document` of `load_show` / `show`. Source of truth:
`crates/lc-proto/src/show.rs`. Every field is optional and has a default;
unknown fields are preserved. The Director refuses a document that fails
validation with an `error` naming the problem: non-finite numbers,
non-positive sizes, a display or window larger than 16384 pixels per side,
warp corners that are not a convex quad, a cue referring to an unknown
asset.

The Producer adds a top-level `_edit: { "by": <client id>, "n": <counter> }`
to each document it sends, to recognise its own edits when the Director
broadcasts them back. Other tools may ignore it.

```json
{
  "name": "Demo", "duration_s": 60,
  "stage": {
    "width": 3840, "height": 1080, "background": [0, 0, 0, 1],
    "displays": [
      { "id": "d1", "name": "Left", "runner": "left",
        "canvas_rect": { "x": 0, "y": 0, "w": 1920, "h": 1080 },
        "window": { "fullscreen": true, "monitor": 1, "position": null, "size": null },
        "rotation_degrees": 0,
        "warp_corners": [[0,0],[1,0],[1,1],[0,1]],
        "blend": { "left": 0, "right": 0.125, "top": 0, "bottom": 0, "gamma": 2.2 } }
    ],
    "virtuals": [
      { "id": "v1", "name": "LED feed",
        "canvas_rect": { "x": 4000, "y": 0, "w": 960, "h": 540 },
        "width": 1920, "height": 1080, "keep_aspect": true }
    ]
  },
  "assets": [
    { "id": "636c6970", "name": "clip.mp4", "kind": "video",
      "url": "http://10.0.0.1:7403/optimized/636c6970?v=1794930967",
      "size": 594087, "path": "Optimized/clip.mov",
      "width": 1920, "height": 1080, "duration_s": 12.0 }
  ],
  "control_cues": [
    { "id": "k1", "at_s": 30, "action": "pause", "target": "", "to_s": 0 },
    { "id": "k2", "at_s": 10, "action": "play", "target": "sting" }
  ],
  "tasks": [
    { "id": "sting", "name": "Sting", "duration_s": 8, "tracks": [ … ], "control_cues": [ … ] }
  ],
  "tracks": [
    { "id": "t1", "name": "Track 1", "cues": [
      { "id": "c1", "name": "clip.mp4",
        "source": { "type": "asset", "asset": "636c6970" },
        "start_s": 0, "duration_s": 12, "in_s": 0, "loop": false, "free_run": false,
        "x": 0, "y": 0, "w": 1920, "h": 1080,
        "rotation": 0, "opacity": 1, "fade_in_s": 1, "fade_out_s": 1,
        "keep_aspect": false }
    ] }
  ]
}
```

* Space: canvas pixels, origin top-left, y down; the canvas is unbounded
  (negative positions are fine). `stage.width/height` must be positive but
  are informational: the Producer sets them to the displays' bottom-right
  extent. `x, y, w, h` is the cue's
  rectangle; `rotation` is degrees clockwise about its centre.
* `source` is `{ "type": "asset", "asset": <id> }`,
  `{ "type": "color", "rgba": [r, g, b, a] }` (linear 0..1), or
  `{ "type": "sync_test", "fps": 60 }` — a test pattern drawn by the Runner:
  frame number in digits and 16 binary cells, a marker stepping one place per
  frame, and a white flash on the first frame of each second — or
  `{ "type": "virtual", "virtual": <id> }`, the live picture of a virtual
  display (`in_s`, `loop` and `free_run` do not apply).
* `stage.virtuals` are virtual displays: `canvas_rect` is the area captured,
  `width` × `height` (1..16384) the resolution it is captured at. Not tied
  to any Runner: each Runner renders, each frame before its outputs, the
  ones its outputs show (and those they show in turn). The picture has a
  transparent background and holds every cue on air in the area, of every
  timeline; display test patterns are not captured. In a loop (a virtual
  display shown, directly or through others, inside its own area), one cue
  of the loop is left out: no feedback, no picture from an earlier frame.
  Runners older than this field refuse a show with a `virtual` source.
* The first track is in front.
* `keep_aspect` (cues, virtual displays) is for the Producer only: it keeps
  width : height when one is changed. Runners ignore it.
* `runner` is a Runner's `--name`; empty means any Runner.
* A display's `test_pattern` (optional): `{ "kind": "lines" | "counter",
  "fps": 60 }` — drawn by its Runner over the content, on that display only,
  on the cluster clock.
* A Runner downloads `url` once and caches it under `id` + `size`.
* `tasks` are auxiliary timelines: same shape as the main one (`tracks`,
  `duration_s`, `control_cues`), each with its own transport, drawn in front
  of the main timeline while active.
* A control cue fires when its timeline, playing forward, reaches `at_s` —
  including when playback *starts* exactly on it (a `play` cue at 0 starts
  its task with the show). The one exception: a cue that pauses or stops its
  own timeline does not fire when playback starts on it, otherwise one could
  never play on from a pause cue.
* A task keeps running past its `duration_s` (which is the length shown in
  the editor) until it is stopped or a control cue acts on it.
  `action` is `pause`, `play`, `stop` or `jump` (to `to_s`); `target` is the
  timeline acted on — empty for the cue's own, `"main"`, or a task id. The
  Director schedules the resulting transport change for the exact cluster
  instant of the cue, ahead of time.
* `free_run` on a cue (video): it plays at normal speed from the instant it
  goes on air — the instant its timeline, playing forward, crossed
  `start_s`, or else the instant of the transport state that put it on air —
  and keeps playing while the timeline is paused, until it goes off air. At
  the end it holds the last frame, or loops from `in_s` if `loop` is set.
* `free_run` on a track (legacy): the same for every cue on the track.

## 6. OSC (Director, UDP 7405)

| Address | Argument | Effect |
|---|---|---|
| `/lcplay/play`, `/lcplay/pause` | — | |
| `/lcplay/stop` | — | pause and return to 0 |
| `/lcplay/seek` | seconds | jump |
| `/lcplay/rate` | speed (1.0 = normal) | |
| `/lcplay/loop` | seconds, 0 = off | |
| `/lcplay/task/<id>/play`, `/pause`, `/stop`, `/seek`, `/rate`, `/loop` | as above | the same for task `<id>` |

Numeric arguments may be float, double, int or long. For play/pause/stop a
single argument of `0` is ignored (a control surface's button release).
Bundles are accepted; their time tags are ignored.

## 7. Asset Manager (HTTP 7403)

| Method & path | |
|---|---|
| `GET /api/library` | `{ count, items: [{ id, name, path, kind, size, mime, width, height, duration_s, source, stream_url, thumb_url, play_url, play_size, play_path, optimized }] }` — `play_*`: the file to play (the optimised copy when ready, else the original); `optimized` (videos): `{ state: "original" \| "pending" \| "optimizing" \| "done" \| "failed", source, progress?, rule?, error?, output?, output_size?, version? }` |
| `GET /api/optimize` | the codec map: `{ enabled, rules: { <source>: { output, chunks, snappy } } }`, sources `h264 hevc prores prores_alpha hap hap_alpha hap_q hap_r hap_q_alpha notchlc dxv other`, outputs `keep rechunk hap hap_alpha hap_q`, chunks 1–64 |
| `PUT /api/optimize` | replace it (values out of range are corrected; returns what was stored) |
| `POST /api/optimize/apply` | optimise again files made with other rules, retry failures: `{ queued }` |
| `POST /api/items/{id}/optimize` | optimise one video again |
| `GET /optimized/{id}` | the optimised copy, with `Range` (`404` if there is none) |
| `POST /api/upload?name=<file name>` | body = the raw file; returns the item |
| `DELETE /api/items/{id}` | removes the file |
| `GET /stream/{id}` | the file, with `Range` support |
| `GET /thumb/{id}` | small PNG |
| `GET /transcode/{id}` | on-the-fly transcode for browser preview |
| `GET /proxy/{id}` | a 640-pixel H.264 preview copy of a video (stored in `.lcplay-proxies` in the media folder, which is not listed), served with `Range` — what the Producer's stage preview plays. While it is being made (the first request starts that, in the background): `503` with `Retry-After`; `404` if it cannot be made. `?status` answers `{ "state": "ready" \| "making" \| "failed" }` at once. |

`kind` is `video`, `image` or `audio`. By default browsers may use the
server only from pages served on port 7404 (the Producer); `POST`/`PUT`/`DELETE`
carrying any other `Origin` get `403`. Set `LCMS_CORS` to change that.

## 8. Security

None in version 1: no authentication, no encryption. The protocol assumes a
closed show network. Do not expose these ports to an untrusted network.
