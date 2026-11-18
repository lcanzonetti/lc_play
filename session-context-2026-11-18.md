# LC Play — session context (2026-11-17/18): HAP playback performance on a laptop

Use this file as context for a new session. Repo: `lcanzonetti/lc_play`. Session base: `d2ce22c`.
Everything below is pushed to `main` (fast-forward) and to `claude/relaxed-sagan-p7dpj4`. Head: `86a9c3e`.

## Test machine and show
- Windows 11 laptop. GPU: RTX 3050 Ti Laptop (D3D12, presenter `dxgi`).
- Displays:
  - `DISPLAY1`: laptop panel, 2560×1440, normally 165 Hz.
  - `DISPLAY5`: external 3840×2160 at 60 Hz. Plugged into the NVIDIA GPU directly. This is the Runner's output.
- Runs everything on one machine with `lc-play.exe`. Repo at `C:\__TOOLS\lcmediaserver`; Runner folder `target\release\lc-runner\`.
- Test show: three HAP layers, often free-running:

  | Clip | Size | Rate | Codec | Per frame |
  |---|---|---|---|---|
  | BACKGROUND PARTICELLARE | 9978×2176 | 25 fps | DXT1 | 10.9 MB |
  | fish | 3840×2160 | 60 fps | DXT1 | 4.1 MB |
  | Pan Tilt | 3840×2160 | 50 fps | DXT1 | 4.1 MB |

- Optimiser rules set to 1 chunk, Snappy on. The default is 8 chunks.
- Logs:
  - Start `.\target\release\lc-play.exe 2>&1 | Tee-Object -FilePath lcplay-console.log` in **PowerShell**, not cmd. In cmd use `> lcplay-console.log 2>&1`.
  - The PowerShell file is UTF-16; convert it with `iconv -f UTF-16LE -t UTF-8`.
  - The Runner also writes `runner.log` in its folder.
  - Log timestamps are UTC; the user is UTC+1.

## Changes made (commits)
1. **`5210e41` Frame timing log** (`crates/lc-runner/src/frametime.rs`, hooks in `app.rs`).
   - Each render-loop iteration is split into `outside`, `housekeeping`, `acquire` (waiting for the swap chain), `update` (layers; the slowest layer is named), `render`, `present` and `other`.
   - A frame longer than two refreshes is logged as `slow frame: …` with tags such as `[show applied]`, `[windows rebuilt]`, `[content loaded]`, `[preview …]`, `[late decode]`.
   - At most 4 lines per second. The 5 s fps line adds "slowest frame X ms, N over T ms".
   - `LCPLAY_FRAME_LOG=0` turns it off; `LCPLAY_FRAME_LOG=<ms>` sets the threshold.
2. **`a3a9ff2` Diagnostics.**
   - Director: logs `'<name>' disconnected: <reason>` at info level. `idle timeout` means 6 s without messages; clients ping every 2 s.
   - Producer: detects its own UI freezes over 1 s and shows them in a toast if the Director connection drops right after.
   - Frame log: frames of 100 ms or more bypass the rate limit.
3. **`deaaa4d` HAP uploads off the render thread** (`hap.rs`, `content.rs` `Uploads`, `render.rs` `take_uploads`).
   - Before: `Queue::write_texture` on the render thread cost 0.5–0.8 ms/MB plus a staging buffer per frame, so `update` took 12–16 ms.
   - Now each playing HAP layer has a ring of 8 persistently mapped upload buffers (`SLOTS`).
     - Workers unpack into ordinary RAM (upload memory is write-combined, and Snappy reads back its output), then copy rows padded to 256 bytes.
     - The render thread unmaps and records `copy_buffer_to_texture`. That copy is submitted ahead of the frame, in the same submission.
     - The buffer is re-mapped once its batch has been submitted. Map callbacks are filtered by a generation counter.
   - Also: `LazyVideo::ready()` is false while loading.
   - Tests: `upload_rows_are_padded_to_the_copy_alignment` and `hap_playback_cycles_its_upload_buffers` (GPU). Ignored benchmark: `bench_hap_update`.
4. **`5bb3697` Upload buffers created by the workers, lazily.**
   - wgpu zero-fills buffers created mapped; creating them on the render thread stalled it 111–191 ms at every layer start.
   - Start cost is now 0.2 ms (it was 46.6 ms in the container).
5. **`86a9c3e` Windows power throttling opt-out** (`priority.rs`, `main.rs`).
   - The process calls `SetProcessInformation(ProcessPowerThrottling)` with `ControlMask` = EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION and `StateMask` = 0. If refused, it falls back to speed only.
   - The log line says `power throttling off: …`.
   - Background threads are also tagged HighQoS (`SetThreadInformation(ThreadPowerThrottling)`) and keep BELOW_NORMAL priority.
   - `LCPLAY_THREAD_PRIORITY=0` disables all of it.
   - Measured result: **no effect** on the focus drops.

The reviewer subagent reviewed twice: no critical findings and no should-fix findings. Open nitpicks, not fixed because the user said fix only on request:
- `frametime.rs`: `begin()` clears notes that were set before it; clear them in `end()`/`skip()` instead.
- `hap.rs`: `Uploads::make` panics in a worker on device loss or out-of-memory, and the frame stays busy.
- `wgpu` map error on a slot: that slot is lost until `idle()`.
- `SLOTS` must grow if `AHEAD` or the worker count grows.

## Measurements and conclusions
- **CPU decode is not a bottleneck.**
  - `bench_hap_unpack` on the laptop: 4K Hap1 2.2 ms/frame on one worker, about 448 fps. The optimised copy was faster still.
  - 8 chunks spawn threads per frame (`lc-hap/src/frame.rs`, `std::thread::scope`). That isn't a large cost; 1 chunk felt better.
- **After the upload fix,** `update` is about 0.5 ms. What remained were slow frames spent in `acquire` (30–40 ms) while the render thread itself worked only about 7 ms.
- **nvidia-smi** (`gpu.csv`):
  - 60 fps at 1942 MHz, only about 40% busy, 40 W, 70 °C, PCIe gen4 ×8. No throttling: reason `0x1` is "GPU idle".
  - Slow stretches coincided with clocks of 1000–1600 MHz.
  - NVCP "Prefer maximum performance" for `lc-runner.exe`: no effect. No background or max frame-rate cap was set.
- **The drops depend on focus.** Fps dropped whenever another window (browser, Notepad, cmd, taskbar thumbnail) had focus.
- **PresentMon 2.3.1** (`--process_name lc-runner.exe --output_file pm.csv --timed 60 --terminate_after_timed`, admin):
  - `PresentMode` was always `Hardware Composed: Independent Flip` (MPO). There is no compositor switch.
  - `MsGPUBusy` was 4–7 ms.
  - Unfocused, the Runner waits 45–68 ms for the frame-latency waitable, so frames come in pairs displayed for 50 + 16.7 ms: about 30 fps.
  - Notepad focused gave the same result as the browser, so it's focus itself, not GPU contention.
- **Laptop panel set to 60 Hz (same as the external):**

  | Phase | Fps | Refreshes repeated |
  |---|---|---|
  | Runner focused | 57–60 | 3.6% |
  | Notepad focused | 52–60 | 5.2% |
  | Browser focused | 52–56 | 8.9% |

  - Before, unfocused was 30–33 fps.
  - Main cause: **mixed refresh rates (165 + 60 Hz)** with Windows/DWM presentation scheduling.
  - Residual: the same mechanism (late waitable signal, `cpuBusy` 36–46 ms, GPU 7 ms) at a much lower rate.
- **Single-laptop artefacts seen in logs:**
  - The Runner re-downloads multi-GB assets from the local Asset Manager. A 5.7 GB copy caused 85–107 ms stalls.
  - Big uploads through the Producer caused stalls even with an empty show.
  - At start the Director sometimes restored an *older* show than the Runner's cache. That caused re-downloads plus a 291 ms content reload (`[show applied, content loaded]`). Not investigated.
- **Refresh estimate bug (open):** during slow stretches the Runner's refresh-period estimate drifts from 16.7 to 17–19.3 ms on a 60 Hz screen. This affects frame selection and the log threshold. Not investigated.
- **GUI hangs and Producer disconnects:** not reproduced in later sessions. The Runner stayed connected throughout, so the Director was healthy and the dropped client was the Producer window. Waiting for a `disconnected: <reason>` line and the Producer toast. Reminder: clicking inside a classic console window (Quick Edit) pauses all services that write to it.

## Next steps (in order)
1. Repeat the focus test (Runner focused / Notepad / browser, 20 s each, watch the fps lines or run PresentMon) with one change at a time:
   - a. Win+P "Second screen only";
   - b. `LCPLAY_FRAME_LATENCY=3`;
   - c. `LCPLAY_SWAPCHAIN=wgpu`. If that helps, rework how `dxgi.rs` waits on the frame-latency waitable;
   - d. MPO off: `HKLM\SOFTWARE\Microsoft\Windows\Dwm` `OverlayTestMode`=DWORD 5, reboot. Unofficial; undo by deleting the value.
2. Investigate the refresh-period drift (`vsync.rs` `RefreshClock`).
3. Non-HAP video (`video.rs`, ffmpeg) still calls `write_texture` on the render thread: about 12 MB per 4K YUV frame.
4. Director restoring an older show than the Runner's cache, which forces re-downloads at start.
5. Possibly change the optimiser default from 8 chunks to 1, or use a persistent thread pool in `decode_chunked`.
6. Reviewer nitpicks listed above.

Rule for show machines: run all screens on a Runner machine at the same refresh rate.

## Working conventions (from CLAUDE.md and the user)
- Before pushing:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`. Also run clippy with `--target x86_64-pc-windows-msvc`, because Windows-only code hides Linux lints.
  - `cargo test --workspace`. GPU tests need `mesa-vulkan-drivers`; video tests need ffmpeg with HAP; the Linux build needs `libgtk-3-dev` and `libwebkit2gtk-4.1-dev`.
  - `cargo check --workspace --target x86_64-pc-windows-msvc`.
- Push to the working branch and fast-forward `main`.
- "Spawn the reviewer": foreground `reviewer` subagent, given only the base ref, the changed files and "Review these changes." Relay its report verbatim and don't fix anything until asked.
- The user prefers precise, sourced answers; flag uncertain points.
- `learn.microsoft.com` works via `curl` in the session. The web-fetch helper fails on DNS for it.
