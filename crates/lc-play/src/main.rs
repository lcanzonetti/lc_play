//! `lc-play` — the whole system on one machine, with one command.
//!
//! ```text
//! lc-play                 start everything, data in ./lcplay-data
//! lc-play --dir D:\show   keep media, show and caches somewhere else
//! lc-play --no-runner     services only (Runners are on other machines)
//! ```
//!
//! It starts the four services as ordinary child processes — exactly the
//! binaries you would start by hand on separate machines — wired to each
//! other over localhost:
//!
//! | Service        | Binary        | Port            |
//! |----------------|---------------|-----------------|
//! | Asset Manager  | `lc-assets`   | HTTP 7403       |
//! | Director       | `lc-director` | TCP 7401, UDP 7402, OSC UDP 7405 |
//! | Producer       | `lc-producer` | HTTP 7404 (open this in a browser) |
//! | Runner         | `lc-runner`   | — (opens the output windows) |
//!
//! Nothing here is special-cased for the single-machine setup: the services
//! still find and talk to each other over the network stack, so what works
//! here is what will work across machines.
//!
//! Stopping: Ctrl-C stops everything. A single service that exits (closing
//! the Runner's window, or killing the Director to test what happens) does
//! not take the others down.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context as _;
use clap::Parser;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::process::{Child, Command};

#[derive(Parser)]
#[command(version, about = "Run a complete LC Play system on this machine")]
struct Args {
    /// Where media, the show and caches are kept.
    #[arg(long, default_value = "lcplay-data")]
    dir: PathBuf,
    /// Do not start a local Runner.
    #[arg(long)]
    no_runner: bool,
    /// Name of the local Runner (what a display's "Runner" field refers to).
    #[arg(long, default_value = "runner")]
    runner_name: String,
}

/// A service binary: it lives next to this executable.
fn sibling(name: &str) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locating this executable")?;
    let path = exe
        .parent()
        .context("executable has no directory")?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !path.is_file() {
        anyhow::bail!(
            "{} not found — build the whole workspace first (cargo build --release --workspace)",
            path.display()
        );
    }
    Ok(path)
}

/// Start a child and forward its output, each line prefixed with its name.
fn spawn(name: &'static str, mut command: Command) -> anyhow::Result<Child> {
    let mut child = command
        // Output goes through a pipe, not a terminal: plain text, no colour
        // escape codes.
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If this launcher dies, so do the services.
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {name}"))?;
    forward(name, child.stdout.take());
    forward(name, child.stderr.take());
    Ok(child)
}

fn forward(name: &'static str, stream: Option<impl AsyncRead + Unpin + Send + 'static>) {
    let Some(stream) = stream else { return };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            println!("[{name:<8}] {line}");
        }
    });
}

fn dir_arg(dir: &Path, sub: &str) -> PathBuf {
    dir.join(sub)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let media = dir_arg(&args.dir, "media");
    std::fs::create_dir_all(&media)
        .with_context(|| format!("creating data directory {}", args.dir.display()))?;

    // Order matters only for tidiness: every service retries until the
    // others are there.
    let mut children: Vec<(&'static str, Option<Child>)> = Vec::new();

    let mut assets = Command::new(sibling("lc-assets")?);
    assets
        .env("LCMS_BIND", "0.0.0.0:7403")
        .env("LCMS_MEDIA_DIR", &media)
        // Files copied into the media folder by hand show up within seconds.
        .env("LCMS_SCAN_INTERVAL_SECS", "10");
    children.push(("assets", Some(spawn("assets", assets)?)));

    let mut director = Command::new(sibling("lc-director")?);
    director
        .arg("--state-dir")
        .arg(dir_arg(&args.dir, "director"));
    children.push(("director", Some(spawn("director", director)?)));

    children.push((
        "producer",
        Some(spawn("producer", Command::new(sibling("lc-producer")?))?),
    ));

    if !args.no_runner {
        let mut runner = Command::new(sibling("lc-runner")?);
        runner
            // Explicit address: on one machine there is nothing to discover.
            .args(["--director", "127.0.0.1:7401", "--name", &args.runner_name])
            .arg("--cache-dir")
            .arg(dir_arg(&args.dir, "runner"));
        children.push(("runner", Some(spawn("runner", runner)?)));
    }

    println!();
    println!("  LC Play is starting.");
    println!("  Producer (open in a browser):  http://localhost:7404");
    println!("  Media folder:                  {}", media.display());
    println!(
        "  OSC control:                   udp port 7405  (/lcplay/play, /pause, /stop, /seek)"
    );
    if args.no_runner {
        println!("  Runners on other machines: open the Producer by this machine's LAN address,");
        println!("  not `localhost` — the address in the browser is the one written into the show");
        println!("  for Runners to download media from.");
    }
    println!("  Ctrl-C stops everything.");
    println!();

    // Run until Ctrl-C. A service that exits — the operator closed the
    // Runner's window, or killed the Director to see what happens — is
    // reported and the others keep running: that independence is the point
    // of separate services, and this launcher must not undo it.
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(300));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = poll.tick() => {
                for (name, slot) in &mut children {
                    let exited = match slot {
                        Some(child) => child.try_wait().ok().flatten(),
                        None => None,
                    };
                    if let Some(status) = exited {
                        println!("[launcher] {name} exited ({status}). The other services keep running; restart `lc-play` to bring it back.");
                        *slot = None;
                    }
                }
                if children.iter().all(|(_, slot)| slot.is_none()) {
                    println!("[launcher] every service has exited.");
                    return Ok(());
                }
            }
        }
    }

    println!("[launcher] stopping…");
    for (_, slot) in &mut children {
        if let Some(child) = slot {
            let _ = child.kill().await;
        }
    }
    Ok(())
}
