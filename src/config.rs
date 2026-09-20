//! Runtime configuration, loaded from environment variables.
//!
//! Every setting has a sensible default so the server runs with zero
//! configuration. All variables are prefixed with `LCMS_`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to (e.g. `0.0.0.0:8080`).
    pub bind: SocketAddr,
    /// Root directory scanned for media files.
    pub media_dir: PathBuf,
    /// Path to the `ffmpeg` binary used for transcoding.
    pub ffmpeg: String,
    /// How often the library is rescanned in the background. `0` disables it.
    pub scan_interval: Duration,
    /// Maximum number of concurrent transcode jobs. Protects CPU under load.
    pub max_transcodes: usize,
    /// Whether to enable permissive CORS (handy for browser front-ends).
    pub cors: bool,
}

impl Config {
    /// Build a [`Config`] from the process environment, falling back to
    /// defaults for anything unset or unparseable.
    pub fn from_env() -> Self {
        let bind = env_or("LCMS_BIND", "0.0.0.0:8080")
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 8080)));

        let media_dir = PathBuf::from(env_or("LCMS_MEDIA_DIR", "./media"));

        let ffmpeg = env_or("LCMS_FFMPEG", "ffmpeg");

        let scan_interval = Duration::from_secs(
            env_or("LCMS_SCAN_INTERVAL_SECS", "300")
                .parse()
                .unwrap_or(300),
        );

        // Default concurrency tracks the available CPU parallelism so the box
        // is used but not overwhelmed.
        let default_transcodes = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let max_transcodes = env_or("LCMS_MAX_TRANSCODES", "0")
            .parse()
            .ok()
            .filter(|&n: &usize| n > 0)
            .unwrap_or(default_transcodes);

        let cors = matches!(
            env_or("LCMS_CORS", "false").to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );

        Config {
            bind,
            media_dir,
            ffmpeg,
            scan_interval,
            max_transcodes,
            cors,
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
