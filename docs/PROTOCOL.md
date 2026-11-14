# LC Play wire protocol (version 1)

Source of truth: `crates/lc-proto` (messages) and `crates/lc-time/src/wire.rs`
(sync packet). This file is the human-readable summary. All times are signed
64-bit **nanoseconds**.

| Channel | Transport | Default | Encoding |
|---|---|---|---|
| Discovery | UDP multicast `239.255.76.67` | port 7400 | JSON datagram |
| Control | WebSocket `ws://director:7401/ws` | TCP 7401 | JSON text frames |
| Clock sync | UDP unicast | port 7402 | fixed 44-byte binary |
| HTTP helpers | `GET /status`, `POST /command` | TCP 7401 | JSON |

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
{ "type": "command", "command": { "type": "play" } }
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
| `seek` | `show_ns ≥ 0` | jump; play state unchanged |
| `set_rate` | `rate_permille` (1000 = 1×, negative = reverse, ‖rate‖ ≤ 16000) | position stays continuous |
| `set_loop` | `loop_ns` or `null` | wrap show time into `0..loop_ns` |
| `load_show` | `document` (JSON object) | new show revision, distributed to Runners |
| `status` | — | Director replies with `cluster` |

Transport commands take effect `lead_ns` (default 100 ms) after the Director
receives them, simultaneously on all Runners.

### Director → client

```json
{ "type": "welcome", "protocol": 1, "director_id": "…", "name": "main",
  "clock_id": 1234, "sync_port": 7402, "lead_ns": 100000000,
  "frame_rate": { "num": 60, "den": 1 }, "transport": { … }, "show_revision": 3 }
{ "type": "transport", "clock_id": 1234, "state": { … } }
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
  "anchor_cluster_ns": 123000000000, "anchor_show_ns": 42000000000, "loop_ns": null }
```

From cluster time `anchor_cluster_ns` on:

```
show_ns = anchor_show_ns + (cluster_ns − anchor_cluster_ns) × rate_permille / 1000     (if playing)
show_ns = anchor_show_ns                                                               (if paused)
then: wrapped into 0..loop_ns if looping, else clamped at ≥ 0
frame   = floor(show_ns × num / (den × 1e9))
```

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

## 5. Security

None in version 1: no authentication, no encryption. The protocol assumes a
closed show network. Do not expose these ports to an untrusted network.
