//! lcmediaserver — a lightweight, scalable media server with on-the-fly
//! transcoding.
//!
//! Design goals:
//!   * **Lightweight**: a single async binary, in-memory index, no database.
//!   * **Optimized**: direct streaming uses HTTP Range for seeking and CDN
//!     caching; transcoding streams `ffmpeg` output with backpressure.
//!   * **Scalable**: fully stateless per request, so it runs as N identical
//!     replicas behind a load balancer over shared storage. A semaphore bounds
//!     concurrent transcodes per node.

mod config;
mod error;
mod handlers;
mod library;
mod state;

use axum::response::Html;
use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::state::AppState;

/// The built-in test player, embedded so the binary stays self-contained.
const PLAYER_HTML: &str = include_str!("web/player.html");

#[tokio::main]
async fn main() {
    // Logging honours RUST_LOG; defaults to `info`.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();

    let config = Config::from_env();
    tracing::info!(?config, "starting lcmediaserver");

    let library = library::from_config(&config);

    // Kick off the initial + periodic background scan.
    library::spawn_rescan(library.clone(), config.scan_interval).await;

    let cors_enabled = config.cors;
    let bind = config.bind;
    let state = AppState::new(config, library);

    let mut app = Router::new()
        .route("/", get(|| async { Html(PLAYER_HTML) }))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/library", get(handlers::browse::list_library))
        .route("/api/items/{id}", get(handlers::browse::get_item))
        .route("/stream/{id}", get(handlers::stream::stream_media))
        .route("/transcode/{id}", get(handlers::transcode::transcode_media))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    if cors_enabled {
        app = app.layer(CorsLayer::permissive());
    }

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind}: {e}"));
    tracing::info!("listening on http://{bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

/// Liveness probe: the process is up and serving. Always 200, so an
/// orchestrator never restarts a node just because storage is unreachable
/// (a restart would not fix that, and would lose the last good index).
/// `status` still says `degraded` when the last scan failed.
async fn healthz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let (_, body) = health_body(&state);
    body
}

/// Readiness probe for load balancers: 503 while the last library scan could
/// not read the media root, so traffic goes to nodes that can serve files.
async fn readyz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    health_body(&state)
}

fn health_body(state: &AppState) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    let items = state.library.len();
    match state.library.scan_error() {
        None => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "status": "ok", "items": items })),
        ),
        Some(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "status": "degraded",
                "items": items,
                "error": error,
            })),
        ),
    }
}

/// Resolve when the process receives Ctrl-C or SIGTERM, for clean shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
