//! `lc-ctl` — command-line controller for an LC Play cluster.
//!
//! ```text
//! lc-ctl discover                 list nodes announcing on the network
//! lc-ctl status                   cluster overview: transport, nodes, sync quality
//! lc-ctl play | pause             drive the transport
//! lc-ctl seek 12.5                jump to 12.5 s
//! lc-ctl rate 0.5                 half speed
//! lc-ctl loop 30 | loop off       loop the first 30 s / stop looping
//! lc-ctl stop                     pause and rewind
//! lc-ctl --timeline intro play    start the auxiliary timeline "intro"
//! lc-ctl reload runner            make a Runner rebuild its outputs
//! lc-ctl load show.json           distribute a show document
//! lc-ctl watch                    live status, once a second
//! ```
//!
//! The Director is found by multicast discovery unless `--director host:port`
//! is given. Besides being handy on site, this tool is the reference client
//! for the control protocol: everything it does is a few JSON messages over
//! one WebSocket.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context as _};
use clap::{Parser, Subcommand};
use lc_discovery::DiscoveryConfig;
use lc_net::Connection;
use lc_proto::{
    ClientMessage, ClusterStatus, Command, Hello, NodeId, Role, ServerMessage, NANOS_PER_SEC,
    PROTOCOL_VERSION,
};

#[derive(Parser)]
#[command(version, about = "LC Play command-line controller")]
struct Args {
    /// Director control address (`host:port`). Default: discover it.
    #[arg(long, global = true)]
    director: Option<String>,
    /// Auxiliary timeline (task id) the transport command is for. Default:
    /// the main timeline.
    #[arg(long, global = true)]
    timeline: Option<String>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// List the nodes announcing themselves on the network.
    Discover {
        /// How long to listen, in seconds.
        #[arg(long, default_value_t = 2.5)]
        seconds: f64,
    },
    /// Print the cluster status.
    Status {
        /// Raw JSON instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Print the status once a second until interrupted.
    Watch,
    Play,
    Pause,
    /// Main timeline: pause and rewind. A task: take it off stage.
    Stop,
    /// Ask a Runner (by name) to close and rebuild its outputs.
    Reload {
        runner: String,
    },
    /// Act on a Runner (by name): restart, quit, standby, resume, reboot,
    /// poweroff.
    Runner {
        runner: String,
        action: String,
    },
    /// Switch a Runner's computer on (Wake-on-LAN).
    Wake {
        runner: String,
    },
    /// Jump to a show time in seconds.
    Seek {
        seconds: f64,
    },
    /// Set the playback rate (1.0 = normal, 0.5 = half speed, -1 = reverse).
    Rate {
        // Without this, `rate -1` would be read as an unknown flag.
        #[arg(allow_negative_numbers = true)]
        rate: f64,
    },
    /// Loop the show at this length in seconds, or `off`.
    Loop {
        length: String,
    },
    /// Load a show document (JSON file) into the Director.
    Load {
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let command = match args.action {
        Action::Discover { seconds } => return discover(seconds).await,
        Action::Status { json } => {
            let (mut conn, _) = connect(args.director.as_deref()).await?;
            let status = request_status(&mut conn).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            conn.close().await;
            return Ok(());
        }
        Action::Watch => {
            let (mut conn, _) = connect(args.director.as_deref()).await?;
            // The Director pushes a status every second; pings keep our side
            // of the link alive.
            let mut keepalive = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    msg = conn.recv() => {
                        if let ServerMessage::Cluster(status) = msg? {
                            print_status(&status);
                            println!();
                        }
                    }
                    _ = keepalive.tick() => conn.send(&ClientMessage::Ping).await?,
                    _ = tokio::signal::ctrl_c() => break,
                }
            }
            conn.close().await;
            return Ok(());
        }
        Action::Play => Command::Play,
        Action::Pause => Command::Pause,
        Action::Stop => Command::Stop,
        Action::Reload { runner } => Command::RunnerAction {
            runner,
            action: lc_proto::RunnerAction::ReloadOutputs,
        },
        Action::Runner { runner, action } => {
            use lc_proto::RunnerAction as A;
            let action = match action.as_str() {
                "restart" => A::Restart,
                "quit" => A::Quit,
                "standby" => A::Standby,
                "resume" => A::Resume,
                "reboot" => A::Reboot,
                "poweroff" => A::PowerOff,
                "reload" => A::ReloadOutputs,
                other => bail!(
                    "unknown runner action '{other}' (restart, quit, standby, resume, reboot, poweroff, reload)"
                ),
            };
            Command::RunnerAction { runner, action }
        }
        Action::Wake { runner } => Command::WakeRunner { runner },
        Action::Seek { seconds } => {
            if !(seconds.is_finite() && seconds >= 0.0) {
                bail!("seek: the time must be zero or positive");
            }
            Command::Seek {
                show_ns: (seconds * NANOS_PER_SEC as f64).round() as i64,
            }
        }
        Action::Rate { rate } => {
            if !(rate.is_finite() && rate.abs() <= 16.0) {
                bail!("rate: must be between -16 and 16");
            }
            Command::SetRate {
                rate_permille: (rate * 1000.0).round() as i32,
            }
        }
        Action::Loop { length } => Command::SetLoop {
            loop_ns: match length.as_str() {
                "off" | "none" | "0" => None,
                s => {
                    let seconds: f64 = s.parse().context("loop: expected seconds or 'off'")?;
                    if !(seconds.is_finite() && seconds > 0.0) {
                        bail!("loop: the length must be positive");
                    }
                    Some((seconds * NANOS_PER_SEC as f64).round() as i64)
                }
            },
        },
        Action::Load { file } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            Command::LoadShow {
                document: serde_json::from_str(&text)
                    .with_context(|| format!("{} is not valid JSON", file.display()))?,
            }
        }
    };

    // Send the command, then ask for the status: the Director processes
    // messages of one connection in order, so the status reflects the command
    // (and an Error, if the command was refused, arrives before it).
    let scheduled = !matches!(
        command,
        Command::LoadShow { .. } | Command::RunnerAction { .. } | Command::WakeRunner { .. }
    );
    let (mut conn, welcome) = connect(args.director.as_deref()).await?;
    conn.send(&ClientMessage::Command {
        command,
        timeline: args.timeline,
    })
    .await?;
    let status = request_status(&mut conn).await?;
    if scheduled {
        // The status below is read immediately, i.e. *before* the change
        // applies on the Runners.
        println!(
            "ok — takes effect on all runners {} ms after the Director received it",
            welcome.lead_ns / 1_000_000
        );
    } else {
        println!("ok");
    }
    print_status(&status);
    conn.close().await;
    Ok(())
}

/// Find (or resolve) the Director and open a control connection.
async fn connect(director: Option<&str>) -> anyhow::Result<(Connection, lc_proto::Welcome)> {
    let addr: SocketAddr = match director {
        Some(addr) => addr
            .to_socket_addrs()
            .with_context(|| format!("resolving '{addr}' (expected host:port)"))?
            .next()
            .with_context(|| format!("'{addr}' did not resolve"))?,
        None => lc_discovery::find(
            DiscoveryConfig::default(),
            Role::Director,
            Duration::from_secs(3),
        )
        .await
        .context("discovery failed; use --director host:port")?
        .and_then(|found| found.control_addr())
        .context("no Director found on the network; use --director host:port")?,
    };
    let hello = Hello {
        protocol: PROTOCOL_VERSION,
        node_id: NodeId::random(),
        role: Role::Controller,
        name: "lc-ctl".into(),
        runner: None,
    };
    Connection::connect(&lc_net::control_url(addr), || hello)
        .await
        .with_context(|| format!("connecting to the Director at {addr}"))
}

/// Ask for a status and wait for it, surfacing any error the Director sends
/// first.
async fn request_status(conn: &mut Connection) -> anyhow::Result<ClusterStatus> {
    conn.send(&ClientMessage::Command {
        command: Command::Status,
        timeline: None,
    })
    .await?;
    loop {
        match conn.recv_timeout(Duration::from_secs(3)).await? {
            ServerMessage::Cluster(status) => return Ok(status),
            ServerMessage::Error { message, .. } => bail!("the Director refused: {message}"),
            _ => continue,
        }
    }
}

async fn discover(seconds: f64) -> anyhow::Result<()> {
    let duration = Duration::try_from_secs_f64(seconds).context("--seconds must be positive")?;
    let nodes = lc_discovery::scan(DiscoveryConfig::default(), duration)
        .await
        .context("cannot listen for discovery beacons")?;
    if nodes.is_empty() {
        println!("no nodes heard in {seconds} s (is multicast allowed on this network?)");
    }
    for node in nodes {
        let b = &node.beacon;
        let endpoint = node
            .control_addr()
            .map_or_else(|| node.addr.to_string(), |a| a.to_string());
        println!(
            "{:<10} {:<20} {:<22} {}",
            format!("{:?}", b.role),
            b.name,
            endpoint,
            b.node_id
        );
    }
    Ok(())
}

fn print_status(s: &ClusterStatus) {
    let t = &s.transport;
    println!(
        "transport  {}  {:.3} s  frame {}  rate {}  loop {}  epoch {}",
        if t.playing { "PLAYING" } else { "paused " },
        s.show_ns as f64 / NANOS_PER_SEC as f64,
        s.frame,
        t.rate_permille as f64 / 1000.0,
        t.loop_ns.map_or("off".to_string(), |l| format!(
            "{:.3} s",
            l as f64 / NANOS_PER_SEC as f64
        )),
        t.epoch,
    );
    println!(
        "show       {}   fps {}/{}   timebase {:#x}",
        s.show_revision
            .map_or("none".to_string(), |r| format!("revision {r}")),
        s.frame_rate.num,
        s.frame_rate.den,
        s.clock_id
    );
    for (id, t) in &s.tasks {
        println!(
            "task       {id:<14} {}  {:.3} s  epoch {}",
            match (t.active, t.playing) {
                (false, _) => "stopped",
                (true, true) => "PLAYING",
                (true, false) => "paused ",
            },
            t.show_time_at(s.cluster_ns) as f64 / NANOS_PER_SEC as f64,
            t.epoch,
        );
    }
    println!("nodes      {}", s.nodes.len());
    for n in &s.nodes {
        print!(
            "  {:<10} {:<18} {:<22}",
            format!("{:?}", n.role),
            n.name,
            n.addr
        );
        if let Some(h) = &n.health {
            print!(
                " {}  sync {} err {} µs  rtt {} µs  drift {:+.1} ppm  skew {} µs",
                match n.frame_error {
                    Some(0) => "on frame".to_string(),
                    Some(e) => format!("{e:+} FRAMES"),
                    // The Runner is executing a different transport state
                    // than the Director: normal for an instant around a
                    // change, a problem if it persists.
                    None => format!("EPOCH {} (director {})", h.transport_epoch, t.epoch),
                },
                if h.sync.locked { "locked" } else { "UNLOCKED" },
                h.sync.error_ns / 1000,
                h.sync.rtt_ns / 1000,
                h.sync.drift_ppm,
                n.clock_skew_ns.unwrap_or(0) / 1000,
            );
        }
        println!();
    }
}
