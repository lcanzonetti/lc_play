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

| Part | State |
|---|---|
| Communication & synchronisation (discovery, control channel, cluster clock, transport, holdover, Director restart) | **implemented and tested** |
| Runner renderer (canvas, multi-output, warp, edge blend; images and colours) | implemented; timed by the cluster clock |
| Asset Manager | stand-alone media server; cluster integration is next (M2) |
| Video playback, show distribution to the renderer, Producer UI, OSC/MIDI/Art-Net/LTC | not yet — see the [roadmap](docs/ARCHITECTURE.md#8-roadmap) |

Tested on one machine over loopback with simulated clock offset and drift.
**Not yet verified on real multi-machine hardware**: switch behaviour, GPU
vblank timing and multi-monitor output need a bring-up on a rig.

## Try it (one machine)

```bash
cargo build --workspace

# 1. the Director
target/debug/lc-director --state-dir /tmp/lcplay/director

# 2. two Runners (headless: no window, they report the frame they would show)
target/debug/lc-runner --headless --discover --name left  --cache-dir /tmp/lcplay/left
target/debug/lc-runner --headless --discover --name right --cache-dir /tmp/lcplay/right

# 3. drive it
target/debug/lc-ctl discover
target/debug/lc-ctl play
target/debug/lc-ctl seek 42.5
target/debug/lc-ctl status      # per Runner: on frame? sync error, RTT, drift
```

Then kill the Director: both Runners log `HOLDOVER` and keep counting the same
frames. Start it again: it adopts the running timebase and nothing jumps.

With a display attached, drop `--headless` and pass a show file:

```bash
target/debug/lc-runner crates/lc-runner/examples/preview-3-screens.json --discover
```

If discovery finds nothing (multicast is often filtered on managed switches),
give the address explicitly: `--director 10.0.0.1:7401`.

Default ports: discovery UDP 7400 (multicast 239.255.76.67), control TCP 7401,
clock sync UDP 7402.

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

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The Runner's GPU tests need a Vulkan adapter (on a headless Linux box:
`apt install mesa-vulkan-drivers`); without one they skip.

## License

MIT
