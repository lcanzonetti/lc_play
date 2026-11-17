//! # lc-net — the control channel, client side
//!
//! A typed wrapper around one WebSocket connection to a Director: connect,
//! say `Hello`, receive `Welcome`, then exchange [`ClientMessage`] /
//! [`ServerMessage`] values. Runners, the Producer and `lc-ctl` all use it;
//! the Director's server side lives in `lc-director` (it is the only server).
//!
//! The connection object is deliberately dumb: no reconnection and no
//! background task. *When* to reconnect and what to do meanwhile is policy
//! and differs per service (a Runner keeps playing in holdover; `lc-ctl`
//! just exits), so it stays with the caller.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use lc_proto::{ClientMessage, Hello, ServerMessage, Welcome, PROTOCOL_VERSION};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Time allowed for TCP connect + WebSocket upgrade + `Welcome`. Short: on a
/// LAN anything slower means the Director is not there.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("timed out")]
    Timeout,
    #[error("connection closed")]
    Closed,
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("protocol version mismatch: director speaks {director}, this node {ours}")]
    VersionMismatch { director: u32, ours: u32 },
    #[error("director refused the connection: {0}")]
    Refused(String),
    #[error("unexpected reply to hello: {0}")]
    UnexpectedReply(String),
}

/// One established control connection.
pub struct Connection {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// WebSocket URL of a Director's control endpoint.
pub fn control_url(addr: std::net::SocketAddr) -> String {
    format!("ws://{addr}/ws")
}

impl Connection {
    /// Connect to `url` (see [`control_url`]), introduce ourselves and wait
    /// for the Director's `Welcome`.
    ///
    /// `hello` is a closure, called only once the connection is up and
    /// immediately before the message is sent. A Runner's `Hello` carries its
    /// current cluster time (for timebase adoption); building it before the
    /// TCP and WebSocket handshakes would make that time stale by however
    /// long they took.
    pub async fn connect(
        url: &str,
        hello: impl FnOnce() -> Hello,
    ) -> Result<(Connection, Welcome), NetError> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            let (ws, _response) = tokio_tungstenite::connect_async(url).await?;
            let mut conn = Connection { ws };
            conn.send(&ClientMessage::Hello(hello())).await?;
            match conn.recv().await? {
                ServerMessage::Welcome(w) if w.protocol == PROTOCOL_VERSION => Ok((conn, w)),
                ServerMessage::Welcome(w) => Err(NetError::VersionMismatch {
                    director: w.protocol,
                    ours: PROTOCOL_VERSION,
                }),
                ServerMessage::Error { message, .. } => Err(NetError::Refused(message)),
                other => Err(NetError::UnexpectedReply(format!("{other:?}"))),
            }
        })
        .await
        .map_err(|_| NetError::Timeout)?
    }

    pub async fn send(&mut self, msg: &ClientMessage) -> Result<(), NetError> {
        let text = serde_json::to_string(msg)?;
        self.ws.send(Message::text(text)).await?;
        Ok(())
    }

    /// Next message from the Director. WebSocket-level pings are answered by
    /// the library; non-text frames are skipped.
    ///
    /// Cancel-safe: dropping the future between frames loses nothing, so it
    /// can be used directly in `tokio::select!`.
    pub async fn recv(&mut self) -> Result<ServerMessage, NetError> {
        loop {
            match self.ws.next().await {
                None => return Err(NetError::Closed),
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(text.as_str())?),
                Some(Ok(Message::Close(_))) => return Err(NetError::Closed),
                Some(Ok(_)) => continue,
            }
        }
    }

    /// [`recv`](Self::recv) with a deadline.
    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<ServerMessage, NetError> {
        tokio::time::timeout(timeout, self.recv())
            .await
            .map_err(|_| NetError::Timeout)?
    }

    /// This end's address: the network interface the Director is reached
    /// through.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        match self.ws.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.local_addr().ok(),
            _ => None,
        }
    }

    /// Close politely. Errors are ignored: the peer may already be gone.
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}
