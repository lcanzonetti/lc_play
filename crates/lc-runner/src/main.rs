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
    /// Show file to render instead of the Director's show. Without a
    /// Director and without a file, the bundled example is used.
    show: Option<PathBuf>,
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
    /// Name shown to operators.
    #[arg(long, default_value = "runner")]
    name: String,
    /// Directory for the show/transport cache and this node's identity.
    /// Without it the Runner forgets everything on restart.
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
    let args = Args::parse();

    if !args.worker && !args.no_supervise {
        return supervise();
    }
    if args.worker {
        exit_with_supervisor();
    }
    let supervised = args.worker;

    let discovery = DiscoveryConfig {
        interface: args.interface,
        ..Default::default()
    };
    let director = match (&args.director, args.discover) {
        (Some(addr), _) => Some(DirectorAddr::Static(resolve(addr)?)),
        (None, true) => Some(DirectorAddr::Discover(discovery)),
        (None, false) => None,
    };

    if args.headless {
        let director = director.context("--headless needs --director <host:port> or --discover")?;
        let mut cfg = AgentConfig::new(args.name, director);
        cfg.cache_dir = args.cache_dir;
        cfg.announce = Some(discovery);
        cfg.allow_power = args.allow_power;
        cfg.supervised = supervised;
        return tokio::runtime::Runtime::new()?.block_on(headless(cfg));
    }

    // ---- windowed ----------------------------------------------------------
    let local_clock: Arc<dyn LocalClock> = Arc::new(MonotonicClock::new());

    // Which show? An explicit file wins; otherwise the Director's; with
    // neither, the bundled example (so `cargo run` shows something).
    let local_show = match (&args.show, &director) {
        (Some(path), _) => Some(load_show(path)?),
        (None, Some(_)) => None,
        (None, None) => Some(load_show(
            concat!(env!("CARGO_MANIFEST_DIR"), "/examples/show.json").as_ref(),
        )?),
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

    // Downloaded media lives next to the rest of the cache; without a cache
    // directory it goes to the system's temporary folder.
    let assets_dir = args
        .cache_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("lcplay-runner"))
        .join("assets");
    let runner_name = args.name.clone();

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
            let mut cfg = AgentConfig::new(args.name, director);
            cfg.cache_dir = args.cache_dir;
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
    event_loop.run_app(&mut app)?;
    if app.exit_code != 0 {
        std::process::exit(app.exit_code);
    }
    Ok(())
}

/// Run the Runner as a child process and keep it running.
///
/// The supervisor is this same program, doing nothing but starting the real
/// Runner (with `--worker`) and watching it:
///
/// * exits normally (the operator chose Quit, or closed a window with Esc):
///   the supervisor ends too;
/// * asks to be restarted (the operator chose Restart): started again at
///   once;
/// * crashes (a GPU driver reset, a bug): started again after a pause that
///   grows if it keeps crashing, so a broken setup does not spin.
///
/// The child is tied to the supervisor through its stdin: if the supervisor
/// is killed, the pipe closes and the child exits (see
/// [`exit_with_supervisor`]). That works the same on Windows, where killing
/// a process does not kill its children.
fn supervise() -> anyhow::Result<()> {
    use lc_runner::machine::RESTART_EXIT_CODE;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating this program")?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        let mut child = Command::new(&exe)
            .args(&args)
            .arg("--worker")
            .stdin(Stdio::piped())
            .spawn()
            .context("starting the runner")?;
        // Keep the write end open for as long as we live.
        let _tie = child.stdin.take();
        let status = child.wait().context("waiting for the runner")?;
        match status.code() {
            Some(0) => return Ok(()),
            Some(RESTART_EXIT_CODE) => {
                tracing::info!("supervisor: restarting the runner");
                backoff = Duration::from_secs(1);
            }
            _ => {
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
