//! Control-channel messages (WebSocket text frames, one JSON object each).
//!
//! The channel is always opened *towards* the Director. The first frame a
//! client sends is [`ClientMessage::Hello`]; the Director answers with
//! [`ServerMessage::Welcome`] and from then on both sides talk freely.
//!
//! Enums are *internally tagged* (`{"type":"play"}`): easy to produce by hand
//! or from a script, and unknown tags fail loudly instead of being
//! misinterpreted.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ClockId, FrameRate, NodeId, Role, TransportState};

// ---------------------------------------------------------------- client → director

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(Hello),
    /// An operator action. Any role may send commands; Runners normally don't.
    Command {
        command: Command,
        /// Which timeline a transport command is for: `None` (or absent) is
        /// the main timeline, otherwise the id of an auxiliary timeline.
        #[serde(default)]
        timeline: Option<String>,
    },
    /// Periodic Runner status (1 Hz).
    Health(Health),
    /// Application-level keepalive; answered with `Pong`. Lets a client detect
    /// a dead link faster than TCP would.
    Ping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub node_id: NodeId,
    pub role: Role,
    pub name: String,
    /// Present when `role` is `Runner`.
    #[serde(default)]
    pub runner: Option<RunnerHello>,
}

/// What a Runner already knows when it (re)connects. This is how a restarted
/// Director learns that the cluster kept running without it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RunnerHello {
    /// Revision of the show in the Runner's cache, if any — the Director only
    /// re-sends the show when this differs from its own.
    #[serde(default)]
    pub show_revision: Option<u64>,
    /// Id of that cached show. This, not the revision, decides whether the
    /// cache is current: revision numbers restart if a Director loses its
    /// state, ids are unique per load.
    #[serde(default)]
    pub show_id: Option<String>,
    /// Timebase the Runner is following (still, in holdover), if any.
    #[serde(default)]
    pub clock_id: Option<ClockId>,
    /// The Runner's current estimate of cluster time in that timebase.
    #[serde(default)]
    pub cluster_now_ns: Option<i64>,
    /// Transport state the Runner is currently executing.
    #[serde(default)]
    pub transport: Option<TransportState>,
    /// …and those of the auxiliary timelines, by timeline id.
    #[serde(default)]
    pub tasks: BTreeMap<String, TransportState>,
}

/// Operator actions. Deliberately small: these are also the verbs the
/// external-control gateway (OSC, MIDI, Art-Net, timecode) will map onto.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Play,
    Pause,
    /// Main timeline: pause and return to the start. Auxiliary timeline:
    /// take it off stage (it renders nothing until played again).
    Stop,
    /// Jump to an absolute show time; keeps the play/pause state.
    Seek {
        show_ns: i64,
    },
    /// 1000 = normal speed. Keeps the current position continuous.
    SetRate {
        rate_permille: i32,
    },
    /// Loop the show at this length; `None` disables looping.
    SetLoop {
        loop_ns: Option<i64>,
    },
    /// Replace the show document. The Director assigns the next revision and
    /// distributes it to every Runner.
    LoadShow {
        document: serde_json::Value,
    },
    /// Ask for an immediate [`ServerMessage::Cluster`].
    Status,
    /// Tell one Runner to do something to itself. `runner` is its name or
    /// node id.
    RunnerAction {
        runner: String,
        action: RunnerAction,
    },
}

/// What a Runner can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerAction {
    /// Close and rebuild all outputs and reload all media — the remedy when
    /// a display was re-plugged or an output came up wrong.
    ReloadOutputs,
}

/// One monitor attached to a Runner, as its window system reports it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Index to use in a display's `window.monitor`.
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Position of its top-left corner on the Runner's desktop.
    pub x: i32,
    pub y: i32,
    pub scale: f64,
}

/// One Runner's self-reported state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    /// The Runner's estimate of cluster time when the report was made. The
    /// Director compares it with its own clock on arrival: an independent
    /// (if network-jittered) cross-check of the Runner's sync.
    pub cluster_ns: i64,
    pub show_ns: i64,
    /// Frame the Runner is presenting at `cluster_ns`.
    pub frame: i64,
    /// Epoch of the transport state it is executing.
    pub transport_epoch: u64,
    #[serde(default)]
    pub show_revision: Option<u64>,
    pub sync: SyncQuality,
    /// Measured presentation rate, when a renderer is running.
    #[serde(default)]
    pub fps: Option<f32>,
    /// The Runner's monitors (empty for a headless Runner).
    #[serde(default)]
    pub monitors: Vec<MonitorInfo>,
    /// The GPU it renders with.
    #[serde(default)]
    pub gpu: Option<String>,
    /// Number of output windows currently open.
    #[serde(default)]
    pub outputs: u32,
}

/// How well a Runner's clock is locked to the Director's.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SyncQuality {
    /// True once enough good samples agree and the residual error is small.
    pub locked: bool,
    /// Samples currently in the estimator's window.
    pub samples: u32,
    /// Current local→cluster offset.
    pub offset_ns: i64,
    /// Estimated frequency error of the local oscillator vs the Director's.
    pub drift_ppm: f64,
    /// Best (minimum) round-trip time seen in the window.
    pub rtt_ns: i64,
    /// RMS residual of the fit: the estimator's own opinion of its error.
    pub error_ns: i64,
    /// Time since the last accepted sample; grows while in holdover.
    pub holdover_ns: i64,
}

// ---------------------------------------------------------------- director → client

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome(Welcome),
    /// A new transport state. Sent to everyone on every change.
    Transport {
        clock_id: ClockId,
        state: TransportState,
        /// `None` = the main timeline, otherwise an auxiliary timeline's id.
        #[serde(default)]
        timeline: Option<String>,
    },
    /// Addressed to one Runner (the others ignore it).
    RunnerAction {
        node_id: NodeId,
        action: RunnerAction,
    },
    /// The show document. Sent to a Runner whose cached revision differs, and
    /// to everyone when a new show is loaded.
    Show {
        /// Counts loads; for humans.
        revision: u64,
        /// Unique per load; for cache comparison.
        id: String,
        document: serde_json::Value,
    },
    /// Cluster overview (Producers / controllers; 1 Hz and on request).
    Cluster(ClusterStatus),
    /// The Director could not act on a message. Never fatal for the link.
    Error {
        message: String,
    },
    Pong,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub director_id: NodeId,
    pub name: String,
    /// Timebase served by this Director.
    pub clock_id: ClockId,
    /// UDP port of the clock-sync responder, on the same host as the control
    /// channel.
    pub sync_port: u16,
    /// How far in the future the Director anchors transport changes.
    pub lead_ns: i64,
    pub frame_rate: FrameRate,
    /// Transport state in force right now.
    pub transport: TransportState,
    /// Transport states of the auxiliary timelines, by timeline id.
    #[serde(default)]
    pub tasks: BTreeMap<String, TransportState>,
    #[serde(default)]
    pub show_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterStatus {
    pub clock_id: ClockId,
    /// Director's cluster time when the status was built.
    pub cluster_ns: i64,
    pub frame_rate: FrameRate,
    pub transport: TransportState,
    /// Transport states of the auxiliary timelines, by timeline id.
    #[serde(default)]
    pub tasks: BTreeMap<String, TransportState>,
    /// Show time / frame at `cluster_ns` according to the Director.
    pub show_ns: i64,
    pub frame: i64,
    #[serde(default)]
    pub show_revision: Option<u64>,
    pub nodes: Vec<NodeStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub node_id: NodeId,
    pub role: Role,
    pub name: String,
    /// Remote address of the control connection.
    pub addr: String,
    /// Last health report (Runners only).
    #[serde(default)]
    pub health: Option<Health>,
    /// `health.cluster_ns` minus the Director's clock when the report arrived.
    /// Includes one network transit, so it over-states the real error; a
    /// value that is large or growing still means "look at this Runner".
    #[serde(default)]
    pub clock_skew_ns: Option<i64>,
    /// Frames between what the Runner reported and what the transport
    /// prescribes for the cluster time of its report. Zero means "on the
    /// right frame"; unlike `clock_skew_ns` this is not affected by how long
    /// the report took to arrive.
    #[serde(default)]
    pub frame_error: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_have_a_readable_wire_form() {
        let msg = ClientMessage::Command {
            command: Command::Seek {
                show_ns: 1_500_000_000,
            },
            timeline: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"command","command":{"type":"seek","show_ns":1500000000},"timeline":null}"#
        );
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), msg);
    }

    #[test]
    fn unit_variants_round_trip() {
        let json = serde_json::to_string(&ClientMessage::Ping).unwrap();
        assert_eq!(json, r#"{"type":"ping"}"#);
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            ClientMessage::Ping
        );
        let json = serde_json::to_string(&ServerMessage::Pong).unwrap();
        assert_eq!(json, r#"{"type":"pong"}"#);
    }

    #[test]
    fn hello_tolerates_missing_optional_fields() {
        let json =
            r#"{"type":"hello","protocol":1,"node_id":"n","role":"controller","name":"cli"}"#;
        let ClientMessage::Hello(h) = serde_json::from_str(json).unwrap() else {
            panic!("not a hello");
        };
        assert_eq!(h.runner, None);
    }

    #[test]
    fn unknown_message_types_are_rejected() {
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"explode"}"#).is_err());
    }
}
