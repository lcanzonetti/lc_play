//! Shared application state passed to every handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::config::Config;
use crate::library::Library;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub library: Arc<Library>,
    /// Caps concurrent transcode subprocesses so a burst of clients cannot
    /// spawn unbounded `ffmpeg` jobs and saturate the CPU.
    pub transcode_slots: Arc<Semaphore>,
    /// Probe results by item id, tagged with the file size they were measured
    /// at so a replaced file is probed again.
    pub probes: Arc<Mutex<HashMap<String, (u64, crate::handlers::manage::Probe)>>>,
    /// Thumbnails (JPEG bytes) by item id, tagged the same way.
    pub thumbs: Arc<Mutex<HashMap<String, (u64, axum::body::Bytes)>>>,
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
        }
    }
}
