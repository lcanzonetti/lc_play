# LC Play: multi-GPU feasibility assessment and implementation plan

Repository `lcanzonetti/lc_play` at `2535b5b`, studied 2026-11-19 without changing any code.
Scope: Windows 11 hosts with 2 or more NVIDIA professional GPUs, no SLI or NVLink.
Method: a Graphify code graph and source reading, five specialist agents, and spot-checks by the lead of every claim used below.

---

## 0. Summary

**The current design does not match the brief.** LC Play has **no CUDA, NVENC, NVDEC-SDK or NVML code**:

- The Runner uses the GPU in two ways: **wgpu/D3D12 rendering**, and **hardware video decode by an `ffmpeg` child process**.
- The Asset Manager encodes **on the CPU only** (`hap`, `libx264`, `libvpx-vp9`, `h264_mf`, `libopenh264`).
- NVENC session limits therefore do not affect the observed symptom.
- The "GPU-heavy" work that can pile up on one card is **video decode**, plus whatever the render thread fails to feed to the other cards.

**Main cause, high confidence, confirmed in code:**

- Every video decoder is started with `-hwaccel auto` and **no device** (`crates/lc-runner/src/video.rs:270-274`).
- On Windows, FFmpeg's auto mode tries the decoder's hardware types in this order: DXVA2 → D3D11VA → D3D12VA → NVDEC… It takes the **first device it can open, on the default adapter**, which is the card driving the primary desktop display.
- The Runner already creates **one wgpu device per output card** (`app.rs:625-671`). But each card's copy of a layer starts its own ffmpeg, and **all of them decode on the same default card** (`app.rs:684-712`).
- A layer that spans two cards is **decoded twice, on that same card**.
- The docs say the opposite: `docs/PLAYBACK_PLAN.md:152` says "decoded on each GPU that shows it". The code does not do that.

**Feasible?** Yes. The cheapest route (Route 1: bind each decoder to its render card's DXGI adapter) is a small change in `video.rs` and `dxgi.rs`. It needs no driver or Windows configuration change and spreads NVDEC load in proportion to the outputs on each card.

The real throughput ceiling after that is the **CPU/PCIe round trip** of every decoded frame:
- 4K60 4:2:0 is about 746 MB/s down plus 746 MB/s up per stream, plus 4–5 memcpy passes.
- Removing it needs zero-copy decode (Route 3), which depends on Route 1.

**Important caveat:** `docs/ARCHITECTURE.md` says Windows has been type-checked but **not run** ("Only Linux has been run"), and the D3D12/NVAPI path has "only been compiled, not run". The per-card render path in this report has never been validated on hardware. Check the Runner's logs from your test before assuming which path actually ran (see §5, step 0).

---

## 1. Root-cause diagnosis (ranked)

| # | Cause | Evidence | Confidence |
|---|---|---|---|
| **1** | **Decode is not tied to the render card.** `-hwaccel auto` with no `-hwaccel_device` or `-init_hw_device`. FFmpeg ≥ 6.1 tries the decoder's `hw_configs` in order: H.264, HEVC and AV1 list DXVA2 first. DXVA2 opens `D3DADAPTER_DEFAULT` and D3D11VA opens `D3D11CreateDevice(NULL)`; both are the primary-display adapter. Frames come back to system RAM (no `-hwaccel_output_format`), go through a rawvideo pipe, and are uploaded to *whichever* card renders them (`video.rs:280`, `video.rs:481`). | `video.rs:270-274`; FFmpeg `fftools/ffmpeg_dec.c` (auto loop) and `libavcodec/h264dec.c` `hw_configs`, tags n6.1–n8.1.3 on GitHub. Agents read them; the lead did not re-read FFmpeg. | **High** (code). The order comes from the FFmpeg source; your ffmpeg build may lack DXVA2, which would move the choice to D3D11VA, still on the default adapter. |
| **2** | **Every card decodes its own copy.** `rebuild_contents_on(g)` builds a `Content` for every layer on every card (`app.rs:684-712`). A layer on outputs of both cards gets two ffmpeg processes (cause 1 puts both on card 0) or two HAP CPU worker pools. Images are decoded and uploaded on every card, even ones that never show them (`content.rs:338-352`). | `app.rs:124-129, 684-712, 1015-1025`; the gap is admitted in `ARCHITECTURE.md` "decoding shared between cards". | **High** |
| **3** | **Silent fallbacks to `gpus[0]` for rendering.** `gpu_for` returns 0 whenever `dedicated_adapter` returns `None`: backend not DX12 (`LCPLAY_BACKEND=vulkan`, where `luid()` is always None), an integrated GPU, or a LUID not found (`app.rs:630-651`, `render.rs:1366-1379`). A fullscreen display with no monitor index, or an index out of range, opens on the **primary monitor** (`app.rs:1191-1206`). A window moved to a card with no device yet stays on its old card (`app.rs:533-536`). Any of these puts every output on one card, and Windows copies the picture across. | Code. Whether it happened in *your* test shows in the log lines `output opened … gpu=` and `GPU: …` (`app.rs:599, 654`). | **Medium**: depends on configuration. |
| **4** | **One thread drives every card in turn.** `render_frame` loops `for g in 0..gpus.len()` on the winit thread (`app.rs:763-767`). Card 1's uploads, encoding and submit wait until card 0 has waited for its frame (up to 100 ms on the DXGI path, `render.rs:756-771`) and done its CPU uploads. Card 1 is under-fed by the CPU, not lacking work. | Code. | **Medium**: adds to the imbalance. It does not leave card 1 completely idle on its own. |
| **5** | **The bottleneck is CPU/PCIe; the GPUs are mostly not the limit.** Per 4K60 yuv420p stream: 12.4 MB per frame, ≈ 746 MB/s each way, about 4–5 memcpy passes on the CPU, and `write_texture` on the render thread. HAP Snappy decompression is on the CPU, repeated per card, with threads sized globally (2 workers × cores/2 per instance, `hap.rs:343-349`). Drawing textured quads costs little, so a card's "3D" engine stays low even when it is doing its full share. | `video.rs:241-305, 477-513`; `hap.rs:29, 343-349`. The DRAM figure is an estimate, not a measurement. | **Medium-high** as an explanation for "the other GPU looks idle". |
| **6** | **What gets measured.** `nvidia-smi` "GPU-Util" and Task Manager's default graph show 3D/compute. NVDEC load appears only as `dec %` or the "Video Decode" engine. A card can look "idle" while doing its share of rendering, and the "busy" card may be busy only in Video Decode. | Platform behaviour. | **Medium**: check it before and after any fix. |
| 7 | Cross-adapter presentation (only if cause 3 happens). Card A renders, Windows copies the frame into card B's display path. Card A shows 3D + Copy, card B shows only light DWM work. The output also loses independent flip and the present barrier. | [MS Learn, Cross-adapter resources, updated 2025-03-25](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/rendering-on-a-discrete-gpu-using-cross-adapter-resources). That page covers iGPU + dGPU; dGPU + dGPU is **[Uncertain]**. | Low–medium |

**Not causes:**
- **NVENC session limits:** nothing in the repo uses NVENC.
- **NVIDIA Container Toolkit or WSL device exposure:** the Runner runs natively. Only `lc-assets` has a Dockerfile, and it uses no GPU.
- **HAGS, TDR, MPO, ECC:** they change scheduling, not which card gets the work.
- **The "Multi-GPU acceleration" control-panel setting:** it does not apply to independent decode sessions.

**Linux, for comparison.** `gpu_for` never creates a second device off Windows (`own_card = false`, `app.rs:637`). Every output renders on `gpus[0]` **by design**, and decode goes to the default VAAPI/CUDA device. On Linux the symptom is expected, not a bug.

---

## 2. Current architecture: where the device gets chosen

```
                         ┌───────────────────── lc-runner process (one per machine today) ─────────────────────┐
 show (Director) ──WS──► │ stage.rs  filter displays by runner name                                             │
                         │   │                                                                                  │
                         │   ▼                                                                                  │
                         │ app.rs open_window ──► fullscreen_target()  ⚠ P1: monitor idx missing → PRIMARY     │
                         │   │                     (available_monitors().nth(i) | primary_monitor())           │
                         │   ▼                                                                                  │
                         │ gpu_for(window) ──► monitor_adapter() → dxgi::adapter_of_monitor (EnumAdapters1 LUID)│
                         │   │   ⚠ P2: not DX12 / iGPU / LUID None → return 0                                  │
                         │   │   ⚠ P3: moved window → only cards that already have a device                    │
                         │   ▼                                                                                  │
                         │ Gpu::new(luid) → dedicated_adapter(luid) │ else new_any(): request_adapter(HighPerf) │
                         │   │                                         ⚠ P4: 2 dGPUs → EnumAdapters1 order → #0 │
                         │   ▼                                                                                  │
                         │ gpus[g] (wgpu Device+Queue per card) ── contents[g] (EVERY layer, per card)  ⚠ P5    │
                         │                                              │                                       │
                         │        ┌─────────────────────────────────────┼──────────────────────┐               │
                         │        ▼                                     ▼                      ▼               │
                         │  VideoContent (ffmpeg child)          HapContent (CPU workers)   Image/Virtual      │
                         │   ffmpeg -hwaccel auto  ⚠ P6           snappy on CPU, per card    per card          │
                         │   (no device → DXVA2/D3D11 adapter 0)  ⚠ P7 duplicated per card                    │
                         │        │ NVDEC on PRIMARY-DISPLAY GPU                                               │
                         │        ▼ hwdownload → CPU scale/format → rawvideo pipe → Vec                        │
                         │   queue.write_texture on gpus[g]  (PCIe up)                                         │
                         │                                                                                      │
                         │ render_frame: for g in gpus { acquire(wait vblank) → update → render → present }    │
                         │                        ⚠ P8 one thread, cards one after another                     │
                         └──────────────────────────────────────────────────────────────────────────────────────┘

 lc-assets (separate service, Docker or native): CPU only. hap / libx264 / vp9 / h264_mf. Transcode Semaphore = CPU count,
 proxies = 2, optimiser = 1 job at a time. No GPU device selection anywhere.
```

**Points where a "primary GPU" assumption can come in:**
- P1: `app.rs:1191-1206`
- P2: `app.rs:630-651`, `render.rs:1366-1379`
- P3: `app.rs:533-536`
- P4: `render.rs:160-180`, which with wgpu-hal 24.0.4 uses `EnumAdapters1` order, adapter 0 being the primary desktop output ([MS Learn EnumAdapters1, updated 2024-02-22](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgifactory1-enumadapters1))
- P5: `app.rs:684-712`
- **P6: `video.rs:270-274`**
- P7: `hap.rs:330-367`
- P8: `app.rs:763-767`

**Device identity:**
- Cards are matched by **LUID** within one run only. LUIDs are valid "until the system is next restarted" ([MS Learn AllocateLocallyUniqueId, updated 2025-07-01](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-allocatelocallyuniqueid)).
- Nothing persists an adapter. Show files store only `WindowSpec.monitor: Option<usize>`, an index into winit's monitor enumeration (`lc-proto/src/show.rs:256`). Which card an output lands on therefore depends on monitor enumeration order.
- The protocol carries GPU **names** only (`RunnerHealth.gpu`, `OutputHealth.gpu`). There is no LUID or PCI bus ID, which makes two identical cards indistinguishable.

---

## 3. Adaptation routes (ranked by effort and risk)

### Route 1: bind each decoder to its render card (low effort, low risk). **Do this first.**

**Changes:**
1. `dxgi.rs`: `fn dxgi_index_of_luid(luid) -> Option<u32>`, by walking `CreateDXGIFactory1` → `EnumAdapters1` (the same API ffmpeg's d3d11va device uses).
2. `TextureFactory` / `Gpu`: expose the card's LUID (it is already available as `Gpu::luid()`, `render.rs:408`). Pass it through `VideoContent::new` (`video.rs:377`) to `Decoder::start` (`video.rs:230`).
3. ffmpeg arguments on Windows, when the card is known:
   `-init_hw_device d3d11va=dec:<idx> -hwaccel d3d11va -hwaccel_device dec`
   (or the shorter `-hwaccel d3d11va -hwaccel_device <idx>`; ffmpeg's d3d11va device reads the string as an adapter index).
   - **Do not** use `-hwaccel auto -hwaccel_device N`: auto passes the same N to DXVA2 (D3D9 ordinal), D3D11 (DXGI index) and CUDA (CUDA ordinal), which number cards differently.
   - **Avoid `cuda`**: its ordinal follows `CUDA_DEVICE_ORDER` (default FASTEST_FIRST), not DXGI order.
   - Keep `LCPLAY_HWACCEL=none`, and add `LCPLAY_HWACCEL=auto` to restore today's behaviour.
4. Move the argument building into a pure function (`decoder_args(info, layout, hw: Option<HwTarget>) -> Vec<String>`) so it can be unit-tested.
5. Raise ffmpeg logging for the first second, or run one `-v verbose` probe at startup. Log the device ffmpeg reports opening, so a mismatch is visible.
6. Optional in the same route: **share one decoder between cards for a spanning layer.** Frames already come through CPU memory, so one ffmpeg can feed N `write_texture` calls: 1 decode, 1 download, N uploads, instead of N of each. Place the decoder on the card showing the larger part of the layer. Touch points: the content cache in `app.rs:684-712, 947-1023`.

**Driver and Windows settings:**
- No new driver branch is needed: DXVA/D3D11VA decode is long-standing.
- Make sure **no per-app GPU preference** is set for `ffmpeg.exe` or `lc-runner.exe` (Settings → System → Display → Graphics). **[Uncertain]** whether such a preference reorders `EnumAdapters1` for the process; Microsoft does not document it. Check it with step 3 of §5.
- Use the Windows ffmpeg build you will ship and check `ffmpeg -hwaccels` lists `d3d11va`.

**Expected impact:**
- NVDEC load on each card proportional to the video layers shown on it.
- Spanning layers no longer double the load on one card (with item 6, they halve it).
- 3D load on each card does not change, and should not.

**Risks and failure modes:**
- The child process enumerates adapters in a different order than the parent: unlikely within one boot, but a GPU preference or hot-plug could cause it. Mitigation: check the device reported by ffmpeg at `-v verbose`, and the GPU-engine attribution per process (§5).
- **Two identical cards:** vendor/device ID and name do not tell them apart, so the index is the only selector and verification must use LUID (PDH counters).
- Codec or profile not supported by D3D11VA on that card (VP9 profile 2, AV1 on pre-Ampere cards, 4:2:2 HEVC before Blackwell): ffmpeg falls back to **software** silently. Detect it by the missing `engtype_VideoDecode` activity, or by parsing ffmpeg's warning.
- D3D11VA vs today's DXVA2: the output is NV12/P010 → CPU `format=yuv420p`. Same cost as today. Check 10-bit HEVC.

### Route 2: render threads per card, decoder placement and per-GPU telemetry (medium effort, medium risk)

**Changes:**
- **A render/submit thread per `Gpu`.** wgpu `Device`/`Queue` are `Send + Sync`, and D3D12 swap chains can be presented from a worker thread. The winit window stays on the main thread. Each card's `acquire → update → render → present` then runs in parallel, so card 1 no longer waits on card 0's vblank wait and uploads (cause 4).
  - Main work: `contents[g]` must be owned by that card's thread. Check that `Box<dyn Content>` is `Send` **[not verified]**.
  - The shared `playhead`/`timing`/`onsets` state needs sharing or a message channel.
  - The "first sampled position" logic in `render_frame` (`app.rs:762-767`) has to be done per card.
- **Decoder placement policy (pure, testable):**
  - Default to affinity (decode where the layer is displayed).
  - For spanning layers, a single shared decoder on the least-loaded card.
  - Hysteresis: re-place only at a cue start, seek or loop; never while on air; minimum pinning time about 10 s.
  - Admission: when a card's late-frame rate goes over a threshold, put new decoders elsewhere, or fall back to software or a lower decode size (the `scale` filter already exists).
- **Telemetry:** `Health.gpu_load: Vec<GpuLoad>` keyed by LUID, sampled about once per second:
  - from LC Play: active decoders, decoded and late fps, upload MB/s;
  - from NVML: decoder and encoder utilisation, PCIe RX/TX;
  - from PDH: `\GPU Engine(*)\Utilization Percentage`.
  - Show it per card in the Producer. This also closes the "GPU load and temperature" item in `ARCHITECTURE.md`.
- **lc-assets (only if GPU encoding is wanted):**
  - Use `h264_nvenc`/`hevc_nvenc` with an explicit `-gpu N` for `/transcode` and proxies, plus NVDEC decode of sources.
  - Assign jobs to cards round-robin or least-loaded, with a semaphore per card.
  - Return 503 with Retry-After instead of an unbounded wait on the transcode semaphore (`transcode.rs:67-72`).
  - HAP encoding stays on the CPU: NVENC cannot produce HAP.

**Driver:**
- NVML ships with the driver (`nvml.dll`). Load it at run time and degrade gracefully without it.
- Professional cards (RTX Ada / RTX PRO) have **no NVENC session cap**; low-end parts such as RTX A400/A1000 are reported at 8 **[Uncertain]: secondary source; NVIDIA's support matrix could not be fetched from this session**.
- NVDEC has no fixed session cap: engine throughput and memory limit it ([NVDEC Application Note 13.1](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvdec-application-note/index.html), via search excerpt).

**Expected impact:** balanced CPU feeding of both cards, measurable per-card load, and automatic relief when one card's NVDEC is saturated.

**Risks:**
- Threading changes touch the sync path. The `vsync.rs`/`frametime.rs` timing assumes one loop.
- Regressions in frame pacing are possible. Gate the change behind a flag and validate with the `sync_test` cue.

### Route 3: zero-copy hardware decode per card (high effort, medium risk; the real throughput fix)

**Changes:**
- Decode into GPU memory **on the render card** and import it without a CPU round trip. Two options:
  - **(a)** link FFmpeg as a library: `AV_HWDEVICE_TYPE_D3D11VA` on the card's adapter, then share the D3D11 texture as an NT handle and open it in D3D12 (wgpu-hal `as_hal`). A YUV→RGB shader is already in place.
  - **(b)** D3D12 Video Decode, or Vulkan Video through wgpu-hal, as `PLAYBACK_PLAN.md` phase P4 suggests.
- **Requires Route 1's affinity**: shared handles only work on the same adapter.
- Spanning layers then need either per-card decode (N NVDEC sessions, no PCIe round trip) or a decode followed by a cross-adapter copy. For two discrete cards, per-card decode is usually cheaper.

**Expected impact:**
- Removes about 1.5 GB/s of PCIe and 6–8 GB/s of DRAM traffic per 4K60 stream (estimate).
- Decode and render of a layer stay on one card.
- Utilisation becomes limited by NVDEC engines: 2 per card on RTX 4000/5000 Ada, 4 on RTX PRO 6000 Blackwell, per NVIDIA datasheets.

**Risks:**
- FFmpeg linking means LGPL compliance and build complexity.
- Shared-handle and format interop (NV12/P010 as D3D12 planar formats; wgpu has limited multi-planar support) and keyed-mutex synchronisation.
- A larger testing surface on hardware that has never run the D3D12 path.

### Route 4: one Runner process per GPU (medium effort, highest risk)

**This mostly works by configuration today:**
- Run two Runners with different `--name` values.
- Assign each card's displays to the matching Runner name.
- State folders, node IDs and ports are already separate per name (`folders.rs:28-30`, `main.rs:296-298`).
- Add a `--gpu <LUID|PCI bus|name>` guard so `gpu_for` and window placement refuse other cards.

**What breaks or needs work:**
- `lc-play` launches only one Runner (`lc-play/src/main.rs:144-151`).
- Autostart is one machine-wide entry (`machine.rs:252-276`).
- The shared asset index is guarded by an in-process Mutex only, so concurrent processes can lose entries (`assets.rs:342-358`).
- Power actions on one Runner take down its siblings (`lc-director/src/lib.rs:984-991`).
- Spanning layers can never share a decode.
- **A cross-process NVAPI present barrier on one machine is unverified.**

**Gain:** fault isolation (a TDR on one card does not stop the other) and natural CPU/thread separation.

**Use it when:** fault isolation is a requirement. It is not the first fix for the imbalance: by itself it does not change which adapter ffmpeg decodes on, so Route 1 is still needed.

### Not a fix, but available: NVIDIA Mosaic

- Presents several cards as one logical display ([NVIDIA Control Panel help](https://www.nvidia.com/content/control-panel-help/vlatest/en-us/mergedprojects/nvwks/SLI_Mosaic_Mode.htm)).
- Simplifies outputs and sync, but adds no decode throughput: the app sees one adapter.
- Reports say each card repeats the whole render **[Uncertain]**.

---

## 4. What each specialist agent found

**Codebase archaeologist**
- Full device-selection map (§2).
- Content is built per card for every layer.
- The HAP-or-ffmpeg choice is made **per card** (`hap.rs:771`, from `supports_bc`/`max_dimension`). One card can play HAP on the GPU while another falls back to ffmpeg.
- Virtual-display textures are allocated on every card (`app.rs:722-738`).
- `Gpu::new` hardcodes `Bgra8UnormSrgb` on the dedicated path (`render.rs:152`).
- Outputs on one card share the first output's clock sample (`app.rs:855-859`).

**NVDEC/NVENC/CUDA specialist**
- The FFmpeg auto order (DXVA2 first) and default device for each type: DXVA2 `D3DADAPTER_DEFAULT`, D3D11 `D3D11CreateDevice(NULL)`, D3D12 `EnumAdapters(0)`, CUDA `cuDeviceGet(0)`, Vulkan physical device 0.
- No FFmpeg device type selects by LUID or PCI bus ID.
- CUDA ordinals relate to DXGI only by matching `cudaDeviceProp.luid` with the adapter's LUID.
- The "NVENC on target GPU, CUDA on GPU0" problem (`hwupload_cuda` defaults to `device=0`; `nvenc` with `-gpu any`) **does not apply here**: there are no CUDA filters, and frames are downloaded by the decoding device itself.
- Copy-path cost: about 746 MB/s each way per 4K60 stream.
- Engine counts in the table below.
- `ffmpeg.org` and `docs.nvidia.com` were blocked from this session. FFmpeg facts were read from the GitHub source; NVIDIA facts come from search excerpts and datasheets.

**Windows platform engineer**
- Confirmed cause 1.
- wgpu-hal 24.0.4 enumerates with `EnumAdapters1`, never `EnumAdapterByGpuPreference`. With two discrete cards, `request_adapter(HighPerformance)` therefore picks adapter 0.
- Cross-adapter presentation makes the display card look idle (cause 7).
- The serial render loop (cause 4).
- HAGS, TDR, MPO and ECC are not causes.
- Proposed checking the swap chain's containing output against its card (`dxgi.rs:146`), so a cross-adapter present is logged.

**Distributed systems architect**
- Two Runners on one machine already work (separate state, ephemeral UDP, `node_id`-keyed Director).
- The Director broadcasts the whole show and each Runner filters it, so there is no GPU field to schedule on.
- `lc-assets` optimise is strictly sequential, and `/transcode` waits with no limit on the semaphore.
- Multiple `lc-assets` instances on one media folder would race on `.lcplay/optimized.json`.
- Ranked the routes as 1 → 2 → 4.

**Performance and load-balancing engineer**
- Existing telemetry: per-output `gpu` name, `presents`, `missed`, `late_decodes`, `latency_frames` (`lc-proto/src/vblank.rs:193-233`); frame-phase timings (`frametime.rs`).
- Missing: GPU identity (LUID/PCI), decoders per card, NVDEC/Copy/PCIe use, upload MB/s.
- Designed `GpuLoad`, the placement policy with hysteresis, and the validation matrix in §5.

**NVDEC/NVENC engines on target cards.** Verify against NVIDIA's *Video Encode and Decode GPU Support Matrix* before sizing.

| GPU | NVENC | NVDEC | Source quality |
|---|---|---|---|
| RTX A4000 | 1 | 1 | third-party **[Uncertain]** |
| RTX A5000 / A6000 | 1 | 2 | third-party **[Uncertain]** |
| RTX 4000 / 5000 Ada | 2 | 2 | NVIDIA datasheet |
| RTX 6000 Ada | 3 | 3 | NVIDIA docs excerpt **[Uncertain]** |
| RTX PRO 6000 Blackwell | 4 | 4 | [NVIDIA datasheet](https://www.nvidia.com/content/dam/en-zz/Solutions/products/workstations/professional-desktop-gpus/rtx-pro-6000-max-q/workstation-datasheet-blackwell-rtx-pro-6000-max-q-nvidia-us-5349650-web.pdf) |

**Driver minimums.** These are lower bounds; use the current production branch.

| Need | Minimum | Source |
|---|---|---|
| Blackwell workstation cards | R570 | NVIDIA Omniverse requirements |
| NVAPI present barrier | R470 | the repo's own claim (`nvapi.rs:12`), not checked against release notes **[Uncertain]** |
| AV1 NVDEC | Ampere GA10x or newer | — |

---

## 5. Validation plan

### Step 0: diagnose the current build (no code changes)

1. **Runner log:** confirm one `GPU: <name>` line per card, and an `output opened … gpu=<name>` line per output naming the card its monitor is on.
   - If every output names one card, cause 3 applies. Check the `monitor` indices in the show, `LCPLAY_BACKEND`, and the warning "monitor N not found … using the primary monitor".
2. **Task Manager → Details:** add the **"GPU"** and **"GPU engine"** columns. Every `ffmpeg.exe` will show `GPU N - Video Decode`.
   - With today's code, expect the same N for all of them. This is the quickest proof of cause 1.
3. Run `ffmpeg -v verbose -hwaccel auto -i <clip> -f null -` by hand and read which hwaccel and device it opens.
   - Repeat with `-hwaccel d3d11va -hwaccel_device 1`; that is Route 1 by hand.
4. Run the show with `LCPLAY_HWACCEL=none`. If the "busy" card drops and the CPU rises, decode was the load.
5. `nvidia-smi dmon -s puct` reports per card: sm, mem, enc, **dec**, rxpci, txpci. Compare `dec` with `sm`.
   - The column letters come from a secondary source **[Uncertain]**; check `nvidia-smi dmon -h` on the machine.

### Metrics and tools after the change

| Metric | Source | Pass criterion |
|---|---|---|
| NVDEC use per card | NVML `nvmlDeviceGetDecoderUtilization`, or PDH `engtype_VideoDecode` per `luid_` | Within ±10 percentage points of each card's share of the video layers it displays |
| Process → card | PDH `\GPU Engine(*)\Utilization Percentage`, instances `pid_<pid>_luid_…_engtype_VideoDecode`. Enumerate the instances; don't build the names (MS guidance). | Every `ffmpeg.exe` PID shows decode **only** on the LUID of the card that displays it |
| Decoders per card | LC Play (new `GpuLoad.decoders`) | Matches the placement plan. 1 for a spanning layer once sharing is on |
| Late and missed frames | Existing `OutputHealth.late_decodes`, `missed` | 0 over 10 minutes |
| Upload bandwidth | LC Play bytes per frame × fps, against NVML `nvmlDeviceGetPcieThroughput` RX | Within 5% of the computed figure; no cross-adapter copy traffic |
| Cross-adapter present | `IDXGISwapChain::GetContainingOutput` → adapter LUID vs the render card (new check) | Never differs |

**Mapping NVML devices to DXGI cards:**
- NVML side: `nvmlDeviceGetPciInfo`.
- DXGI side: `D3DKMTOpenAdapterFromLuid` + `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERADDRESS)`.
- The exact NVML field names and units (PCIe throughput in KB/s, `nvmlDeviceGetProcessUtilization` under WDDM) are **[Uncertain]**: docs.nvidia.com was unreachable. Check them against the NVML headers shipped with the driver.

**Test matrix:**
- {1 or 2 GPUs} × {1 or 2 outputs per GPU} × {1, 2, 4 layers per output} × {H.264 4K60, HEVC 10-bit 4K60, HAP, HAP Q}
- plus one layer spanning both cards, a scrub/loop stress test, and the `sync_test` cue for frame pacing
- 10 minutes per configuration.

**CI regression tests**, all runnable on Linux except the last:
1. `decoder_args()` emits `-hwaccel d3d11va -hwaccel_device <idx>` for a given target; nothing with `LCPLAY_HWACCEL=none`; the old behaviour with `=auto`.
2. LUID → DXGI-index mapping against a fake adapter list.
3. Placement policy as a pure function, with property tests: no flapping under noisy load; admission thresholds respected.
4. A layer spanning two (mock) cards creates one decoder when sharing is on.
5. Serde round-trip of `GpuLoad`, with `#[serde(default)]` so older Producers still work.
6. `#[cfg(windows)]` smoke test: PDH enumeration finds at least one `luid_` instance.

---

## Sources (accessed 2026-11-19)

- FFmpeg source, tags n6.1 to n8.1.3: `fftools/ffmpeg_dec.c`, `libavcodec/h264dec.c`, `libavutil/hwcontext_{dxva2,d3d11va}.c`. Read on GitHub; ffmpeg.org was not reachable.
- Microsoft Learn: [IDXGIFactory1::EnumAdapters1](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgifactory1-enumadapters1) (2024-02-22); [EnumAdapterByGpuPreference](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/nf-dxgi1_6-idxgifactory6-enumadapterbygpupreference) (2024-02-22); [Cross-adapter resources](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/rendering-on-a-discrete-gpu-using-cross-adapter-resources) (2025-03-25).
- NVIDIA: [NVDEC Application Note 13.1](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvdec-application-note/index.html); [CUDA environment variables](https://docs.nvidia.com/cuda/cuda-programming-guide/05-appendices/environment-variables.html); RTX PRO 6000 Blackwell datasheet. These came from search excerpts; direct fetch was blocked.
- Repository docs: `docs/ARCHITECTURE.md` (open items, the "only Linux has been run" status), `docs/PLAYBACK_PLAN.md` §2 and §3.4.

---

## Status (2026-11-20)

Phase 1 (sync) is implemented on the long-lived branch (not on `main`):

- **Machine frame plan** (`crates/lc-runner/src/plan.rs`): one plan per
  refresh for every card — the Director's grid instant when frame-locked,
  otherwise the reference output's nearest refresh at the same rate.
- **One late-frame decision** per layer and refresh: the first card's frame
  stands if it is at most one frame from the one due; followers show it
  (`Content::follow`). Any card showing another frame than one before it
  for the same refresh is counted as `split_frames` in `OutputHealth`
  (Producer: `split N`).
- **Render thread per card** (`crates/lc-runner/src/card.rs`), with
  `LCPLAY_CARD_THREADS=0` to go back to one loop, and `LCPLAY_SPLIT_CARDS=1`
  to run the multi-card path on one card for testing.

Tested on Linux (lavapipe, Xvfb, two devices on one adapter). **Not yet run
on Windows/D3D12 or on two physical cards.**

## Status (2026-11-21): Route 1

Route 1 is implemented on the same branch:

- `dxgi::dxgi_index_of_luid` finds the DXGI adapter index (and the
  "VVVV:DDDD (name)" ffmpeg will print for it) of a card's LUID; software
  adapters (WARP) are left out. When a card's device is created, `Gpu`
  reads its LUID and decides where its video is decoded (`HwAccel`), once,
  off the render threads; the log says `video shown on <card>: …`.
  `TextureFactory.decode` carries that to `VideoContent::new`, and every
  `Decoder::start` uses it.
- On Windows: `-hwaccel d3d11va -hwaccel_device <index>`. Never auto with a
  device, never `cuda`. If `ffmpeg -hwaccels` does not list `d3d11va`, or
  the card has no LUID (Vulkan backend), decoding stays on `-hwaccel auto`
  and the log says why.
- `LCPLAY_HWACCEL`: unset (or `card`) binds to the card; `auto` is the old
  behaviour; `none` is software.
- ffmpeg now logs at `+level+info` into a pipe the Runner reads. Each
  distinct line is passed on once per file: errors, warnings, and the
  opened device (`video '…': ffmpeg decodes on 10de:…`). Errors that only
  mean "not in hardware" (auto probing CUDA/VAAPI/Vulkan, "Failed setup
  for format d3d11") become warnings. If ffmpeg reports a different
  adapter than the card's, a warning names both. ffmpeg falls back to the
  default adapter *without a message* when the index does not exist, so a
  missing "decodes on" line for a layer is itself a sign of trouble. Two
  identical cards in swapped order would not be caught by this check:
  verify with Task Manager's per-process GPU engine column.
- `decoder_args` is a pure function with unit tests (item 4).
- **Not done: item 6, one shared decoder for a spanning layer.** With the
  render thread per card, a shared decoder means a frame queue read by
  two threads, released only when both cards have uploaded a frame: the
  slower card would hold back the faster card's decoder, coupling the
  cards again, and a seek or loop on one would have to restart the other.
  With Route 1 the two decodes of a spanning layer already land on two
  different cards' NVDEC, so the cost left is one extra decode and
  download per spanning layer, on CPU/PCIe. Worth doing with Route 2's
  placement policy, once there are measurements from real cards.

