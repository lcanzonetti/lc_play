//! Shared application state passed to every handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::config::Config;
use crate::library::Library;

/// Item id → (file size when probed, result).
pub type ProbeCache = HashMap<String, (u64, Option<crate::handlers::manage::Probe>)>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub library: Arc<Library>,
    /// Caps concurrent transcode subprocesses so a burst of clients cannot
    /// spawn unbounded `ffmpeg` jobs and saturate the CPU.
    pub transcode_slots: Arc<Semaphore>,
    /// Probe results by item id, tagged with the file size they were measured
    /// at so a replaced file is probed again.
    /// `None` records a file `ffprobe` could not read, so it is not asked
    /// again on every listing.
    pub probes: Arc<Mutex<ProbeCache>>,
    /// Thumbnails (JPEG bytes) by item id, tagged the same way.
    pub thumbs: Arc<Mutex<HashMap<String, (u64, axum::body::Bytes)>>>,
    /// One lock per item whose preview proxy is being made, so that two
    /// requests for it start one ffmpeg, not two.
    pub proxy_jobs: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Proxies made at the same time (their own limit: a show full of new
    /// videos must not hold up thumbnails and probes for minutes).
    pub proxy_slots: Arc<Semaphore>,
    /// Items no proxy could be made for, with the (size, mtime) tried.
    pub proxy_failed: Arc<Mutex<HashMap<String, (u64, u64)>>>,
}

impl AppState {
    pub fn new(config: Config, library: Arc<Library>) -> Self {
        let slots = Arc::new(Semaphore::new(config.max_transcodes));
        AppState {
            config: Arc::new(config),
            library,
            transcode_slots: slots,
            probes: Arc::default(),
            thumbs: Arc::default(),
            proxy_jobs: Arc::default(),
            proxy_slots: Arc::new(Semaphore::new(crate::handlers::proxy::PROXY_JOBS)),
            proxy_failed: Arc::default(),
        }
    }
}
