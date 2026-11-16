//! The few NVAPI functions LC Play uses: NVIDIA's present barrier and the
//! frame-lock frame counter.
//!
//! NVAPI ships inside the NVIDIA driver (`nvapi64.dll`). Its only export is
//! `nvapi_QueryInterface(id)`, which returns the address of a function given
//! its id; the ids and signatures come from NVIDIA's public headers
//! (`nvapi_interface.h`, `nvapi.h`, MIT-licensed, github.com/NVIDIA/nvapi).
//! Loading it at run time means LC Play needs no NVIDIA SDK to build and runs
//! unchanged on machines without an NVIDIA GPU: there [`Nvapi::get`] simply
//! returns `None`.
//!
//! **Present barrier** (driver R470+, professional GPUs): swap chains that
//! join it present on the same refresh — all outputs of a machine, and with a
//! Quadro Sync / RTX Pro Sync card and frame lock, all machines. It is only
//! effective in a borderless full-screen window with no desktop scaling or
//! taskbar composition; otherwise it reports `client` (joined, not synced).

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Fence, ID3D12Resource};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

type Status = i32;
const NVAPI_OK: Status = 0;

// Function ids, from nvapi_interface.h.
const ID_INITIALIZE: u32 = 0x0150_e828;
const ID_QUERY_SUPPORT: u32 = 0xa15f_aef7;
const ID_CREATE_CLIENT: u32 = 0x4d81_5de9;
const ID_REGISTER_RESOURCES: u32 = 0xd53c_9ef0;
const ID_DESTROY_CLIENT: u32 = 0x3c5c_351b;
const ID_JOIN: u32 = 0x17f6_bf82;
const ID_LEAVE: u32 = 0xc3ec_5a7f;
const ID_QUERY_STATS: u32 = 0x61b8_44a1;
const ID_QUERY_FRAME_COUNT: u32 = 0x9152_e055;

/// `NvPresentBarrierClientHandle`: an opaque pointer.
type ClientHandle = *mut c_void;

/// `MAKE_NVAPI_VERSION(type, ver)`: the struct's size in the low 16 bits.
const fn version<T>(v: u32) -> u32 {
    std::mem::size_of::<T>() as u32 | (v << 16)
}

#[repr(C)]
struct JoinParams {
    dwVersion: u32,
}

#[repr(C)]
#[derive(Default)]
struct FrameStatistics {
    dwVersion: u32,
    SyncMode: u32,
    PresentCount: u32,
    PresentInSyncCount: u32,
    FlipInSyncCount: u32,
    RefreshCount: u32,
}

type FnInitialize = unsafe extern "C" fn() -> Status;
type FnQuerySupport = unsafe extern "C" fn(*mut c_void, *mut bool) -> Status;
type FnCreateClient = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut ClientHandle) -> Status;
type FnRegister =
    unsafe extern "C" fn(ClientHandle, *mut c_void, *const *mut c_void, u32) -> Status;
type FnHandle = unsafe extern "C" fn(ClientHandle) -> Status;
type FnJoin = unsafe extern "C" fn(ClientHandle, *mut JoinParams) -> Status;
type FnQueryStats = unsafe extern "C" fn(ClientHandle, *mut FrameStatistics) -> Status;
type FnQueryFrameCount = unsafe extern "C" fn(*mut c_void, *mut u32) -> Status;

/// The loaded functions. Any of them may be missing on an old driver.
pub struct Nvapi {
    query_support: Option<FnQuerySupport>,
    create_client: Option<FnCreateClient>,
    register: Option<FnRegister>,
    destroy: Option<FnHandle>,
    join: Option<FnJoin>,
    leave: Option<FnHandle>,
    query_stats: Option<FnQueryStats>,
    query_frame_count: Option<FnQueryFrameCount>,
}

// The function pointers are plain code addresses.
unsafe impl Send for Nvapi {}
unsafe impl Sync for Nvapi {}

impl Nvapi {
    /// NVAPI, loaded and initialised once; `None` without an NVIDIA driver.
    pub fn get() -> Option<&'static Nvapi> {
        static NVAPI: OnceLock<Option<Nvapi>> = OnceLock::new();
        NVAPI.get_or_init(|| unsafe { Self::load() }).as_ref()
    }

    unsafe fn load() -> Option<Nvapi> {
        let lib = LoadLibraryA(PCSTR(c"nvapi64.dll".as_ptr().cast())).ok()?;
        let query = GetProcAddress(lib, PCSTR(c"nvapi_QueryInterface".as_ptr().cast()))?;
        let query: unsafe extern "C" fn(u32) -> *mut c_void = std::mem::transmute(query);
        let get = |id: u32| {
            let p = query(id);
            (!p.is_null()).then_some(p)
        };
        let init: FnInitialize = std::mem::transmute(get(ID_INITIALIZE)?);
        if init() != NVAPI_OK {
            tracing::info!("NVAPI did not initialise: no present barrier");
            return None;
        }
        macro_rules! f {
            ($id:expr) => {
                get($id).map(|p| std::mem::transmute(p))
            };
        }
        Some(Nvapi {
            query_support: f!(ID_QUERY_SUPPORT),
            create_client: f!(ID_CREATE_CLIENT),
            register: f!(ID_REGISTER_RESOURCES),
            destroy: f!(ID_DESTROY_CLIENT),
            join: f!(ID_JOIN),
            leave: f!(ID_LEAVE),
            query_stats: f!(ID_QUERY_STATS),
            query_frame_count: f!(ID_QUERY_FRAME_COUNT),
        })
    }

    /// Whether `device` can use the present barrier (professional GPU and a
    /// driver that has it).
    pub fn barrier_supported(&self, device: &ID3D12Device) -> bool {
        let Some(query) = self.query_support else {
            return false;
        };
        let mut supported = false;
        let status = unsafe { query(device.as_raw(), &mut supported) };
        status == NVAPI_OK && supported
    }

    /// The frame-lock frame counter: identical on every frame-locked machine
    /// at the same refresh. `None` when frame lock is not active or the
    /// driver does not offer it.
    pub fn frame_count(&self, device: &ID3D12Device) -> Option<u32> {
        let query = self.query_frame_count?;
        let mut count = 0u32;
        (unsafe { query(device.as_raw(), &mut count) } == NVAPI_OK).then_some(count)
    }
}

/// Where a swap chain stands with the present barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierState {
    /// Not asked for (`LCPLAY_PRESENT_BARRIER=0`, or not our swap chain).
    Off,
    /// No NVIDIA driver, or a GPU / driver without it.
    Unsupported,
    NotJoined,
    /// Joined, but not synchronized (yet): typically not full screen.
    Client,
    /// Synchronized with the other outputs of this machine.
    System,
    /// Synchronized across machines through frame lock.
    Cluster,
}

impl BarrierState {
    pub fn as_str(self) -> &'static str {
        match self {
            BarrierState::Off => "off",
            BarrierState::Unsupported => "unsupported",
            BarrierState::NotJoined => "not_joined",
            BarrierState::Client => "client",
            BarrierState::System => "system",
            BarrierState::Cluster => "cluster",
        }
    }
}

/// One swap chain's membership of the present barrier.
pub struct BarrierClient {
    api: &'static Nvapi,
    handle: ClientHandle,
    joined: bool,
}

impl BarrierClient {
    /// Create the client for `swap_chain` (made on `device`).
    pub fn new(
        api: &'static Nvapi,
        device: &ID3D12Device,
        swap_chain: &IDXGISwapChain,
    ) -> Option<Self> {
        let create = api.create_client?;
        let mut handle: ClientHandle = std::ptr::null_mut();
        let status = unsafe { create(device.as_raw(), swap_chain.as_raw(), &mut handle) };
        if status != NVAPI_OK || handle.is_null() {
            tracing::warn!("present barrier: cannot create a client (NVAPI status {status})");
            return None;
        }
        Some(BarrierClient {
            api,
            handle,
            joined: false,
        })
    }

    /// Register the swap chain's buffers and the fence the driver signals.
    /// Must be repeated whenever the buffers change (resize).
    pub fn register(&self, fence: &ID3D12Fence, buffers: &[ID3D12Resource]) -> bool {
        let Some(register) = self.api.register else {
            return false;
        };
        let raw: Vec<*mut c_void> = buffers.iter().map(|b| b.as_raw()).collect();
        let status =
            unsafe { register(self.handle, fence.as_raw(), raw.as_ptr(), raw.len() as u32) };
        if status != NVAPI_OK {
            tracing::warn!("present barrier: cannot register the buffers (NVAPI status {status})");
        }
        status == NVAPI_OK
    }

    pub fn join(&mut self) -> bool {
        let Some(join) = self.api.join else {
            return false;
        };
        let mut params = JoinParams {
            dwVersion: version::<JoinParams>(1),
        };
        let status = unsafe { join(self.handle, &mut params) };
        self.joined = status == NVAPI_OK;
        if !self.joined {
            tracing::warn!("present barrier: cannot join (NVAPI status {status})");
        }
        self.joined
    }

    /// The barrier state as of the last present.
    pub fn state(&self) -> BarrierState {
        if !self.joined {
            return BarrierState::NotJoined;
        }
        let Some(query) = self.api.query_stats else {
            return BarrierState::Client;
        };
        let mut stats = FrameStatistics {
            dwVersion: version::<FrameStatistics>(1),
            ..Default::default()
        };
        if unsafe { query(self.handle, &mut stats) } != NVAPI_OK {
            return BarrierState::Client;
        }
        match stats.SyncMode {
            0 => BarrierState::NotJoined,
            1 => BarrierState::Client,
            2 => BarrierState::System,
            3 => BarrierState::Cluster,
            _ => BarrierState::Client,
        }
    }
}

impl Drop for BarrierClient {
    fn drop(&mut self) {
        unsafe {
            if self.joined {
                if let Some(leave) = self.api.leave {
                    leave(self.handle);
                }
            }
            if let Some(destroy) = self.api.destroy {
                destroy(self.handle);
            }
        }
    }
}
