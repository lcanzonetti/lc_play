//! The Producer's own window: a native window whose whole content is a web
//! view showing the interface served by this same process.
//!
//! Deliberately thin. The interface does not know (or need to know) that it
//! is not in a browser; everything it does — WebSocket to the Director, HTTP
//! to the Asset Manager, drag and drop, file pickers — is ordinary web
//! platform behaviour that the system web view provides.
//!
//! Two details that are easy to get wrong:
//!
//! * **No drag-and-drop handler is installed.** The web-view library offers
//!   one for receiving dropped files natively, but on Windows installing it
//!   replaces WebView2's own handler and thereby disables HTML drag and drop
//!   inside the page — which is how assets are dragged onto the timeline.
//! * **Downloads are given a destination.** "Save…" in the interface is a
//!   browser download; a bare web view has no download UI, so the file is
//!   written to the user's Downloads folder (or the home folder).

use std::path::PathBuf;

use anyhow::Context as _;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

/// Where a file saved from the interface goes.
fn downloads_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let downloads = home.join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        home
    }
}

/// Open the window on `url` and run until it is closed. Never returns
/// normally: closing the window ends the process (and with it the server).
pub fn run(url: &str) -> anyhow::Result<()> {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("LC Play")
        .with_inner_size(LogicalSize::new(1500.0, 950.0))
        .with_min_inner_size(LogicalSize::new(900.0, 600.0))
        .build(&event_loop)
        .context("creating the application window")?;

    let builder = WebViewBuilder::new()
        .with_url(url)
        .with_download_started_handler(|_url, path| {
            // `path` arrives as the suggested file name; anchor it.
            let name = path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("show.lcplay.json"));
            *path = downloads_dir().join(name);
            tracing::info!("saving to {}", path.display());
            true
        });

    // Windows and macOS: the web view fills the window directly. Linux
    // (GTK): it is packed into the window's container.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let _webview = builder
        .build(&window)
        .context("creating the web view (on Windows this needs the WebView2 runtime)")?;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window
            .default_vbox()
            .context("the window has no GTK container")?;
        builder.build_gtk(vbox).context("creating the web view")?
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}
