//! # lc-director — the coordinator
//!
//! The Director is the one place where the cluster's truth lives:
//!
//! * **Cluster time.** Its monotonic clock *is* cluster time
//!   ([`lc_time::MasterClock`]), served to Runners over UDP.
//! * **Transport.** It turns operator commands into scheduled
//!   [`TransportState`]s and publishes them.
//! * **Show.** It holds the current show document and its revision, and hands
//!   it to Runners that do not have it.
//! * **Membership.** It knows which nodes are connected and how healthy they
//!   are, and reports that to Producers and controllers.
//!
//! It is deliberately *not* in the playback path. Once a Runner has the show,
//! a transport state and a clock lock, it needs nothing further from the
//! Director to keep playing — so the Director can crash, restart or be
//! unplugged mid-show without the audience noticing.
//!
//! ## Coming back after a crash: timebase adoption
//!
//! A restarted Director has a new monotonic clock, so by itself it would
//! define a *new* timebase and every Runner would have to jump onto it. To
//! avoid that visible glitch, a freshly started Director is **unanchored**:
//! if the first Runner to connect reports that it is still following a
//! timebase (in holdover), the Director adopts that timebase — clock id,
//! current cluster time and transport state — and carries on as if it had
//! never been away. See [`Core::register`].
//!
//! ## Structure
//!
//! * [`Core`]: all state behind one mutex, and the pure-ish command logic.
//!   Used directly by the HTTP/WebSocket layer and, later, by the external
//!   control gateway (OSC/MIDI/Art-Net/timecode map to the same `command`).
//! * [`server`]: the axum WebSocket/HTTP front end, one task per connection.
//! * [`Director`]: wires sockets, tasks and persistence together.

pub mod osc;
pub mod server;
mod store;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Context as _;
use lc_discovery::DiscoveryConfig;
use lc_proto::show::{ControlAction, ControlCue, ShowDocument, MAIN_TIMELINE};
use lc_proto::{
    Beacon, ClockId, ClusterStatus, Command, FrameRate, Health, Hello, NodeId, NodeStatus, Role,
    RunnerHello, ServerMessage, TransportState, Welcome, NANOS_PER_SEC, PROTOCOL_VERSION,
};
use lc_time::{
    next_state, LocalClock, MasterClock, MonotonicClock, TransportChange, TransportTimeline,
};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::JoinHandle;

pub use store::{Persisted, ShowDoc};

/// How far ahead of a control cue the Director schedules it. Must exceed the
/// scheduler period plus the time a message needs to reach every Runner.
const CONTROL_LOOKAHEAD_NS: i64 = 300_000_000;
/// How often the control-cue scheduler runs.
const CONTROL_PERIOD: Duration = Duration::from_millis(50);

/// Cluster time at which `state` (in force, playing forward, currently at
/// `position`) reaches `cue` — or `None` if it will not (the cue is behind
/// the playhead and the timeline does not loop).
fn cue_fire_time(state: &TransportState, position: i64, cue: &ControlCue, now: i64) -> Option<i64> {
    let at_show = (cue.at_s * NANOS_PER_SEC as f64).round() as i64;
    let mut ahead = at_show - position;
    match state.loop_ns {
        // Looping: a cue behind the playhead comes round again — unless it
        // lies beyond the loop length and is never reached.
        Some(len) if len > 0 => {
            if at_show >= len {
                return None;
            }
            if ahead <= 0 {
                ahead += len;
            }
        }
        // A cue exactly at the playhead has just been executed (that is how
        // a pause cue leaves things); only cues strictly ahead count.
        _ if ahead <= 0 => return None,
        _ => {}
    }
    // Show time advances at `rate`; convert the distance to cluster time.
    let cluster = (ahead as i128 * 1000 / state.rate_permille as i128) as i64;
    Some(now + cluster)
}

/// Fastest/slowest playback rate accepted (±16×).
const MAX_RATE_PERMILLE: u32 = 16_000;

pub struct DirectorConfig {
    pub name: String,
    /// TCP address of the control channel. Port 0 picks a free port (tests).
    pub control_addr: SocketAddr,
    /// UDP address of the clock-sync responder.
    pub sync_addr: SocketAddr,
    /// How far in the future transport changes are anchored. Must exceed the
    /// worst-case time for a control message to reach every Runner; 100 ms is
    /// generous on a LAN and still feels immediate to an operator.
    pub lead: Duration,
    pub frame_rate: FrameRate,
    /// Where to persist state across restarts. `None` = keep nothing.
    pub state_dir: Option<PathBuf>,
    /// UDP address to accept OSC show-control messages on. `None` = off.
    pub osc_addr: Option<SocketAddr>,
    /// Multicast announcement. `None` = do not announce (static addressing).
    pub discovery: Option<DiscoveryConfig>,
    /// The clock that defines cluster time. Tests substitute a skewed one.
    pub local_clock: Arc<dyn LocalClock>,
}

impl Default for DirectorConfig {
    fn default() -> Self {
        DirectorConfig {
            name: "director".into(),
            control_addr: SocketAddr::from(([0, 0, 0, 0], lc_proto::DEFAULT_CONTROL_PORT)),
            sync_addr: SocketAddr::from(([0, 0, 0, 0], lc_proto::DEFAULT_SYNC_PORT)),
            lead: Duration::from_millis(100),
            frame_rate: FrameRate::default(),
            state_dir: None,
            osc_addr: None,
            discovery: Some(DiscoveryConfig::default()),
            local_clock: Arc::new(MonotonicClock::new()),
        }
    }
}

/// The Director's state and logic, independent of any socket.
pub struct Core {
    pub id: NodeId,
    pub name: String,
    pub clock: Arc<MasterClock>,
    lead_ns: i64,
    frame_rate: FrameRate,
    sync_port: u16,
    state: Mutex<State>,
    /// Fan-out of state changes to every connection task.
    events: broadcast::Sender<ServerMessage>,
    /// Wakes the persistence task after a change worth saving.
    dirty: Notify,
}

struct State {
    /// Transport of the main timeline.
    timeline: TransportTimeline,
    /// Transports of the auxiliary timelines ("tasks"), by timeline id. An
    /// entry appears when a task is first commanded; a task without one is
    /// inactive.
    tasks: BTreeMap<String, TransportTimeline>,
    show: Option<ShowDoc>,
    /// The show, parsed: the timelines and control cues the Director itself
    /// needs to know about.
    parsed: Option<Arc<ShowDocument>>,
    /// Control cues already scheduled: (timeline, cue id) → the cluster time
    /// they were scheduled for. Keeps a cue from firing on every scheduler
    /// pass while its moment approaches.
    fired: HashMap<(String, String), i64>,
    /// Connected nodes by connection id (a node may briefly have two
    /// connections while it reconnects).
    nodes: BTreeMap<u64, NodeStatus>,
    next_conn: u64,
    /// False until this Director has committed to a timebase; see module docs.
    anchored: bool,
    /// Timebase this Director served before its last shutdown, if known. Only
    /// that one may be adopted (when nothing is known, any may).
    previous_clock: Option<ClockId>,
    /// The transport exactly as it was saved, anchors and all. Meaningless
    /// in a new timebase, but valid again if `previous_clock` is adopted.
    saved_transport: Option<TransportState>,
    saved_tasks: BTreeMap<String, TransportState>,
}

impl Core {
    fn new(cfg: &DirectorConfig, sync_port: u16, persisted: Option<Persisted>) -> Self {
        // Zero is reserved for "no timebase" on the sync wire.
        let clock_id = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        let clock = Arc::new(MasterClock::new(cfg.local_clock.clone(), clock_id));

        let saved_transport = persisted.as_ref().map(|p| p.transport);
        let saved_tasks = persisted
            .as_ref()
            .map(|p| p.tasks.clone())
            .unwrap_or_default();
        let parsed = persisted
            .as_ref()
            .and_then(|p| p.show.as_ref())
            .and_then(|s| serde_json::from_value(s.document.clone()).ok())
            .map(Arc::new);
        // Like the main transport below: paused where they were, in the new
        // timebase, unless a Runner hands the old one back.
        let restored_tasks = saved_tasks
            .iter()
            .map(|(id, t)| {
                let paused = TransportState {
                    playing: false,
                    anchor_cluster_ns: clock.now_ns(),
                    ..*t
                };
                (id.clone(), TransportTimeline::new(paused))
            })
            .collect();
        let (id, transport, show, previous_clock) = match persisted {
            Some(p) => {
                // The saved anchors refer to a clock that no longer exists.
                // Unless a Runner hands the old timebase back (adoption), the
                // safe reading of the saved state is "paused where it last
                // changed", in the new timebase. The epoch is kept as it was,
                // so a Runner still executing that state is at least as new.
                let restored = TransportState {
                    playing: false,
                    anchor_cluster_ns: clock.now_ns(),
                    ..p.transport
                };
                (p.node_id, restored, p.show, Some(p.clock_id))
            }
            None => (NodeId::random(), TransportState::STOPPED, None, None),
        };

        Core {
            id,
            name: cfg.name.clone(),
            clock,
            lead_ns: cfg.lead.as_nanos() as i64,
            frame_rate: cfg.frame_rate,
            sync_port,
            state: Mutex::new(State {
                timeline: TransportTimeline::new(transport),
                tasks: restored_tasks,
                show,
                parsed,
                fired: HashMap::new(),
                nodes: BTreeMap::new(),
                next_conn: 0,
                anchored: false,
                previous_clock,
                saved_transport,
                saved_tasks,
            }),
            events: broadcast::channel(64).0,
            dirty: Notify::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // State changes are small and complete under the lock; a panic
        // elsewhere must not take the whole Director down with a poison error.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerMessage> {
        self.events.subscribe()
    }

    fn publish(&self, msg: ServerMessage) {
        // No receivers (nobody connected) is not an error.
        let _ = self.events.send(msg);
    }

    /// Admit a node. Returns its connection id, the `Welcome` to send, and
    /// the show to push if the node's cached copy is missing or outdated.
    pub fn register(&self, hello: &Hello, addr: SocketAddr) -> (u64, Welcome, Option<ShowDoc>) {
        let mut st = self.state();

        if hello.role == Role::Runner {
            if !st.anchored {
                if let Some(runner) = &hello.runner {
                    self.try_adopt(&mut st, runner, &hello.name);
                }
            }
            // From the first Runner on there is a cluster to keep consistent:
            // the timebase is fixed for the lifetime of this process.
            st.anchored = true;

            // A Runner on our timebase may hold a transport state newer than
            // ours: the Director crashed before saving it, or adopted from a
            // Runner that had missed the last change. Take it and tell
            // everyone — otherwise that Runner would stay on its own state
            // and silently ignore our next commands as "older".
            if let Some(runner) = &hello.runner {
                let same_timebase = runner.clock_id == Some(self.clock.clock_id());
                if let Some(state) = runner.transport.filter(|_| same_timebase) {
                    if st.timeline.push(state) {
                        tracing::info!(
                            "runner '{}' holds a newer transport (epoch {}); taking it over",
                            hello.name,
                            state.epoch
                        );
                        self.publish(ServerMessage::Transport {
                            clock_id: self.clock.clock_id(),
                            state,
                            timeline: None,
                        });
                        self.dirty.notify_one();
                    }
                }
            }
        }

        let conn = st.next_conn;
        st.next_conn += 1;
        st.nodes.insert(
            conn,
            NodeStatus {
                node_id: hello.node_id.clone(),
                role: hello.role,
                name: hello.name.clone(),
                addr: addr.to_string(),
                health: None,
                clock_skew_ns: None,
                frame_error: None,
            },
        );

        let now = self.clock.now_ns();
        let welcome = Welcome {
            protocol: PROTOCOL_VERSION,
            director_id: self.id.clone(),
            name: self.name.clone(),
            clock_id: self.clock.clock_id(),
            sync_port: self.sync_port,
            lead_ns: self.lead_ns,
            frame_rate: self.frame_rate,
            // The newest state, even if it has not taken effect yet: the
            // client's own timeline will switch to it at the anchor.
            transport: st.timeline.latest(),
            tasks: st
                .tasks
                .iter()
                .map(|(id, tl)| (id.clone(), tl.latest()))
                .collect(),
            show_revision: st.show.as_ref().map(|s| s.revision),
        };
        st.timeline.prune(now);

        // Compared by id, not revision: revisions restart when a Director
        // loses its state, ids never repeat.
        let cached = hello.runner.as_ref().and_then(|r| r.show_id.as_deref());
        let show = st
            .show
            .as_ref()
            .filter(|s| match hello.role {
                Role::Runner => cached != Some(s.id.as_str()),
                // The Producer edits the show, so it always starts from the
                // current one.
                Role::Producer => true,
                _ => false,
            })
            .cloned();
        (conn, welcome, show)
    }

    /// Take over the timebase (and transport) a Runner kept alive.
    fn try_adopt(&self, st: &mut State, runner: &RunnerHello, runner_name: &str) {
        let (Some(clock_id), Some(cluster_now)) = (runner.clock_id, runner.cluster_now_ns) else {
            return; // the Runner is not following any timebase
        };
        if st.previous_clock.is_some_and(|prev| prev != clock_id) {
            tracing::warn!(
                "runner '{runner_name}' follows timebase {clock_id:#x}, which is not the one this \
                 Director served before; not adopting it"
            );
            return;
        }
        // `cluster_now` is the Runner's estimate when it sent Hello, so it is
        // late by one network transit (well under a millisecond on a LAN).
        // Every Runner then slews onto this clock, so that error is shared
        // and invisible.
        self.clock.adopt(clock_id, cluster_now);
        // Which transport continues? The Runner's, or — if this is the very
        // timebase we served before — the one we saved, whose anchors are
        // valid again now. The newer of the two wins.
        let own_saved = st
            .saved_transport
            .filter(|_| st.previous_clock == Some(clock_id));
        let newest = [runner.transport, own_saved]
            .into_iter()
            .flatten()
            .max_by_key(|t| t.epoch);
        match newest {
            Some(transport) => st.timeline.replace(transport),
            None => {
                // Keep our own state, but its anchor was written against the
                // pre-adoption clock: re-anchor it "now" in the adopted one.
                let mut own = st.timeline.latest();
                own.anchor_show_ns = own.show_time_at(own.anchor_cluster_ns);
                own.anchor_cluster_ns = cluster_now;
                st.timeline.replace(own);
            }
        }
        // The auxiliary timelines continue the same way: for each, the newer
        // of what the Runner is executing and what we saved.
        let own_tasks = if st.previous_clock == Some(clock_id) {
            st.saved_tasks.clone()
        } else {
            BTreeMap::new()
        };
        let ids: Vec<&String> = runner.tasks.keys().chain(own_tasks.keys()).collect();
        let mut tasks = BTreeMap::new();
        for id in ids {
            let newest = [runner.tasks.get(id), own_tasks.get(id)]
                .into_iter()
                .flatten()
                .max_by_key(|t| t.epoch);
            if let Some(state) = newest {
                tasks.insert(id.clone(), TransportTimeline::new(*state));
            }
        }
        st.tasks = tasks;

        tracing::info!(
            "adopted timebase {clock_id:#x} from runner '{runner_name}' (transport epoch {})",
            st.timeline.latest().epoch
        );
        self.publish(ServerMessage::Transport {
            clock_id,
            state: st.timeline.latest(),
            timeline: None,
        });
        for (id, tl) in &st.tasks {
            self.publish(ServerMessage::Transport {
                clock_id,
                state: tl.latest(),
                timeline: Some(id.clone()),
            });
        }
        self.dirty.notify_one();
    }

    pub fn unregister(&self, conn: u64) {
        self.state().nodes.remove(&conn);
    }

    /// Record a Runner's health report, and judge it: is its clock where
    /// ours is, and is it on the frame the transport prescribes?
    pub fn health(&self, conn: u64, health: Health) {
        // Values from the wire: never let arithmetic on them overflow.
        let skew = health.cluster_ns.saturating_sub(self.clock.now_ns());
        let mut st = self.state();
        // Only comparable while the Runner executes the state we would pick
        // for that instant (around a transport change it may be one behind).
        let expected = st.timeline.state_at(health.cluster_ns);
        let frame_error = (expected.epoch == health.transport_epoch).then(|| {
            health.frame.saturating_sub(
                self.frame_rate
                    .frame_at(expected.show_time_at(health.cluster_ns)),
            )
        });
        if let Some(node) = st.nodes.get_mut(&conn) {
            node.clock_skew_ns = Some(skew);
            node.frame_error = frame_error;
            node.health = Some(health);
        }
    }

    /// Execute an operator command on the main timeline. See
    /// [`command_on`](Self::command_on).
    pub fn command(&self, command: Command) -> Result<(), String> {
        self.command_on(command, None)
    }

    /// Execute an operator command. This is the single entry point for every
    /// control surface (WebSocket, HTTP, OSC, control cues).
    ///
    /// `timeline` selects which transport a transport command acts on:
    /// `None` or `"main"` is the main timeline, anything else the id of an
    /// auxiliary timeline of the current show.
    pub fn command_on(&self, command: Command, timeline: Option<&str>) -> Result<(), String> {
        let timeline = timeline.filter(|t| !t.is_empty() && *t != MAIN_TIMELINE);
        let change = match command {
            Command::Play => TransportChange::Play,
            Command::Pause => TransportChange::Pause,
            // Stopping the main timeline rewinds it; stopping a task also
            // takes it off stage.
            Command::Stop => TransportChange::Stop {
                deactivate: timeline.is_some(),
            },
            Command::Seek { show_ns } if show_ns >= 0 => TransportChange::Seek(show_ns),
            Command::Seek { .. } => return Err("seek: show time must not be negative".into()),
            Command::SetRate { rate_permille }
                if rate_permille.unsigned_abs() <= MAX_RATE_PERMILLE =>
            {
                TransportChange::SetRate(rate_permille)
            }
            Command::SetRate { .. } => {
                return Err(format!(
                    "set_rate: |rate| must not exceed {MAX_RATE_PERMILLE} permille"
                ))
            }
            Command::SetLoop { loop_ns } => TransportChange::SetLoop(loop_ns),
            Command::LoadShow { document } => return self.load_show(document),
            Command::RunnerAction { runner, action } => return self.runner_action(&runner, action),
            // Answered by the connection that asked; nothing to change here.
            Command::Status => return Ok(()),
        };

        let mut st = self.state();
        // Anchor in the future so every Runner holds the new state before it
        // applies.
        let at = self.clock.now_ns() + self.lead_ns;
        self.apply(&mut st, timeline, change, at)?;
        st.anchored = true;
        Ok(())
    }

    /// Give `timeline` a new transport state: `change`, taking effect at
    /// cluster time `at`. Publishes it and returns it.
    fn apply(
        &self,
        st: &mut State,
        timeline: Option<&str>,
        change: TransportChange,
        at: i64,
    ) -> Result<TransportState, String> {
        let now = self.clock.now_ns();
        let tl = match timeline {
            None => &mut st.timeline,
            Some(id) => {
                let known = st
                    .parsed
                    .as_ref()
                    .is_some_and(|doc| doc.tasks.iter().any(|t| t.id == id));
                if !known {
                    return Err(format!("unknown timeline '{id}'"));
                }
                // A task's transport comes into being the first time it is
                // commanded; until then it is simply inactive.
                st.tasks
                    .entry(id.to_string())
                    .or_insert_with(|| TransportTimeline::new(TransportState::INACTIVE))
            }
        };
        // Derive from the state that will be in force *then*, so two commands
        // issued within one lead time compose correctly.
        let base = tl.state_at(at);
        let mut next = next_state(&base, change, at);
        // Epochs must increase across *all* known states, not just from `base`.
        next.epoch = tl.latest().epoch + 1;
        tl.push(next);
        tl.prune(now);

        self.publish(ServerMessage::Transport {
            clock_id: self.clock.clock_id(),
            state: next,
            timeline: timeline.map(str::to_string),
        });
        self.dirty.notify_one();
        Ok(next)
    }

    /// Forward an action to the Runner called (or identified as) `runner`.
    fn runner_action(&self, runner: &str, action: lc_proto::RunnerAction) -> Result<(), String> {
        let targets: Vec<NodeId> = self
            .state()
            .nodes
            .values()
            .filter(|n| n.role == Role::Runner && (n.name == runner || n.node_id.0 == runner))
            .map(|n| n.node_id.clone())
            .collect();
        if targets.is_empty() {
            return Err(format!("no connected runner is called '{runner}'"));
        }
        for node_id in targets {
            self.publish(ServerMessage::RunnerAction { node_id, action });
        }
        Ok(())
    }

    /// Fire the control cues that are about to be reached.
    ///
    /// Called a few times a second. For every playing timeline it looks a
    /// short way ahead; a control cue inside that window is turned into a
    /// transport change anchored at the **exact** cluster instant the
    /// playhead will reach the cue. Because the change is published before
    /// that instant, every Runner executes it on the same frame — a "pause"
    /// cue stops all outputs precisely on the cue, not roughly near it.
    ///
    /// Control cues are executed here, by the Director. While it is down
    /// they do not fire: Runners keep playing through them.
    pub fn run_control_cues(&self) {
        let mut st = self.state();
        let Some(doc) = st.parsed.clone() else {
            return;
        };
        let now = self.clock.now_ns();
        st.fired.retain(|_, at| *at > now - NANOS_PER_SEC);

        let main = (None, &doc.control_cues);
        let tasks = doc
            .tasks
            .iter()
            .map(|t| (Some(t.id.as_str()), &t.control_cues));
        for (timeline, cues) in std::iter::once(main).chain(tasks) {
            let state = match timeline {
                None => st.timeline.latest(),
                Some(id) => match st.tasks.get(id) {
                    Some(tl) => tl.latest(),
                    None => continue,
                },
            };
            // Only a state that is in force, playing forward, has a future
            // to look into. (A pending state is looked at once it applies.)
            if !state.active
                || !state.playing
                || state.rate_permille <= 0
                || state.anchor_cluster_ns > now
            {
                continue;
            }
            let position = state.show_time_at(now);
            for cue in cues {
                let Some(at) = cue_fire_time(&state, position, cue, now) else {
                    continue;
                };
                if at - now > CONTROL_LOOKAHEAD_NS {
                    continue;
                }
                let key = (
                    timeline.unwrap_or(MAIN_TIMELINE).to_string(),
                    cue.id.clone(),
                );
                if st.fired.contains_key(&key) {
                    continue;
                }
                st.fired.insert(key, at);

                // Which timeline the cue acts on.
                let target = match cue.target.as_str() {
                    "" => timeline,
                    MAIN_TIMELINE => None,
                    other => Some(other),
                };
                let change = match cue.action {
                    ControlAction::Play => TransportChange::Play,
                    ControlAction::Pause => TransportChange::Pause,
                    ControlAction::Stop => TransportChange::Stop {
                        deactivate: target.is_some(),
                    },
                    ControlAction::Jump => {
                        TransportChange::Seek((cue.to_s * NANOS_PER_SEC as f64).round() as i64)
                    }
                };
                match self.apply(&mut st, target, change, at) {
                    Ok(_) => tracing::info!(
                        "control cue: {:?} {} (at {:.2} s on {})",
                        cue.action,
                        target.unwrap_or(MAIN_TIMELINE),
                        cue.at_s,
                        timeline.unwrap_or(MAIN_TIMELINE),
                    ),
                    Err(e) => tracing::warn!("control cue at {:.2} s: {e}", cue.at_s),
                }
            }
        }
    }

    fn load_show(&self, document: serde_json::Value) -> Result<(), String> {
        if !document.is_object() {
            return Err("load_show: the show document must be a JSON object".into());
        }
        // Refuse what no Runner could render, here, with a message for the
        // operator — instead of distributing it and failing on every node.
        // (Unknown fields are allowed and preserved: the document is stored
        // and forwarded as received.)
        let parsed: ShowDocument =
            serde_json::from_value(document.clone()).map_err(|e| format!("load_show: {e}"))?;
        parsed.validate().map_err(|e| format!("load_show: {e}"))?;
        let mut st = self.state();
        // A task that no longer exists has no transport either.
        st.tasks
            .retain(|id, _| parsed.tasks.iter().any(|t| &t.id == id));
        st.parsed = Some(Arc::new(parsed));
        let revision = st.show.as_ref().map_or(1, |s| s.revision + 1);
        // A fresh id per load: unlike the revision counter it can never
        // collide with a show some Runner cached from an earlier life of
        // this Director.
        let id = uuid::Uuid::new_v4().to_string();
        st.show = Some(ShowDoc {
            revision,
            id: id.clone(),
            document: document.clone(),
        });
        drop(st);
        self.publish(ServerMessage::Show {
            revision,
            id,
            document,
        });
        self.dirty.notify_one();
        Ok(())
    }

    /// Current cluster overview.
    pub fn status(&self) -> ClusterStatus {
        let st = self.state();
        let now = self.clock.now_ns();
        let show_ns = st.timeline.show_time_at(now);
        ClusterStatus {
            clock_id: self.clock.clock_id(),
            cluster_ns: now,
            frame_rate: self.frame_rate,
            transport: st.timeline.state_at(now),
            tasks: st
                .tasks
                .iter()
                .map(|(id, tl)| (id.clone(), tl.state_at(now)))
                .collect(),
            show_ns,
            frame: self.frame_rate.frame_at(show_ns),
            show_revision: st.show.as_ref().map(|s| s.revision),
            nodes: st.nodes.values().cloned().collect(),
        }
    }

    /// The latest state of every timeline: the main one (`None`) first, then
    /// each task by id.
    pub fn transports(&self) -> Vec<(Option<String>, TransportState)> {
        let st = self.state();
        std::iter::once((None, st.timeline.latest()))
            .chain(
                st.tasks
                    .iter()
                    .map(|(id, tl)| (Some(id.clone()), tl.latest())),
            )
            .collect()
    }

    /// The latest transport state (in force or pending).
    pub fn transport(&self) -> TransportState {
        self.state().timeline.latest()
    }

    /// The current show, if any.
    pub fn show(&self) -> Option<ShowDoc> {
        self.state().show.clone()
    }

    fn snapshot(&self) -> Persisted {
        let st = self.state();
        // Until this Director has committed to a timebase, what is on disk
        // (the timebase and transport of its previous life) is still the
        // best record there is, and the only thing that lets it adopt a
        // cluster that kept running. Do not overwrite it with the
        // provisional clock id and paused transport of a start that no
        // Runner ever reached.
        let live_tasks = || {
            st.tasks
                .iter()
                .map(|(id, tl)| (id.clone(), tl.latest()))
                .collect()
        };
        let (clock_id, transport, tasks) = if st.anchored {
            (self.clock.clock_id(), st.timeline.latest(), live_tasks())
        } else {
            (
                st.previous_clock.unwrap_or_else(|| self.clock.clock_id()),
                st.saved_transport.unwrap_or_else(|| st.timeline.latest()),
                st.saved_tasks.clone(),
            )
        };
        Persisted {
            node_id: self.id.clone(),
            clock_id,
            transport,
            tasks,
            show: st.show.clone(),
        }
    }

    fn beacon(&self, control_port: u16) -> Beacon {
        let mut b = Beacon::new(self.id.clone(), Role::Director, self.name.clone());
        b.control_port = Some(control_port);
        b.sync_port = Some(self.sync_port);
        b.clock_id = Some(self.clock.clock_id());
        b
    }
}

/// A running Director: the [`Core`] plus its network tasks.
pub struct Director {
    core: Arc<Core>,
    control_addr: SocketAddr,
    sync_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    server: JoinHandle<()>,
    persist: Option<JoinHandle<()>>,
}

impl Director {
    /// Bind the sockets and start serving.
    pub async fn start(cfg: DirectorConfig) -> anyhow::Result<Director> {
        let persisted = match &cfg.state_dir {
            Some(dir) => store::load(dir)?,
            None => None,
        };

        let sync_socket = UdpSocket::bind(cfg.sync_addr)
            .await
            .with_context(|| format!("binding sync socket {}", cfg.sync_addr))?;
        let sync_addr = sync_socket.local_addr()?;
        let listener = TcpListener::bind(cfg.control_addr)
            .await
            .with_context(|| format!("binding control socket {}", cfg.control_addr))?;
        let control_addr = listener.local_addr()?;

        let core = Arc::new(Core::new(&cfg, sync_addr.port(), persisted));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let mut tasks = Vec::new();

        tasks.push(tokio::spawn(lc_time::net::run_sync_server(
            sync_socket,
            core.clock.clone(),
        )));

        if let Some(discovery) = cfg.discovery {
            let core = core.clone();
            let port = control_addr.port();
            tasks.push(tokio::spawn(lc_discovery::announce(
                discovery,
                Arc::new(move || core.beacon(port)),
            )));
        }

        let persist = cfg.state_dir.clone().map(|dir| {
            let core = core.clone();
            let mut stop = shutdown_rx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = core.dirty.notified() => {}
                        _ = stop.changed() => break,
                    }
                    if let Err(e) = store::save(&dir, &core.snapshot()).await {
                        tracing::error!("cannot persist director state: {e:#}");
                    }
                }
                // Final save so a clean shutdown never loses the last change.
                if let Err(e) = store::save(&dir, &core.snapshot()).await {
                    tracing::error!("cannot persist director state: {e:#}");
                }
            })
        });

        if let Some(addr) = cfg.osc_addr {
            // OSC is an extra way in, not a requirement: a busy port (another
            // OSC application on this machine) must not keep the Director
            // from starting.
            match UdpSocket::bind(addr).await {
                Ok(socket) => {
                    tracing::info!("OSC control on udp://{}", socket.local_addr()?);
                    tasks.push(tokio::spawn(osc::run(socket, core.clone())));
                }
                Err(e) => tracing::warn!(
                    "OSC control is off: cannot bind udp://{addr}: {e} (choose another --osc-port)"
                ),
            }
        }

        {
            let core = core.clone();
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(CONTROL_PERIOD);
                loop {
                    tick.tick().await;
                    core.run_control_cues();
                }
            }));
        }

        let server = tokio::spawn(server::serve(listener, core.clone(), shutdown_rx));

        tracing::info!(
            "director '{}' up: control ws://{control_addr}/ws, sync udp://{sync_addr}, timebase {:#x}",
            core.name,
            core.clock.clock_id()
        );
        Ok(Director {
            core,
            control_addr,
            sync_addr,
            shutdown,
            tasks,
            server,
            persist,
        })
    }

    pub fn core(&self) -> &Arc<Core> {
        &self.core
    }

    pub fn control_addr(&self) -> SocketAddr {
        self.control_addr
    }

    pub fn sync_addr(&self) -> SocketAddr {
        self.sync_addr
    }

    /// Stop serving: close every connection, release the ports, flush state.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        // The server task ends once every connection task has seen the signal.
        let _ = self.server.await;
        if let Some(persist) = self.persist {
            let _ = persist.await;
        }
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
    }
}
