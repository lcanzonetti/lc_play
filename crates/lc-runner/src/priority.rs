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
//! `LCPLAY_THREAD_PRIORITY=0` leaves all priorities as the system sets them.

/// Whether priorities are adjusted at all.
fn enabled() -> bool {
    std::env::var("LCPLAY_THREAD_PRIORITY").map_or(true, |v| v != "0")
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
    }
}
