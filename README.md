# lcmediaserver (`lcme`)

A **lightweight show-control media server** — the WATCHOUT / Pixera / disguise
category, not a home-media or streaming server. Arrange displays and
projectors on a virtual canvas, place and time content across them, let one
piece of content **span multiple outputs**, and apply per-output **warp +
edge-blend** for projection mapping. One machine drives all outputs; a single
master clock keeps them in sync.

Built in Rust on **wgpu** (Vulkan/Metal/DX12) + **winit**.

## Project goal (read this first)

"Media server" means two different things. This repository is the **live
events / installations** meaning: software that plays content out to
displays, LED walls and projectors during a show, frame-accurately and
without dropping out.

**In scope** — judge every change by these:

- Playback on **multiple physical outputs** from one canvas: spanning,
  orientation, warp, edge-blend, later mesh warp.
- **Timing**: one master clock, frame-accurate video and audio locked to it,
  timeline/cue playback, transport control.
- **Show reliability**: a bad file, missing media or a GPU hiccup must never
  take the outputs down mid-show; clear load-time validation; predictable
  behaviour under an operator's hands.
- **Control and sync** (later): external control (OSC/TCP/MIDI/Art-Net,
  timecode), multiple render machines locked together, an operator UI with
  preview.
- **Lightweight**: one native binary, no database, runs on an ordinary show PC.

**Out of scope** — don't build or optimise for these here:

- Browser playback, HTTP streaming, web transcoding, CDN caching, media
  libraries for many users, Plex/Jellyfin-style features.
- Scaling out as a stateless web service behind a load balancer.

> **History:** until 2026-11-13 this repository also contained an HTTP
> streaming/transcoding server (the *IT* kind of media server) at its root,
> with this engine in `engine/`. That server was split into its own
> repository, **`lcstream`**, with its history. If old commits, issues or
> sessions mention `/stream`, `/transcode`, `LCMS_*` variables, Docker or a
> web player, they refer to that server, not to this product.

> **Status: Phase 1 foundation.** This runs today with image and solid-color
> content, full affine placement, per-output corner-warp + edge-blend +
> orientation, and synchronized transport. **It does not play video yet** —
> that is the next milestone (Phase 2). Live sources and audio are scaffolded
> behind clear boundaries and land in later phases (see [Roadmap](#roadmap)). The compositor is tested by rendering real frames
> offscreen on a software GPU (blend, compositing, warp). Windowing on
> physical multi-monitor hardware still needs a first bring-up on your machine.

## Concepts

- **Canvas** — one shared 2D pixel space (origin top-left, y down). Everything
  lives here.
- **Output** — a rectangle on the canvas (`canvas_rect`) mapped to one physical
  display. Each output has its own window placement, display **rotation**
  (0/90/180/270), **warp** (four movable corners), and **edge-blend** (per-edge
  soft feather + gamma).
- **Layer** — a piece of content placed on the canvas with position, size,
  `scale`, `anchor`, `rotation`, `opacity`, and `z` order, plus optional
  `timing`.

Because layers live in **canvas** space and each output renders the slice of
canvas it covers, **content spanning multiple outputs is automatic** — no
special case.

### Render pipeline

Each frame samples the show clock **once**, encodes **every** output into one
GPU submission, then presents all outputs together — so every output renders
the same show time. Each display still refreshes on its own clock: without
genlocked/synchronized displays (or a sync-capable GPU setup) outputs can be up
to one refresh apart on screen, which can show as a brief tear on fast motion
across a seam. Per output:

```
layers ──▶ [scene pass] ──▶ canvas-slice texture ──▶ [warp + edge-blend] ──▶ display
           draw each layer                            perspective corner-pin,
           via output.view_proj                       multiply by blend mask
```

## Build & run

```bash
cargo run --release                                       # bundled examples/show.json
cargo run --release -- examples/preview-3-screens.json
cargo run --release -- /path/to/my-show.json
```

On Linux the windowing layer needs `libxkbcommon-x11-0` (present on desktop
distros) and a Vulkan or GL driver.

At startup the engine logs every monitor (`monitor 0: … 1920x1080 at (0, 0)`)
so you can set `"monitor": N`, and every 5 s it logs the frame rate and show
time. A layer whose source fails to load (missing file, image larger than the
GPU's texture limit) shows a magenta checkerboard and logs an error; the rest
of the show keeps running.

### Starter shows

| File | Setup | Use it to test |
|------|-------|----------------|
| `examples/preview-3-screens.json` | 3-screen wall, previewed as 3 small windows on **one** monitor | layout, content spanning, timing — on a laptop |
| `examples/two-monitors.json` | 2 monitors side by side, fullscreen | real multi-display output |
| `examples/two-projectors-blend.json` | 2 projectors with a 240px soft-edge overlap | corner-pin + edge blend alignment |
| `examples/show.json` | 2 windows at 75% size, colors only | zero-asset smoke test |

Window `position` is honoured on Windows, macOS and X11. Wayland compositors
ignore client-requested positions, so there windows open wherever the
compositor puts them (use `fullscreen` + `monitor` for real outputs).

Copy one next to your media and edit it. JSON has no comments, but unknown keys
are ignored, so the templates document themselves with `"_note"` fields.

### Writing your own show

1. **Canvas** = the whole stage in pixels: usually the outputs' resolutions put
   together (minus blend overlaps).
2. **One output per display.** `canvas_rect` is the part of the canvas it shows;
   keep its aspect equal to the display's, or the image stretches. The window
   size can be smaller (a scaled preview) as long as the aspect matches.
3. **Layers** are placed in canvas pixels. `position` is where the layer's
   anchor (default: its center) lands, so a layer filling `x..x+w` sits at
   `x + w/2`. A layer crossing an output boundary simply appears on both.
4. **Media paths** are relative to the show file.
5. **Projectors:** overlap the `canvas_rect`s by the physical overlap and set
   the mirrored `blend` to `overlap / output width` (validated as consistent in
   the bundled examples). Then move `warp.corners` (0..1, TL/TR/BR/BL) in small
   steps such as `0.01` until grid lines meet.
6. **Portrait display:** if the OS already rotates it, just use a portrait
   `canvas_rect` and window size. If the panel is physically rotated but the
   OS isn't, keep the landscape window, use a portrait `canvas_rect`, and set
   `rotation_degrees` to 90 or 270. If you omit `window.size`, the window
   already defaults to the rotated (landscape) size.

Load errors name the problem (`output 'B': canvas_rect must have positive w
and h`, `warp corners must form a convex quad…`), and nothing is drawn until
the show is valid.

**Test patterns** for your own canvas size (lines must stay straight across
seams, the circle must stay round, corner blocks show orientation):

```bash
python3 tools/test_pattern.py 3840 1080 my-show/media/grid.png
```

### Transport controls

| Key | Action |
|-----|--------|
| `Space` | play / pause |
| `→` / `←` | seek ±5 s |
| `↑` / `↓` | seek ±30 s |
| `Esc` | quit |

## Show file format

```jsonc
{
  "canvas": { "width": 1920, "height": 1080, "background": [0.05,0.06,0.09,1.0] },
  "playback": { "autoplay": true, "loop": true, "duration": 20.0 }, // loop needs duration

  "outputs": [
    {
      "id": "left",
      "canvas_rect": { "x": 0, "y": 0, "w": 1024, "h": 1080 },  // slice of canvas shown
      "window": { "size": [1024,1080], "position": [0,0],
                  "fullscreen": false, "monitor": 0 },           // physical window
      "rotation_degrees": 0,                                     // display orientation
      "warp": { "corners": [[0,0],[1,0],[1,1],[0,1]] },          // TL,TR,BR,BL (normalized)
      "blend": { "right": 0.125, "gamma": 2.2 }                  // overlap fraction per edge
    }
  ],

  "layers": [
    {
      "id": "banner",
      "source": { "type": "color", "rgba": [0.1,0.65,0.62,1.0] },
      "transform": {
        "position": [960,300],   // canvas point the anchor pins to
        "size": [1400,220],      // canvas pixels before scale
        "anchor": [0.5,0.5],     // 0..1 within the layer (default center)
        "scale": [1,1],
        "rotation": 0,           // degrees, clockwise, about the anchor
        "opacity": 1.0,
        "z": 1
      },
      "timing": { "start": 0, "end": null }   // show-clock seconds
    }
  ]
}
```

### Source types

| `type` | Status | Notes |
|--------|--------|-------|
| `color` | ✅ Phase 1 | `{ "rgba": [r,g,b,a] }`, linear 0..1 |
| `image` | ✅ Phase 1 | `{ "path": "..." }` (PNG/JPEG/BMP/GIF/WebP) |
| `video` | ⏳ Phase 2 | `{ "path": "..." }`. Not decoded yet: always shows the placeholder. Without `--features video` it logs a warning; with it, an error. The feature only links ffmpeg/libav, so building with it also needs their dev libraries. |
| `live`  | ⏳ Phase 3 | `{ "kind": "..." }` (capture/NDI). Placeholder otherwise. |

### Projection mapping

- **Warp (corner-pin)**: move an output's four corners (normalized display
  space, TL/TR/BR/BL) to keystone the image onto an off-axis flat surface. The
  mapping is a true perspective transform (homography), so straight lines stay
  straight. Corners must form a convex quad (validated at load). Omit
  `corners` for identity. Corners always refer to the physical display, after
  `rotation_degrees` is applied: on a rotated output, TL is still the
  display's top-left. Freeform mesh warp for curved surfaces is Phase 3.
- **Edge blend**: projector images must physically overlap, and their
  `canvas_rect`s must overlap by the same amount. Set each output's blend on
  the shared edge to that overlap as a fraction of the output (e.g. 128px of a
  1024px output → `0.125`), mirrored on the neighbor. The ramp is
  complementary, so the two projectors' light sums to exactly 100% across the
  overlap. `gamma` is the projector's gamma (default `2.2` = no correction);
  nudge it if the seam reads slightly bright or dark. Blend edges are in
  canvas-slice space, *before* `rotation_degrees`: on a 90° output, `left`
  is the slice's left edge, which lands at the display's top. (Warp corners,
  by contrast, are in display space.)

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Besides unit tests for the math, the suite renders real frames offscreen and
checks pixels: blend overlaps sum to full light, translucent layers composite
correctly, the corner-pin confines the image. These need a GPU adapter; on a
headless Linux box install Mesa's software Vulkan driver
(`apt install mesa-vulkan-drivers`). Without an adapter they skip on a dev
machine, but fail when `CI` or `LCME_REQUIRE_GPU` is set (GitHub Actions sets
`CI`), so they can't pass silently there. CI runs them on pushes to `main` and
on pull requests.

## Roadmap

In priority order. A media server for shows is only as good as its video
playback, so Phase 2 comes before any other feature work.

- **Phase 1 (done):** canvas/output/layer model, JSON show, master clock +
  transport, scene compositor with full affine transforms, image + color
  content, per-output corner-warp + edge-blend + orientation, multi-output
  spanning.
- **Phase 2 (next): video playback.** Decode locked to the master clock
  (frame-accurate start, seek and loop), a decode path that keeps up with
  large canvases (choice of playback codec and hardware decode to be decided
  first), audio out locked to the same clock (`cpal`).
- **Phase 3:** timeline/cue model and external control (OSC/TCP/MIDI/Art-Net,
  timecode), operator UI with preview.
- **Phase 4:** multi-machine shows (network clock, frame lock, media
  distribution to render nodes), live inputs (NDI/capture), freeform
  mesh warp for curved surfaces.

## License

MIT
