//! `lc-runner` — the LC Play display server.
//!
//! Three ways to run it:
//!
//! ```text
//! lc-runner show.json                              stand-alone: local clock, keyboard transport
//! lc-runner show.json --director 10.0.0.1:7401     renders show.json, timed by the Director
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
use lc_runner::app::App;
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
    /// Show file to render (defaults to the bundled example).
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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();
    let args = Args::parse();

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
        return tokio::runtime::Runtime::new()?.block_on(headless(cfg));
    }

    // ---- windowed ----------------------------------------------------------
    // Default to the bundled example wherever `cargo run` is invoked from.
    let path = args
        .show
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/examples/show.json").into());
    let text = std::fs::read_to_string(&path)
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

    let local_clock: Arc<dyn LocalClock> = Arc::new(MonotonicClock::new());
    // What the show file asks for until (unless) a Director says otherwise.
    let initial = TransportState {
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
    };

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
            let agent = runtime.block_on(async { Agent::spawn(cfg) });
            (Some(runtime), agent.playhead.clone())
        }
        None => {
            let clock = Arc::new(ClusterClock::new(local_clock));
            let playhead = Playhead::new(clock, initial, FrameRate::default());
            (None, Arc::new(playhead))
        }
    };

    let event_loop = EventLoop::new()?;
    let mut app = App::new(show, playhead);
    event_loop.run_app(&mut app)?;
    Ok(())
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
