//! `lc-producer` — serves the operator's interface.
//!
//! The Producer is a web application: open it in a browser on the operator's
//! machine (or any machine on the show network). This binary only *serves*
//! it — three static files embedded at build time, plus one small JSON
//! document telling the page where the Director and the Asset Manager are.
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

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

    let addr = SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("producer interface on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
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
