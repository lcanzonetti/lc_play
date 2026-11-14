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
    /// Path to the `ffprobe` binary used to read media dimensions/duration.
    pub ffprobe: String,
    /// How often the library is rescanned in the background. `0` disables it.
    pub scan_interval: Duration,
    /// Maximum number of concurrent transcode jobs. Protects CPU under load.
    pub max_transcodes: usize,
    /// Whether to enable permissive CORS. On by default: the Producer UI is
    /// served from another origin and must be able to list and upload.
    pub cors: CorsPolicy,
}

/// Which web pages (origins) may use this server from a browser.
///
/// This matters because the server has no authentication: without a limit,
/// *any* page open in a browser that can reach it — not just the Producer —
/// could list, upload and delete media. Non-browser clients (Runners, curl)
/// send no `Origin` and are not affected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsPolicy {
    /// No cross-origin access at all.
    Off,
    /// Pages served from this port on any host — i.e. the Producer, wherever
    /// it is reached from (`localhost`, a LAN address, a host name). The
    /// default, with the Producer's default port.
    Port(u16),
    /// Exactly these origins, e.g. `http://10.0.0.1:9000`.
    Origins(Vec<String>),
    /// Any page. Only for development.
    Any,
}

impl CorsPolicy {
    /// Port the Producer serves its interface on unless told otherwise.
    pub const PRODUCER_PORT: u16 = 7404;

    /// `LCMS_CORS`: `producer` (default) · `off` · `*` · a port number · or a
    /// comma-separated list of origins.
    pub fn parse(value: &str) -> CorsPolicy {
        let v = value.trim();
        match v.to_ascii_lowercase().as_str() {
            "" | "producer" | "true" | "1" | "yes" | "on" => CorsPolicy::Port(Self::PRODUCER_PORT),
            "off" | "false" | "0" | "no" => CorsPolicy::Off,
            "*" | "any" => CorsPolicy::Any,
            other => match other.parse::<u16>() {
                Ok(port) => CorsPolicy::Port(port),
                Err(_) => CorsPolicy::Origins(
                    v.split(',')
                        .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
                        .filter(|o| !o.is_empty())
                        .collect(),
                ),
            },
        }
    }

    /// Whether a request carrying this `Origin` header is acceptable.
    pub fn allows(&self, origin: &str) -> bool {
        let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
        match self {
            CorsPolicy::Off => false,
            CorsPolicy::Any => true,
            CorsPolicy::Origins(list) => list.contains(&origin),
            // "scheme://host:port" — the port is whatever follows the last
            // colon (this also works for bracketed IPv6 hosts).
            CorsPolicy::Port(port) => origin
                .rsplit_once(':')
                .and_then(|(_, p)| p.parse::<u16>().ok())
                .is_some_and(|p| p == *port),
        }
    }
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
        let ffprobe = env_or("LCMS_FFPROBE", "ffprobe");

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

        let cors = CorsPolicy::parse(&env_or("LCMS_CORS", "producer"));

        Config {
            bind,
            media_dir,
            ffmpeg,
            ffprobe,
            scan_interval,
            max_transcodes,
            cors,
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_admits_the_producer_and_nothing_else() {
        let p = CorsPolicy::parse("producer");
        assert!(p.allows("http://localhost:7404"));
        assert!(p.allows("http://10.0.0.5:7404"));
        assert!(p.allows("http://[::1]:7404"));
        assert!(
            !p.allows("https://evil.example"),
            "no port: the default 443"
        );
        assert!(!p.allows("http://localhost:8000"));
        assert!(!p.allows("null"));
    }

    #[test]
    fn policies_parse_from_the_environment_value() {
        assert_eq!(CorsPolicy::parse("off"), CorsPolicy::Off);
        assert_eq!(CorsPolicy::parse("*"), CorsPolicy::Any);
        assert_eq!(CorsPolicy::parse("9000"), CorsPolicy::Port(9000));
        let list = CorsPolicy::parse("http://A:1/, http://b:2");
        assert!(list.allows("http://a:1") && list.allows("http://b:2"));
        assert!(!list.allows("http://c:3"));
        assert!(!CorsPolicy::Off.allows("http://localhost:7404"));
    }
}
