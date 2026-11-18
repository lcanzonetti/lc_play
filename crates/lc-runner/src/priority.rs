//! Thread priorities: the render loop before everything else.
//!
//! A frame must be rendered and presented within one refresh (16.7 ms at
//! 60 Hz). HAP decoding, by contrast, is bulk work that runs ahead of time on
//! every core: two 4K HAP layers keep all of them busy. At equal priority the
//! operating system shares the cores fairly, so the render loop sometimes
//! waits for a time slice and its frame reaches the screen a refresh late —
//! reported as a *missed* frame, seen as a stutter.
//!
//! So, on Windows:
//! * the render thread joins the Multimedia Class Scheduler Service (MMCSS,
//!   task "Playback"), which raises it into the range the system reserves
//!   for media, and otherwise gets `THREAD_PRIORITY_HIGHEST`;
//! * decoder, loader and download threads get `THREAD_PRIORITY_BELOW_NORMAL`:
//!   they still use every idle cycle, but give way to the render loop.
//!
//! Elsewhere these are no-ops (Linux would need privileges to raise a
//! thread's priority).
//!
//! The trade-off: below-normal decoders also give way to any other program
//! at normal priority. On a dedicated show machine nothing else competes;
//! where something does, the result is a late decode (one layer repeats a
//! frame) instead of a missed present (every output stutters).
//!
//! ## Power throttling (Quality of Service)
//!
//! Windows also gives each thread a *Quality of Service* level, which
//! decides its CPU clock speed and, on CPUs with efficiency cores, which
//! cores it runs on. For a program with a window, the level follows the
//! window: **High** while it has the focus, **Medium** while merely visible
//! (an output on another screen while the operator works in the Producer),
//! **Low** when minimised or covered; and threads of reduced priority — the
//! decoders — "can imply a lower QoS level" (Microsoft, "Quality of
//! Service"). On a laptop, Runner output dropped to ~40 fps whenever another
//! window had the focus.
//!
//! So the Runner tags itself **High** whatever its windows do
//! (`SetProcessInformation(ProcessPowerThrottling)`, execution-speed
//! throttling off, and timer-resolution requests honoured even when not
//! visible), and tags each background thread High as well
//! (`SetThreadInformation(ThreadPowerThrottling)`): the decoders keep their
//! lower *priority*, so the render thread still goes first, but are not sent
//! to slow cores or clocks for it. The cost is power and heat on a laptop;
//! a show machine wants exactly this.
//!
//! `LCPLAY_THREAD_PRIORITY=0` leaves all priorities and Quality of Service
//! levels as the system sets them.

/// Whether priorities are adjusted at all.
fn enabled() -> bool {
    std::env::var("LCPLAY_THREAD_PRIORITY").map_or(true, |v| v != "0")
}

/// Ask Windows to run the whole process at full speed whatever the state of
/// its windows (see the module docs). Called once, early.
pub fn full_speed_process() {
    #[cfg(windows)]
    if enabled() {
        opt_out_of_power_throttling();
    }
}

#[cfg(windows)]
fn opt_out_of_power_throttling() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    };
    // `ControlMask`: the mechanisms we take control of; `StateMask` 0: all of
    // them off ("HighQoS" in Microsoft's example). The timer flag may not be
    // known to older Windows: then the speed alone.
    let set = |mechanisms: u32| {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: mechanisms,
            StateMask: 0,
        };
        unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                std::ptr::from_ref(&state).cast(),
                std::mem::size_of_val(&state) as u32,
            )
        }
    };
    let speed = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
    match set(speed | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION) {
        Ok(()) => tracing::info!(
            "power throttling off: full speed and exact timers, whether or not a window has the focus"
        ),
        Err(_) => match set(speed) {
            Ok(()) => tracing::info!(
                "power throttling off: full speed whether or not a window has the focus"
            ),
            Err(e) => tracing::warn!("cannot turn power throttling off ({e}): Windows may slow \
                 this Runner down while another window has the focus"),
        },
    }
}

/// Tag the calling thread High Quality of Service (Windows), so a lowered
/// priority does not also mean slow cores and clocks.
#[cfg(windows)]
fn full_speed_thread() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadInformation, ThreadPowerThrottling,
        THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        THREAD_POWER_THROTTLING_STATE,
    };
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // Best effort: the process-wide setting is the main one (and is logged).
    let _ = unsafe {
        SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            std::ptr::from_ref(&state).cast(),
            std::mem::size_of_val(&state) as u32,
        )
    };
}

/// Make the calling thread the render thread.
pub fn render_thread() {
    if !enabled() {
        tracing::info!("thread priorities left alone (LCPLAY_THREAD_PRIORITY=0)");
    } else {
        #[cfg(windows)]
        raise_render_thread();
    }
}

#[cfg(windows)]
fn raise_render_thread() {
    use windows::core::w;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };
    let mut task = 0u32;
    match unsafe { AvSetMmThreadCharacteristicsW(w!("Playback"), &mut task) } {
        Ok(_) => tracing::info!("render thread: MMCSS \"Playback\""),
        Err(e) => {
            tracing::info!("render thread: no MMCSS ({e}); raising its priority instead");
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
        }
    }
}

/// Make the calling thread a background worker (decoding, loading,
/// downloading).
pub fn background_thread() {
    #[cfg(windows)]
    if enabled() {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
        // Lower priority, but not slower cores or clocks.
        full_speed_thread();
    }
}
