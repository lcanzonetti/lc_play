//! In-memory media catalog.
//!
//! The library walks [`Config::media_dir`] and keeps a lightweight index of
//! playable files in memory behind an `RwLock`. Reads (browse/stream) take a
//! cheap shared lock; a background task periodically rebuilds the index and
//! swaps it in under a brief write lock.
//!
//! Item IDs are the hex-encoded relative path. That makes them stable across
//! restarts and across replicas (important for caching), collision-free and
//! reversible, while never leaking absolute paths.
//!
//! Because a stream is only ever served for a path present in the index, this
//! also closes off path-traversal: an arbitrary decoded id that is not indexed
//! resolves to nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use serde::Serialize;
use walkdir::WalkDir;

use crate::config::Config;

/// Recognised media extensions and the broad kind they map to.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "m4v", "flv", "wmv", "mpeg", "mpg", "ts", "m2ts", "3gp",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "aac", "ogg", "oga", "opus", "wav", "m4a", "wma", "aiff", "alac",
];

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Audio,
}

/// A single indexed media file.
#[derive(Debug, Clone, Serialize)]
pub struct MediaItem {
    /// Stable, URL-safe identifier (hex of the relative path).
    pub id: String,
    /// Display name (file stem + extension).
    pub name: String,
    /// Path relative to the media root, for display and grouping.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// Guessed MIME type of the source file.
    pub mime: String,
    /// Broad category.
    pub kind: MediaKind,
    /// Absolute path on disk. Not serialised — internal use only.
    #[serde(skip)]
    pub abs_path: PathBuf,
}

#[derive(Default)]
struct Index {
    /// id -> item
    by_id: HashMap<String, MediaItem>,
}

pub struct Library {
    root: PathBuf,
    index: RwLock<Index>,
}

impl Library {
    pub fn new(root: PathBuf) -> Self {
        Library {
            root,
            index: RwLock::new(Index::default()),
        }
    }

    /// Number of items currently indexed.
    pub fn len(&self) -> usize {
        self.index.read().unwrap().by_id.len()
    }

    #[allow(dead_code)] // part of the natural len()/is_empty() pair
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a single item by id.
    pub fn get(&self, id: &str) -> Option<MediaItem> {
        self.index.read().unwrap().by_id.get(id).cloned()
    }

    /// Return all items, sorted by relative path for a stable listing.
    pub fn list(&self) -> Vec<MediaItem> {
        let idx = self.index.read().unwrap();
        let mut items: Vec<MediaItem> = idx.by_id.values().cloned().collect();
        items.sort_by(|a, b| a.path.cmp(&b.path));
        items
    }

    /// Walk the media root and rebuild the index, then swap it in atomically.
    /// Blocking filesystem work is done before taking the write lock so readers
    /// are only ever blocked for the duration of a `HashMap` move.
    pub fn rescan(&self) {
        let mut by_id = HashMap::new();

        for entry in WalkDir::new(&self.root)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let abs_path = entry.path().to_path_buf();
            let Some(kind) = classify(&abs_path) else {
                continue;
            };
            let Ok(rel) = abs_path.strip_prefix(&self.root) else {
                continue;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let mime = mime_guess::from_path(&abs_path)
                .first_or_octet_stream()
                .to_string();
            let name = abs_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| rel_str.clone());
            let id = encode_id(&rel_str);

            by_id.insert(
                id.clone(),
                MediaItem {
                    id,
                    name,
                    path: rel_str,
                    size,
                    mime,
                    kind,
                    abs_path,
                },
            );
        }

        let count = by_id.len();
        {
            let mut idx = self.index.write().unwrap();
            idx.by_id = by_id;
        }
        tracing::info!(items = count, "library rescan complete");
    }
}

/// Classify a path by extension, or `None` if it is not a known media type.
fn classify(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if VIDEO_EXTS.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else if AUDIO_EXTS.contains(&ext.as_str()) {
        Some(MediaKind::Audio)
    } else {
        None
    }
}

/// Hex-encode the relative path to form a stable, URL-safe id.
fn encode_id(rel: &str) -> String {
    let mut s = String::with_capacity(rel.len() * 2);
    for b in rel.as_bytes() {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

/// Spawn the periodic background rescan loop. The first scan runs immediately.
pub async fn spawn_rescan(library: std::sync::Arc<Library>, interval: Duration) {
    // Initial scan runs on a blocking thread so we never stall the runtime.
    {
        let lib = library.clone();
        let _ = tokio::task::spawn_blocking(move || lib.rescan()).await;
    }

    if interval.is_zero() {
        return; // periodic rescanning disabled
    }

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate first tick; we already scanned above.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let lib = library.clone();
            let _ = tokio::task::spawn_blocking(move || lib.rescan()).await;
        }
    });
}

/// Convenience constructor used by `main`.
pub fn from_config(config: &Config) -> std::sync::Arc<Library> {
    std::sync::Arc::new(Library::new(config.media_dir.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ids_are_stable_and_reversible_hex() {
        let id = encode_id("shows/ep1.mkv");
        // Deterministic: same input always yields the same id.
        assert_eq!(id, encode_id("shows/ep1.mkv"));
        // Hex decodes back to the original path.
        let bytes: Vec<u8> = (0..id.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&id[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(String::from_utf8(bytes).unwrap(), "shows/ep1.mkv");
    }

    #[test]
    fn distinct_paths_get_distinct_ids() {
        assert_ne!(encode_id("a/b.mp4"), encode_id("a/c.mp4"));
    }

    #[test]
    fn classify_recognises_video_and_audio_ignores_others() {
        assert_eq!(classify(&PathBuf::from("v.MP4")), Some(MediaKind::Video));
        assert_eq!(classify(&PathBuf::from("v.mkv")), Some(MediaKind::Video));
        assert_eq!(classify(&PathBuf::from("song.FLAC")), Some(MediaKind::Audio));
        assert_eq!(classify(&PathBuf::from("readme.txt")), None);
        assert_eq!(classify(&PathBuf::from("noext")), None);
    }
}
