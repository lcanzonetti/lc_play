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
    /// This computer's name: tells the page which Runner is on the
    /// operator's own machine (its primary screen is left alone).
    hostname: String,
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
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
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
        .route("/config.json", get(move || async move { Json(config) }))
        .route("/local/remote-desktop", axum::routing::post(remote_desktop));
    let app = app.into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("producer server stopped: {e}");
    }
}

#[derive(serde::Deserialize)]
struct RemoteDesktop {
    host: String,
    /// `rdp` (Windows Remote Desktop) or `vnc`.
    #[serde(default = "rdp")]
    kind: String,
}

fn rdp() -> String {
    "rdp".into()
}

/// `POST /local/remote-desktop?host=<ip>&kind=rdp|vnc` — open this
/// machine's remote-desktop client on a Runner's computer.
///
/// Only for the operator sitting at this machine, and only from the
/// Producer's own page:
///
/// * the connection must come from this machine (not from the network);
/// * the `Host` header must name this machine, so a hostile web site cannot
///   reach the endpoint by pointing a DNS name at 127.0.0.1;
/// * the request must carry the header `X-LCPlay: 1`. A browser sends a
///   cross-origin request with a custom header only after asking the server
///   (CORS preflight), which this server never approves — so another web
///   page open in the operator's browser cannot trigger this;
/// * the host to connect to must be a plain address or host name.
async fn remote_desktop(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(req): axum::extract::Query<RemoteDesktop>,
) -> (axum::http::StatusCode, String) {
    use axum::http::StatusCode;
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            "remote desktop can only be opened from the Producer's own machine".into(),
        );
    }
    let host_header = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let host_name = host_header
        .rsplit_once(':')
        .map_or(host_header, |(h, _)| h)
        .trim_matches(['[', ']']);
    let local_host = matches!(host_name, "localhost" | "127.0.0.1" | "::1");
    let from_page = headers.get("x-lcplay").and_then(|v| v.to_str().ok()) == Some("1");
    if !local_host || !from_page {
        return (
            StatusCode::FORBIDDEN,
            "remote desktop can only be opened from the Producer's own page".into(),
        );
    }
    // A leading '-' would be read as an option by the program it is given to.
    let host_ok = !req.host.is_empty()
        && !req.host.starts_with('-')
        && req.host.len() < 256
        && req
            .host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    if !host_ok {
        return (StatusCode::BAD_REQUEST, "not a host name or address".into());
    }
    let host = req.host;
    let mut command = match (req.kind.as_str(), std::env::consts::OS) {
        ("rdp", "windows") => {
            let mut c = std::process::Command::new("mstsc");
            c.arg(format!("/v:{host}"));
            c
        }
        // Windows has no VNC viewer of its own, and no handler for vnc://
        // links (it would ask which program to use): start an installed
        // viewer directly.
        ("vnc", "windows") => match windows_vnc_viewer() {
            Some(viewer) => {
                // VNC viewers read `host:N` as display N (port 5900 + N);
                // the operator means a port, which is `host::port`. A
                // bracketed IPv6 address is passed without brackets.
                let target = match host.rsplit_once("]:").or_else(|| {
                    (host.matches(':').count() == 1)
                        .then(|| host.split_once(':'))
                        .flatten()
                }) {
                    Some((h, port)) => format!("{}::{port}", h.trim_start_matches('[')),
                    None => host.trim_matches(['[', ']']).to_string(),
                };
                let mut c = std::process::Command::new(viewer);
                c.arg(target);
                c
            }
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "No VNC viewer found on this computer. Install one (RealVNC Viewer, \
                     TightVNC, TigerVNC or UltraVNC), or set LCPLAY_VNC_VIEWER to its \
                     program file. For Windows runners, RDP needs no extra software."
                        .into(),
                );
            }
        },
        (kind @ ("rdp" | "vnc"), os) => {
            let url = if kind == "rdp" {
                format!("rdp://full%20address=s:{host}")
            } else {
                format!("vnc://{host}")
            };
            // (Windows is handled above, for both kinds.)
            let mut c = std::process::Command::new(if os == "macos" { "open" } else { "xdg-open" });
            c.arg(url);
            c
        }
        _ => return (StatusCode::BAD_REQUEST, "kind must be rdp or vnc".into()),
    };
    match command.spawn() {
        Ok(mut child) => {
            // Reap it when it ends, so it does not linger as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            (StatusCode::OK, "opened".into())
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot start the remote-desktop client: {e}"),
        ),
    }
}

/// An installed VNC viewer on Windows: `LCPLAY_VNC_VIEWER` if set, else the
/// usual install locations of the common viewers.
fn windows_vnc_viewer() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("LCPLAY_VNC_VIEWER") {
        return Some(path.into());
    }
    let roots = ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(std::path::PathBuf::from);
    let candidates = [
        r"RealVNC\VNC Viewer\vncviewer.exe",
        r"TightVNC\tvnviewer.exe",
        r"TigerVNC\vncviewer.exe",
        r"uvnc bvba\UltraVNC\vncviewer.exe",
        r"UltraVNC\vncviewer.exe",
    ];
    roots
        .flat_map(|root| candidates.iter().map(move |c| root.join(c)))
        .find(|p| p.is_file())
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
