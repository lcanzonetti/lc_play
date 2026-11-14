# LC Play — architecture

Status of this document: sections 1–6 describe what is **implemented**
(milestones M0–M1). Section 7 (show data model) is the **target** design;
section 8 is the roadmap. Anything not measured or not verified is labelled.

## 1. Components

```
                         OPERATOR LAPTOP
        ┌────────────────────────────────────────────────┐
        │  PRODUCER  (planned, M4)                       │
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
   │ CLUSTER CLOCK (master)   │    │ (cluster integration M2) │
   │ external control gateway │    └────────────┬─────────────┘
   │ (OSC/MIDI/Art-Net/LTC M5)│                 │
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

### 3.3 Presentation

The renderer samples the playhead once per frame and draws every output from
that one sample, in one GPU submission. `Playhead::position_at_local(t)`
exists so the sample can be taken for the *predicted time of the next vertical
blank* instead of "now". **Not yet used**: the renderer currently samples at
render time, so it shows content up to one refresh period late — equally on
all Runners, hence invisible as a sync error, but it is latency. Using vblank
prediction is part of M3.

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
  WebSocket: any page open in a browser on the show network could drive the
  transport. Closed show networks only.
* **A panic in the Director is fatal in release builds** (`panic = "abort"`).
  Arithmetic on values from the wire is overflow-safe, but there is no
  supervisor: run it under one (systemd, a service manager).

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

## 6. Code structure

```
crates/
  lc-proto      wire messages, transport state, beacon. No I/O.
  lc-time       estimator (pure) · clocks · UDP sync client/server · transport timeline
  lc-discovery  multicast announce / scan / find
  lc-net        control-channel client (WebSocket)
  lc-director   Core (state + command logic) · server (WS/HTTP) · store (persistence)
  lc-runner     agent · playhead · renderer (app, render, content, show)
  lc-assets     media HTTP server (library scan, Range streaming, transcode)
  lc-ctl        command-line controller, reference protocol client
docs/           this file, PROTOCOL.md
```

Dependency direction: `proto ← time ← {director, runner}`; `discovery` and
`net` depend only on `proto`. The pure parts (`lc-proto`, the estimator, the
timeline) have no sockets and no real clock, which is why they can be tested
against simulated networks.

## 7. Show data model (target)

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

**Today:** the Director distributes the show as an opaque, revisioned JSON
document; Runners cache it. The renderer still uses its own show file format
(`crates/lc-runner/src/show.rs`: canvas, outputs, layers) given on the command
line, and does not yet render the distributed document. Unifying the two is
part of M3, together with timeline evaluation.

Assets will be content-addressed (hash in the show, HTTP URL derived from
it), so a Runner can verify what it downloaded and never plays a stale file.

## 8. Roadmap

| | Milestone | Content |
|---|---|---|
| ✅ | M0 | Workspace restructure: existing server → `lc-assets`, existing engine → `lc-runner`. |
| ✅ | M1 | Communication and synchronisation: protocol, cluster clock, discovery, control channel, Director, Runner agent, `lc-ctl`, failure handling. |
| | M2 | Asset Manager in the cluster: content hashing, ingest, optimisation to a playback codec, Runner prefetch and verification. Codec to be decided there; HAP (GPU-decoded, cheap on CPU, large files) is the leading candidate against H.264/HEVC with hardware decode. |
| | M3 | Runner playback: unified show model, timeline evaluation, video decode, vblank-predicted sampling, GPU health metrics (load, temperature). |
| | M4 | Producer: Stage, Timeline, Assets, cluster health — web UI served by its own service. |
| | M5 | External control gateway in the Director: OSC first, then MIDI/MTC, Art-Net, LTC chase. All map onto the existing `Command` set. |
| | M6 | Mesh warp; SDI (e.g. DeckLink) and NDI outputs; standby Director; framelock integration. NDI requires the proprietary NDI SDK and its licence. |

### Deviations from the approved plan

* The plan had the renderer's `Canvas/Output/Warp/…` types move into
  `lc-proto` in M1. They stayed in `lc-runner`: moving them would pull the
  maths library into the protocol crate before the unified show model (M3)
  decides their final shape. The show travels as an opaque document meanwhile.
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
