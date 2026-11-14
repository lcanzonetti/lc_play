//! The Runner **agent**: everything that connects a Runner to the cluster.
//!
//! The agent finds the Director, keeps a control connection to it, keeps the
//! cluster clock locked, receives transport states and the show, reports
//! health — and writes all of it into the shared [`Playhead`] that the
//! renderer reads.
//!
//! ## Resilience rules
//!
//! 1. **The agent can only make things better.** The renderer never waits
//!    for it. If the agent has nothing new, the playhead keeps running on
//!    what it has.
//! 2. **Losing the Director changes nothing on screen.** The session ends,
//!    the agent starts reconnecting with back-off, and the playhead holds
//!    over on the last transport state and the clock's drift estimate.
//! 3. **A restart resumes from disk.** Show and transport are cached on
//!    every change, so a Runner that reboots with no Director on the network
//!    comes back with the last show instead of a black screen.
//! 4. **Coming back is seamless when possible.** On reconnect the agent
//!    tells the Director which timebase and transport it is running, so a
//!    restarted Director can adopt them (see `lc-director`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lc_discovery::DiscoveryConfig;
use lc_net::Connection;
use lc_proto::{
    Beacon, ClientMessage, FrameRate, Health, Hello, NodeId, Role, RunnerHello, ServerMessage,
    TransportState, Welcome, PROTOCOL_VERSION,
};
use lc_time::{ClusterClock, LocalClock, MonotonicClock};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, Instant, MissedTickBehavior};

use crate::playhead::{Playhead, ShowDoc};

/// Health report / keepalive period. The Director drops connections that are
/// silent for 6 s, so this leaves room for several lost messages.
const REPORT_PERIOD: Duration = Duration::from_secs(1);
/// If the Director says nothing for this long (it answers every keepalive),
/// the link is dead even if TCP has not noticed: reconnect.
const DIRECTOR_SILENCE: Duration = Duration::from_secs(5);
/// Reconnect back-off bounds.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// How long one discovery attempt listens for a Director beacon.
const DISCOVERY_WAIT: Duration = Duration::from_secs(5);

/// How the agent locates its Director.
#[derive(Debug, Clone)]
pub enum DirectorAddr {
    /// A fixed control address — the robust choice for an installation.
    Static(SocketAddr),
    /// Listen for the Director's multicast beacon.
    Discover(DiscoveryConfig),
}

pub struct AgentConfig {
    pub name: String,
    pub director: DirectorAddr,
    /// Where show, transport and identity are cached. `None` = no cache (the
    /// Runner then forgets everything on restart).
    pub cache_dir: Option<PathBuf>,
    /// Announce this Runner on the network so tools can list it.
    pub announce: Option<DiscoveryConfig>,
    /// The Runner's local clock. Tests substitute a skewed one.
    pub local_clock: Arc<dyn LocalClock>,
    /// Transport used when there is nothing cached and no Director yet.
    pub initial_transport: TransportState,
    pub frame_rate: FrameRate,
}

impl AgentConfig {
    pub fn new(name: impl Into<String>, director: DirectorAddr) -> Self {
        AgentConfig {
            name: name.into(),
            director,
            cache_dir: None,
            announce: None,
            local_clock: Arc::new(MonotonicClock::new()),
            initial_transport: TransportState::STOPPED,
            frame_rate: FrameRate::default(),
        }
    }
}

/// A running agent. Dropping it does not stop the tasks; call
/// [`shutdown`](Agent::shutdown).
pub struct Agent {
    pub node_id: NodeId,
    pub playhead: Arc<Playhead>,
    tasks: Vec<JoinHandle<()>>,
}

impl Agent {
    /// Build the playhead (restoring the cache) and start the network tasks.
    /// Must be called inside a tokio runtime.
    pub fn spawn(cfg: AgentConfig) -> Agent {
        let cache = cfg.cache_dir.clone().map(Cache::new);
        let node_id = cache
            .as_ref()
            .map(Cache::node_id)
            .unwrap_or_else(NodeId::random);

        let clock = Arc::new(ClusterClock::new(cfg.local_clock.clone()));
        let cached_transport = cache.as_ref().and_then(Cache::load_transport);
        let initial = match cached_transport {
            // Cold boot from cache. The saved anchor refers to a clock that
            // died with the previous process, so re-anchor at "now" in local
            // time, at the position of the last transport change. If the show
            // was playing it plays again: screens come back alive on their
            // own after a power cut, and a Director that appears later
            // overrides this state anyway.
            Some(saved) => TransportState {
                anchor_cluster_ns: clock.now_ns(),
                ..saved
            },
            None => cfg.initial_transport,
        };
        let playhead = Arc::new(Playhead::new(clock.clone(), initial, cfg.frame_rate));
        if let Some(show) = cache.as_ref().and_then(Cache::load_show) {
            tracing::info!("restored show revision {} from cache", show.revision);
            playhead.set_show(show);
        }

        let (sync_target, sync_rx) = watch::channel(None);
        let mut tasks = vec![tokio::spawn(lc_time::net::run_sync_client(clock, sync_rx))];

        if let Some(discovery) = cfg.announce {
            let (id, name, playhead) = (node_id.clone(), cfg.name.clone(), playhead.clone());
            tasks.push(tokio::spawn(lc_discovery::announce(
                discovery,
                Arc::new(move || {
                    let mut b = Beacon::new(id.clone(), Role::Runner, name.clone());
                    b.clock_id = playhead.clock.clock_id();
                    b
                }),
            )));
        }

        let link = Link {
            node_id: node_id.clone(),
            name: cfg.name,
            director: cfg.director,
            playhead: playhead.clone(),
            cache,
            sync_target,
        };
        tasks.push(tokio::spawn(link.run()));

        Agent {
            node_id,
            playhead,
            tasks,
        }
    }

    /// Stop all network activity. The playhead stays valid (and keeps
    /// advancing) for whoever still holds it.
    pub async fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// The control-connection task and what it needs.
struct Link {
    node_id: NodeId,
    name: String,
    director: DirectorAddr,
    playhead: Arc<Playhead>,
    cache: Option<Cache>,
    /// Points the sync client at the current Director.
    sync_target: watch::Sender<Option<SocketAddr>>,
}

impl Link {
    /// Connect, serve the session, reconnect — forever.
    async fn run(self) {
        let mut backoff = BACKOFF_MIN;
        let mut announced_failure = false;
        loop {
            let Some(addr) = self.locate().await else {
                continue;
            };
            match Connection::connect(&lc_net::control_url(addr), || self.hello()).await {
                Ok((conn, welcome)) => {
                    tracing::info!("connected to director '{}' at {addr}", welcome.name);
                    backoff = BACKOFF_MIN;
                    announced_failure = false;
                    let reason = self.session(conn, welcome, addr).await;
                    self.playhead.set_connected(false);
                    tracing::warn!(
                        "lost director ({reason}); holding over on the last known state"
                    );
                }
                Err(e) => {
                    // Say it once, then stay quiet: a Runner may wait for its
                    // Director for hours and must not flood the log.
                    if !announced_failure {
                        tracing::warn!("cannot reach director at {addr}: {e}; retrying");
                        announced_failure = true;
                    } else {
                        tracing::debug!("cannot reach director at {addr}: {e}");
                    }
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    /// Resolve the Director's control address.
    async fn locate(&self) -> Option<SocketAddr> {
        match &self.director {
            DirectorAddr::Static(addr) => Some(*addr),
            DirectorAddr::Discover(cfg) => {
                match lc_discovery::find(*cfg, Role::Director, DISCOVERY_WAIT).await {
                    Ok(Some(found)) => found.control_addr(),
                    Ok(None) => None, // nothing heard; listen again
                    Err(e) => {
                        tracing::warn!("discovery unavailable: {e}");
                        sleep(BACKOFF_MAX).await;
                        None
                    }
                }
            }
        }
    }

    /// What we tell the Director about ourselves — including the timebase
    /// and transport we are still running, so it can adopt them.
    fn hello(&self) -> Hello {
        let reading = self.playhead.clock.read(self.playhead.clock.local_now_ns());
        // Only offer a timebase we actually hold: a clock id plus at least
        // one measurement of it, with the transport expressed in it.
        let holding = reading.synced
            && reading.clock_id.is_some()
            && reading.clock_id == self.playhead.timeline_clock();
        Hello {
            protocol: PROTOCOL_VERSION,
            node_id: self.node_id.clone(),
            role: Role::Runner,
            name: self.name.clone(),
            runner: Some(RunnerHello {
                show_revision: self.playhead.show().map(|s| s.revision),
                clock_id: reading.clock_id.filter(|_| holding),
                cluster_now_ns: holding.then_some(reading.cluster_ns),
                transport: holding.then(|| self.playhead.transport()),
            }),
        }
    }

    /// Serve one established connection. Returns why it ended.
    async fn session(&self, mut conn: Connection, welcome: Welcome, addr: SocketAddr) -> String {
        let playhead = &self.playhead;

        // Order matters. `follow` first: from here the playhead holds the
        // picture if the timebase changed. Then the transport of that
        // timebase, then the sync target; the first sample releases the hold.
        playhead.clock.follow(welcome.clock_id);
        playhead.set_frame_rate(welcome.frame_rate);
        if playhead.accept(welcome.clock_id, welcome.transport) {
            self.save_transport();
        }
        let _ = self
            .sync_target
            .send(Some(SocketAddr::new(addr.ip(), welcome.sync_port)));
        playhead.set_connected(true);

        let mut report = interval(REPORT_PERIOD);
        report.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_heard = Instant::now();

        loop {
            tokio::select! {
                incoming = conn.recv() => {
                    let msg = match incoming {
                        Ok(msg) => msg,
                        Err(e) => return e.to_string(),
                    };
                    last_heard = Instant::now();
                    match msg {
                        ServerMessage::Transport { clock_id, state } => {
                            if clock_id != welcome.clock_id {
                                // The Director switched timebase under us.
                                // Start over so the handshake re-establishes
                                // clock and transport together.
                                return "director changed timebase".into();
                            }
                            if playhead.accept(clock_id, state) {
                                self.save_transport();
                            }
                        }
                        ServerMessage::Show { revision, document } => {
                            tracing::info!("received show revision {revision}");
                            let show = ShowDoc { revision, document };
                            if let Some(cache) = &self.cache {
                                cache.save_show(&show);
                            }
                            playhead.set_show(show);
                        }
                        ServerMessage::Error { message } => {
                            tracing::warn!("director reported: {message}");
                        }
                        ServerMessage::Pong | ServerMessage::Cluster(_) | ServerMessage::Welcome(_) => {}
                    }
                }

                _ = report.tick() => {
                    playhead.prune();
                    let health = ClientMessage::Health(self.health());
                    // Ping makes the Director answer, which is how *we*
                    // notice a dead link (see DIRECTOR_SILENCE).
                    for msg in [health, ClientMessage::Ping] {
                        if let Err(e) = conn.send(&msg).await {
                            return e.to_string();
                        }
                    }
                }

                _ = tokio::time::sleep_until(last_heard + DIRECTOR_SILENCE) => {
                    return "director stopped answering".into();
                }
            }
        }
    }

    fn health(&self) -> Health {
        let pos = self.playhead.position();
        Health {
            cluster_ns: pos.cluster_ns,
            show_ns: pos.show_ns,
            frame: pos.frame,
            transport_epoch: pos.transport.epoch,
            show_revision: self.playhead.show().map(|s| s.revision),
            sync: self.playhead.clock.quality(),
            fps: self.playhead.fps(),
        }
    }

    fn save_transport(&self) {
        if let Some(cache) = &self.cache {
            cache.save_transport(&self.playhead.transport());
        }
    }
}

/// The Runner's on-disk cache: identity, last show, last transport.
///
/// Files are small and written rarely (on operator actions), so plain
/// synchronous I/O is used; each is replaced atomically via rename so a power
/// cut leaves either the old or the new file, never half of one. Failures are
/// logged, never fatal: a Runner with a read-only disk must still play.
struct Cache {
    dir: PathBuf,
}

impl Cache {
    fn new(dir: PathBuf) -> Self {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!("cannot create cache dir {}: {e}", dir.display());
        }
        Cache { dir }
    }

    fn read<T: serde::de::DeserializeOwned>(&self, file: &str) -> Option<T> {
        let path = self.dir.join(file);
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes)
            .map_err(|e| tracing::error!("ignoring corrupt cache file {}: {e}", path.display()))
            .ok()
    }

    fn write<T: serde::Serialize>(&self, file: &str, value: &T) {
        let result = (|| -> anyhow::Result<()> {
            let tmp = self.dir.join(format!("{file}.tmp"));
            std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
            std::fs::rename(&tmp, self.dir.join(file))?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::error!(
                "cannot write cache file {file} in {}: {e}",
                self.dir.display()
            );
        }
    }

    /// This Runner's identity: created on first start, then kept, so the
    /// Director recognises the node (and its display assignment) after a
    /// restart.
    fn node_id(&self) -> NodeId {
        self.read::<NodeId>("node_id.json").unwrap_or_else(|| {
            let id = NodeId::random();
            self.write("node_id.json", &id);
            id
        })
    }

    fn load_show(&self) -> Option<ShowDoc> {
        self.read("show.json")
    }

    fn save_show(&self, show: &ShowDoc) {
        self.write("show.json", show);
    }

    fn load_transport(&self) -> Option<TransportState> {
        self.read("transport.json")
    }

    fn save_transport(&self, state: &TransportState) {
        self.write("transport.json", state);
    }
}

/// Convenience for binaries: the default cache location under `base`.
pub fn default_cache_dir(base: &Path, name: &str) -> PathBuf {
    base.join(format!("runner-{name}"))
}
