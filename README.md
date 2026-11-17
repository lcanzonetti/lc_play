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
| Director: cluster clock, transport per timeline, control cues, show distribution, persistence, restart adoption, OSC input | implemented, tested |
| Runner: renders the Director's show live (edits appear while playing), images, colours, video (HAP decoded by the GPU; other codecs hardware-decoded through ffmpeg), fades, warp, edge blend, auxiliary timelines, free-running cues, local asset cache, holdover | implemented, tested |
| Frame sync on frame-locked hardware: frames rendered for the refresh they appear on, a cluster-wide refresh grid, NVIDIA present barrier, one GPU device per graphics card, per-output sync health, sync test pattern | implemented, **not yet run on real hardware** (see below) |
| Runner management: restart, quit, standby, reboot / power off the computer (opt-in), Wake-on-LAN, remote log, live output pictures, remote desktop | implemented |
| Asset Manager: upload, list, thumbnails, metadata, HTTP Range delivery | implemented |
| Producer: stand-alone window with floating panels — stage (zoom, pan, snapping) with live content preview, timelines, tasks, runners, assets, inspector, undo, save/open | implemented (basic) |
| One-command launcher for a single machine | implemented |

**What "basic" means — read before testing:**

* **For large canvases, use HAP** (Hap, Hap Alpha, Hap Q, Hap R in a
  `.mov`). It is decoded by the GPU: a 9976×2176 HAP played at ~57 fps here
  on a *software* GPU with 4 CPU cores. Other codecs (H.264, HEVC, ProRes …)
  go through ffmpeg with hardware decoding where available and YUV
  conversion on the GPU — much better than before, but still a copy of
  every frame through the CPU: fine for HD and some 4K layers, not for huge
  walls. No audio. Reverse playback is smooth for HAP, choppy for other
  codecs.
* **No keyframe animation.** A cue has one position, size, rotation and
  opacity, plus fade in/out.
* **Control cues are executed by the Director.** They are frame-exact on all
  Runners, but do not fire while the Director is down.
* **Free-running cues can be out of step on a Runner that (re)joins late.**
  Each Runner works out when the cue went on air from the shared transport
  state, so Runners that saw it go on air agree. A Runner that starts or
  reconnects while the cue is on air, after the timeline was paused, its
  rate changed, or a looping timeline wrapped, picks a later instant and
  plays the clip from an earlier point than the others.
* **Warp is corner-pin only**, and is set as numbers in the show file — the
  Producer has fields for edge blend but no warp editor yet.
* **MIDI, Art-Net and LTC are not implemented**; OSC is.
* **The Producer's stage preview is played by the web view**, not by the
  Runner: it follows the timeline closely but is not frame-exact, and a
  codec the web view cannot decode (ProRes, HAP) shows a still. The Runner's
  output is the real picture.
* **No authentication anywhere.** Closed show networks only.
* **Tested on Linux only**, on one machine with a software GPU, plus
  simulated clock skew. It type-checks for Windows and macOS but has **not
  been run there**, nor on real multi-machine hardware. In particular the
  Windows swap chain and NVIDIA present barrier code (`dxgi.rs`,
  `nvapi.rs`) has only been compiled: the first run on a show machine is its
  first run.
* **Runners copy media into their own cache before playing it** — on one
  machine too. Until a file is complete its cues show a black-and-magenta
  checkerboard; the Runners window shows a *Loading media* bar per file
  (progress, rate, time left, or the error and the next retry). Load the
  show and let the bars finish before the doors open.
* **A layer that spans two graphics cards of one Runner is decoded once per
  card.** Fine for HAP at moderate sizes; plan canvases so that big video
  stays on one card where possible.

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
the Director and one Runner, and opens the **Producer in its own window**.
Closing that window stops everything. (`lc-play --browser` serves the
interface at `http://localhost:7404` instead, and a second operator can
always open that address in a browser.)

The Producer window needs the system web view: WebView2 on Windows (part of
Windows 11 and of up-to-date Windows 10), WebKitGTK on Linux
(`libwebkit2gtk-4.1`).

1. **Upload** media (button, or drop files on the Assets window). Files can
   also be copied into `lcplay-data/media/`; they appear within seconds.
2. **Double-click** an asset to add it at the playhead of the open timeline,
   or drag it onto a track. A Runner window opens showing the stage.
3. **Play** (Space plays / pauses the timeline open in the Timeline window).
   Drag cues on the stage or the timeline while it plays:
   the output follows.

The interface is a desktop of floating windows — drag a title bar to move
one, its bottom-right corner to resize it; the **Windows** menu shows, hides
and resets them. The layout is remembered.

| Window | What it is for |
|---|---|
| Stage | The canvas — unbounded, the whole window — with its displays and the content on air, live. Drag cues to place them; click a display's name tab to select it; wheel to zoom, drag the background to pan, double-click to fit the displays. Untick **live video** to show video cues as still thumbnails: the Producer's computer then decodes no video (remembered on that computer; Runners are not affected). |
| Timeline | One timeline at a time; the tabs switch between **Main** and the tasks. Drag cues, trim their ends, drag on the ruler to scrub. **+ Control cue** adds a pause / play / stop / jump at the playhead. A video cue's **Free run** box (inspector) keeps it playing while its timeline is paused; it holds its last frame unless **Loop** is on. |
| Tasks | The auxiliary timelines: independent timelines with their own play / pause / stop, drawn in front of the main one. |
| Runners | Each Runner: health, GPU, monitors, the displays it drives (fullscreen, monitor), and **Reload outputs**. |
| Inspector | Properties of whatever is selected; with nothing selected, the show and stage. |
| Assets | Media on the Asset Manager. |

**Several people at once.** Anyone who can reach the Producer's address
(`http://<producer machine>:7404`, started with `--browser` or alongside the
window) can open it in a browser and work on the same show at the same time
— one on displays and warp, another on the timeline. Each change is sent as a
small edit to the Director, which applies everyone's edits in turn; objects
are tracked by identity, so simultaneous changes to different things all
survive, and for the same field the last change wins. Each user's name and
colour are in the top bar (click your own to rename yourself); what the
others have selected is outlined in their colour on the stage and the
timeline, dashed while they drag it, and the inspector says when someone
else has the same thing selected. **Undo** reverts only your own changes.
**New** and **Open…** replace the show for everyone (with a warning). There
is no password: keep the show network closed.

The default display is a 960×540 window showing a 1920×1080 stage — a
preview. For a real output, pick **Fullscreen · #n** for the display in the
Runners window. New displays are placed to the right of the existing ones,
and go fullscreen by themselves on a monitor of their Runner that no other
display uses (on the operator's own computer, never on its primary screen,
which would hide the Producer).

The Stage zooms with the mouse wheel, pans by dragging an empty area (or
with the middle button), and **Fit** shows it all again. Dragged displays
and cues snap to the edges of the stage and of other displays (hold Alt to
place freely). **Side by side** and **Stage = displays** in the Inspector
(nothing selected) line displays up and size the stage to them.

**Runners window**, per Runner: **Preview** shows small live pictures of its
outputs (after warp and blend — what the projector gets), **Log** its recent
log, **Reload outputs**, **Standby / Resume** (close the outputs, stay
connected), **Restart** and **Quit** the Runner program, **Reboot** and
**Power off** the computer (only if the Runner was started with
`--allow-power`), **Start** (start the Runner program again after it was
closed — its supervisor waits for this), **Wake** (Wake-on-LAN, for an
offline Runner that was connected before), **Test** next to each display
(moving test lines on that display — kind and rate in the display's
inspector), **RDP / VNC** (open this computer's remote-desktop client
on it; the Runner's computer must have Remote Desktop or a VNC server
enabled, and this only works from the Producer's own machine). RDP uses
Windows' own `mstsc`. VNC needs a VNC viewer on the Producer's computer:
RealVNC Viewer, TightVNC, TigerVNC or UltraVNC in their default install
folders are found automatically; any other one can be named with the
`LCPLAY_VNC_VIEWER` environment variable (the full path of its `.exe`).

Things worth trying:

* Stop the Director (`pkill lc-director`, or end the process): the Runner
  keeps playing. `lc-play` leaves the other services running.
* Restart `lc-play`: the show and its position are restored from
  `lcplay-data/`, and the Runner keeps the media it already copied (see
  *Where a Runner keeps its files* below).
* Send OSC to UDP port 7405: `/lcplay/play`, `/lcplay/pause`, `/lcplay/stop`,
  `/lcplay/seek <seconds>`, `/lcplay/rate <speed>`, `/lcplay/loop <seconds|0>`,
  and `/lcplay/task/<id>/play` (`pause`, `stop`, `seek`) for a task — its id
  is shown in the Inspector when the task is selected.
* `target/release/lc-ctl status` — per Runner: on frame? sync error, RTT, drift.

### Several machines

Run the services by hand instead of `lc-play`:

```bash
# on the server (use its LAN address where Runners must reach it)
LCMS_BIND=0.0.0.0:7403 LCMS_MEDIA_DIR=/data/media target/release/lc-assets
target/release/lc-director --state-dir /data/director
target/release/lc-producer --browser --assets http://10.0.0.1:7403

# on each render machine
target/release/lc-runner --director 10.0.0.1:7401 --name left --allow-power
```

`lc-runner` runs under a small supervisor (the same program): **Restart**
from the Producer brings it back, and it restarts itself after a crash. When
the Runner is closed (its window, or **Quit**), the supervisor keeps waiting,
and **Start** in the Runners window opens it again; closing the supervisor's
console window ends it for good. `lc-runner --autostart on` (with the
options it should start with) makes it start at login, so a reboot does not
leave it closed either; `--autostart off` undoes that.

Started with no options, `lc-runner` looks for the Director by itself (its
multicast beacon, else this computer); `runner.director` and `runner.name` in
`lcplay.json` next to the programs set them for a machine. `--demo` plays
the built-in example show without a Director.
`--allow-power` lets the Producer reboot and shut down that computer —
anyone on the show network could, so only use it on a closed network. For
Wake-on-LAN, enable it in the computer's BIOS/UEFI and network adapter
settings; the packet does not cross routers.

In the Producer, set each display's **Runner** field to that Runner's
`--name`. `--assets` matters: that URL is written into the show and is where
Runners download media from. `--discover` can replace `--director` where
multicast works.

Default ports: discovery UDP 7400 (multicast 239.255.76.67), control TCP 7401,
clock sync UDP 7402, assets HTTP 7403, Producer HTTP 7404, OSC UDP 7405,
Runner start requests UDP 7406.

### Display test patterns

A display's inspector has a **Test pattern**: *Moving lines* (a vertical
and a horizontal line sweeping across the display, one step per frame — a
smooth sweep means every frame reached the screen once; judder, repeats and
drops show at a glance) or *Frame counter* (the sync-test pattern), at a
rate you set (normally the refresh rate, or the content's frame rate). It is
drawn by that display's Runner over the content, fitted to that display and
on it only, and runs on the cluster clock — it moves whether the show plays
or not, and every display showing one is on the same step.

### HAP and the CPU

HAP is decoded by the GPU, but each frame is first decompressed (Snappy)
and copied by the CPU: measured here, about 2.5–3 ms of one core per frame
of 4K HAP Q — 15–20 % of a core per layer at 60 fps, more for footage that
compresses less well, and double at 8K. A laptop that also runs the Producer
feels that; a show machine with more cores much less. To lower it: encode
with chunks (`-chunks 8`: decompressed on several cores), use plain HAP
(half the data of HAP Q) where its quality is enough, or encode without
Snappy (`-compressor none`): no decompression at all, at the price of
larger files (8 MB per 4K HAP Q frame, ≈ 500 MB/s at 60 fps — fast NVMe
only).

### Where a Runner keeps its files

Each Runner has one folder, kept across restarts: downloaded media in
`assets\` (named `<asset id>-<size>.<ext>`), plus its last show, its
identity and its log. By default it is **`lc-runner\` in the folder the LC
Play programs are in** — e.g. `C:\LC Play\lc-runner\assets\` — so media
is copied once and survives restarts and reboots. (On Linux and macOS the
folder is `lc-runner-data/`: the program itself is called `lc-runner`.)

To put it elsewhere (a separate media drive, for instance), in order of
priority:

1. `--cache-dir D:\lcplay-runner` on the command line;
2. the `LCPLAY_RUNNER_DIR` environment variable;
3. a settings file `lcplay.json` next to the programs — the file a future
   installer will write:

   ```json
   { "runner": { "dir": "D:\\lcplay-runner" } }
   ```

   A relative path is relative to the programs' folder.

If the programs' folder cannot be written (installed under
`C:\Program Files`), the Runner uses `%LOCALAPPDATA%\LC Play\lc-runner`
instead. The Runner logs which folder it uses when it starts. Nothing in
the folder is deleted automatically: remove unused media by hand, with the
Runner stopped. A Runner started with a `--name` other than the default
keeps its show and identity in `runner-<name>\` inside that folder (the media
folder is shared), so several Runners can be tried on one machine.

### Frame-locked rigs (Quadro Sync / RTX Pro Sync)

Frame lock makes every display refresh together; LC Play then makes every
output show the *same frame* on each refresh (how: `docs/ARCHITECTURE.md`
§3.3). On Windows the Runner presents through its own DXGI swap chain and
joins NVIDIA's present barrier by itself; nothing to configure. To get
there:

1. Set up frame lock in the NVIDIA Control Panel (*Synchronize displays*)
   as for any other media server, all outputs at the same refresh rate.
2. Displays **full screen** in LC Play (borderless, on their monitor),
   Windows display scaling at 100 % on those monitors — otherwise the
   present barrier does not engage.
3. In the Producer's **Runners** window, each output shows a *Sync* line.
   On a working rig it reads `dxgi · 59.94 Hz · barrier cluster ·
   grid ±0.x ms · … missed 0 · late 0`. `barrier system` = synchronised
   within the machine only (frame lock not active); `client` = joined but
   not synchronised (usually: not truly full screen); `off grid` = this
   output is not frame-locked with the reference Runner.
4. Add a **Sync test** cue (Assets window) across the whole canvas, play,
   and film adjacent outputs (or two machines) with a fast camera: the frame
   numbers must match. Set its frame rate to the refresh rate.

If a driver misbehaves: `LCPLAY_SWAPCHAIN=wgpu` falls back to wgpu's
presentation (no statistics, no barrier), `LCPLAY_PRESENT_BARRIER=0` keeps
the own swap chain without the barrier, `LCPLAY_FRAME_LATENCY=1..3` (default
2) sets how many frames are queued ahead of the display.

The render loop runs at multimedia priority (MMCSS "Playback") and decoding
below normal, so a busy CPU delays a video layer rather than every output.
If `late` climbs on a machine where other software competes for the CPU,
`LCPLAY_THREAD_PRIORITY=0` leaves priorities as Windows sets them.

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
