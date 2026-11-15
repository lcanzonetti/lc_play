//! `lc-producer` — the operator's application.
//!
//! ```text
//! lc-producer             open the Producer in its own window
//! lc-producer --browser   only serve it; open http://<host>:7404 in a browser
//! ```
//!
//! The interface itself is a web page (three files embedded at build time).
//! This binary serves it over HTTP and — unless `--browser` is given — shows
//! it in a native window of its own, using the operating system's web view
//! (WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux). The same
//! page therefore works both as a stand-alone application and, for a second
//! operator or a tablet, in any browser on the show network.
//!
//! All the work happens in the browser, which talks to the other services
//! directly:
//!
//! ```text
//!   browser ──WebSocket──▶ Director        show edits, transport, cluster status
//!   browser ──HTTP───────▶ Asset Manager   list, upload, thumbnails
//! ```
//!
//! That keeps this service stateless (the show lives in the Director) and
//! means the Producer speaks exactly the protocol documented in
//! `docs/PROTOCOL.md` — it has no private back door.

mod window;

use std::net::{IpAddr, SocketAddr};

use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use serde::Serialize;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLE_CSS: &str = include_str!("../ui/style.css");

#[derive(Parser, Clone)]
#[command(version, about = "LC Play Producer: the operator's web interface")]
struct Args {
    /// Address to serve the interface on.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,
    #[arg(long, default_value_t = 7404)]
    port: u16,
    /// Director control address as the *browser* must reach it, `host:port`.
    /// Default: port 7401 on the host the page was loaded from.
    #[arg(long)]
    director: Option<String>,
    /// Do not open a window: only serve the interface, for use in a browser.
    #[arg(long)]
    browser: bool,
    /// Asset Manager base URL as browsers *and Runners* must reach it, e.g.
    /// `http://10.0.0.1:7403`. Default: port 7403 on the host the page was
    /// loaded from — fine on one machine; on a network, give the address
    /// Runners can reach, because this URL is written into the show.
    #[arg(long)]
    assets: Option<String>,
}

/// What the page needs to know to find the other services. `None` means
/// "same host as this page, default port" and is resolved in the browser.
#[derive(Serialize, Clone)]
struct UiConfig {
    director: Option<String>,
    director_port: u16,
    assets: Option<String>,
    assets_port: u16,
    protocol: u32,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let browser_only = args.browser;

    // The HTTP server runs on its own thread: on most platforms the window
    // system insists on owning the main thread.
    let runtime = tokio::runtime::Runtime::new()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind(SocketAddr::new(
        args.bind, args.port,
    )))?;
    let local = listener.local_addr()?;
    tracing::info!("producer interface on http://{local}");
    // Where the window finds the interface: the address actually bound
    // (loopback if it is bound to all addresses).
    let host = if local.ip().is_unspecified() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        local.ip()
    };
    let url = format!("http://{}/", SocketAddr::new(host, local.port()));
    let server = runtime.spawn(serve(listener, args));

    if !browser_only {
        // Closing the window ends the process from inside `run`. If we get
        // here, the window could not be opened at all (no display, no web
        // view runtime): say so, and keep serving for a browser — the
        // operator still needs the interface.
        let failure = match std::panic::catch_unwind(|| window::run(&url)) {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "the window system could not be initialised".to_string(),
        };
        tracing::error!(
            "cannot open the Producer window: {failure}. Open {url} in a browser instead."
        );
    }
    // Serve until Ctrl-C.
    runtime.block_on(async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = server => {}
        }
    });
    Ok(())
}

/// Serve the interface on `listener` forever.
async fn serve(listener: tokio::net::TcpListener, args: Args) {
    let config = UiConfig {
        director: args.director,
        director_port: lc_proto::DEFAULT_CONTROL_PORT,
        assets: args.assets.map(|a| a.trim_end_matches('/').to_string()),
        assets_port: 7403,
        protocol: lc_proto::PROTOCOL_VERSION,
    };
    let app = Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/style.css",
            get(|| async { asset("text/css; charset=utf-8", STYLE_CSS) }),
        )
        .route("/config.json", get(move || async move { Json(config) }));
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("producer server stopped: {e}");
    }
}

/// A static file. `no-cache` so a rebuilt binary is picked up on reload
/// without a hard refresh.
fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}
