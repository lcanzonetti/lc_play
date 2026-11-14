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
//! * [`app`], [`render`], [`content`], [`show`] — the renderer: canvas →
//!   outputs with warp and edge blend (wgpu + winit).
//!
//! The split is what makes playback resilient: the renderer depends only on
//! the playhead, never on the network.

pub mod agent;
pub mod app;
pub mod content;
pub mod playhead;
pub mod render;
pub mod show;
