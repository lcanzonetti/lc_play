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
    /// `max-age` for `/stream` responses. `0` (default) lets caches store
    /// files but makes them revalidate (cheap `304`s) before every reuse.
    pub cache_max_age: u64,
}

impl Config {
    /// Build a [`Config`] from the process environment. Unset variables take
    /// their default. A variable that is set but can't be parsed logs a
    /// warning and falls back to the default — except `LCMS_BIND`, which
    /// stops startup: silently binding `0.0.0.0` when you meant `127.0.0.1`
    /// would expose the server on every interface.
    pub fn from_env() -> Self {
        let bind = match std::env::var("LCMS_BIND") {
            Err(_) => SocketAddr::from(([0, 0, 0, 0], 8080)),
            Ok(v) => v.parse().unwrap_or_else(|e| {
                eprintln!("LCMS_BIND={v:?} is not a valid address (e.g. 0.0.0.0:8080): {e}");
                std::process::exit(2);
            }),
        };

        let media_dir = PathBuf::from(env_or("LCMS_MEDIA_DIR", "./media"));

        let ffmpeg = env_or("LCMS_FFMPEG", "ffmpeg");

        let scan_interval = Duration::from_secs(parse_env("LCMS_SCAN_INTERVAL_SECS", 300));

        // Default concurrency tracks the available CPU parallelism so the box
        // is used but not overwhelmed. `0` (or unset) means "use the default".
        let default_transcodes = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let max_transcodes = match parse_env::<usize>("LCMS_MAX_TRANSCODES", 0) {
            0 => default_transcodes,
            n => n,
        };

        let cors = match env_or("LCMS_CORS", "false").to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" | "" => false,
            other => {
                tracing::warn!("LCMS_CORS={other:?} is not a boolean; CORS stays disabled");
                false
            }
        };

        let cache_max_age = parse_env("LCMS_CACHE_MAX_AGE_SECS", 0);

        Config {
            bind,
            media_dir,
            ffmpeg,
            scan_interval,
            max_transcodes,
            cors,
            cache_max_age,
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Parse `key` as a `T`; unset → `default`, unparseable → warn + `default`.
fn parse_env<T>(key: &str, default: T) -> T
where
    T: std::str::FromStr + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) => v.trim().parse().unwrap_or_else(|e| {
            tracing::warn!("{key}={v:?} is invalid ({e}); using the default {default}");
            default
        }),
    }
}
