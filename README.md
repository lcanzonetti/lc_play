# LC Play

A distributed media server: one **Producer** designs and cues a show on a
single large canvas, a **Director** keeps the cluster's time and state, and
any number of **Runners** render their slice of the canvas to the displays
they are wired to — frame-synchronised, and able to keep playing if the
Director disappears. An **Asset Manager** stores and serves the media.

```
 Producer ──WS──▶ Director ◀──WS + UDP time sync──▶ Runner ─▶ displays
    │                │                              Runner ─▶ displays
    └──HTTP──▶ Asset Manager ◀────────HTTP──────────Runner ─▶ displays
```

| Document | Contents |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | components, communication flows, the synchronisation design and its limits, data model, roadmap |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | the wire format: discovery beacon, control messages, sync packet |
| [crates/lc-runner/README.md](crates/lc-runner/README.md) | the renderer and the show file format |
| [crates/lc-assets/README.md](crates/lc-assets/README.md) | the media HTTP server |

## Status

A basic version of the whole system runs end to end: design a show in the
Producer, upload media, and see it play, synchronised, on a Runner's outputs.

| Part | State |
|---|---|
| Director: cluster clock, transport, show distribution, persistence, restart adoption, OSC input | implemented, tested |
| Runner: renders the Director's show live (edits appear while playing), images, colours, video, fades, warp, edge blend, local asset cache, holdover | implemented, tested |
| Asset Manager: upload, list, thumbnails, metadata, HTTP Range delivery | implemented |
| Producer: stage, timeline, assets, inspector, transport, cluster health, undo, save/open | implemented (basic) |
| One-command launcher for a single machine | implemented |

**What "basic" means — read before testing:**

* **Video is decoded by an `ffmpeg` child process into uncompressed frames.**
  Correct and frame-exact, but CPU-bound: fine for a few HD layers, not for
  4K60 walls. No audio. Reverse playback of video is choppy.
* **No keyframe animation.** A cue has one position, size, rotation and
  opacity, plus fade in/out.
* **Warp is corner-pin only**, and is set as numbers in the show file — the
  Producer has fields for edge blend but no warp editor yet.
* **MIDI, Art-Net and LTC are not implemented**; OSC is.
* **The Producer's stage shows thumbnails**, not live video. The Runner's
  output is the real picture.
* **No authentication anywhere.** Closed show networks only.
* **Tested on Linux only**, on one machine with a software GPU, plus
  simulated clock skew. It type-checks for Windows and macOS but has **not
  been run there**, nor on real multi-machine hardware.

## Run it on one laptop

Prerequisites: a [Rust toolchain](https://rustup.rs) and **ffmpeg** (with
`ffprobe`) on the `PATH`. On Linux also a Vulkan or GL driver and
`libxkbcommon-x11`.

```bash
cargo build --release --workspace
```

```bash
target/release/lc-play
```

(On Windows: `target\release\lc-play.exe`.) This starts the Asset Manager,
the Director, the Producer and one Runner, and prints:

```
Producer (open in a browser):  http://localhost:7404
```

Then, in the browser:

1. **Upload** media (button, or drop files on the Assets panel). Files can
   also be copied into `lcplay-data/media/`; they appear within seconds.
2. **Double-click** an asset to add it at the playhead, or drag it onto a
   track. A Runner window opens showing the stage.
3. **Play** (Space). Drag cues on the stage or the timeline while it plays:
   the output follows.
4. Select nothing to edit the show and stage; click a display's name tab on
   the stage to edit that display (area of the stage, window size, fullscreen
   and monitor, edge blend).

The default display is a 960×540 window showing a 1920×1080 stage — a
preview. For a real output, select the display, tick **Fullscreen** and set
the **Monitor #** (the Runner logs the monitor list when it starts).

Things worth trying:

* Stop the Director (`pkill lc-director`, or end the process): the Runner
  keeps playing. `lc-play` leaves the other services running.
* Restart `lc-play`: the show, its position and the Runner's media cache are
  restored from `lcplay-data/`.
* Send OSC to UDP port 7405: `/lcplay/play`, `/lcplay/pause`, `/lcplay/stop`,
  `/lcplay/seek <seconds>`, `/lcplay/rate <speed>`, `/lcplay/loop <seconds|0>`.
* `target/release/lc-ctl status` — per Runner: on frame? sync error, RTT, drift.

### Several machines

Run the services by hand instead of `lc-play`:

```bash
# on the server (use its LAN address where Runners must reach it)
LCMS_BIND=0.0.0.0:7403 LCMS_MEDIA_DIR=/data/media target/release/lc-assets
target/release/lc-director --state-dir /data/director
target/release/lc-producer --assets http://10.0.0.1:7403

# on each render machine
target/release/lc-runner --director 10.0.0.1:7401 --name left --cache-dir /data/runner
```

In the Producer, set each display's **Runner** field to that Runner's
`--name`. `--assets` matters: that URL is written into the show and is where
Runners download media from. `--discover` can replace `--director` where
multicast works.

Default ports: discovery UDP 7400 (multicast 239.255.76.67), control TCP 7401,
clock sync UDP 7402, assets HTTP 7403, Producer HTTP 7404, OSC UDP 7405.

## Workspace

| Crate | Role |
|---|---|
| `lc-proto` | wire messages and shared types; no I/O |
| `lc-time` | cluster clock estimator, UDP sync, transport timeline |
| `lc-discovery` | multicast node discovery |
| `lc-net` | control-channel client (WebSocket) |
| `lc-director` | Director service |
| `lc-runner` | Runner service: cluster agent + wgpu renderer |
| `lc-assets` | Asset Manager service |
| `lc-ctl` | command-line controller |
| `lc-producer` | Producer: serves the operator's web interface |
| `lc-play` | launcher: all services on one machine |

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The Runner's GPU tests need a Vulkan adapter (on a headless Linux box:
`apt install mesa-vulkan-drivers`) and its video test needs `ffmpeg`; without
them those tests skip.

## License

MIT
