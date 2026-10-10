# lcmediaserver-engine (`lcme`)

A **multi-output canvas playback engine** — the WATCHOUT / disguise / Pixera
category. Arrange displays on a virtual canvas, place content across them
(scaled, positioned, rotated, oriented), let a single piece of content **span
multiple outputs**, and apply per-output **warp + edge-blend** for projection
mapping. One machine drives all outputs; a single master clock keeps them
synchronized.

Built in Rust on **wgpu** (Vulkan/Metal/DX12) + **winit**.

> **Status: Phase 1 foundation.** This runs today with image and solid-color
> content, full affine placement, per-output corner-warp + edge-blend +
> orientation, and synchronized transport. Video, live sources, and audio are
> scaffolded behind clear boundaries and land in later phases (see
> [Roadmap](#roadmap)). It must be built and run on a machine with a GPU and
> displays — it was developed headless and the rendering has not been run on
> hardware yet, so treat the first bring-up as a bring-up.

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

### Render pipeline (per output, per frame)

```
layers ──▶ [scene pass] ──▶ canvas-slice texture ──▶ [warp + edge-blend] ──▶ display
           draw each layer                            sample through warp mesh,
           via output.view_proj                       multiply by blend mask
```

## Build & run

```bash
# from the repo root (Cargo workspace)
cargo run -p lcmediaserver-engine --release            # loads engine/examples/show.json
cargo run -p lcmediaserver-engine --release -- my-show.json
```

The example show uses solid-color + placeholder sources, so it runs with **no
external assets**: two windows form one 1920×1080 canvas, a teal banner spans
the seam, an orange box is rotated 30°, and both outputs carry an 8% edge-blend.

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
  "playback": { "autoplay": true, "loop": true, "duration": 20.0 },

  "outputs": [
    {
      "id": "left",
      "canvas_rect": { "x": 0, "y": 0, "w": 960, "h": 1080 },   // slice of canvas shown
      "window": { "size": [960,1080], "position": [0,0],
                  "fullscreen": false, "monitor": 0 },           // physical window
      "rotation_degrees": 0,                                     // display orientation
      "warp": { "corners": [[0,0],[1,0],[1,1],[0,1]] },          // TL,TR,BR,BL (normalized)
      "blend": { "left": 0, "right": 0.08, "top": 0, "bottom": 0, "gamma": 2.2 }
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
| `video` | ⏳ Phase 2 | `{ "path": "..." }`; needs `--features video` (ffmpeg/libav). Placeholder otherwise. |
| `live`  | ⏳ Phase 3 | `{ "kind": "..." }` (capture/NDI). Placeholder otherwise. |

### Projection mapping

- **Warp**: move an output's four corners (normalized output space, TL/TR/BR/BL)
  to keystone/soft-warp the image onto an off-axis surface. Interpolated across
  a 32×32 mesh. Omit `corners` for identity. (Full freeform mesh warp is Phase 3.)
- **Edge blend**: set `left`/`right`/`top`/`bottom` to the overlap fraction with
  a neighboring projector; `gamma` shapes the falloff (2.2 is a good start).
  Give adjacent outputs mirrored blends on their shared edge.

## Roadmap

- **Phase 1 (now):** canvas/output/layer model, JSON show, master clock + transport,
  scene compositor with full affine transforms, image + color content,
  per-output corner-warp + edge-blend + orientation, multi-output spanning.
- **Phase 2:** video decode via `ffmpeg-next` (`--features video`) with
  clock-synced frames; audio out via `cpal` (`--features audio`).
- **Phase 3:** live inputs (NDI/capture), freeform mesh-warp editor, a timeline/
  cue editor and operator UI, and a network time source for multi-machine shows.

## License

MIT
