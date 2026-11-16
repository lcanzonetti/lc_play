# Playback and frame-sync plan

Status, November 2026: **P0–P2 implemented** (see ARCHITECTURE §3.3) and
awaiting their first run on frame-locked hardware; P3–P6 not started.
Differences from the plan below: the shared timebase is a refresh grid
published by the Director (exact integer instants) rather than NVIDIA's frame
counter, which is reported for diagnosis only — the grid works without
NVAPI and on any frame-locked hardware; and the present-wait / Vulkan timing
extensions are not used (Windows uses DXGI statistics; elsewhere the render
loop's timing). The rest of this document is the plan as written. Claims marked
**[Uncertain]** come from documentation or inference, not from measurements on
our hardware; each says what would settle it.

## 1. Summary

Two problems, one pipeline:

1. **Throughput.** Large canvases (10K × 2K) and many layers need codecs whose
   decode cost is small and predictable. HAP is the backbone, and should stay
   so. H.264/H.265 and ProRes are useful as convenience formats, but only with
   decode on the GPU and frames that never leave it.
2. **Content one frame apart on outputs that are frame-locked.** Frame lock
   (Quadro Sync II) aligns *when* every output scans out. It does not decide
   *which content frame* each machine or GPU put in the buffer it flips. That is
   the application's job, and the usual causes of a one-frame offset are
   in the application:
   - the content time is taken at the moment rendering *starts* instead of at
     the vblank where the picture will *appear*;
   - the delay between those two differs between machines and GPUs
     (queue depth, load, compositor, cross-GPU copy);
   - that time is floored to a content frame right at a frame boundary, where a
     clock error far smaller than a frame flips the result;
   - a decoder that is late shows the previous frame.

   LC Play today has all four (§2). The fix is to make **the vblank the unit
   of time**: every output renders for a specific, numbered vblank, the content
   frame is a pure function of that number, and a present barrier ensures every
   output flips on that vblank or visibly reports that it did not.

The proposed order:
- First, measure (P0).
- Then fix the timing model, which needs no special hardware (P1).
- Then integrate frame lock (P2).
- Then the decoder work (P3–P6).

The decoder work rests on one structural decision: the Runner's primary backend
becomes **Vulkan**, and FFmpeg is linked as a library. Vulkan is the one API
that has all of the following:
- hardware H.264/H.265 decode with frames that stay on the GPU (Vulkan Video);
- FFmpeg's new compute-shader ProRes decoder;
- NVIDIA's present barrier;
- present timing.

DX12 stays as the fallback for HAP.

## 2. Why content can be one frame off, in LC Play specifically

| # | Cause | Where | Effect |
|---|---|---|---|
| 1 | The content time is read at the **start of rendering** (`self.playhead.position()`), not at the predicted display vblank. `position_at_local()` exists for that but is unused. | `lc-runner/src/app.rs` (`render_frame`) | The picture appears 1–2 refreshes later (`PresentMode::Fifo`, `desired_maximum_frame_latency: 2` in `render.rs`). How much later depends on load, so two machines with the same clock show different frames. |
| 2 | **Floor at the frame boundary.** `media_frame` floors `t × fps`. With content fps = refresh rate, the sampling instant sits at an arbitrary phase relative to the content frame grid; if it is near a boundary, a sync error of 0.1 ms is enough to pick the neighbour. | `video.rs` `media_frame`, `hap.rs` | Random one-frame disagreements that come and go as the clocks slew. |
| 3 | **One GPU renders every output** (one wgpu adapter, chosen for the first surface). | `render.rs` `request_adapter` | An output connected to another GPU is copied across by Windows (cross-adapter), which adds latency to that output only and keeps it out of any swap group. **[Uncertain]**: the size of that delay varies with the driver. A laptop's external ports wired to the integrated GPU are the same case. |
| 4 | **Late frames are shown** rather than held back or reported: correct for continuity, but one machine that decodes slower shows N−1. | `video.rs` (late-frame policy), HAP worker pool | A one-frame difference that tracks decode load. |
| 5 | **Presents are not in a swap group or barrier.** Frame lock aligns scanout, but a GPU that misses a vblank flips one refresh later, while the others flip on time. | everywhere | Occasional one-frame slips under load, even with frame lock. |
| 6 | **Windowed / composited outputs.** A borderless window that does not get "independent flip" goes through the compositor (DWM), which can add a refresh. | window setup | A constant offset on some outputs. |

Frame lock alone cannot fix any of these: it is about scanout timing, and all
six are about which picture is in the buffer.

## 3. Synchronisation design: render *for* a vblank

### 3.1 The model

For each output `o` the Runner keeps a **vblank clock**:

- `period_o`, measured, for example 16 683 333 ns at 59.94 Hz;
- an anchor pair `(vblank index, cluster time)`, from the swap chain's
  statistics:
  - DXGI: `GetFrameStatistics` → `SyncRefreshCount`, `SyncQPCTime`;
  - Vulkan: `VK_KHR_present_id` / `VK_KHR_present_wait`, or where available
    `VK_EXT_present_timing` / `VK_GOOGLE_display_timing`
    **[Uncertain]: availability of the timing extensions on NVIDIA Windows
    drivers; check `vulkaninfo` on the target machines.**

Each frame is rendered **for a target vblank** `k = last_presented + depth`,
where the pipeline depth is fixed (the same on every Runner, e.g. 2). The
content time is

```
display_time(k) = anchor_cluster + (k − anchor_index) × period
show_time       = transport.show_time_at(display_time(k))
```

which is exactly what `position_at_local()` was meant to receive.

### 3.2 Quantise to the vblank grid, sample mid-frame

To kill the boundary flip (cause 2), the content frame for vblank `k` is
chosen at the **centre** of the content frame grid, not at its edge:

```
content_frame = round(show_time × fps − 0.5 + ε)
```

- When content fps equals the refresh rate, every vblank maps to exactly one
  content frame. Machines whose clocks differ by up to ±½ refresh then agree,
  instead of agreeing only up to the sub-millisecond distance to a boundary.
- When content and refresh rates differ (e.g. 25p on 60 Hz), the mapping is
  still deterministic for a given `display_time`, so locked machines pick the
  same frame. The cadence is uneven, but uneven in the same way on every
  machine.

### 3.3 With frame lock: the frame counter is the clock

With Quadro Sync, NVIDIA provides a **frame counter shared by every system in
the swap group/barrier** ("universal frame counter", incrementing only while
frame lock is active). With it:

- the Director publishes the transport anchor as
  `(counter value, show time)` in addition to cluster time;
- each Runner computes `show_time(k)` from the **counter**, not from its clock;
- the network clock then only has to be right to within half a refresh to pick
  the right counter value, and it already is.

The result is that content is identical by construction on every frame-locked
output. This is how it should work on a synced rig; the cluster clock remains
the fallback for unlocked systems.

The barrier itself:

- **DX12:**
  - `NvAPI_D3D12_CreatePresentBarrierClient(device, swapchain)`;
  - `NvAPI_JoinPresentBarrier`;
  - `NvAPI_QueryPresentBarrierFrameStatistics` gives the sync state
    (not joined / local sync / sync across systems) plus present counts. We
    surface it in the Runners window.
  - Since driver R470; it only engages in true fullscreen: no borders, no
    desktop scaling, no taskbar composition.
- **Vulkan:** `VK_NV_present_barrier`, added to the swapchain create info.
- **wgpu does not expose either.** Joining the barrier needs the raw swap
  chain. **[Uncertain]**: whether wgpu-hal's raw handles are enough or the
  Runner needs its own swap-chain code per backend. Settle it with a spike on
  a machine that has a Quadro Sync card.

### 3.4 One device per GPU

The Runner opens **one wgpu device per adapter that drives an output** and
renders each output on its own GPU (cause 3):

- Content that is decoded on the CPU (HAP) is uploaded once per GPU. This
  costs PCIe bandwidth but no latency.
- Content decoded in hardware is decoded on each GPU that shows it.
- NVIDIA Mosaic, where the driver presents several GPUs as one display,
  remains an alternative that needs no per-GPU code.

### 3.5 Deadlines, not "latest frame"

Decoders work to a deadline: frame `n` must be ready by the time vblank `k` is
rendered, i.e. `depth` refreshes ahead.

- If it is late, the Runner still shows the previous frame. Continuity wins,
  because a black frame is worse.
- It **reports a late frame** for that output, with the vblank number. The
  Director shows the per-output count, so a one-frame slip is visible in the
  Runners window rather than discovered on the LED wall.

### 3.6 Measuring it (P0, before any fix)

- A built-in **sync test source** rendered by the Runner, not decoded:
  - the content frame number as large binary blocks plus digits;
  - a one-frame white flash every second.
- A camera at 240 fps, or a photodiode across two outputs, then shows the
  offset in frames directly.
- Per-output **present telemetry**:
  - target vblank vs actual vblank, from present statistics;
  - late-decode count;
  - barrier state.

  Reported in `Health`.

Acceptance:
- **Without frame lock:** zero frame disagreement between outputs over
  10 minutes on a wired LAN with content fps = refresh rate.
- **With frame lock and a barrier:** zero disagreement, with zero late presents
  reported.

## 4. Decoder design

### 4.1 Shape

```
demux (MOV/MP4/MXF) ─▶ decode stage ─▶ GPU frame on device(s) ─▶ frame queue keyed by frame index ─▶ compositor
                        │ per codec                                  (deadline = target vblank)
```

- **One decoder per (asset, in-point, rate) per machine**, shared by every layer
  and output that shows it, and by every GPU that needs it (§3.4).
- **Region of interest.** Only the part of a frame that falls on this Runner's
  outputs is decoded or uploaded, where the codec allows it (HAP: §4.2).
- **The scheduler knows the future.** The timeline is deterministic, so
  pre-roll, loops and cue starts are prefetched by deadline, as the HAP path
  already does.

### 4.2 HAP / HAP Alpha / HAP Q / HAP R (primary)

The design: the GPU samples the block-compressed (BC) texture directly; the
CPU only does Snappy, which is optional per chunk. The section header holds a
compressor byte and a size for each chunk, and FFmpeg's encoder allows up to
64 chunks.

Cost at 9976 × 2176, after Snappy:

| Variant | Size per frame | At 60 fps |
|---|---|---|
| BC1 (HAP) | ≈ 10.9 MB | ≈ 0.65 GB/s |
| BC3 (HAP Q, HAP Alpha) | ≈ 21.7 MB | ≈ 1.3 GB/s |
| BC7 (HAP R) | ≈ 21.7 MB | ≈ 1.3 GB/s |

The limits are therefore storage throughput, Snappy throughput (it runs per
chunk, so it scales with chunks × cores) and the PCIe upload.

Work:
1. **Chunked encoding on ingest.** The Asset Manager re-encodes to HAP with
   `-chunks 16..64`. Today a single-chunk file is decompressed by one thread.
2. **Persistent thread pool** for chunk decompression, instead of scoped
   threads per frame.
3. **Upload ring.** Persistently mapped staging buffers, copied on a transfer
   queue; the render pass waits on the copy only if it is due.
4. **Region-of-interest upload.** A chunk is a contiguous byte range of the
   texture, so for a row-ordered BC layout it is a band of block rows
   **[Uncertain: confirm that every encoder splits on row boundaries; the
   draft spec in Vidvox's `hap` repository is the reference]**. A Runner whose
   outputs cover only part of the canvas decompresses and uploads only the
   bands it shows.
5. **HAP R (BC7)** is decoded the same way. Check BC7 support on the adapter;
   it is universal on desktop DX12/Vulkan GPUs.
6. **Storage.** Show files are read with large sequential reads, unbuffered
   (`FILE_FLAG_NO_BUFFERING` / `O_DIRECT`), and loops short enough are held in
   RAM. Recommend NVMe; RAID-0 for multi-stream 10K.

### 4.3 H.264 / H.265 (convenience, hardware-decoded)

- **Path:** link FFmpeg as a library (`rsmpeg`/`ffmpeg-next`, LGPL build,
  decoders only, no GPL components), `hwaccel vulkan` on the wgpu Vulkan
  device. Decoded NV12/P010 images are sampled as two plane views by our own
  shader (wgpu cannot express a `VkSamplerYcbcrConversion`). The frame never
  leaves the GPU.
- **Prior art:**
  - `ffgpu` lists Vulkan Video decode on a wgpu-created device as
    *experimental*;
  - `wgpu-external-frame` imports D3D12 shared handles and DMA-BUF.

  Neither is production-proven. Plan for our own interop layer through
  `wgpu-hal`.
- **Limits:**
  - NVDEC resolution caps, typically 4096 × 4096 for H.264 and 8192 × 8192 for
    HEVC on recent generations;
  - session and throughput limits per GPU generation.

  **[Uncertain]: check NVIDIA's NVDEC support matrix for the exact GPUs.**
  The Asset Manager should warn at ingest, and above the caps transcode to
  HAP.
- **Long-GOP seeking.** A long-GOP file cannot seek instantly. Cue starts are
  pre-rolled from the previous keyframe, which the scheduler can do because it
  knows them in advance.
- **DX12 fallback:** the current subprocess path (hardware decode, then a copy
  back to the CPU). Kept as is, for machines without a Vulkan driver.

### 4.4 ProRes

- **FFmpeg 8.1 (March 2026)** ships a **Vulkan compute-shader ProRes decoder**,
  which came out of a 2025 GSoC project. FFmpeg 8.0 had ProRes RAW only. It
  runs on any Vulkan 1.3 implementation, so it is not tied to a vendor's video
  engine.
- The same library integration as §4.3, so frames stay on the GPU.
- **Fallback:** FFmpeg's CPU decoder (slice-threaded), plus an upload of
  10-bit 4:2:2. That costs about 4× the bytes of BC1 per pixel; fine for HD,
  not for 10K.
- **[Uncertain]: throughput of the compute decoder at 4K+ on an RTX-class
  GPU.** Benchmark it in P5 before recommending ProRes for large canvases.

### 4.5 NotchLC

- **What NotchLC is:**
  - designed for GPU decode;
  - royalty-free for playback;
  - Notch's SDK is commercial, with DirectX, Metal and OpenGL
    implementations (no Vulkan listed);
  - max 16384 px per side.
- **FFmpeg** has a native CPU decoder (`libavcodec/notchlc.c`). From its
  source, the payload is LZF-compressed followed by a block-coded
  luma/chroma/alpha stage.
- **Options:**
  1. **License the SDK.** This is the supported route. It needs a DX12 or GL
     interop with our renderer.
     **[Uncertain]: which DirectX version the SDK targets, and whether it can
     share a device with wgpu.**
  2. **Port the decoder to a compute shader**: LZF on the CPU (it is serial
     per frame unless the encoder splits it), the block stage on the GPU.
     Technically feasible from FFmpeg's LGPL code, but it is a reimplementation
     of a proprietary format. **Ask Notch before shipping it.**
  3. **CPU decode via FFmpeg:** works today through the subprocess path, but
     is too slow for large canvases **[Uncertain, not measured]**.
- **Recommendation:** 3 now (it is free), with a decision between 1 and 2 when
  a client actually needs NotchLC at scale.

### 4.6 Backend choice

| Need | Vulkan | DX12 |
|---|---|---|
| HAP (BC textures) | ✓ | ✓ |
| H.264/H.265 zero-copy | Vulkan Video via FFmpeg | D3D12 Video / D3D11VA via FFmpeg, then shared-handle import (more interop) |
| ProRes on the GPU | FFmpeg 8.1 compute | — |
| Present barrier | `VK_NV_present_barrier` | NvAPI present barrier |
| Present timing | present_id / present_wait (+ timing extensions where present) | DXGI frame statistics |

**Decision:** Vulkan primary on NVIDIA and AMD (Windows and Linux), DX12
fallback. `LCPLAY_BACKEND` already exists.

**[Uncertain]: windowed/borderless-fullscreen behaviour of Vulkan swap chains
on Windows** (whether they get independent flip as DXGI flip-model does).
Check in P1 with the present telemetry; it decides whether outputs must use
exclusive fullscreen.

## 5. Phases

| Phase | Work | Done when |
|---|---|---|
| **P0 Measure** | Sync test source; per-output present telemetry and late-frame counts in Health. | Offsets are visible in numbers and on camera. |
| **P1 Timing model** | Render for a target vblank (predicted display time); fixed pipeline depth; mid-frame quantisation; one device per GPU; fullscreen with independent flip. | Two unlocked Runners on a LAN, and two GPUs in one machine: zero frame disagreement in 10 min at content fps = refresh. |
| **P2 Frame lock** | Present barrier (DX12 NvAPI / Vulkan `VK_NV_present_barrier`); frame-counter timebase from the Director; barrier state in the Runners window. | On a Quadro Sync rig: zero disagreement and zero late presents across machines and GPUs. Needs that hardware. |
| **P3 HAP** | Chunked ingest; persistent pool; upload ring; ROI bands; HAP R. | 10K × 2K HAP Q at 60 fps on one RTX-class GPU with headroom, measured. |
| **P4 Vulkan + FFmpeg library** | Vulkan as primary backend; FFmpeg linked; Vulkan Video H.264/H.265 zero-copy; Asset Manager warns about NVDEC limits. | 2× 4K H.265 60p with no CPU copy (GPU memory traffic only). |
| **P5 ProRes** | FFmpeg 8.1 Vulkan ProRes; CPU fallback. | Benchmark published per resolution. |
| **P6 NotchLC** | Decide SDK vs port; implement. | — |

P1 is the one that directly addresses the one-frame offset, and it needs no
special hardware: it can be developed and verified on two ordinary PCs.

## 6. Open questions needing hardware

- Does wgpu give enough access to the swap chain for the present barrier, or
  does the Runner need its own swap chain per backend?
- Vulkan on Windows: independent flip, present-wait accuracy, barrier
  behaviour with several outputs on one GPU.
- RTX 3050 Ti laptop: which outputs are wired to the integrated GPU (cause 3).
  A laptop cannot carry a Quadro Sync card, so P2 needs a workstation with an
  RTX A-series (or Ada/Blackwell pro) GPU and a sync card.

## 7. Sources

- NVIDIA, *Synchronizing Present Calls Between Applications on Distributed Systems with DirectX 12* — https://developer.nvidia.com/blog/synchronizing-present-calls-between-applications-on-distributed-systems-with-directx-12
- NVIDIA, `dx12_present_barrier` sample — https://github.com/nvpro-samples/dx12_present_barrier
- NVAPI reference (present barrier functions, since R470) — https://docs.nvidia.com/nvapi/nvapi_8h.html
- Khronos, `VK_NV_present_barrier` (rev. 1, 2022-07-20) — https://registry.khronos.org/vulkan/specs/1.3-extensions/man/html/VK_NV_present_barrier.html
- NVIDIA, Quadro Sync II User Guide — https://images.nvidia.com/content/quadro/product-literature/user-guides/Quadro-Sync-II-User-Guide-v07.pdf
- NVIDIA, Frame Lock and Genlock (frame counter semantics) — https://download.nvidia.com/XFree86/Linux-x86_64/304.137/README/framelock.html
- Epic, nDisplay synchronization — https://dev.epicgames.com/documentation/en-us/unreal-engine/synchronization-in-ndisplay-in-unreal-engine
- Microsoft, `DXGI_FRAME_STATISTICS` and flip model — https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ns-dxgi-dxgi_frame_statistics , https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model
- FFmpeg 8.0 announcement (Aug 2025) — https://ffmpeg.org/pipermail/ffmpeg-devel/2025-August/347919.html ; FFmpeg news (8.1, Mar 2026) — https://ffmpeg.org/index.html
- Vulkan ProRes decoder, GSoC 2025 report — https://averne.github.io/posts/250831_gsoc-25-report/report.html
- Notch, NotchLC — https://notchlc.notch.one/ ; manual — https://manual.notch.one/2026.1/en/docs/reference/notchlc/
- FFmpeg `notchlc.c` — https://www.ffmpeg.org/doxygen/trunk/notchlc_8c_source.html
- FFmpeg `hapenc.c` — https://ffmpeg.org/doxygen/7.1/hapenc_8c_source.html ; Derivative, Hap — https://derivative.ca/UserGuide/Hap
- `ffgpu` — https://github.com/SymphonyVR/ffgpu ; `wgpu-external-frame` — https://github.com/water-rs/wgpu-external-frame
