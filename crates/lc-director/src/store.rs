//! Persistence of the Director's state across restarts.
//!
//! One small JSON file, replaced atomically: write a temp file, flush it to
//! disk, then rename it over the old one. A power cut leaves either the old
//! state or the new one, not half a file.

use std::path::Path;

use anyhow::Context as _;
use lc_proto::{ClockId, NodeId, TransportState};
use serde::{Deserialize, Serialize};

const FILE: &str = "director.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShowDoc {
    pub revision: u64,
    /// Unique per load; what Runners compare their cache against.
    #[serde(default)]
    pub id: String,
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
    /// Transport states of the auxiliary timelines, by timeline id.
    #[serde(default)]
    pub tasks: std::collections::BTreeMap<String, TransportState>,
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
    let bytes = serde_json::to_vec_pretty(state)?;
    async {
        use tokio::io::AsyncWriteExt as _;
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(&bytes).await?;
        // Without this the rename can reach the disk before the data does,
        // and a power cut would leave an empty file under the final name.
        file.sync_all().await
    }
    .await
    .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
