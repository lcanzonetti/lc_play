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
mod optimize;
mod state;

use axum::response::Html;
use axum::routing::get;
use axum::Router;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
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
    tracing::info!(?config, "starting lc-assets");

    let library = library::from_config(&config);

    // Kick off the initial + periodic background scan.
    library::spawn_rescan(library.clone(), config.scan_interval).await;

    let cors = config.cors.clone();
    let bind = config.bind;
    let state = AppState::new(config, library);
    optimize::spawn(state.clone());

    let mut app = Router::new()
        .route("/", get(|| async { Html(PLAYER_HTML) }))
        .route("/healthz", get(healthz))
        .route("/api/library", get(handlers::browse::list_library))
        .route("/api/items/{id}", get(handlers::browse::get_item))
        .route("/stream/{id}", get(handlers::stream::stream_media))
        .route("/transcode/{id}", get(handlers::transcode::transcode_media))
        .route("/proxy/{id}", get(handlers::proxy::proxy_media))
        .route("/thumb/{id}", get(handlers::manage::thumbnail))
        .route("/optimized/{id}", get(optimize::serve))
        .route(
            "/api/optimize",
            get(optimize::get_settings).put(optimize::put_settings),
        )
        .route("/api/optimize/apply", axum::routing::post(optimize::apply))
        .route(
            "/api/items/{id}/optimize",
            axum::routing::post(optimize::redo_item),
        )
        .route(
            "/api/items/{id}",
            axum::routing::delete(handlers::manage::delete_item),
        )
        .route(
            "/api/upload",
            // Media files are large; the default 2 MB body limit is for forms.
            axum::routing::post(handlers::manage::upload)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // Two lines of defence against pages other than the Producer:
    //
    // 1. Requests that change something (upload, delete) are refused outright
    //    when they carry an `Origin` the policy does not allow. CORS alone
    //    would not do this: a browser *sends* a "simple" cross-origin POST and
    //    merely hides the response from the page.
    // 2. The CORS headers tell browsers which pages may read responses.
    let policy = cors.clone();
    app = app.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let policy = policy.clone();
            async move {
                let changes_state = !matches!(
                    *request.method(),
                    axum::http::Method::GET
                        | axum::http::Method::HEAD
                        | axum::http::Method::OPTIONS
                );
                let foreign = request
                    .headers()
                    .get(axum::http::header::ORIGIN)
                    .is_some_and(|o| !o.to_str().is_ok_and(|o| policy.allows(o)));
                if changes_state && foreign {
                    return axum::response::IntoResponse::into_response((
                        axum::http::StatusCode::FORBIDDEN,
                        "this origin may not modify the library (see LCMS_CORS)",
                    ));
                }
                next.run(request).await
            }
        },
    ));
    if cors != config::CorsPolicy::Off {
        let policy = cors.clone();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(move |origin, _| {
                    origin.to_str().is_ok_and(|o| policy.allows(o))
                }))
                .allow_methods(Any)
                .allow_headers(Any),
        );
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

/// Liveness/readiness probe. Reports the current item count.
async fn healthz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "items": state.library.len(),
    }))
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
