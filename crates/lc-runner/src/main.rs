//! `lc-runner` — the LC Play display server.
//!
//! Three ways to run it:
//!
//! ```text
//! lc-runner --director 10.0.0.1:7401               cluster: show, media and timing from the Director
//! lc-runner --discover                             same, Director found by multicast
//! lc-runner show.json                              stand-alone: local show file, keyboard transport
//! lc-runner show.json --director 10.0.0.1:7401     local show file, timed by the Director
//! lc-runner --headless --discover                  no GPU: sync agent only (bring-up, testing)
//! ```
//!
//! Arrange displays on a virtual canvas, place content across them, let one
//! piece of content span several outputs, with per-output warp + edge blend.

use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use lc_discovery::DiscoveryConfig;
use lc_proto::{FrameRate, TransportState, NANOS_PER_SEC};
use lc_runner::agent::{Agent, AgentConfig, DirectorAddr};
use lc_runner::app::{App, ShowSource};
use lc_runner::assets::AssetCache;
use lc_runner::playhead::Playhead;
use lc_runner::show::Show;
use lc_time::{ClusterClock, LocalClock, MonotonicClock};
use winit::event_loop::EventLoop;

#[derive(Parser)]
#[command(
    version,
    about = "LC Play Runner: synchronised multi-output playback node"
)]
struct Args {
    /// Show file to render instead of the Director's show.
    show: Option<PathBuf>,
    /// Play the bundled example show, without a Director (a quick check
    /// that the graphics work).
    #[arg(long)]
    demo: bool,
    /// Start this Runner when the user logs in (`on`), or not (`off`), with
    /// the other options given here; then exit.
    #[arg(long, value_parser = ["on", "off"])]
    autostart: Option<String>,
    /// Director control address, `host:port`. The robust choice for a fixed
    /// installation.
    #[arg(long, conflicts_with = "discover")]
    director: Option<String>,
    /// Find the Director by multicast discovery instead.
    #[arg(long)]
    discover: bool,
    /// Run the cluster agent only: no window, no GPU.
    #[arg(long)]
    headless: bool,
    /// Name shown to operators (default: `runner.name` in `lcplay.json`,
    /// else "runner").
    #[arg(long)]
    name: Option<String>,
    /// The Runner's folder: downloaded media (`assets`), the last show, its
    /// identity and log. Default: `lc-runner` in the LC Play install folder
    /// (or as set in `lcplay.json` / `LCPLAY_RUNNER_DIR`).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// IPv4 address of the network interface used for discovery.
    #[arg(long, default_value = "0.0.0.0")]
    interface: Ipv4Addr,
    /// Let the operator restart and shut down this computer from the
    /// Producer. Off by default: anyone on the show network could do it.
    #[arg(long)]
    allow_power: bool,
    /// Run without a supervisor: no automatic restart after a crash, and
    /// "Restart" from the Producer only stops the Runner.
    #[arg(long)]
    no_supervise: bool,
    /// Internal: this process was started by the supervisor.
    #[arg(long, hide = true)]
    worker: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        // Also kept in memory, for the operator to read from the Producer.
        .with_writer(lc_runner::machine::LogWriter)
        .init();
    let mut args = Args::parse();
    lc_runner::machine::log_panics();
    if let Some(on) = args.autostart.take() {
        return lc_runner::machine::set_autostart(on == "on");
    }
    let name = args
        .name
        .clone()
        .or_else(|| lc_runner::folders::runner_setting("name"))
        .unwrap_or_else(|| "runner".into());
    // The log also goes to a file next to the cache, so that what happened
    // before a crash or restart can still be read from the Producer. The
    // supervisor writes there too (it reports the crashes); the Runner itself
    // reads the end of the file back when it starts.
    // Kept across restarts: see `folders.rs` for where, and why.
    let folders = lc_runner::folders::runner_folders(args.cache_dir.clone(), &name);
    let log_file = folders.state.join("runner.log");
    // A show file or --demo without a Director is a stand-alone check: no
    // supervisor (it would wait for a Start no Director can send).
    let standalone = (args.show.is_some() || args.demo)
        && args.director.is_none()
        && !args.discover
        && lc_runner::folders::runner_setting("director").is_none();
    let is_supervisor = !args.worker && !args.no_supervise && !standalone;
    lc_runner::machine::persist_log(&log_file, !is_supervisor);

    if is_supervisor {
        return supervise(&folders.state);
    }
    if args.worker {
        exit_with_supervisor();
    }
    let supervised = args.worker;

    let discovery = DiscoveryConfig {
        interface: args.interface,
        ..Default::default()
    };
    // Where the Director is: as told; else as in the settings file; else
    // find it — by its beacon, or on this computer. Only a show file or
    // --demo runs without one.
    let director = match (&args.director, args.discover) {
        (Some(addr), _) => Some(DirectorAddr::Static(resolve(addr)?)),
        (None, true) => Some(DirectorAddr::Discover(discovery)),
        (None, false) => match lc_runner::folders::runner_setting("director") {
            Some(addr) => Some(DirectorAddr::Static(resolve(&addr)?)),
            None if args.show.is_some() || args.demo => None,
            None => Some(DirectorAddr::Auto(
                discovery,
                SocketAddr::from(([127, 0, 0, 1], lc_proto::DEFAULT_CONTROL_PORT)),
            )),
        },
    };

    if args.headless {
        let director = director.context("--headless needs --director <host:port> or --discover")?;
        let mut cfg = AgentConfig::new(name, director);
        cfg.cache_dir = Some(folders.state.clone());
        cfg.announce = Some(discovery);
        cfg.allow_power = args.allow_power;
        cfg.supervised = supervised;
        return tokio::runtime::Runtime::new()?.block_on(headless(cfg));
    }

    // ---- windowed ----------------------------------------------------------
    let local_clock: Arc<dyn LocalClock> = Arc::new(MonotonicClock::new());

    // Which show? An explicit file wins; then the bundled example with
    // --demo; otherwise the Director's.
    let local_show = match (&args.show, args.demo) {
        (Some(path), _) => Some(load_show(path)?),
        // Built into the program: it works wherever it is installed.
        (None, true) => Some(
            Show::from_json(include_str!("../examples/show.json"))
                .context("the bundled example show")?,
        ),
        (None, false) => None,
    };

    // What a local show file asks for until (unless) a Director says otherwise.
    let initial = match &local_show {
        Some(show) => TransportState {
            playing: show.playback.autoplay,
            anchor_cluster_ns: local_clock.now_ns(),
            // Only wrap when looping is explicitly requested.
            loop_ns: show
                .playback
                .looping
                .then_some(show.playback.duration)
                .flatten()
                .map(|seconds| (seconds * NANOS_PER_SEC as f64) as i64)
                .filter(|&ns| ns > 0),
            ..TransportState::STOPPED
        },
        None => TransportState::STOPPED,
    };

    tracing::info!(
        "runner folder {} ({}); media in {}",
        folders.state.display(),
        folders.origin,
        folders.assets.display()
    );
    let assets_dir = folders.assets.clone();
    let runner_name = name.clone();

    // The agent lives on its own small runtime; the main thread belongs to
    // the window system (a winit requirement on some platforms). `_runtime`
    // must stay alive for as long as the window does.
    let (_runtime, playhead) = match director {
        Some(director) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("lc-agent")
                .enable_all()
                .build()?;
            let mut cfg = AgentConfig::new(name, director);
            cfg.cache_dir = Some(folders.state.clone());
            cfg.announce = Some(discovery);
            cfg.local_clock = local_clock;
            cfg.initial_transport = initial;
            cfg.renderer = true;
            cfg.allow_power = args.allow_power;
            cfg.supervised = supervised;
            let agent = runtime.block_on(async { Agent::spawn(cfg) });
            (Some(runtime), agent.playhead.clone())
        }
        None => {
            let clock = Arc::new(ClusterClock::new(local_clock));
            let playhead = Playhead::new(clock, initial, FrameRate::default());
            (None, Arc::new(playhead))
        }
    };

    let source = match local_show {
        Some(show) => ShowSource::Local(show),
        None => ShowSource::Cluster {
            runner_name,
            assets: AssetCache::new(assets_dir),
        },
    };
    let event_loop = EventLoop::new()?;
    let mut app = App::new(source, playhead);
    // This thread renders every frame: it goes before the decoders.
    lc_runner::priority::render_thread();
    event_loop.run_app(&mut app)?;
    if app.exit_code != 0 {
        std::process::exit(app.exit_code);
    }
    Ok(())
}

/// Supervise the Runner: start it as a worker process, restart it after a
/// crash or on request, and — when it was closed (its window, or Quit from
/// the Producer) — wait for the Producer's Start to open it again.
///
/// * One supervisor per Runner folder (a lock file held for its lifetime).
///   Launching `lc-runner` again while one exists asks that one to start
///   the Runner (if it is waiting) instead of making a second one.
/// * It listens for start requests on UDP 7406 — or, if another Runner on
///   this computer has that, on any free port — for its whole life, and
///   tells the Runner which, so the Director knows where to send.
/// * `Exit` from the Producer, or closing this process, ends it for good.
fn supervise(state_dir: &std::path::Path) -> anyhow::Result<()> {
    use lc_runner::machine::{EXIT_FOR_GOOD_CODE, LAUNCHER_PORT_ENV, RESTART_EXIT_CODE};
    use std::process::{Command, Stdio};

    let me: Option<lc_proto::NodeId> = read_node_id(state_dir);
    let lock_path = state_dir.join("supervisor.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .context("opening the supervisor lock")?;
    if lock.try_lock().is_err() {
        // Another supervisor runs this Runner: ask it to start the Runner
        // (harmless if it is running), and leave it at that.
        let port = std::fs::read_to_string(state_dir.join("launcher-port"))
            .ok()
            .and_then(|p| p.trim().parse::<u16>().ok());
        // Before the Runner's first run there is no identity yet; the waiting
        // supervisor then accepts any request.
        let me = me.unwrap_or_else(|| lc_proto::NodeId(String::new()));
        let sent = port.is_some_and(|port| {
            let msg = serde_json::to_vec(&lc_proto::StartRunner { start_runner: me })
                .expect("serialisable");
            std::net::UdpSocket::bind(("127.0.0.1", 0))
                .and_then(|s| s.send_to(&msg, ("127.0.0.1", port)))
                .is_ok()
        });
        if sent {
            println!(
                "This Runner is already running (or waiting to be started): asked it to start."
            );
        } else {
            println!("This Runner is already supervised by another lc-runner, which could not be reached.");
        }
        return Ok(());
    }

    let launcher = std::net::UdpSocket::bind(("0.0.0.0", lc_proto::LAUNCHER_PORT))
        .or_else(|_| std::net::UdpSocket::bind(("0.0.0.0", 0)))
        .context("listening for start requests")?;
    let port = launcher.local_addr()?.port();
    let _ = std::fs::write(state_dir.join("launcher-port"), port.to_string());

    let exe = std::env::current_exe().context("locating this program")?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut backoff = Duration::from_secs(1);
    // Runs that failed within seconds of starting, in a row: a fault that a
    // restart will not cure (a show file that does not parse, a missing
    // GPU). After a few, stop restarting and wait for Start instead.
    let mut quick_failures = 0;
    loop {
        let started = std::time::Instant::now();
        let mut child = Command::new(&exe)
            .args(&args)
            .arg("--worker")
            .env(LAUNCHER_PORT_ENV, port.to_string())
            .stdin(Stdio::piped())
            .spawn()
            .context("starting the runner")?;
        // Keep the write end open for as long as we live.
        let _tie = child.stdin.take();
        let status = child.wait().context("waiting for the runner")?;
        match status.code() {
            Some(EXIT_FOR_GOOD_CODE) => {
                tracing::info!("supervisor: exiting on the operator's request");
                return Ok(());
            }
            // Closed (its window, or Quit from the Producer): stay here and
            // wait to be started again from the Producer.
            Some(0) => {
                wait_for_start(&launcher, me.as_ref().or(read_node_id(state_dir).as_ref()));
                backoff = Duration::from_secs(1);
            }
            Some(RESTART_EXIT_CODE) => {
                tracing::info!("supervisor: restarting the runner");
                backoff = Duration::from_secs(1);
            }
            _ => {
                if started.elapsed() < Duration::from_secs(5) {
                    quick_failures += 1;
                } else {
                    quick_failures = 0;
                }
                if quick_failures >= 3 {
                    tracing::error!(
                        "supervisor: the runner failed at start {quick_failures} times in a row \
                         ({status}); not restarting it again until Start (see the log above)"
                    );
                    quick_failures = 0;
                    wait_for_start(&launcher, me.as_ref().or(read_node_id(state_dir).as_ref()));
                    backoff = Duration::from_secs(1);
                    continue;
                }
                // A runner that ran for a while and then died gets restarted
                // quickly; one that dies at once, more and more slowly.
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
                tracing::error!(
                    "supervisor: the runner stopped unexpectedly ({status}); restarting in {} s",
                    backoff.as_secs()
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// This Runner's node id, as it saved it.
fn read_node_id(state_dir: &std::path::Path) -> Option<lc_proto::NodeId> {
    std::fs::read(state_dir.join("node_id.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// The Runner has been closed: wait until a start request for it arrives
/// (`lc_proto::StartRunner`, for this Runner's node id).
fn wait_for_start(socket: &std::net::UdpSocket, me: Option<&lc_proto::NodeId>) {
    tracing::info!(
        "supervisor: the runner is closed; it can be started again from the Producer \
         (Runners window → Start). Close this window to stop it for good."
    );
    let mut buf = [0u8; 512];
    // Requests that arrived while the Runner was running are stale.
    let _ = socket.set_nonblocking(true);
    while socket.recv_from(&mut buf).is_ok() {}
    let _ = socket.set_nonblocking(false);
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let Ok(req) = serde_json::from_slice::<lc_proto::StartRunner>(&buf[..n]) else {
                    continue;
                };
                // Without a known identity (never connected yet), any
                // request for this machine will do.
                if me.is_none_or(|id| *id == req.start_runner) {
                    tracing::info!("supervisor: start requested from {from}");
                    return;
                }
            }
            // E.g. an oversized datagram (Windows): ignore and go on.
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// In a supervised Runner: exit when the supervisor goes away (its end of
/// our stdin closes).
fn exit_with_supervisor() {
    std::thread::spawn(|| {
        let mut buf = [0u8; 64];
        let mut stdin = std::io::stdin();
        loop {
            match std::io::Read::read(&mut stdin, &mut buf) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(_) => {}
            }
        }
    });
}

/// Read and validate a local show file.
fn load_show(path: &std::path::Path) -> anyhow::Result<Show> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading show file '{}'", path.display()))?;
    let mut show = Show::from_json(&text)
        .with_context(|| format!("parsing show file '{}'", path.display()))?;
    // Media paths in a show are relative to the show file, not the shell.
    show.resolve_paths(path.parent().unwrap_or(".".as_ref()));
    tracing::info!(
        canvas = format!("{}x{}", show.canvas.width, show.canvas.height),
        outputs = show.outputs.len(),
        layers = show.layers.len(),
        "loaded show '{}'",
        path.display()
    );
    Ok(show)
}

/// Resolve `host:port` (name or literal address) to one socket address.
fn resolve(addr: &str) -> anyhow::Result<SocketAddr> {
    addr.to_socket_addrs()
        .with_context(|| format!("resolving director address '{addr}' (expected host:port)"))?
        .next()
        .with_context(|| format!("'{addr}' did not resolve to any address"))
}

/// Agent only: follow the Director and log what a renderer would show.
async fn headless(cfg: AgentConfig) -> anyhow::Result<()> {
    let agent = Agent::spawn(cfg);
    tracing::info!("headless runner {} started", agent.node_id);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let pos = agent.playhead.position();
                let sync = agent.playhead.clock.quality();
                tracing::info!(
                    "{} | {} frame {} ({:.2}s) | sync {} offset-error {}µs rtt {}µs drift {:+.1}ppm",
                    if agent.playhead.is_connected() { "online" } else { "HOLDOVER" },
                    if pos.transport.playing { "playing" } else { "paused" },
                    pos.frame,
                    pos.show_seconds(),
                    if sync.locked { "locked" } else { "unlocked" },
                    sync.error_ns / 1000,
                    sync.rtt_ns / 1000,
                    sync.drift_ppm,
                );
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    agent.shutdown().await;
    Ok(())
}
