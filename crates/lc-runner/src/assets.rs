//! The Runner's local asset cache.
//!
//! A show names its media by URL on the Asset Manager; a Runner plays from
//! local disk. This module is the bridge: ask for an asset, get a local path
//! if it is already here, otherwise a download starts in the background and
//! the answer is "not yet".
//!
//! The renderer never waits for a download. A cue whose media has not
//! arrived shows a placeholder; when the file lands, the cache's
//! [`generation`](AssetCache::generation) changes and the renderer rebuilds
//! its layers.
//!
//! Files are kept under their asset id plus size, so a file that is replaced
//! on the Asset Manager under the same name is fetched again rather than
//! played stale. (Identifying files by content hash would be stronger; the
//! Asset Manager does not provide one yet.)

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lc_proto::show::Asset;
use lc_proto::AssetDownload;

/// Wait before retrying a failed download: this long after the first
/// failure, doubling up to the maximum. An asset that was deleted or
/// replaced on the Asset Manager will never succeed, and must not be
/// hammered (or fill the log) for the rest of the show.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(120);

pub struct AssetCache {
    dir: PathBuf,
    /// Cache file name → what we know about it.
    entries: Mutex<HashMap<String, Entry>>,
    /// Incremented whenever an asset becomes available (or should be asked
    /// for again), so the renderer knows to look again.
    generation: AtomicU64,
    /// Consecutive failed downloads per cache file name.
    failures: Mutex<HashMap<String, u32>>,
    /// Downloads in progress or waiting to retry, by cache file name: what
    /// the operator sees as progress bars.
    progress: Mutex<HashMap<String, Progress>>,
}

/// One download's progress, shared with its thread.
struct Progress {
    name: String,
    total: u64,
    /// Bytes received, updated by the download thread as data arrives.
    done: Arc<AtomicU64>,
    started: Instant,
    /// After a failed attempt: why, and when the next one starts.
    failed: Option<(String, Instant)>,
}

enum Entry {
    Ready(PathBuf),
    /// A download thread is on it (or waiting to retry).
    Pending,
}

impl AssetCache {
    pub fn new(dir: PathBuf) -> Arc<Self> {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!("cannot create asset cache {}: {e}", dir.display());
        }
        Arc::new(AssetCache {
            dir,
            entries: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            failures: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
        })
    }

    /// Downloads in progress or waiting to retry, for the health report.
    pub fn downloads(&self) -> Vec<AssetDownload> {
        let progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<AssetDownload> = progress
            .values()
            .map(|p| {
                let done = p.done.load(Ordering::Relaxed);
                let secs = p.started.elapsed().as_secs_f64();
                AssetDownload {
                    name: p.name.clone(),
                    done,
                    total: p.total,
                    bytes_per_s: if secs > 0.0 { done as f64 / secs } else { 0.0 },
                    error: p.failed.as_ref().map(|(e, _)| e.clone()),
                    retry_in_s: p
                        .failed
                        .as_ref()
                        .map(|(_, at)| at.saturating_duration_since(Instant::now()).as_secs()),
                }
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Local path of `asset`, if it is in the cache. If it is not, a
    /// download is started (once) and `None` is returned.
    pub fn local_path(self: &Arc<Self>, asset: &Asset) -> Option<PathBuf> {
        let name = file_name(asset);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match entries.get(&name) {
            Some(Entry::Ready(path)) => return Some(path.clone()),
            Some(Entry::Pending) => return None,
            None => {}
        }

        // Already on disk from an earlier run? The size check rejects a file
        // cut short by a crash (downloads are written under a temporary name,
        // but a disk can still lie).
        let path = self.dir.join(&name);
        let on_disk = std::fs::metadata(&path).map(|m| m.len()).ok();
        if on_disk.is_some() && (asset.size == 0 || on_disk == Some(asset.size)) {
            entries.insert(name, Entry::Ready(path.clone()));
            return Some(path);
        }

        entries.insert(name.clone(), Entry::Pending);
        let (cache, asset, thread_name) = (self.clone(), asset.clone(), name.clone());
        let spawned = std::thread::Builder::new()
            .name("lc-asset".into())
            .spawn(move || cache.fetch(&asset, &thread_name, &path));
        if let Err(e) = spawned {
            // Not `Pending` forever: the next request tries again.
            tracing::error!("cannot start a download thread: {e}");
            entries.remove(&name);
        }
        None
    }

    /// Download thread body.
    fn fetch(&self, asset: &Asset, name: &str, path: &Path) {
        tracing::info!("fetching asset '{}' from {}", asset.name, asset.url);
        let done = Arc::new(AtomicU64::new(0));
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                name.to_string(),
                Progress {
                    name: asset.name.clone(),
                    total: asset.size,
                    done: done.clone(),
                    started: Instant::now(),
                    failed: None,
                },
            );
        let result = download(&asset.url, path, asset.size, &done);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(bytes) => {
                tracing::info!("asset '{}' ready ({bytes} bytes)", asset.name);
                entries.insert(name.to_string(), Entry::Ready(path.to_path_buf()));
                self.progress
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name);
                self.failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name);
            }
            Err(e) => {
                drop(entries);
                let attempt = {
                    let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
                    let n = failures.entry(name.to_string()).or_insert(0);
                    *n += 1;
                    *n
                };
                let wait = RETRY_FIRST
                    .saturating_mul(1 << (attempt - 1).min(10))
                    .min(RETRY_MAX);
                // Loud once, quiet afterwards.
                if attempt == 1 {
                    tracing::error!(
                        "asset '{}': {e:#}; retrying in {} s",
                        asset.name,
                        wait.as_secs()
                    );
                } else {
                    tracing::debug!("asset '{}': {e:#} (attempt {attempt})", asset.name);
                }
                if let Some(p) = self
                    .progress
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(name)
                {
                    p.failed = Some((format!("{e:#}"), Instant::now() + wait));
                }
                std::thread::sleep(wait);
                // Forget it, so the next request starts a fresh attempt (and
                // the bar, if the show still needs it).
                self.progress
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name);
                self.entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name);
            }
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

/// `<id>-<size><.ext>`: unique per file version, and keeps the extension so
/// decoders that sniff by name still work.
fn file_name(asset: &Asset) -> String {
    // Ids come from the network: keep only characters that are safe in a
    // file name, so an id can never point outside the cache directory.
    let id: String = asset
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(120)
        .collect();
    let ext = Path::new(&asset.name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    format!("{id}-{}{ext}", asset.size)
}

/// Fetch `url` into `path` via a temporary file, so a partial download is
/// never mistaken for the asset.
fn download(url: &str, path: &Path, expected: u64, done: &AtomicU64) -> anyhow::Result<u64> {
    let response = ureq::get(url)
        .timeout(Duration::from_secs(3600))
        .call()
        .map_err(|e| anyhow::anyhow!("download failed: {e}"))?;
    // If the server already says the file has a different size than the show
    // recorded, it was replaced after the cue was made: say so before
    // downloading gigabytes only to throw them away.
    let announced = response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok());
    if let (Some(announced), true) = (announced, expected != 0) {
        if announced != expected {
            anyhow::bail!(
                "the file on the Asset Manager is {announced} bytes, the show expects {expected} \
                 (it was replaced; re-add it in the Producer)"
            );
        }
    }
    // Per process: two Runners on one machine share the media folder and may
    // fetch the same file at once.
    let tmp = path.with_extension(format!("{}.part", std::process::id()));
    let mut file = std::fs::File::create(&tmp)?;
    let mut reader = Counting {
        inner: response.into_reader().take(u64::MAX),
        done,
    };
    let bytes = std::io::copy(&mut reader, &mut file)?;
    file.sync_all()?;
    if expected != 0 && bytes != expected {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("got {bytes} bytes, expected {expected}");
    }
    std::fs::rename(&tmp, path)?;
    Ok(bytes)
}

/// A reader that publishes how much has been read, for the progress bar.
struct Counting<'a, R> {
    inner: R,
    done: &'a AtomicU64,
}

impl<R: std::io::Read> std::io::Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.done.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_file_names_are_safe_and_versioned() {
        let asset = |id: &str, name: &str, size| Asset {
            id: id.into(),
            name: name.into(),
            size,
            ..Default::default()
        };
        assert_eq!(
            file_name(&asset("abc123", "Clip One.MP4", 42)),
            "abc123-42.mp4"
        );
        // A new size is a new file.
        assert_ne!(
            file_name(&asset("abc123", "clip.mp4", 42)),
            file_name(&asset("abc123", "clip.mp4", 43))
        );
        // Nothing in an id or name can escape the cache directory.
        let hostile = file_name(&asset("../../etc/passwd", "x.sh/../../y", 1));
        assert!(
            !hostile.contains('/') && !hostile.contains(".."),
            "{hostile}"
        );
    }

    #[test]
    fn download_progress_is_reported_while_it_runs_and_cleared_after() {
        use std::io::{Read as _, Write as _};
        // A tiny HTTP server that sends 64 KiB slowly, in 16 chunks.
        const SIZE: usize = 64 * 1024;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\n\r\n").unwrap();
            for _ in 0..16 {
                s.write_all(&[7u8; SIZE / 16]).unwrap();
                s.flush().unwrap();
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let dir = std::env::temp_dir().join(format!("lcplay-dl-test-{}", std::process::id()));
        let cache = AssetCache::new(dir.clone());
        let asset = Asset {
            id: "big".into(),
            name: "big.mov".into(),
            size: SIZE as u64,
            url: format!("http://127.0.0.1:{port}/stream/big"),
            ..Default::default()
        };
        assert_eq!(cache.local_path(&asset), None, "starts a download");

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen_partial = false;
        while cache.local_path(&asset).is_none() {
            assert!(Instant::now() < deadline, "download never finished");
            if let Some(d) = cache.downloads().first() {
                assert_eq!((d.name.as_str(), d.total), ("big.mov", SIZE as u64));
                seen_partial |= d.done > 0 && d.done < d.total;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(seen_partial, "progress was visible mid-way");
        assert!(
            cache.downloads().is_empty(),
            "a finished download leaves the list"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_already_on_disk_is_used_without_downloading() {
        let dir = std::env::temp_dir().join(format!("lcplay-assets-test-{}", std::process::id()));
        let cache = AssetCache::new(dir.clone());
        let asset = Asset {
            id: "id1".into(),
            name: "a.png".into(),
            size: 3,
            // Nothing listens here: a download attempt would fail.
            url: "http://127.0.0.1:1/none".into(),
            ..Default::default()
        };
        std::fs::write(dir.join(file_name(&asset)), b"abc").unwrap();
        assert_eq!(cache.local_path(&asset), Some(dir.join("id1-3.png")));

        // Same id, different size: not this file.
        let changed = Asset { size: 4, ..asset };
        assert_eq!(cache.local_path(&changed), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
