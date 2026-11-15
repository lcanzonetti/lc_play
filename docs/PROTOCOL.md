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
            "rtt_ns": 180000, "error_ns": 21000, "holdover_ns": 400000000 } }
{ "type": "ping" }
```

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
| `runner_action` | `runner` (name or node id), `action`: `reload_outputs` | that Runner closes and rebuilds its outputs |
| `seek` | `show_ns ≥ 0` | jump; play state unchanged |
| `set_rate` | `rate_permille` (1000 = 1×, negative = reverse, ‖rate‖ ≤ 16000) | position stays continuous |
| `set_loop` | `loop_ns` or `null` | wrap show time into `0..loop_ns` |
| `load_show` | `document` (JSON object) | new show revision, distributed to Runners |
| `status` | — | Director replies with `cluster` |

Transport commands take effect `lead_ns` (default 100 ms) after the Director
receives them, simultaneously on all Runners. `timeline` on the `command`
message selects the transport they act on: `null`/absent/`"main"` is the main
timeline, otherwise the id of an auxiliary timeline (task) of the show.

### Director → client

```json
{ "type": "welcome", "protocol": 1, "director_id": "…", "name": "main",
  "clock_id": 1234, "sync_port": 7402, "lead_ns": 100000000,
  "frame_rate": { "num": 60, "den": 1 }, "transport": { … }, "show_revision": 3 }
{ "type": "transport", "clock_id": 1234, "state": { … }, "timeline": null }
{ "type": "runner_action", "node_id": "…", "action": "reload_outputs" }
{ "type": "show", "revision": 3, "id": "…uuid…", "document": { … } }
{ "type": "cluster", "clock_id": 1234, "cluster_ns": 0, "frame_rate": { … },
  "transport": { … }, "show_ns": 0, "frame": 0, "show_revision": 3,
  "nodes": [ { "node_id": "…", "role": "runner", "name": "left", "addr": "10.0.0.11:51000",
               "health": { … }, "clock_skew_ns": -350000, "frame_error": 0 } ] }
{ "type": "error", "message": "seek: show time must not be negative" }
{ "type": "pong" }
```

`cluster` is pushed every second to non-Runner clients, and on `status`.

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
`gpu` and `outputs` (open windows).

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
    ]
  },
  "assets": [
    { "id": "636c6970", "name": "clip.mp4", "kind": "video",
      "url": "http://10.0.0.1:7403/stream/636c6970",
      "size": 594087, "width": 1920, "height": 1080, "duration_s": 12.0 }
  ],
  "control_cues": [
    { "id": "k1", "at_s": 30, "action": "pause", "target": "", "to_s": 0 },
    { "id": "k2", "at_s": 10, "action": "play", "target": "sting" }
  ],
  "tasks": [
    { "id": "sting", "name": "Sting", "duration_s": 8, "tracks": [ … ], "control_cues": [ … ] }
  ],
  "tracks": [
    { "id": "t1", "name": "Track 1", "free_run": false, "cues": [
      { "id": "c1", "name": "clip.mp4",
        "source": { "type": "asset", "asset": "636c6970" },
        "start_s": 0, "duration_s": 12, "in_s": 0, "loop": false,
        "x": 0, "y": 0, "w": 1920, "h": 1080,
        "rotation": 0, "opacity": 1, "fade_in_s": 1, "fade_out_s": 1 }
    ] }
  ]
}
```

* Space: canvas pixels, origin top-left, y down. `x, y, w, h` is the cue's
  rectangle; `rotation` is degrees clockwise about its centre.
* `source` is `{ "type": "asset", "asset": <id> }` or
  `{ "type": "color", "rgba": [r, g, b, a] }` (linear 0..1).
* The first track is in front.
* `runner` is a Runner's `--name`; empty means any Runner.
* A Runner downloads `url` once and caches it under `id` + `size`.
* `tasks` are auxiliary timelines: same shape as the main one (`tracks`,
  `duration_s`, `control_cues`), each with its own transport, drawn in front
  of the main timeline while active.
* A control cue fires when its timeline, playing forward, reaches `at_s`.
  `action` is `pause`, `play`, `stop` or `jump` (to `to_s`); `target` is the
  timeline acted on — empty for the cue's own, `"main"`, or a task id. The
  Director schedules the resulting transport change for the exact cluster
  instant of the cue, ahead of time.
* `free_run` on a track: its video ignores the timeline's position and loops
  on cluster time (`frame = floor(cluster_seconds × fps) mod length`); the
  cues still decide when it is visible.

## 6. OSC (Director, UDP 7405)

| Address | Argument | Effect |
|---|---|---|
| `/lcplay/play`, `/lcplay/pause` | — | |
| `/lcplay/stop` | — | pause and return to 0 |
| `/lcplay/seek` | seconds | jump |
| `/lcplay/rate` | speed (1.0 = normal) | |
| `/lcplay/loop` | seconds, 0 = off | |
| `/lcplay/task/<id>/play`, `/pause`, `/stop`, `/seek` | as above | the same for task `<id>` |

Numeric arguments may be float, double, int or long. For play/pause/stop a
single argument of `0` is ignored (a control surface's button release).
Bundles are accepted; their time tags are ignored.

## 7. Asset Manager (HTTP 7403)

| Method & path | |
|---|---|
| `GET /api/library` | `{ count, items: [{ id, name, kind, size, mime, width, height, duration_s, stream_url, thumb_url }] }` |
| `POST /api/upload?name=<file name>` | body = the raw file; returns the item |
| `DELETE /api/items/{id}` | removes the file |
| `GET /stream/{id}` | the file, with `Range` support |
| `GET /thumb/{id}` | small PNG |
| `GET /transcode/{id}` | on-the-fly transcode for browser preview |

`kind` is `video`, `image` or `audio`. By default browsers may use the
server only from pages served on port 7404 (the Producer); `POST`/`DELETE`
carrying any other `Origin` get `403`. Set `LCMS_CORS` to change that.

## 8. Security

None in version 1: no authentication, no encryption. The protocol assumes a
closed show network. Do not expose these ports to an untrusted network.
