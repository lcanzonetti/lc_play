//! # lc-runner — the display server
//!
//! A Runner is two halves joined by one shared object:
//!
//! ```text
//!   network ──▶ [ agent ] ──writes──▶ ( Playhead ) ◀──reads── [ renderer ] ──▶ displays
//! ```
//!
//! * [`agent`] — cluster side: Director connection, clock sync, show and
//!   transport cache, health reports. Async (tokio), no GPU.
//! * [`playhead`] — "what show time / frame is it now?", valid with or
//!   without a Director.
//! * [`stage`], [`assets`] — turn the cluster's show document into what this
//!   Runner draws, fetching media into a local cache.
//! * [`app`], [`render`], [`content`], [`video`], [`show`] — the renderer:
//!   canvas → outputs with warp and edge blend (wgpu + winit); video frames
//!   from an `ffmpeg` child process.
//!
//! The split is what makes playback resilient: the renderer depends only on
//! the playhead, never on the network.

pub mod agent;
pub mod app;
pub mod assets;
pub mod content;
#[cfg(windows)]
pub mod dxgi;
pub mod folders;
pub mod hap;
pub mod machine;
pub mod mov;
#[cfg(windows)]
pub mod nvapi;
pub mod playhead;
pub mod render;
pub mod show;
pub mod stage;
pub mod testpattern;
pub mod video;
pub mod vsync;
