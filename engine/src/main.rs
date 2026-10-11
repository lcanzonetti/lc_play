//! lcmediaserver-engine (`lcme`) — a multi-output canvas playback engine.
//!
//! Arrange displays on a virtual canvas, place content across them (scaled,
//! positioned, rotated), let a single piece of content span multiple outputs,
//! and apply per-output warp + edge-blend for projection mapping.
//!
//! Usage: `lcme [path/to/show.json]`  (defaults to `examples/show.json`)

mod app;
mod clock;
mod content;
mod render;
mod show;

use anyhow::Context as _;
use winit::event_loop::EventLoop;

use crate::app::App;
use crate::show::Show;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();

    // Default to the bundled example wherever `cargo run` is invoked from.
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/examples/show.json").to_string());
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading show file '{path}'"))?;
    let mut show = Show::from_json(&text).with_context(|| format!("parsing show file '{path}'"))?;
    // Media paths in a show are relative to the show file, not the shell.
    show.resolve_paths(std::path::Path::new(&path).parent().unwrap_or(".".as_ref()));

    tracing::info!(
        canvas = format!("{}x{}", show.canvas.width, show.canvas.height),
        outputs = show.outputs.len(),
        layers = show.layers.len(),
        "loaded show '{path}'"
    );

    let event_loop = EventLoop::new()?;
    let mut app = App::new(show);
    event_loop.run_app(&mut app)?;
    Ok(())
}
