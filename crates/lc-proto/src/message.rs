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
    /// Who this Producer's user is and what they are working on, relayed to
    /// the other Producers.
    Presence(Presence),
    /// Application-level keepalive; answered with `Pong`. Lets a client detect
    /// a dead link faster than TCP would.
    Ping,
    /// A Runner's recent log, sent when asked (`RunnerAction::SendLogs`).
    Logs {
        lines: Vec<String>,
    },
    /// Small pictures of a Runner's outputs, sent when asked
    /// (`RunnerAction::SendPreview`).
    Preview {
        outputs: Vec<OutputPreview>,
    },
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
    /// Go to an absolute show time; keeps the play/pause state. A stopped
    /// auxiliary timeline comes back on stage, paused at that time.
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
        /// The sender's edit tag `{ by, n }` (see `EditShow`), recorded as
        /// applied with the show.
        #[serde(default)]
        tag: Option<serde_json::Value>,
    },
    /// Change part of the show (see [`crate::edit`]): what Producers send,
    /// so that several can edit at once. `tag` is `{ "by": <editor id>,
    /// "n": <edit number, increasing> }`: the show records the last `n`
    /// applied per editor in `_applied`, so an edit sent twice is applied
    /// once and the sender can tell which of its edits have arrived.
    EditShow {
        ops: Vec<crate::edit::EditOp>,
        #[serde(default)]
        tag: Option<serde_json::Value>,
    },
    /// Ask for an immediate [`ServerMessage::Cluster`].
    Status,
    /// Tell one Runner to do something to itself. `runner` is its name or
    /// node id.
    RunnerAction {
        runner: String,
        action: RunnerAction,
    },
    /// Switch on a Runner's computer with a Wake-on-LAN packet. Works for a
    /// Runner that has been connected before (its network card's address is
    /// remembered) and whose computer allows it (BIOS / network settings).
    WakeRunner {
        runner: String,
    },
    /// Start the Runner program again on a computer where it was closed
    /// (its supervisor keeps waiting for this). `runner` is its name or node
    /// id.
    StartRunner {
        runner: String,
    },
}

/// One output picture of a Runner, for the operator to see what is on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputPreview {
    /// The display's name.
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// PNG, base64-encoded.
    pub png_base64: String,
}

/// What a Runner can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerAction {
    /// Close and rebuild all outputs and reload all media — the remedy when
    /// a display was re-plugged or an output came up wrong.
    ReloadOutputs,
    /// Restart the Runner program.
    Restart,
    /// Stop the Runner program. Its supervisor stays, waiting for a
    /// `start_runner`.
    Quit,
    /// Stop the Runner program and its supervisor: it can only be started
    /// again on its computer.
    Exit,
    /// Close the outputs but stay connected (screens go to the desktop or
    /// dark), until `Resume`.
    Standby,
    Resume,
    /// Restart the computer. Only if the Runner was started with
    /// `--allow-power`.
    Reboot,
    /// Shut the computer down. Only with `--allow-power`.
    PowerOff,
    /// Reply with the recent log (`ClientMessage::Logs`).
    SendLogs,
    /// Reply with pictures of the outputs (`ClientMessage::Preview`).
    SendPreview,
}

/// One monitor attached to a Runner, as its window system reports it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Index to use in a display's `window.monitor`.
    pub index: usize,
    /// The operating system's primary monitor (where a display with no
    /// monitor number goes fullscreen).
    #[serde(default)]
    pub primary: bool,
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
    /// The machine, for the operator and for Wake-on-LAN.
    #[serde(default)]
    pub machine: Option<MachineInfo>,
    /// Outputs closed by `Standby`.
    #[serde(default)]
    pub standby: bool,
    /// Presentation health of each output.
    #[serde(default)]
    pub output_health: Vec<crate::OutputHealth>,
    /// The refresh of this Runner's first output, for the Director's grid.
    #[serde(default)]
    pub vblank: Option<crate::VblankReport>,
    /// Media being copied into the Runner's cache (cues using it show a
    /// placeholder until it is complete).
    #[serde(default)]
    pub downloads: Vec<AssetDownload>,
}

/// A Producer's user, as the other Producers show them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Presence {
    /// The user's name, as they typed it.
    pub name: String,
    /// CSS colour their selection is outlined in.
    pub color: String,
    /// What they have selected: `{ "type": "cue", "id": "…" }` and the like.
    #[serde(default)]
    pub selection: Option<serde_json::Value>,
    /// Whether they are dragging it right now.
    #[serde(default)]
    pub dragging: bool,
}

/// One asset on its way into a Runner's cache.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AssetDownload {
    /// The asset's file name, as the operator knows it.
    pub name: String,
    /// Bytes received so far.
    pub done: u64,
    /// Size of the file (0 = unknown).
    pub total: u64,
    /// Average rate since the download started, bytes per second.
    pub bytes_per_s: f64,
    /// Why the last attempt failed, while waiting to retry.
    #[serde(default)]
    pub error: Option<String>,
    /// Seconds until the next attempt, while waiting to retry.
    #[serde(default)]
    pub retry_in_s: Option<u64>,
}

/// Facts about a Runner's computer.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MachineInfo {
    pub hostname: String,
    /// `windows`, `linux`, `macos`.
    pub os: String,
    /// LC Play version.
    pub version: String,
    /// IP address the Runner reaches the Director from.
    pub ip: String,
    /// Hardware address of that network interface (for Wake-on-LAN).
    #[serde(default)]
    pub mac: Option<String>,
    /// Whether it accepts `Reboot` / `PowerOff`.
    pub allow_power: bool,
    /// Whether a supervisor restarts it (`Restart` brings it back).
    pub supervised: bool,
    /// UDP port its supervisor listens on for `start_runner` (default 7406).
    #[serde(default)]
    pub launcher_port: Option<u16>,
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
    /// A Runner's log, forwarded to Producers and controllers.
    RunnerLogs {
        node_id: NodeId,
        name: String,
        lines: Vec<String>,
    },
    /// A Runner's output pictures, forwarded to Producers and controllers.
    RunnerPreview {
        node_id: NodeId,
        name: String,
        outputs: Vec<OutputPreview>,
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
    /// The refresh grid frame-locked Runners render for (see
    /// [`crate::vblank`]). Sent when it changes and on connection.
    Grid {
        grid: crate::VblankGrid,
    },
    /// Another Producer's user and what they are working on; `presence:
    /// null` when they have left. Not sent to Runners.
    Presence {
        node_id: NodeId,
        presence: Option<Presence>,
    },
    /// The Director could not act on a message. Never fatal for the link.
    /// `edit`: the tag of the refused `edit_show`, so its sender knows which
    /// of its edits it was.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit: Option<serde_json::Value>,
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
    /// Every Runner that has ever connected, connected or not — so that an
    /// offline one can still be listed and woken.
    #[serde(default)]
    pub known_runners: Vec<KnownRunner>,
    /// The refresh grid in force, and the Runner it is measured on.
    #[serde(default)]
    pub grid: Option<crate::VblankGrid>,
    #[serde(default)]
    pub grid_source: Option<NodeId>,
}

/// A Runner the Director remembers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownRunner {
    pub name: String,
    pub node_id: NodeId,
    #[serde(default)]
    pub machine: Option<MachineInfo>,
    pub online: bool,
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
