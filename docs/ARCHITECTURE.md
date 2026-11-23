# LC Play — architecture

Status of this document: it describes what is **implemented** — a basic
version of every component — and section 8 lists what is still missing.
Anything not measured or not verified is labelled.

## 1. Components

```
                         OPERATOR LAPTOP
        ┌────────────────────────────────────────────────┐
        │  PRODUCER  (lc-producer, in the browser)       │
        │  Web UI: Stage · Timeline · Assets · Health    │
        │  owns the editable project + undo history      │
        └───────┬───────────────────────────┬────────────┘
     WebSocket  │ show edits, transport      │ HTTP upload / browse
     (control)  │ cmds; state + health back  │
                ▼                            ▼
   ┌──────────────────────────┐    ┌──────────────────────────┐
   │ DIRECTOR (lc-director)   │    │ ASSET MANAGER (lc-assets)│
   │ authoritative show state │◄──►│ media store, HTTP Range  │
   │ transport state machine  │HTTP│ transcode (ffmpeg)       │
   │ CLUSTER CLOCK (master)   │    │ upload, thumbnails       │
   │ external control gateway │    └────────────┬─────────────┘
   │ (OSC; MIDI/Art-Net/LTC—) │                 │
   └───┬──────────┬───────────┘                 │ HTTP GET (Range)
       │ WS       │ UDP time sync               │ separate from control
       │ control  │ (4-timestamp)               │
  ┌────▼──────────▼────┐  ┌─────────────────────▼┐  ┌────────────────────┐
  │ RUNNER A           │  │ RUNNER B             │  │ RUNNER N …         │
  │ agent: sync client,│  │                      │  │                    │
  │  show+transport    │  │   (same)             │  │   (same)           │
  │  cache, health     │  │                      │  │                    │
  │ ── playhead ──     │  │                      │  │                    │
  │ renderer: wgpu     │  │                      │  │                    │
  │  warp · blend      │  │                      │  │                    │
  └──┬──────┬──────────┘  └──┬───────┬───────────┘  └──┬─────────────────┘
   HDMI/DP  (SDI/NDI M6)    proj 1  proj 2            LED processor

   All nodes: UDP multicast discovery beacons (239.255.76.67:7400)
```

Every box is a separate binary. One machine can run all of them (small show);
the normal deployment is Producer on the operator's laptop, Director and Asset
Manager on a server (or the laptop), Runners on render hardware.

The split follows the same roles as Dataton WATCHOUT 7 (Producer, Director,
Runner, Asset Manager). *[Not re-verified in this work: stated from general
knowledge of WATCHOUT 7's published architecture; nothing in LC Play depends
on it.]*

**Key property:** the Director is not in the playback path. A Runner that has
(a) the show, (b) a transport state and (c) a clock estimate needs nothing
else to keep rendering the correct frame.

## 2. Communication

| Flow | Transport | Why this and not something else |
|---|---|---|
| Discovery | UDP multicast beacon, 1 Hz, JSON | Zero configuration. Multicast is frequently filtered on show networks (IGMP snooping without a querier), so **every service also takes an explicit address**; discovery is a convenience, never a requirement. |
| Control / state | WebSocket, JSON | Bidirectional (commands down, health up), reliable and ordered, native in browsers (the Producer UI), readable with any tool. Server-Sent Events were rejected: one-way. A custom TCP framing was rejected: no browser support, no benefit. |
| Clock sync | Dedicated UDP socket, 44-byte binary | Must not share a TCP stream: one lost segment delays everything behind it and that delay would be measured as clock offset. |
| Assets | HTTP/1.1 with Range | Resumable, cacheable, and completely independent of the control socket, so a 50 GB transfer cannot delay a "pause". |

Wire details: [PROTOCOL.md](PROTOCOL.md).

### Join

1. Runner finds the Director: beacon, or `--director host:port`.
2. Runner opens the WebSocket and sends `hello` — including, if it has been
   running without a Director, the timebase and transport it is executing.
3. Director answers `welcome`: timebase id, sync port, lead time, frame rate,
   current transport.
4. Runner points its sync client at the Director and locks its clock
   (8 polls/s for the first 4 s, then 2/s).
5. If the Runner's cached show revision differs, the Director sends the show.
6. Runner reports `health` every second.

### A transport command ("play")

```
Producer/lc-ctl ──play──▶ Director
                           │ new state: playing, anchored at cluster_now + lead (100 ms)
                           ├──transport──▶ Runner A ┐ each queues the state and switches
                           ├──transport──▶ Runner B ┤ when ITS cluster clock reaches the
                           └──transport──▶ Runner C ┘ anchor → same instant everywhere
```

The Director never says "now". If it did, each Runner would start whenever
the message happened to arrive, i.e. network jitter would become start-time
error. Anchoring in the future turns unpredictable latency into a constant,
configurable one (`--lead-ms`). The lead must exceed the worst-case delivery
time to any Runner; 100 ms is far more than a LAN needs and still feels
immediate.

## 3. Synchronisation

This is the part everything else rests on. It has three layers.

### 3.1 Cluster time

The **Director's monotonic clock defines cluster time**. Monotonic, not
wall-clock: an NTP correction, DST, or someone setting the date cannot make
the show jump. Cluster time has no relation to time of day; it is only ever
compared with itself. Each timebase has a random `clock_id`.

Each Runner estimates `cluster − local` with the four-timestamp exchange used
by NTP (RFC 5905 §8):

```
offset = ((t2 − t1) + (t3 − t4)) / 2        delay = (t4 − t1) − (t3 − t2)
```

The estimator (`lc-time/src/estimator.rs`):

* **Minimum-delay filter.** The formula assumes both network legs take
  equally long; queueing breaks that, and only ever by *adding* delay. So of
  the last 64 samples only the 50 % with the smallest round trip are used.
* **Offset and drift.** A least-squares line through those samples gives the
  offset *and* the frequency error (quartz oscillators differ by tens of ppm,
  i.e. about a frame every few minutes if ignored).
* **Slew, don't step.** A corrected estimate is approached at ≤ 500 ppm,
  which is invisible; stepping would drop or repeat frames. Only errors above
  20 ms are stepped (first lock, or a different Director).
* **Holdover.** With no samples arriving, the mapping keeps extrapolating
  along the fitted line.

### 3.2 Deterministic transport

The transport is not a stream of events but a small **state**:

```
{ epoch, playing, rate_permille, anchor_cluster_ns, anchor_show_ns, loop_ns }

show_ns = anchor_show_ns + (cluster_now − anchor_cluster_ns) × rate / 1000
frame   = floor(show_ns × fps_num / (fps_den × 10⁹))
```

Everything is integer arithmetic on nanoseconds with a rational frame rate
(59.94 is 60000/1001), so all nodes compute bit-identical results and nothing
accumulates over a long show. `epoch` only ever increases, which makes
duplicated or reordered delivery harmless.

Because there are no per-frame messages, there is nothing to lose: holdover
needs no special mode, it is simply the same formula evaluated without new
input.

### 3.3 Presentation: render for the refresh the frame will appear on

Frame lock (Quadro Sync) makes every display refresh at the same instants.
Which *content* frame each Runner puts in front of that refresh is decided
in software, and three things used to make Runners disagree by one frame:
sampling the show clock when rendering starts rather than when the picture
appears; a queue between the two whose depth depends on the machine; and
rounding a slightly different instant on each Runner to a frame number right
at a frame boundary. The renderer now works per refresh:

1. **Refresh timing per output** (`vsync.rs`). A `RefreshClock` fits the
   display's refresh train (`t(n) = phase + n × period`). On Windows/D3D12 the
   observations are the driver's own (`GetFrameStatistics`: refresh counter
   and the QPC time of a refresh) through LC Play's own DXGI swap chain
   (`dxgi.rs`); elsewhere they are the moments the swap chain released the
   render loop. The driver's *timestamps* are exact, but its *counter* is
   not: on a Windows laptop it lost one refresh at a time while frames were
   being repeated, which bent the fitted period from 16.67 to 18.8 ms. So the
   clock numbers refreshes from the timestamps (elapsed time over the period,
   rounded) and uses the counter only to tell a new report from a repeated
   one and, before the first fit, for a provisional period (the shortest
   time per count, which lost counts can only lengthen). After more than a
   second without a report the fit starts again and the count numbers the
   report, since a period error too small to see could add up to half a
   refresh over a long gap. Refresh numbers the
   driver reports, such as the refresh a present reached, are translated into
   the clock's numbering (`RefreshClock::from_driver`).

   Numbering by time can lock onto a wrong period (a wrong first estimate
   numbers the next observations to match it), and the render loop's own
   timing can run away while frames are irregular (on a laptop it slid from
   16.7 to 6.9 ms, and the Director's grid followed to 145 Hz). So every fit
   is checked against the period of the display's mode, read from the window
   system (`render::nominal_period`, refreshed twice a second): one more than
   3 % away, and more than 1.05 Hz away, is not believed; the observations
   are numbered again with the mode's period and fitted again, and failing
   that only the phase is fitted. The 1.05 Hz is for Windows' whole-hertz
   mode rates: it lists TV timings rounded down (59 for 59.94 Hz, 29 for
   29.97, 23 for 23.976; Microsoft KB 2006076), up to 4.2 % off by
   themselves, more than 3 % below 35 Hz.
   Observations more than a quarter period off the line are dropped first,
   unless more than a quarter of them are (then the numbering is at fault);
   that removes the off-grid report the driver gives when a swap chain's
   counter starts, which had made the first fit 1 % fast. The fit spans ten
   seconds of observations, whatever the refresh rate (at most 6000): the
   driver's timestamps jitter by about ±0.2 ms, which over the two seconds
   the fit used to span let the period wander by ±50 ppm.

   When the driver's counter ticks faster than the mode (its lower-quartile
   time per count shorter than the mode's period, by more than both 3 % and
   1.05 Hz), its reports
   describe another display: on a laptop whose output was composed by the
   desktop compositor, they counted the 165 Hz panel's refreshes for a 60 Hz
   output. The clock then keeps the mode's period, the reports are no longer
   `exact`, the `PresentTracker` stops learning from them, and the Runner
   logs a warning that the compositor is presenting the output on another
   display's clock. A counter slower than the display cannot be told from one
   that loses counts.

   The period is reported to the Director (for the grid, below) only once the
   fit is *mature*: at least two seconds of observations, not foreign.

   Replays of three laptop captures (`crates/lc-runner/testdata/`: a slow
   stretch with lost counts, the start-up with the off-grid report, and the
   composed output) are unit tests.
2. **The target refresh.** After waiting for the swap chain, the frame will
   reach refresh `next + latency − 1 + correction`, where `latency` is the
   number of frames queued (`LCPLAY_FRAME_LATENCY`, default 2) and
   `correction` is learnt from the driver's report of which refresh each
   present actually reached (`PresentTracker`). Frames that land later than
   that are counted as **missed**.
3. **The refresh grid** (`lc_proto::vblank`). The Director publishes one
   reference refresh instant and the refresh period (in picoseconds), measured
   on one Runner (the first by node id, preferring driver-measured ones). Each
   Runner rounds its target instant to the nearest grid line and uses *the
   grid instant* — the same integer on every Runner — as the time the frame
   is for. From there show time and frame index are integer arithmetic, so
   frame-locked Runners compute exactly the same frame, whatever their clock
   or measurement error (it has to be within a tenth of a period to join the
   grid, a quarter to stay on it — the gap keeps a drifting, unlocked display
   from flipping in and out). A Runner whose refreshes are not on the grid
   (no frame lock, another refresh rate) uses its own estimate. The Director re-measures the grid when the
   reference drifts from it by 200 µs or 2 ppm (at most every 10 s); a new
   grid takes effect on a grid line every Runner identifies identically.
4. **NVIDIA present barrier.** Each output's swap chain joins NVIDIA's
   present barrier when the GPU and driver support it (`nvapi.rs`, NVAPI
   loaded from the driver at run time): the outputs of a machine — and with
   frame lock, of every machine — flip on the same refresh, so a late
   machine delays everyone by one refresh instead of showing a different
   frame. Effective only in a borderless full-screen window without desktop
   scaling or taskbar composition; its state (`client`, `system`, `cluster`)
   is reported per output. Before rendering into a buffer again, the Runner waits
   (100 ms at most) on the barrier's fence for the present that last showed
   it, as NVAPI requires; it assumes the driver counts presents on that
   fence, and stops waiting, with a log line, if it does not **[to verify on
   hardware]**.
5. **One device per graphics card.** Each output is rendered by the card its
   monitor is connected to (matched through DXGI), so Windows never copies a
   picture from one card to another (a frame of latency on that output only,
   and no barrier). Contents are loaded per card, and a card only decodes
   the layers that fall on its outputs (or in the area of a virtual display
   they show). Laptops whose screens hang off the integrated GPU keep
   rendering on the discrete one.

Escape hatches, for a driver that misbehaves: `LCPLAY_SWAPCHAIN=wgpu` (wgpu's
own presentation, no statistics, no barrier), `LCPLAY_PRESENT_BARRIER=0`.

**Frame timing log** (`frametime.rs`). Every render-loop iteration is split
into *outside* (winit events), *housekeeping* (operator requests, show
edits, asset swaps, reloads), *acquire* (waiting for the swap chains),
*update* (layer contents, uploads included; the slowest layer is named),
*render* (recording and submitting) and *present*. An iteration longer than
two refreshes is logged as a `slow frame` line with that breakdown and what
else happened in it (show applied, content loaded, preview read back, late
decode…); at most four lines a second (a frame of 100 ms or more is always
logged), and the periodic frame-rate line adds
the slowest frame and the count over the threshold. `LCPLAY_FRAME_LOG=0`
turns it off, `LCPLAY_FRAME_LOG=<ms>` sets the threshold.

**Raw refresh log** (`vsync.rs` `RawLog`, opt-in: `LCPLAY_VSYNC_LOG=<file.csv>`).
One CSV row per frame on LC Play's own DXGI swap chain: the
`DXGI_FRAME_STATISTICS` sync refresh and its time (converted to local time),
the last present's id and the refresh it reached, the wait for the
frame-latency waitable, and the `RefreshClock` period and `PresentTracker`
correction after the report, and the clock's own number for the reported
refresh (`clock_refresh`: where it runs ahead of the driver's `sync_refresh`,
the driver lost counts). It exists because a drifting period estimate
cannot be diagnosed from the estimate: over the fitting window (two seconds
when the log was added), reports arriving progressively late and a refresh
counter that skips refreshes give the same straight line. Over a whole run the raw counter steps against time
tell them apart — which is how the counter fault above was found.

**[Not yet verified on frame-locked hardware.]** The pieces are unit-tested
(grid arithmetic, refresh fitting, present tracking, the Director's grid
selection) and run end to end on Linux, but the D3D12/NVAPI path has only
been compiled, not run: it needs the show machines. The `sync_test` cue
source and the per-output health (presenter, refresh rate, barrier state,
grid offset, missed presents, late decodes, frame-lock counter) are there to
verify it.

### 3.4 Measured, and not measured

| Claim | Evidence |
|---|---|
| Estimator converges to < 100 µs with 150–450 µs random per-leg delay and 80 ppm drift | simulation, `lc-time` unit tests |
| 20 ms delay spikes on 25 % of exchanges do not disturb it | simulation |
| 5 min of holdover on a 100 ppm oscillator stays < 2 ms | simulation |
| Three agents with clocks off by −5 min…+1 h and −80…+150 ppm agree with the Director and present the prescribed frame | real sockets on loopback, `lc-runner/tests/cluster.rs` (tolerance 2 ms; observed 0.1–0.3 ms right after lock, tens of µs RMS in steady state) |
| **Accuracy on a real switched LAN** | **[Not measured.]** Expected well under 1 ms from the above and from typical NTP-on-LAN behaviour; a Runner reports its own RTT, residual and drift so this can be read off the real rig with `lc-ctl status`. |
| **Wi-Fi** | Out of scope. Asymmetric, bursty delay defeats the method. |

### 3.5 What software sync cannot do

All Runners agreeing on the frame *number* does not align the *phase* of their
video outputs. Each GPU scans out on its own free-running pixel clock, so two
unlocked outputs can be up to one refresh period (16.7 ms at 60 Hz) apart, and
slowly drift through each other. On LED walls and on blended projectors with
fast motion this can be visible as tearing between adjacent outputs.

Removing it requires hardware that locks the outputs' scan-out: framelock /
genlock boards (e.g. NVIDIA Quadro Sync, AMD's equivalent) or genlocked SDI
output cards. LC Play's design is compatible with that — the content timing
above is exactly what such hardware needs to be fed with — but it does not
and cannot replace it. Likewise, PTP (IEEE 1588) with hardware timestamping
would improve the *clock* further (sub-microsecond), which only matters once
the outputs themselves are locked; the estimator can be fed from a
PTP-disciplined system clock without changing anything above it.

## 4. Failure behaviour

| Event | What happens | Verified by |
|---|---|---|
| Director crashes / is unplugged | Runners log `HOLDOVER`, keep rendering the right frames from their last transport state and drift estimate, retry the connection with back-off (0.5–5 s). | integration test, manual |
| Director returns (new process, new clock) | It starts *unanchored*. The first Runner's `hello` carries the timebase it kept alive; the Director **adopts** it (clock id, current cluster time, transport) rather than imposing a new one. Runners re-lock by slewing, without a step as long as their clocks stayed within 20 ms of each other during the outage. If a later Runner holds a newer transport state than the one adopted, the Director takes that and re-broadcasts it. | integration test (zero clock steps asserted), manual |
| Director returns but no Runner kept running | It restores show and position from disk, **paused**: nothing can tell it how much time passed. | test |
| A Runner dies | The others are unaffected; the Director drops it after ≤ 6 s of silence. | integration test |
| A Runner restarts with no Director around | It restores show and transport from its cache and resumes from the last transport change. | integration test |
| Control link dead but TCP not yet aware (cable pull) | Both sides use application keepalives: Director drops after 6 s silence, Runner reconnects after 5 s without an answer. | by construction; not tested with a real cable |
| Message loss / duplication / reordering | Epochs make transport delivery idempotent; a client that falls behind the Director's broadcast queue is re-sent the current transport and show. | unit tests (epochs); lag path by construction |
| Director started and stopped without any Runner reaching it | Its saved timebase and transport are left untouched, so a later start can still adopt. | test |
| Runner connected but clock-sync UDP blocked | The picture is held (the new timebase is unmeasured) and the Runner logs an error naming the port after 3 s. | by construction; not tested |

Known gaps:

* **The Director is a single point of failure for *control*** (not for
  playback): while it is down nobody can pause or cue. A hot standby is M6;
  `epoch` and `clock_id` are designed to arbitrate between two Directors.
* **Adoption trusts the first Runner's clock.** If Runners drifted apart
  during a long holdover, the others are slewed (or, beyond 20 ms, stepped —
  a visible jump on those Runners) to the first one's time. With a state directory the Director only adopts the timebase it
  served before; without one it adopts whatever it is offered.
* **Two Directors on one network** are not arbitrated: Runners follow the one
  they were pointed at or heard first.
* **No authentication or encryption**, and no `Origin` check on the
  Director's WebSocket: any page open in a browser on the show network could
  drive the transport. (The Asset Manager does restrict browser access to
  the Producer's origin.) Closed show networks only.
* **A panic in the Director is fatal in release builds** (`panic = "abort"`).
  Arithmetic on values from the wire is overflow-safe, but the Director,
  unlike the Runner, has no supervisor of its own: run it under one
  (systemd, a Windows service manager).

## 5. Runner internals

```
network ──▶ [ agent ] ──writes──▶ ( Playhead ) ◀──reads once per frame── [ renderer ]
```

The renderer depends only on the `Playhead`; it has no network code and never
waits. The agent can only improve what the playhead knows. During a change of
timebase the playhead *holds the picture* until the new clock has been
measured, rather than evaluate a transport against the wrong clock.

A stand-alone Runner (no `--director`) uses the same playhead with the local
clock as cluster clock and the keyboard as transport.

### Several graphics cards

Each output is rendered by the card its monitor is connected to (Windows,
D3D12), and each card has a render thread of its own (`card.rs`): a card
never waits for another card's refresh, uploads or encoding. The window
thread keeps the windows, the show and the operator's requests, and changes
a card only while holding that card's lock, between two of its frames.
`LCPLAY_CARD_THREADS=0` renders every card in turn on the window thread, as
before.

What a frame shows is decided **once per machine** (`plan.rs`): each card
maps the refresh it renders for to a plan key — the Director's grid instant
when frame-locked, otherwise the nearest refresh of the machine's reference
output (the first output of the lowest-numbered card) if the card refreshes
at the same rate — and the first card to need that key builds the plan (show
position, each layer's time and media time, free-run on-air instants) from
one clock reading; the others reuse it. A plan also records, per layer, the
video frame the first card showed: a card rendering the same refresh later
shows that frame (`Content::follow`), so when a decoder is a frame late every
card holds the same older frame instead of splitting the picture at the
boundary between the cards. Only a frame at most one frame from the one due
is imposed this way; a card holding a stale picture (its decoder restarting)
shows it alone, and the next card decides. A card that shows another frame
than a card before it for the same refresh counts a `split` frame on the
outputs concerned, including those that show the layer through a virtual
display (output health, Producer).

Without frame lock, the cards' scanouts are not aligned: each card shows the
plan of the reference refresh nearest to its own, up to half a refresh away.
Only frame lock (NVIDIA RTX PRO Sync / Quadro Sync) aligns the scanouts
themselves. `LCPLAY_SPLIT_CARDS=1` opens one device per output even on one
card: a test aid that runs the multi-card code on any machine.

Decoding is still per card (a layer spanning two cards is decoded on each,
on the card ffmpeg picks, see `MULTI_GPU.md`); a decode pool shared by the
cards is the next phase.

## 6. Code structure

```
crates/
  lc-proto      wire messages, transport state, beacon. No I/O.
  lc-time       estimator (pure) · clocks · UDP sync client/server · transport timeline
  lc-discovery  multicast announce / scan / find
  lc-net        control-channel client (WebSocket)
  lc-director   Core (state + command logic) · server (WS/HTTP) · store (persistence)
  lc-runner     agent · playhead · stage (document → layers) · assets (cache) ·
                renderer (app, card, plan, render, content, video, show, virtuals) ·
                audio (device, mixer, resample, pcm)
  lc-assets     media HTTP server (library, Range streaming, upload, thumbnails,
                optimisation jobs, sound extraction)
  lc-hap        HAP frames (decode, encode, re-chunk) and QuickTime read/write
  lc-producer   the operator's application: serves the interface (ui/) and shows
                it in a native window (system web view)
  lc-ctl        command-line controller, reference protocol client
  lc-play       launcher: all of the above on one machine
docs/           this file, PROTOCOL.md
```

Dependency direction: `proto ← time ← {director, runner}`; `discovery` and
`net` depend only on `proto`. The pure parts (`lc-proto`, the estimator, the
timeline) have no sockets and no real clock, which is why they can be tested
against simulated networks.

## 7. Show data model

```
Show { id, name, revision, frame_rate }
 ├─ Stage   { canvas w×h, Display[] }
 │    Display { id, runner_id, output_index, canvas_rect, rotation, warp, blend, colour }
 ├─ Asset[] { id, kind (video|image|sequence|live), content_hash, variants[], duration, size }
 ├─ Timeline[] { id, duration, Track[] }
 │    Track { id, z, Cue[] }
 │      Cue = Media   { asset_id, start, duration, in_point, loop, transform, keyframes[] }
 │          | Control { at, action: pause | jump | trigger timeline }
 └─ ControlMap[] { source (OSC address | MIDI note/CC | Art-Net channel | timecode), action }
```

Content is placed in **canvas** coordinates; each Display is a rectangle of
that canvas assigned to one output of one Runner. Content spanning several
Runners therefore needs no special handling. Every Runner receives the whole
show (it is small) and renders the Displays assigned to it — simpler than
per-Runner subsets, and any Runner can take over any Display.

The document is defined in `lc-proto/src/show.rs` (wire form in
[PROTOCOL.md](PROTOCOL.md) §5). The diagram above is the target shape; today:

* the main timeline is the document's own `tracks` / `control_cues`, and the
  auxiliary timelines ("tasks") are in `tasks` — each with its own transport;
* cues have a static placement plus fades (no keyframes);
* control cues are pause / play / stop / jump, on their own or another
  timeline;
* external control is the fixed OSC address list rather than a per-show map.

### Several timelines

Each timeline has its own `TransportState`; everything in section 3 applies
to each independently, on the one shared cluster clock. A task is *inactive*
(off stage) until played, paused or positioned, and after being stopped: a
`seek` (the operator clicking its ruler) brings it on stage, paused on that
frame; a control cue's jump (`TransportChange::Jump`) does too, and also
pauses a running timeline where it lands; the operator's pause brings it on
stage paused at 0. A control cue's pause (`TransportChange::Hold`) leaves it off stage. The
Director cues a task up on Pause only if it is off stage *now*: a Pause sent
within the lead time of a pending Stop only holds. The Runner evaluates each
layer against the transport of the timeline it belongs to and draws tasks in
front of the main timeline.

### Control cues

The Director, not the Runners, executes control cues. A scheduler looks
300 ms ahead on every playing timeline; a cue inside that window becomes a
transport change anchored at the exact cluster instant the playhead reaches
it, published before that instant. So a pause cue stops every output on the
same frame (verified to the nanosecond in the Director's tests, at normal
speed) — but while
the Director is down, control cues do not fire and Runners play through
them. Moving their execution into the Runners is possible (they are part of
the show document) and is the way to remove that dependency.

### Free-running cues

A video cue with `free_run` set plays at normal speed from the instant it
goes on air, whatever its timeline does afterwards (pause, rate change,
scrub), until it goes off air; at the end of the file it holds the last
frame, or loops if `loop` is set. Its media time is
`in + (cluster_now − on_air)`.

`on_air` must be the same on every Runner without any message carrying it,
so it is derived from the transport state alone (`on_air_since` in
`lc-runner/src/app.rs`): if the timeline is playing forward and has crossed
the cue's start since its anchor, `on_air` is that crossing instant (for a
looping timeline, the most recent lap); otherwise — the timeline was paused,
located or started inside the cue — it is the state's anchor instant. A
Runner remembers the instant while the cue stays on air, so a later pause
does not move it. A Runner that joins while the cue is on air computes it
from the state it receives, which agrees with the others unless the state
changed while the cue was already on air (then the joiner restarts the clip
from that change) [design limit, not measured]. The same happens on a
looping timeline when the cue spans the loop point and never goes off air:
the Runners that were there keep the first instant, a joiner takes the most
recent lap. The fix is for the Director to put each free-running cue's
on-air instant in the state it broadcasts; not done yet.

`free_run` on a *track* is the older form and still read: it makes every cue
on the track free-running. The Producer converts it to per-cue flags when it
opens a show.

### From document to pixels

```
Producer ──load_show──▶ Director ──show──▶ Runner agent ──▶ Playhead
                         validates,                           │ (document + id)
                         stores, persists                     ▼
                                              stage::build(document, runner name, asset cache)
                                                              │
                              displays of this Runner ─▶ outputs (windows)
                              virtual displays ─▶ virtuals (offscreen pictures)
                              all cues ─▶ layers ─▶ content (image / colour / video
                                                            / a virtual's picture)
```

The renderer looks at the playhead's document every frame; when its id (or
the set of downloaded assets) changes it rebuilds its layers. Windows are
rebuilt only if the displays changed, and loaded media is kept across edits,
so dragging a cue in the Producer moves it on the output without a flicker.

### Assets

The show names each asset by its URL on the Asset Manager, with its size and
its path there. A Runner downloads it once into its cache, under that path
(`Optimized/Show A/intro.mov`, cleaned for Windows), and plays from local
disk; until the file has arrived the cue shows a placeholder. An index in the
cache (`.lcplay-index.json`: asset id → URL, size, file) says which version
each file is, so a new version at the same name is fetched again; the
renderer's content key includes the file's size and modification time, so
what was loaded from the old version is reloaded. While a new version
downloads, the cache answers with the newest version it has of the same
asset, so cues do not go blank. If Windows will not replace a file that is
still open, the new version goes beside it under a `~tag` name, and the old
one is listed (`.lcplay-superseded.json`) and deleted later.

Asset ids are derived from the file name, **not** from the content. When a
file is replaced under the same name, or its optimised copy is (re)made, an
open Producer notices the changed URL or size, updates the show, and Runners
fetch the new file; with no Producer open the show keeps the old values —
Runners that have the old file keep playing it and new Runners refuse the
download (reported once, retried with back-off). A replacement of the
original of exactly the same size is not detected. Content hashing is the
planned fix. (Optimised copies carry their version in the URL, `?v=`, so
re-optimising is always detected.)

Folders in the Producer's Assets window are part of the show
(`asset_folders`, `asset_placement`; PROTOCOL.md §5), not of the Asset
Manager: they organise the list for that show, and filing an asset moves no
file. Uploads stay flat in the media folder whatever folder they are
uploaded into, so two files of the same name in different folders are one
file (the last uploaded). A folder given to the Producer (picked, or
dropped) is uploaded file by file, and its folders are made in the show,
reusing those of the same name at the same place.

### Optimisation (Asset Manager)

`lc-assets/src/optimize.rs`. ffprobe classifies each video (codec name,
codec tag, pixel format) into a row of the codec map
(`.lcplay/optimize.json`); the row gives the output, chunks and Snappy. One
background job at a time: HAP → re-chunk goes through `lc_hap::rechunk`
(parse each frame's decode instructions, unpack, split the texture again on
block boundaries, Snappy per chunk where it helps, write a new QuickTime
file with the same timing); everything else through ffmpeg's HAP encoder
(`-c:v hap -format … -chunks N -compressor snappy|none`), padded to a
multiple of 4; its messages are read as they come, and a run with no
progress report for 5 minutes is stopped. HAP sources are recognised by
their codec tag (ffprobe names no codec for HAP R). Output is written hidden and renamed when complete. The
manifest (`.lcplay/optimized.json`) records per source path the version it
was made from (size, mtime), the rule, the output and failures, so nothing
is redone or retried in a loop; a re-optimise marks the record stale and the
old output stays in use until the new one replaces it. The library listing
reports, per item, the status and `play_url`/`play_size`/`play_path` — what
the Producer writes into the show.

### Video

Whatever the decoder, the frame on screen is a pure function of time: each
rendered frame computes the file frame that belongs to the layer's time
(`floor(media_time × fps)`, `video.rs: media_frame`) and shows it if it is
ready, or the newest frame available if not. Nothing ever waits: loading
(probing a file, decoding an image) happens on worker threads and a layer is
transparent until its first frame is in.

**HAP** (`hap.rs`, `mov.rs`). HAP frames are block-compressed GPU textures
(BC1/DXT1, BC3/DXT5, BC7), mostly wrapped in Snappy. The Runner reads the
QuickTime sample table, so every frame is addressed directly in the file;
two worker threads read and unpack the next few frames in the direction of
playback (chunked frames are unpacked in parallel) and copy each into one of
the layer's mapped upload buffers (eight, rows padded to 256 bytes); the
render thread only unmaps the wanted one and records a buffer-to-texture
copy, submitted ahead of the frame, into a BCn texture that the GPU samples
directly. The buffer is mapped again once that frame has been submitted and
the GPU is done with it. (Copying on the render thread with
`Queue::write_texture` cost 0.5–0.8 ms per MB on a laptop: 12–16 ms of a
16.7 ms frame with a 10K and two 4K layers.) Hap
Q's scaled YCoCg is converted in the shader. Seeking, looping and reverse
playback cost nothing extra. Measured here: unpacking a 9976×2176 frame
takes 1–2 ms (test pattern; real footage compresses less and takes longer);
the whole system played it at ~57 fps on lavapipe (software Vulkan, 4 CPU
cores). Needs a GPU with BC texture support — every desktop GPU has it.

**Everything else** (`video.rs`). An `ffmpeg` child process per playing
cue and card decodes on a GPU's video engine where there is one (NVDEC,
Quick Sync, VCN; software otherwise). On Windows each decoder is bound to
the card that renders its layer: `-hwaccel d3d11va -hwaccel_device <DXGI
adapter index>`, found from the card's LUID when its device is created.
The device ffmpeg reports opening is logged ("ffmpeg decodes on …", or a
warning naming both cards if it is another one). Elsewhere, or when the
card's LUID is unknown (not D3D12), ffmpeg picks (`-hwaccel auto`).
`LCPLAY_HWACCEL=auto` restores ffmpeg's pick on Windows too, and
`LCPLAY_HWACCEL=none` decodes in software. ffmpeg writes YUV 4:2:0 planes
to a pipe — 1.5 bytes per pixel instead of 4 for RGBA — and the shader
converts them (BT.709 or BT.601, limited range).
Only video with alpha travels as premultiplied RGBA. A jump restarts
`ffmpeg` at the new position (frame-exact: verified against a clip with
frame-numbered content). A cue is pre-rolled one second before it starts,
and a second decoder is started just before a loop point. Every frame still
crosses the CPU once, so this path scales with CPU and memory bandwidth, not
GPU. Because the decoder only runs forward, reverse playback is choppy
(restarts are limited to four a second).

### Audio

Sound follows the same rule as the picture: **each sample is a pure
function of the show, the transports and the cluster time at which it is
heard.** Nothing is streamed between machines; every Runner computes its own
samples, and two Runners playing one bus agree to within the clock sync
(tens of microseconds on a LAN).

**Routing** (`lc-proto/src/audio.rs`): cue channel → project bus (per cue)
→ device channels (per Runner). The buses are the project's; which device
plays them, and on which channels, is per Runner — moving a show to another
rig changes only that mapping. A Runner has one output device (one cpal
stream) and one input device, reserved for LTC.

**Extraction** (`lc-assets/src/audio.rs`). Runners never decode audio at show
time. The Asset Manager writes each file's first audio stream, once, as WAV
(RF64 when over 4 GB), 32-bit float, 48 kHz, all channels, at
`Optimized/<source path>.wav`, from the original (optimised HAP files carry
no sound), whether video optimisation is on or not. It records the offset
between the audio and video streams' start times, so lip-sync survives
encoder delay and edit lists. A manifest (`.lcplay/audio.json`) keeps it
from redoing work.

**Runner** (`lc-runner/src/audio/`):

* `mod.rs`, the control thread: lists devices (at start and on
  `rescan_audio`), opens the show's device, turns the show into a *plan*
  (per cue with sound: its timing, its memory-mapped file, its routes as
  (file channel, device channel, gain) triples), fetches sidecars through
  the asset cache, touches the pages about to be played (a page fault that
  reads the disk must never happen in the callback), and every 5 ms
  publishes a snapshot: a linear map local → cluster time, the transport
  timelines, and per cue whether it can be heard in the next seconds (cues
  that cannot are skipped, so a show with hundreds of cues costs what its
  audible ones cost). Health goes into the playhead for the agent's report.
* `mixer.rs`, run by the driver's thread, with no lock, no allocation and
  no file access: the plan arrives through a channel (the old one goes back
  to the control thread to be freed), the snapshot through an `ArcSwap`.
  The **mixer** computes frames of the *cluster grid* (frame *g* = cluster
  time *g*/48 kHz): for each cue, the media sample due at that cluster
  instant (`in_s` + elapsed show time, or time on air for a free-running
  cue; looped at the video's length when `loop` is on). A jump — seek,
  pause, a timeline loop, a cue starting mid-file — crossfades over 5 ms;
  a cue that starts at its in point starts on that exact sample; a file's
  own loop is seamless.
* The **engine** reads the grid at the positions of the device's frames.
  The driver says when the callback's first frame will be heard; the cluster
  clock says which cluster instant that is; minus the Runner's `delay_ms`,
  that is the grid position the frame must carry. A PI loop on the
  difference between where the resampler is and where it should be
  steers its step: it learns the card's clock against the cluster's (tens
  of ppm; up to ±1000 ppm, which also covers the cluster clock's own
  slewing), and a step (a dropped buffer) is slewed out mostly by the
  proportional term, so the drift learnt is barely disturbed. Errors over
  20 ms are corrected by a jump with the 5 ms fades. 1000 ppm is a
  sixtieth of a semitone.
* `resample.rs`: polyphase windowed sinc, 32 taps, Kaiser β = 8 (about
  80 dB stop band), 512 phases interpolated linearly, pass band ending at
  0.92 × the lower Nyquist frequency. It also converts 48 kHz to whatever
  rate the device runs at (WASAPI's mix format, the ASIO driver's rate), so
  Windows' own converter is never in the path. Only the device channels the
  plan uses are resampled.
* `device.rs`: cpal. WASAPI (shared mode) by default on Windows; ASIO with
  the `asio` feature (it needs Steinberg's ASIO SDK at build time, under its
  GPLv3 option). cpal's `realtime` feature puts the audio thread in MMCSS
  "Pro Audio" on Windows. A device that fails is reopened every 3 s.

**Cost**, measured in a release build (share of one core of the development
machine, a cloud VM; a Windows laptop will differ): one
cue on a stereo device 0.7 %, sixteen cues 3 %, sixty-four cues on sixteen
channels 11 % — the per-sample work of finding each cue's due sample
dominates, and is the place to optimise if shows need it.

**Limits of this version.** Sound plays at normal speed only (silent at
other rates and while scrubbing). LTC input is configured but not read yet.
Only the device's clock drift is corrected, not its absolute latency: what
the driver reports is trusted, and `delay_ms` trims the rest by ear or by
measurement. Several Runners on one bus are in step within the clock sync,
which suits separate zones; speakers covering the same area from two
Runners would comb-filter, and the Producer says so. Verified on Linux
(ALSA, and a PulseAudio sink as a clocked virtual device); the Windows
build type-checks but has not been run on a real card.

### Virtual displays

A virtual display (`stage.virtuals`; WATCHOUT has the same idea under the
same name) is a camera on the canvas whose picture goes not to a screen but
to the cues whose source is that virtual display. It captures a rectangle of
the canvas at its own resolution, so a scene can be built once and shown
scaled, moved, rotated, faded or repeated elsewhere, or a part of the canvas
fed at an exact resolution to a cue placed on an LED processor's input.

Nothing about it travels over the network. Each Runner renders, per card and
per frame, the virtual displays that its outputs need (`virtuals.rs`
`plan`): those shown by a cue on air over one of its outputs, then those
shown by a cue on air inside *those*, and so on — in that dependency order,
before the outputs, in the same command buffer. Each is a scene pass like an
output's, into its own texture, over a transparent background; a layer
showing it then binds that texture in place of content of its own. Since all
Runners compute the same frame from the same show and transport, the same
picture is on every Runner on the same frame — no latency.

A loop (a virtual display shown inside its own area, directly or through
others) cannot be ordered: the plan is a depth-first walk, and a layer whose
virtual display has not been rendered yet in this frame is left out. One cue
of the loop is dropped; there is no feedback effect. Content inside a
virtual display's area is decoded only while the virtual display is needed
(or about to be: the same pre-roll as cues on outputs).

Not done: per-virtual output delay, warp or colour correction (WATCHOUT has
them), a higher-precision picture format (the picture is 8-bit sRGB, like
the outputs), and 3D positioning.

### Outputs

A Runner opens one window per display assigned to it — borderless
fullscreen on the configured monitor, or a plain window as a preview.
Edits to a display are applied to its open window (position, size,
fullscreen, monitor) and GPU state (warp, blend, rotation) in place; windows
are only opened and closed when displays are added or removed. On Windows
the renderer uses Direct3D 12 (`LCPLAY_BACKEND` overrides).

### Several Producers on one show

The Director holds the one show everybody edits (it already did, for the
Runners). Producers send **edits** (`edit_show`: operations addressed by
object id, `lc-proto/src/edit.rs`) instead of whole documents. The Director
applies them under its state lock, one after another, validates the result
and distributes the show as before. Each Producer shows the Director's show
with its own not-yet-confirmed edits re-applied on top ("server
reconciliation"), so its own changes appear at once and nobody else's are
overwritten. Conflicts: last writer wins per field; edits to something
deleted meanwhile are dropped. No operational transform or CRDT is needed
because there is a single, always-present sequencer, and editing without a
Director is not possible anyway. A show that arrives mid-drag is applied
when the drag ends, so the dragged object does not jump. Undo steps are
stored as operations (forward and back), so undo reverts only that user's
changes. Presence (name, colour, selection, dragging) is relayed by the
Director between Producers only.

### Runner management

`lc-runner` is started as a supervisor that runs the real Runner as a child
(`--worker`): exit code 0 (window closed, Quit) leaves the supervisor
waiting for a `start_runner` from the Director (UDP, its own port reported
as `launcher_port`), the restart code (75) starts it again at once, the exit
code (77, `Exit`) ends both, anything else (a crash) starts it again after a
growing pause — and after three failures within seconds of starting, waits
for Start instead of looping. One supervisor per Runner folder (lock file);
a second launch asks the first to start the Runner. A show file or `--demo`
without a Director runs unsupervised. The child exits when the supervisor's
end of its stdin closes, so killing the supervisor never leaves an orphan,
on Windows too. Both write the log file by path, so a rotation by one does
not lose the other's lines.

Operator actions travel as `runner_action` commands; the Director forwards
them to the named Runner. A Runner reports its machine (host name, OS,
version, IP, the MAC of the interface it reaches the Director through) in
its health; the Director remembers every Runner it has seen, persisted, so
the Producer can list an offline Runner and wake it (Wake-on-LAN, sent by
the Director as a UDP broadcast). Logs (the last 400 lines, kept in memory
by the Runner) and output pictures (rendered small through the warp pass,
PNG) are sent on request and relayed only to Producers and controllers.
Rebooting or powering off the computer requires `--allow-power` on that
Runner. Remote desktop is launched by the Producer service on its own
machine (`mstsc` for RDP on Windows; for VNC on Windows an installed viewer
found in the usual folders or named by `LCPLAY_VNC_VIEWER`, since Windows has
no `vnc://` handler; `vnc://` / `rdp://` URLs elsewhere), for requests from
that machine only.

The canvas is unbounded in the Producer. `stage.width/height` remain in the
document (validated, shown in logs) but the Producer sets them on every send
to the bottom-right corner of the area the displays cover; nothing in
rendering depends on them.

## 8. Roadmap

| | Milestone | Content |
|---|---|---|
| ✅ | M0 | Workspace restructure: existing server → `lc-assets`, existing engine → `lc-runner`. |
| ✅ | M1 | Communication and synchronisation: protocol, cluster clock, discovery, control channel, Director, Runner agent, `lc-ctl`, failure handling. |
| ✅ | M2–M5, basic | Asset upload/serving and Runner cache; unified show document rendered live; video via `ffmpeg`; Producer UI; OSC; one-machine launcher. |
| ✅ | Performance, management | GPU-decoded HAP; hardware-decoded YUV video; non-blocking loading; outputs updated in place; supervisor; restart/quit/standby/power/Wake-on-LAN/log/preview/remote desktop. |

Still to do, roughly in order of how much they limit real use:

| Area | Missing |
|---|---|
| Playback performance | Zero-copy hardware decode for non-HAP codecs (decoded frames stay on the GPU); decoding shared between cards (today a layer spanning two cards is decoded once on each of them). |
| Audio | LTC chase on the input device; sound at rates other than 1×; level automation; testing on Windows with WASAPI and ASIO cards; Dante/AES67 with PTP-locked sample clocks. |
| Sync | Verification of §3.3 on frame-locked hardware; Vulkan present barrier (`VK_NV_present_barrier`) for non-D3D12 Runners. |
| Assets | Content hashing; optimisation (transcode to the playback codec) on ingest; prefetch progress reported to the Producer. |
| Authoring | Keyframes/tweens; a warp editor; control cues executed by Runners (so they work without the Director). |
| Packaging | An installer; a launcher without a console window; the Producer preview is the web view's own video playback, not frame-exact. |
| External control | MIDI/MTC, Art-Net, LTC chase; per-show control mapping. |
| Outputs | Freeform mesh warp; SDI and NDI outputs (NDI needs the proprietary NDI SDK and its licence); framelock integration. |
| Resilience | Standby Director; arbitration between two Directors. |
| Health | GPU load and temperature (the Runner reports fps, sync quality, outputs, monitors and machine only). |
| Security | Authentication, encryption, WebSocket origin check. |
| Platforms | Only Linux has been run. Windows and macOS type-check but are untested. |

### Deviations from the approved plan

* The plan had the renderer's `Canvas/Output/Warp/…` types move into
  `lc-proto`. Instead `lc-proto` got its own, renderer-independent show
  document, and `lc-runner/src/stage.rs` translates it into the renderer's
  model. That keeps the maths library out of the protocol crate and lets the
  two evolve separately.
* The plan named HAP as the likely playback codec, to be decided with the
  Asset Manager work. That decision is still open: the current decoder
  plays whatever `ffmpeg` reads, unoptimised.
* The Producer has no back end of its own beyond serving the page: the
  project lives in the Director (and in files saved from the browser), not
  in a Producer-side project store.
* The control server lives in `lc-director` rather than `lc-net`, since the
  Director is the only server; `lc-net` is the client.

## 9. References

* RFC 5905, *Network Time Protocol Version 4* (June 2010) — on-wire offset and
  delay calculation (§8), clock filter (§10).
* RFC 2365, *Administratively Scoped IP Multicast* (July 1998) — the
  `239.255.0.0/16` local scope used for discovery.
* RFC 6455, *The WebSocket Protocol* (December 2011).
* IEEE 1588-2019, *Precision Time Protocol* — referenced as a possible lower
  layer, not implemented.
