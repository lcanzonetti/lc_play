//! # lc-proto — the LC Play wire protocol
//!
//! Everything two LC Play services say to each other is defined here, and
//! nothing else is: this crate has no sockets, no clocks and no threads, so
//! every service (and any third-party tool) can depend on it cheaply.
//!
//! Three things travel on the network:
//!
//! | What | Where | Module |
//! |------|-------|--------|
//! | Discovery beacons | UDP multicast, JSON | [`beacon`] |
//! | Control + state   | WebSocket, JSON     | [`message`] |
//! | Clock sync        | UDP, fixed binary   | `lc-time` (kept next to the maths that uses it) |
//!
//! Design choices worth knowing:
//!
//! * **JSON on the control channel.** Control traffic is tiny and rare (a few
//!   messages per operator action), so the cost of text is irrelevant, while
//!   being able to read it in a browser console or `websocat` is not.
//! * **All times are integer nanoseconds (`i64`).** Floats would accumulate
//!   rounding differently on different nodes; integers make "which frame is
//!   it?" a deterministic function every Runner evaluates identically.
//!   `i64` nanoseconds cover ±292 years.
//! * **Transport is state, not events.** The Director never says "play now".
//!   It publishes a [`TransportState`]: a formula that maps cluster time to
//!   show time. A Runner that stops hearing from the Director can keep
//!   evaluating the formula forever — that is what makes playback survive a
//!   Director failure.

pub mod beacon;
pub mod message;
pub mod show;
pub mod transport;
pub mod vblank;

pub use beacon::{Beacon, DISCOVERY_GROUP, DISCOVERY_PORT};
pub use message::{
    AssetDownload, ClientMessage, ClusterStatus, Command, Health, Hello, KnownRunner, MachineInfo,
    MonitorInfo, NodeStatus, OutputPreview, RunnerAction, RunnerHello, ServerMessage, SyncQuality,
    Welcome,
};
pub use show::ShowDocument;
pub use transport::{FrameRate, TransportState};
pub use vblank::{GridSchedule, OutputHealth, VblankGrid, VblankReport};

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the messages in this crate. Peers
/// exchange it in `Hello`/`Welcome` and in beacons, and refuse to pair on a
/// mismatch rather than misinterpret each other mid-show.
pub const PROTOCOL_VERSION: u32 = 2;

/// Default TCP port of the Director's control channel (WebSocket + HTTP status).
pub const DEFAULT_CONTROL_PORT: u16 = 7401;
/// Default UDP port of the Director's clock-sync responder.
pub const DEFAULT_SYNC_PORT: u16 = 7402;

/// One nanosecond-resolution second, to keep conversions readable.
pub const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Stable identity of a node. Generated once and persisted by each service so
/// a Runner keeps its identity (and therefore its display assignment) across
/// restarts and IP changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub String);

impl NodeId {
    /// A fresh random identity (UUID v4).
    pub fn random() -> Self {
        NodeId(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which service a node is. Used in beacons and in `Hello` so the Director
/// knows what to send to whom (Runners get transport + show; Producers and
/// controllers additionally get cluster status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Director,
    Runner,
    Producer,
    Assets,
    /// A command-line or third-party controller (`lc-ctl`, a show-control bridge).
    Controller,
}

/// Identifies one *timebase*: a specific zero point of cluster time.
///
/// A Director picks a random id when it creates a timebase. Runners remember
/// it, so that when a Director comes back after a crash it can tell "these
/// Runners are still running my old timebase" (and adopt it, see
/// `lc-director`) from "these Runners know nothing". Timestamps from two
/// different `ClockId`s must never be compared.
pub type ClockId = u64;
