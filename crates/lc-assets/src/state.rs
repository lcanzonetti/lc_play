//! Shared application state passed to every handler.

use std::sync::Arc;

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
}

impl AppState {
    pub fn new(config: Config, library: Arc<Library>) -> Self {
        let slots = Arc::new(Semaphore::new(config.max_transcodes));
        AppState {
            config: Arc::new(config),
            library,
            transcode_slots: slots,
        }
    }
}
