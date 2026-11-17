//! The Director's network front end: one WebSocket endpoint for the control
//! channel, plus two plain HTTP endpoints for scripts and quick checks.
//!
//! | Endpoint        | Purpose |
//! |-----------------|---------|
//! | `GET /ws`       | control channel (see `lc_proto::message`) |
//! | `GET /status`   | cluster status as JSON — `curl` friendly |
//! | `POST /command` | one `Command` as JSON — for scripts and simple integrations |
//!
//! Each WebSocket connection is served by its own task. Tasks share nothing
//! but the [`Core`]; state changes reach them through a broadcast channel, so
//! one slow or dead client can never delay delivery to the others.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use lc_proto::{ClientMessage, ClusterStatus, Command, Role, ServerMessage, PROTOCOL_VERSION};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};

use crate::Core;

/// A client must introduce itself within this time of connecting.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// A connection that sends nothing for this long is considered dead and is
/// dropped. Clients send `Health` or `Ping` at least every 2 s. This is what
/// notices a pulled network cable, which TCP alone may not report for minutes.
const IDLE_TIMEOUT: Duration = Duration::from_secs(6);
/// How often Producers/controllers get an unsolicited cluster status.
const STATUS_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct AppState {
    core: Arc<Core>,
    shutdown: watch::Receiver<bool>,
}

/// Serve until `shutdown` turns true and every connection has closed.
pub async fn serve(listener: TcpListener, core: Arc<Core>, shutdown: watch::Receiver<bool>) {
    let mut stop = shutdown.clone();
    let app = Router::new()
        .route("/ws", get(upgrade))
        .route("/status", get(status))
        .route("/command", post(command))
        .with_state(AppState { core, shutdown });
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { stopped(&mut stop).await })
    .await;
    if let Err(e) = result {
        tracing::error!("control server stopped: {e}");
    }
}

/// Resolve once shutdown has been requested (or the Director is gone).
///
/// A function of its own so the `watch` borrow guard returned by `wait_for`
/// is dropped here and never held across an `.await` in a caller.
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopped| *stopped).await;
}

async fn status(State(app): State<AppState>) -> Json<ClusterStatus> {
    Json(app.core.status())
}

#[derive(serde::Deserialize)]
struct CommandQuery {
    /// Target timeline (`?timeline=<task id>`); absent = the main timeline.
    #[serde(default)]
    timeline: Option<String>,
}

async fn command(
    State(app): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<CommandQuery>,
    Json(command): Json<Command>,
) -> impl IntoResponse {
    match app.core.command_on(command, query.timeline.as_deref()) {
        Ok(()) => (StatusCode::OK, Json(app.core.status())).into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}

async fn upgrade(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(reason) = connection(socket, app, addr).await {
            tracing::debug!("connection {addr} ended: {reason}");
        }
    })
}

async fn send(socket: &mut WebSocket, msg: &ServerMessage) -> Result<(), String> {
    let text = serde_json::to_string(msg).map_err(|e| e.to_string())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|e| format!("send failed: {e}"))
}

/// Why reading a client message failed.
enum ReadError {
    /// The frame arrived but is not a valid message: the sender's bug. The
    /// link itself is fine.
    Malformed(String),
    /// The connection is broken.
    Link(String),
}

impl ReadError {
    fn into_message(self) -> String {
        match self {
            ReadError::Malformed(m) | ReadError::Link(m) => m,
        }
    }
}

/// Read frames until the next text frame and parse it. `Ok(None)` means the
/// peer closed the connection.
async fn next_message(socket: &mut WebSocket) -> Result<Option<ClientMessage>, ReadError> {
    loop {
        match socket.recv().await {
            None | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Err(e)) => return Err(ReadError::Link(format!("receive failed: {e}"))),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(text.as_str())
                    .map(Some)
                    .map_err(|e| ReadError::Malformed(format!("malformed message: {e}")));
            }
            // Ping/pong are handled by axum; binary frames are not part of
            // the protocol.
            Some(Ok(_)) => continue,
        }
    }
}

/// Serve one control connection from handshake to close.
async fn connection(mut socket: WebSocket, app: AppState, addr: SocketAddr) -> Result<(), String> {
    let AppState { core, mut shutdown } = app;

    // ---- handshake -------------------------------------------------------
    let hello = match timeout(HELLO_TIMEOUT, next_message(&mut socket)).await {
        Ok(Ok(Some(ClientMessage::Hello(hello)))) => hello,
        Ok(Ok(Some(_))) => {
            let _ = send(&mut socket, &error("the first message must be hello")).await;
            return Err("no hello".into());
        }
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => {
            let e = e.into_message();
            let _ = send(&mut socket, &error(&e)).await;
            return Err(e);
        }
        Err(_) => return Err("no hello within the timeout".into()),
    };
    if hello.protocol != PROTOCOL_VERSION {
        let msg = format!(
            "protocol version mismatch: director speaks {PROTOCOL_VERSION}, '{}' speaks {}",
            hello.name, hello.protocol
        );
        let _ = send(&mut socket, &error(&msg)).await;
        return Err(msg);
    }

    // Subscribe *before* reading the state for Welcome: a change that slips
    // in between is then delivered twice rather than never, and epochs make
    // duplicates harmless.
    let mut events = core.subscribe();
    let (conn, welcome, show) = core.register(&hello, addr);
    tracing::info!("{:?} '{}' connected from {addr}", hello.role, hello.name);

    let result = async {
        send(&mut socket, &ServerMessage::Welcome(welcome)).await?;
        if let Some(show) = show {
            send(&mut socket, &show_message(show)).await?;
        }
        if let Some(grid) = core.grid() {
            send(&mut socket, &ServerMessage::Grid { grid }).await?;
        }

        // Runners do not need the cluster overview; everyone else gets it.
        let wants_status = hello.role != Role::Runner;
        // First tick one period from now, not immediately: a client that
        // connects, sends a command and asks for the status must get the
        // status it asked for, not an unsolicited one built before its
        // command was processed.
        let mut status_tick = interval_at(Instant::now() + STATUS_PERIOD, STATUS_PERIOD);
        status_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_heard = Instant::now();

        // ---- main loop ---------------------------------------------------
        loop {
            tokio::select! {
                _ = stopped(&mut shutdown) => {
                    let _ = socket.send(Message::Close(None)).await;
                    return Ok(());
                }

                event = events.recv() => match event {
                    // Logs and pictures from Runners are for the operator,
                    // not for the other Runners.
                    // Nor are other users' presences.
                    Ok(ServerMessage::RunnerLogs { .. }
                        | ServerMessage::RunnerPreview { .. }
                        | ServerMessage::Presence { .. })
                        if hello.role == Role::Runner => {}
                    Ok(msg) => send(&mut socket, &msg).await?,
                    // This client fell more than a channel's worth behind.
                    // Transport is state, not history: the current one is
                    // all it needs to be correct again.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for (timeline, state) in core.transports() {
                            let msg = ServerMessage::Transport {
                                clock_id: core.clock.clock_id(),
                                state,
                                timeline,
                            };
                            send(&mut socket, &msg).await?;
                        }
                        // The same goes for the show: one of the dropped
                        // messages may have been a new revision. Re-sending
                        // is rare (only after a lag) and idempotent.
                        if let Some(show) = core.show() {
                            send(&mut socket, &show_message(show)).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },

                _ = status_tick.tick(), if wants_status => {
                    send(&mut socket, &ServerMessage::Cluster(core.status())).await?;
                }

                _ = tokio::time::sleep_until(last_heard + IDLE_TIMEOUT) => {
                    return Err("idle timeout".into());
                }

                incoming = next_message(&mut socket) => {
                    last_heard = Instant::now();
                    match incoming {
                        Ok(None) => return Ok(()),
                        // A message we cannot parse is the sender's bug, not
                        // a reason to drop a Runner mid-show: report it.
                        Err(ReadError::Malformed(e)) => send(&mut socket, &error(&e)).await?,
                        Err(ReadError::Link(e)) => return Err(e),
                        Ok(Some(ClientMessage::Ping)) => send(&mut socket, &ServerMessage::Pong).await?,
                        Ok(Some(ClientMessage::Health(health))) => core.health(conn, health),
                        Ok(Some(ClientMessage::Presence(presence))) => core.presence(conn, presence),
                        Ok(Some(ClientMessage::Logs { lines })) => core.runner_logs(conn, lines),
                        Ok(Some(ClientMessage::Preview { outputs })) => core.runner_preview(conn, outputs),
                        Ok(Some(ClientMessage::Hello(_))) => {
                            send(&mut socket, &error("hello was already sent")).await?
                        }
                        Ok(Some(ClientMessage::Command { command, timeline })) => {
                            let wants_reply = matches!(command, Command::Status);
                            match core.command_on(command, timeline.as_deref()) {
                                Ok(()) if wants_reply => {
                                    send(&mut socket, &ServerMessage::Cluster(core.status())).await?
                                }
                                Ok(()) => {}
                                Err(message) => send(&mut socket, &error(&message)).await?,
                            }
                        }
                    }
                }
            }
        }
    }
    .await;

    core.unregister(conn);
    tracing::info!("{:?} '{}' disconnected", hello.role, hello.name);
    result
}

fn show_message(show: crate::ShowDoc) -> ServerMessage {
    ServerMessage::Show {
        revision: show.revision,
        id: show.id,
        document: show.document,
    }
}

fn error(message: &str) -> ServerMessage {
    ServerMessage::Error {
        message: message.to_string(),
    }
}
