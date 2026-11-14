//! Persistence of the Director's state across restarts.
//!
//! One small JSON file, replaced atomically (write a temp file, then rename)
//! so a power cut can never leave a half-written state behind.

use std::path::Path;

use anyhow::Context as _;
use lc_proto::{ClockId, NodeId, TransportState};
use serde::{Deserialize, Serialize};

const FILE: &str = "director.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShowDoc {
    pub revision: u64,
    pub document: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persisted {
    /// The Director's identity, stable across restarts.
    pub node_id: NodeId,
    /// Timebase served when this was saved; decides which timebase may be
    /// adopted after a restart.
    pub clock_id: ClockId,
    pub transport: TransportState,
    #[serde(default)]
    pub show: Option<ShowDoc>,
}

/// Load the saved state. A missing file is a normal first start; a corrupt
/// one is reported and ignored (starting empty beats not starting).
pub fn load(dir: &Path) -> anyhow::Result<Option<Persisted>> {
    let path = dir.join(FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match serde_json::from_slice(&bytes) {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            tracing::error!("ignoring corrupt state file {}: {e}", path.display());
            Ok(None)
        }
    }
}

pub async fn save(dir: &Path, state: &Persisted) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let (tmp, path) = (dir.join(format!("{FILE}.tmp")), dir.join(FILE));
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(state)?)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
