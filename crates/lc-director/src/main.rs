//! `lc-director` — run the LC Play Director.
//!
//! ```text
//! lc-director --name main --state-dir /var/lib/lcplay/director
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use lc_director::{Director, DirectorConfig};
use lc_discovery::DiscoveryConfig;
use lc_proto::FrameRate;

#[derive(Parser)]
#[command(
    version,
    about = "LC Play Director: master clock, transport and show state for the cluster"
)]
struct Args {
    /// Name shown to operators.
    #[arg(long, default_value = "director")]
    name: String,
    /// Address to bind the control (TCP) and sync (UDP) sockets to.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,
    #[arg(long, default_value_t = lc_proto::DEFAULT_CONTROL_PORT)]
    control_port: u16,
    #[arg(long, default_value_t = lc_proto::DEFAULT_SYNC_PORT)]
    sync_port: u16,
    /// Lead time for transport changes, in milliseconds.
    #[arg(long, default_value_t = 100)]
    lead_ms: u64,
    /// Show frame rate: an integer ("60") or a fraction ("60000/1001").
    #[arg(long, default_value = "60", value_parser = parse_frame_rate)]
    fps: FrameRate,
    /// Directory for persistent state. Without it nothing survives a restart.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Do not announce on the network (Runners must use --director).
    #[arg(long)]
    no_discovery: bool,
    /// IPv4 address of the network interface to announce on.
    #[arg(long, default_value = "0.0.0.0")]
    interface: Ipv4Addr,
}

fn parse_frame_rate(s: &str) -> Result<FrameRate, String> {
    let (num, den) = s.split_once('/').unwrap_or((s, "1"));
    match (num.trim().parse::<u32>(), den.trim().parse::<u32>()) {
        (Ok(num), Ok(den)) if num > 0 && den > 0 => Ok(FrameRate { num, den }),
        _ => Err(format!(
            "'{s}' is not a frame rate; use e.g. 60 or 60000/1001"
        )),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let director = Director::start(DirectorConfig {
        name: args.name,
        control_addr: SocketAddr::new(args.bind, args.control_port),
        sync_addr: SocketAddr::new(args.bind, args.sync_port),
        lead: Duration::from_millis(args.lead_ms),
        frame_rate: args.fps,
        state_dir: args.state_dir,
        discovery: (!args.no_discovery).then(|| DiscoveryConfig {
            interface: args.interface,
            ..Default::default()
        }),
        ..Default::default()
    })
    .await?;

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    director.shutdown().await;
    Ok(())
}
