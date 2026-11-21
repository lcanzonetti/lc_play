//! LC Play's own swap chain on Windows.
//!
//! wgpu presents through a DXGI swap chain it keeps private. Two things the
//! Runner needs are only available on the swap chain itself:
//!
//! * **frame statistics** (`GetFrameStatistics`): the refresh counter, the
//!   exact time of a refresh, and which refresh each present reached — what
//!   makes "render for the refresh the frame will appear on" exact;
//! * **NVIDIA's present barrier**, which is joined per swap chain.
//!
//! So on D3D12 the Runner creates the swap chain itself, on wgpu's own device
//! and queue, and hands its buffers to wgpu as ordinary render targets
//! (`create_texture_from_hal`). wgpu renders into them and knows nothing of
//! presenting; this module does the rest:
//!
//! ```text
//!  wait (frame-latency waitable) ─▶ begin: buffer i, PRESENT→RENDER_TARGET ─▶
//!  wgpu renders + submits ─▶ end: RENDER_TARGET→PRESENT, Present(1) ─▶ stats
//! ```
//!
//! The two state transitions are tiny command lists recorded once per buffer
//! and re-executed every frame, on the same queue as wgpu's work, so their
//! order with it is the order of submission. wgpu's own tracking believes
//! the buffer stays a render target between frames, which it is whenever
//! wgpu touches it.
//!
//! The swap chain is flip-model (`FLIP_DISCARD`, 3 buffers) with a
//! frame-latency waitable object: the render loop waits until the queue has
//! room, so frames are rendered a fixed number of refreshes ahead
//! (`LCPLAY_FRAME_LATENCY`, default 2). In a borderless full-screen window
//! that covers the monitor, Windows hands the buffers straight to the display
//! ("independent flip"): no compositor in the way, which is also a
//! condition of the present barrier.

use std::mem::ManuallyDrop;

use anyhow::{anyhow, Context};
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

use crate::nvapi::{BarrierClient, BarrierState, Nvapi};

const BUFFERS: u32 = 3;
const FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

use crate::render::frame_latency;

/// Whether swap chains should join NVIDIA's present barrier when the GPU
/// supports it (`LCPLAY_PRESENT_BARRIER=0` turns it off).
fn barrier_wanted() -> bool {
    std::env::var("LCPLAY_PRESENT_BARRIER").map_or(true, |v| v != "0")
}

/// Whether to use LC Play's own swap chains at all (`LCPLAY_SWAPCHAIN=wgpu`
/// falls back to wgpu's — a safety valve if a driver misbehaves).
pub fn enabled() -> bool {
    std::env::var("LCPLAY_SWAPCHAIN").map_or(true, |v| !v.eq_ignore_ascii_case("wgpu"))
}

/// DXGI's view of a refresh, from `GetFrameStatistics`.
#[derive(Debug, Clone, Copy)]
pub struct Statistics {
    /// Id (see [`SwapChain::end`]) of the last present that reached the screen.
    pub present_count: u32,
    /// Refresh it reached.
    pub present_refresh: i64,
    /// A refresh and its QPC time.
    pub sync_refresh: i64,
    pub sync_qpc: i64,
}

/// Per-buffer transitions, recorded once.
struct Transitions {
    _alloc: ID3D12CommandAllocator,
    to_target: ID3D12CommandList,
    to_present: ID3D12CommandList,
}

pub struct SwapChain {
    swap: IDXGISwapChain3,
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    waitable: HANDLE,
    raw: Vec<ID3D12Resource>,
    views: Vec<wgpu::TextureView>,
    /// Kept alive for the views.
    _textures: Vec<wgpu::Texture>,
    transitions: Vec<Transitions>,
    /// Whether a buffer has been presented before (its first use starts in
    /// the state wgpu expects, so it needs no transition in).
    presented: Vec<bool>,
    current: Option<usize>,
    format: wgpu::TextureFormat,
    pub width: u32,
    pub height: u32,
    barrier: Option<BarrierClient>,
    barrier_fence: Option<ID3D12Fence>,
    barrier_state: BarrierState,
    /// Presents made while registered with the barrier: the barrier fence's
    /// value is expected to reach this count (NVAPI: "monotonically
    /// increasing on every present call, and tracked by the application").
    barrier_presents: u64,
    /// Per buffer: the barrier-fence value of the present that last showed
    /// it. Before rendering into the buffer again, the fence must reach it.
    barrier_due: Vec<u64>,
    /// The barrier fence never reached a value in time: stop waiting on it
    /// (a driver that does not signal it must not freeze the output).
    barrier_wait_broken: bool,
    /// Event for CPU waits on the barrier fence.
    barrier_event: Option<HANDLE>,
    /// Whether the frame-lock counter can be read (`None`: not tried yet).
    counter_supported: Option<bool>,
    /// The buffers are gone (a resize failed): the swap chain cannot be used.
    broken: bool,
}

// SAFETY: a swap chain is owned by one card and used by whichever thread
// holds that card's lock — its render thread, or the window thread between
// two frames — never by two at once. What it holds may be used from any
// thread: D3D12 and DXGI objects are free-threaded, and the event handles
// (the frame-latency waitable, the barrier event) are process-wide kernel
// handles that any thread can wait on and close. Only `HANDLE` (a raw
// pointer) keeps the compiler from seeing that.
unsafe impl Send for SwapChain {}

impl SwapChain {
    /// Create the swap chain for `hwnd` on wgpu's device. `format` is the
    /// format wgpu renders in (an sRGB view of the swap chain's buffers).
    pub fn new(
        device: &wgpu::Device,
        hwnd: HWND,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> anyhow::Result<Self> {
        let buffer_format = match format {
            wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Bgra8Unorm => {
                DXGI_FORMAT_B8G8R8A8_UNORM
            }
            wgpu::TextureFormat::Rgba8UnormSrgb | wgpu::TextureFormat::Rgba8Unorm => {
                DXGI_FORMAT_R8G8B8A8_UNORM
            }
            other => return Err(anyhow!("no DXGI swap chain for {other:?}")),
        };
        let (raw_device, queue) = unsafe {
            device.as_hal::<wgpu::hal::api::Dx12, _, _>(|d| {
                d.map(|d| (d.raw_device().clone(), d.raw_queue().clone()))
            })
        }
        .ok_or_else(|| anyhow!("not a D3D12 device"))?;

        let (width, height) = (width.max(1), height.max(1));
        let swap: IDXGISwapChain3 = unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: buffer_format,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: FLAGS.0 as u32,
            };
            let swap = factory
                .CreateSwapChainForHwnd(&queue, hwnd, &desc, None, None)
                .context("CreateSwapChainForHwnd")?;
            // Alt+Enter would switch to exclusive full screen behind our back.
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            swap.cast()?
        };
        let waitable = unsafe {
            swap.SetMaximumFrameLatency(frame_latency())?;
            swap.GetFrameLatencyWaitableObject()
        };

        let mut sc = SwapChain {
            swap,
            device: raw_device,
            queue,
            waitable,
            raw: Vec::new(),
            views: Vec::new(),
            _textures: Vec::new(),
            transitions: Vec::new(),
            presented: Vec::new(),
            current: None,
            format,
            width,
            height,
            barrier: None,
            barrier_fence: None,
            barrier_state: BarrierState::Off,
            barrier_presents: 0,
            barrier_due: Vec::new(),
            barrier_wait_broken: false,
            barrier_event: None,
            counter_supported: None,
            broken: false,
        };
        sc.wrap_buffers(device)?;
        sc.setup_barrier();
        Ok(sc)
    }

    /// Hand the swap chain's buffers to wgpu and record their transitions.
    fn wrap_buffers(&mut self, device: &wgpu::Device) -> anyhow::Result<()> {
        let size = wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        };
        for i in 0..BUFFERS {
            let resource: ID3D12Resource = unsafe { self.swap.GetBuffer(i)? };
            let hal = unsafe {
                wgpu::hal::dx12::Device::texture_from_raw(
                    resource.clone(),
                    self.format,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                )
            };
            let texture = unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                    hal,
                    &wgpu::TextureDescriptor {
                        label: Some("swap-chain-buffer"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: self.format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        view_formats: &[],
                    },
                )
            };
            self.views
                .push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
            self._textures.push(texture);
            self.transitions.push(self.record(&resource)?);
            self.raw.push(resource);
            self.presented.push(false);
            self.barrier_due.push(0);
        }
        Ok(())
    }

    /// Record the two transitions of one buffer.
    fn record(&self, resource: &ID3D12Resource) -> anyhow::Result<Transitions> {
        unsafe {
            let alloc: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list = |before, after| -> anyhow::Result<ID3D12CommandList> {
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &alloc,
                    None,
                )?;
                list.ResourceBarrier(&[transition(resource, before, after)]);
                list.Close()?;
                Ok(list.cast()?)
            };
            Ok(Transitions {
                to_target: list(
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                )?,
                to_present: list(
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_PRESENT,
                )?,
                _alloc: alloc,
            })
        }
    }

    fn setup_barrier(&mut self) {
        if !barrier_wanted() {
            self.barrier_state = BarrierState::Off;
            return;
        }
        let Some(api) = Nvapi::get() else {
            self.barrier_state = BarrierState::Unsupported;
            return;
        };
        if !api.barrier_supported(&self.device) {
            self.barrier_state = BarrierState::Unsupported;
            return;
        }
        let fence: Option<ID3D12Fence> =
            unsafe { self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok() };
        let swap: Option<IDXGISwapChain> = self.swap.cast().ok();
        let (Some(fence), Some(swap)) = (fence, swap) else {
            self.barrier_state = BarrierState::Unsupported;
            return;
        };
        let Some(mut client) = BarrierClient::new(api, &self.device, &swap) else {
            self.barrier_state = BarrierState::Unsupported;
            return;
        };
        if client.register(&fence, &self.raw) && client.join() {
            tracing::info!("output joined the NVIDIA present barrier");
            self.barrier_state = BarrierState::Client;
            self.barrier_event = unsafe { CreateEventW(None, false, false, None).ok() };
        } else {
            self.barrier_state = BarrierState::NotJoined;
        }
        self.barrier = Some(client);
        self.barrier_fence = Some(fence);
    }

    /// Wait until the swap chain can take another frame. `false` on timeout
    /// (a hidden window, a lost display): skip this output this time.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.waitable, timeout_ms) == WAIT_OBJECT_0 }
    }

    /// Start a frame: the buffer to render into. `None` if the swap chain
    /// lost its buffers (a failed resize): it must be replaced.
    pub fn begin(&mut self) -> Option<&wgpu::TextureView> {
        if self.broken || self.raw.is_empty() {
            return None;
        }
        let i = unsafe { self.swap.GetCurrentBackBufferIndex() } as usize % self.raw.len();
        self.wait_for_barrier(i);
        if self.presented[i] {
            unsafe {
                self.queue
                    .ExecuteCommandLists(&[Some(self.transitions[i].to_target.clone())]);
            }
        }
        self.current = Some(i);
        Some(&self.views[i])
    }

    /// NVAPI: "Application must wait on this fence to ensure the scanout
    /// resources are ready for use in the next rendering loop." Wait (on the
    /// CPU, with a timeout) until the barrier has released the present that
    /// last showed buffer `i`.
    ///
    /// [Not verified on hardware] how the driver numbers the fence: this
    /// assumes it reaches the count of presents made since registration, as
    /// the header describes. If it never does, the first timeout turns the
    /// waits off and says so in the log — the barrier itself keeps working.
    fn wait_for_barrier(&mut self, i: usize) {
        if self.barrier_wait_broken {
            return;
        }
        let (Some(fence), Some(event)) = (&self.barrier_fence, self.barrier_event) else {
            return;
        };
        let due = self.barrier_due[i];
        unsafe {
            if due == 0 || fence.GetCompletedValue() >= due {
                return;
            }
            if fence.SetEventOnCompletion(due, event).is_err()
                || WaitForSingleObject(event, 100) != WAIT_OBJECT_0
            {
                tracing::warn!(
                    "present barrier: the driver did not release frame {due} in time; \
                     no longer waiting on its fence"
                );
                self.barrier_wait_broken = true;
            }
        }
    }

    /// Finish the frame begun with [`begin`](Self::begin) (wgpu's work for it
    /// must have been submitted) and present it on the next refresh.
    /// Returns the present's id, as used by [`Statistics::present_count`].
    pub fn end(&mut self) -> anyhow::Result<u32> {
        let i = self
            .current
            .take()
            .ok_or_else(|| anyhow!("no frame begun"))?;
        unsafe {
            self.queue
                .ExecuteCommandLists(&[Some(self.transitions[i].to_present.clone())]);
            self.presented[i] = true;
            self.swap.Present(1, DXGI_PRESENT(0)).ok()?;
            if self.barrier_event.is_some() {
                self.barrier_presents += 1;
                self.barrier_due[i] = self.barrier_presents;
            }
            Ok(self.swap.GetLastPresentCount()?)
        }
    }

    /// The latest frame statistics, if DXGI has them (not right after
    /// creation or a mode change, and not for a composed window).
    pub fn statistics(&mut self) -> Option<Statistics> {
        let mut s = DXGI_FRAME_STATISTICS::default();
        unsafe { self.swap.GetFrameStatistics(&mut s) }.ok()?;
        if s.SyncRefreshCount == 0 {
            return None;
        }
        if let Some(client) = &self.barrier {
            self.barrier_state = client.state();
        }
        Some(Statistics {
            present_count: s.PresentCount,
            present_refresh: s.PresentRefreshCount as i64,
            sync_refresh: s.SyncRefreshCount as i64,
            sync_qpc: s.SyncQPCTime,
        })
    }

    pub fn barrier_state(&self) -> BarrierState {
        self.barrier_state
    }

    /// NVIDIA's frame-lock frame counter, when frame lock is active. Not
    /// asked again once the driver has said it does not have one.
    pub fn frame_counter(&mut self) -> Option<u32> {
        if self.counter_supported == Some(false) {
            return None;
        }
        let count = Nvapi::get().and_then(|api| api.frame_count(&self.device));
        if self.counter_supported.is_none() {
            self.counter_supported = Some(count.is_some());
        }
        count
    }

    /// Whether the swap chain lost its buffers and must be replaced.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// Resize the buffers. Waits for the GPU: only on window changes.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) -> anyhow::Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        // Every reference to the old buffers must be gone before DXGI can
        // resize them: ours, and wgpu's (released once its work is done).
        self.views.clear();
        self._textures.clear();
        self.transitions.clear();
        self.raw.clear();
        self.presented.clear();
        self.barrier_due.clear();
        self.current = None;
        // From here on, until the new buffers exist, the swap chain is
        // unusable; on failure it stays so and the caller replaces it.
        self.broken = true;
        let _ = device.poll(wgpu::Maintain::Wait);
        self.wait_idle();
        unsafe {
            self.swap
                .ResizeBuffers(BUFFERS, width, height, DXGI_FORMAT_UNKNOWN, FLAGS)
                .context("ResizeBuffers")?;
        }
        self.width = width;
        self.height = height;
        self.wrap_buffers(device)?;
        self.broken = false;
        if let (Some(client), Some(fence)) = (&self.barrier, &self.barrier_fence) {
            if !client.register(fence, &self.raw) {
                // [Not verified on hardware] whether re-registering after a
                // resize keeps the client joined; if not, say so.
                tracing::warn!("present barrier: buffers not re-registered after a resize");
                self.barrier_state = BarrierState::NotJoined;
                self.barrier_event = None;
            }
        }
        Ok(())
    }

    /// Block until the queue has finished everything submitted so far.
    fn wait_idle(&self) {
        unsafe {
            let Ok(fence) = self
                .device
                .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
            else {
                return;
            };
            if self.queue.Signal(&fence, 1).is_err() {
                return;
            }
            if let Ok(event) = CreateEventW(None, false, false, None) {
                if fence.SetEventOnCompletion(1, event).is_ok() {
                    WaitForSingleObject(event, INFINITE);
                }
                let _ = CloseHandle(event);
            }
        }
    }
}

impl Drop for SwapChain {
    fn drop(&mut self) {
        // Leave the barrier before the swap chain goes away.
        self.barrier = None;
        self.wait_idle();
        unsafe {
            let _ = CloseHandle(self.waitable);
            if let Some(event) = self.barrier_event {
                let _ = CloseHandle(event);
            }
        }
    }
}

/// A transition barrier for `resource`, without taking a COM reference (the
/// barrier only lives while the command list is recorded).
fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Convert a QPC timestamp to the Runner's local clock, given the local
/// clock's reading now.
pub fn qpc_to_local(qpc: i64, local_now_ns: i64) -> i64 {
    let (mut now, mut freq) = (0i64, 0i64);
    unsafe {
        if QueryPerformanceCounter(&mut now).is_err()
            || QueryPerformanceFrequency(&mut freq).is_err()
            || freq <= 0
        {
            return local_now_ns;
        }
    }
    let ago_ns = (now as i128 - qpc as i128) * 1_000_000_000 / freq as i128;
    local_now_ns.saturating_sub(ago_ns.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// The window handle of a winit window.
pub fn hwnd_of(window: &winit::window::Window) -> Option<HWND> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// The LUID of the adapter driving a monitor (by `HMONITOR`), from DXGI.
pub fn adapter_of_monitor(hmonitor: isize) -> Option<i64> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.Monitor.0 as isize == hmonitor {
                        let luid = adapter.GetDesc1().ok()?.AdapterLuid;
                        return Some(luid_value(luid.HighPart, luid.LowPart));
                    }
                }
                o += 1;
            }
            a += 1;
        }
    }
    None
}

/// The DXGI adapter index of the card with this LUID, and how ffmpeg will
/// describe that adapter when it opens it.
///
/// The index is what ffmpeg's D3D11VA device takes (`-hwaccel_device N`):
/// `libavutil/hwcontext_d3d11va.c` parses the string with `atoi` and opens
/// `IDXGIFactory::EnumAdapters(N)`, then logs "Using device
/// VVVV:DDDD (Description)." at info level. The walk here uses the same
/// enumeration (`EnumAdapters1` lists adapters in the same order as
/// `EnumAdapters`), so within one boot the index names the same card in the
/// Runner and in its ffmpeg children — unless something makes the two
/// processes see different orders (a per-app GPU preference for
/// `ffmpeg.exe` is the suspect). The description is returned so that the
/// Runner can compare it with what ffmpeg reports and say so when they
/// differ (which only shows when the cards are different models).
///
/// `None` when no adapter has this LUID (a card removed since).
pub fn dxgi_index_of_luid(luid: i64) -> Option<(u32, String)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let mut index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            if let Ok(desc) = adapter.GetDesc1() {
                if luid_value(desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart) == luid {
                    let len = desc
                        .Description
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.Description.len());
                    let name = String::from_utf16_lossy(&desc.Description[..len]);
                    // ffmpeg's format: "%04x:%04x (%ls)".
                    let ident = format!("{:04x}:{:04x} ({name})", desc.VendorId, desc.DeviceId);
                    return Some((index, ident));
                }
            }
            index += 1;
        }
    }
    None
}

/// The LUID of a wgpu adapter (D3D12 only).
pub fn adapter_luid(adapter: &wgpu::Adapter) -> Option<i64> {
    unsafe {
        adapter.as_hal::<wgpu::hal::api::Dx12, _, _>(|a| {
            let desc = a?.raw_adapter().GetDesc1().ok()?;
            Some(luid_value(
                desc.AdapterLuid.HighPart,
                desc.AdapterLuid.LowPart,
            ))
        })
    }
}

fn luid_value(high: i32, low: u32) -> i64 {
    ((high as i64) << 32) | low as i64
}
