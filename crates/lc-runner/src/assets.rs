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
//! Files are kept under readable names, as on the Asset Manager (WATCHOUT 6
//! style): `assets/Optimized/Show A/intro.mov`, not an id. Which version of
//! each file is there is recorded in an index, `assets/.lcplay-index.json`
//! (asset id → URL, size and file), so a file replaced or re-optimised on
//! the Asset Manager — a new URL or size — is fetched again rather than
//! played stale. (Identifying files by content hash would be stronger; the
//! Asset Manager does not provide one yet.)
//!
//! A file copied into the folder by hand, under the name the Asset Manager
//! uses and with the right size, is taken as it is: big shows can be
//! pre-loaded from a disk instead of over the network.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lc_proto::show::Asset;
use lc_proto::AssetDownload;
use serde::{Deserialize, Serialize};

/// The version index, in the cache folder.
const INDEX_FILE: &str = ".lcplay-index.json";

/// What the index records about one asset's file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct IndexEntry {
    /// The version: where it was fetched from, and its size.
    url: String,
    size: u64,
    /// The file, relative to the cache folder, with `/`.
    file: String,
}

/// Asset id → its file.
type Index = BTreeMap<String, IndexEntry>;

/// Wait before retrying a failed download: this long after the first
/// failure, doubling up to the maximum. An asset that was deleted or
/// replaced on the Asset Manager will never succeed, and must not be
/// hammered (or fill the log) for the rest of the show.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(120);

pub struct AssetCache {
    dir: PathBuf,
    /// Held while the index file is read and written (download threads
    /// finish at the same time).
    index_lock: Mutex<()>,
    /// Asset version (see [`version_key`]) → what we know about it.
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
            index_lock: Mutex::new(()),
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
        let key = version_key(asset);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match entries.get(&key) {
            Some(Entry::Ready(path)) => return Some(path.clone()),
            Some(Entry::Pending) => return None,
            None => {}
        }

        // Already on disk from an earlier run? The size check rejects a file
        // cut short by a crash (downloads are written under a temporary name,
        // but a disk can still lie).
        let index = self.read_index();
        let intact = |file: &str| {
            let on_disk = std::fs::metadata(self.dir.join(file)).map(|m| m.len()).ok();
            on_disk.is_some() && (asset.size == 0 || on_disk == Some(asset.size))
        };
        let preferred = local_name(asset);
        // Another asset already has that name (two names the file system
        // cannot tell apart): this one gets a suffix.
        let claimed = |file: &str| {
            index
                .iter()
                .any(|(id, e)| *id != asset.id && e.file == file)
        };
        let target = if claimed(&preferred) {
            with_suffix(&preferred, &short_hash(&asset.id))
        } else {
            preferred
        };
        let known = match index.get(&asset.id) {
            // This very version.
            Some(e) if e.url == asset.url && e.size == asset.size && intact(&e.file) => {
                Some(e.file.clone())
            }
            Some(_) => None,
            // Never fetched, but there under its name with the right size
            // (copied in by hand, or an index lost): take it.
            None if asset.size != 0 && intact(&target) => {
                self.record(asset, &target);
                Some(target.clone())
            }
            None => None,
        };
        if let Some(file) = known {
            let path = self.dir.join(file);
            entries.insert(key, Entry::Ready(path.clone()));
            return Some(path);
        }

        entries.insert(key.clone(), Entry::Pending);
        let (cache, asset, thread_key) = (self.clone(), asset.clone(), key.clone());
        let spawned = std::thread::Builder::new()
            .name("lc-asset".into())
            .spawn(move || {
                crate::priority::background_thread();
                cache.fetch(&asset, &thread_key, &target)
            });
        if let Err(e) = spawned {
            // Not `Pending` forever: the next request tries again.
            tracing::error!("cannot start a download thread: {e}");
            entries.remove(&key);
        }
        None
    }

    fn read_index(&self) -> Index {
        std::fs::read(self.dir.join(INDEX_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Record `file` as the current version of `asset`. Read, changed and
    /// written in one go (the file is re-read: another Runner process on
    /// this machine may share the folder), via a temporary file.
    fn record(&self, asset: &Asset, file: &str) {
        let _guard = self.index_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = self.read_index();
        index.insert(
            asset.id.clone(),
            IndexEntry {
                url: asset.url.clone(),
                size: asset.size,
                file: file.to_string(),
            },
        );
        let tmp = self
            .dir
            .join(format!("{INDEX_FILE}.{}.tmp", std::process::id()));
        let written = serde_json::to_vec_pretty(&index)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&tmp, json))
            .and_then(|()| std::fs::rename(&tmp, self.dir.join(INDEX_FILE)));
        if let Err(e) = written {
            tracing::warn!("cannot write the asset index: {e}");
        }
    }

    /// Download thread body. `key` is the version's entry, `file` where it
    /// goes (relative to the cache folder).
    fn fetch(&self, asset: &Asset, key: &str, file: &str) {
        tracing::info!("fetching asset '{}' from {}", asset.name, asset.url);
        let name = key;
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
        let previous = self.read_index().get(&asset.id).map(|e| e.file.clone());
        let result =
            download(&asset.url, &self.dir.join(file), asset.size, &done).and_then(|tmp| {
                // Into place. The old version may still be open (a Runner
                // playing it, a viewer): if Windows will not replace it, the
                // new one goes beside it under a versioned name.
                let path = self.dir.join(file);
                if std::fs::rename(&tmp, &path).is_ok() {
                    return Ok(file.to_string());
                }
                let alt = with_suffix(file, &short_hash(&version_key(asset)));
                std::fs::rename(&tmp, self.dir.join(&alt)).inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp);
                })?;
                Ok(alt)
            });
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(file) => {
                tracing::info!("asset '{}' ready as {file}", asset.name);
                self.record(asset, &file);
                // The version it replaces is not needed any more (if it is
                // still open, it stays until the next replacement).
                if let Some(old) = previous.filter(|old| *old != file) {
                    let _ = std::fs::remove_file(self.dir.join(old));
                }
                entries.insert(name.to_string(), Entry::Ready(self.dir.join(&file)));
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

/// Identifies one version of an asset: the same file at another URL (e.g.
/// re-optimised) or of another size is another version.
fn version_key(asset: &Asset) -> String {
    format!("{}\n{}\n{}", asset.id, asset.url, asset.size)
}

/// Longest local name, in characters, before it is shortened: with the
/// cache folder in front, Windows paths stay well under 260.
const MAX_NAME: usize = 160;

/// Where `asset` is kept, relative to the cache folder: its path on the
/// Asset Manager (`Optimized/Show A/intro.mov`), made safe — every
/// component one Windows can store, nothing that leaves the folder.
fn local_name(asset: &Asset) -> String {
    let source = if asset.path.is_empty() {
        &asset.name
    } else {
        &asset.path
    };
    let parts: Vec<String> = source
        .split(['/', '\\'])
        .filter_map(safe_component)
        .collect();
    // Deep trees keep their last levels.
    let parts = &parts[parts.len().saturating_sub(8)..];
    let joined = parts.join("/");
    if !joined.is_empty() && joined.chars().count() <= MAX_NAME {
        return joined;
    }
    // Nothing usable, or too long: the file name alone (shortened), plus a
    // tag from the id so it stays unique.
    let last = parts.last().map_or("asset", String::as_str);
    let ext = Path::new(last)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 8)
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let stem: String = last.chars().take(60).collect();
    let stem = stem
        .strip_suffix(&ext)
        .unwrap_or(&stem)
        .trim_end_matches(['.', ' ']);
    format!("{stem}~{}{ext}", short_hash(&asset.id))
}

/// One path component as Windows (and Linux) can store it, or `None` for
/// nothing usable (`..`, `.`, empty).
fn safe_component(part: &str) -> Option<String> {
    let cleaned: String = part
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(100)
        .collect();
    // No leading dots (hidden files, `..`), no trailing dots or spaces
    // (Windows drops them, so two names would become one).
    let cleaned = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' ']);
    if cleaned.is_empty() {
        return None;
    }
    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    Some(if reserved {
        format!("_{cleaned}")
    } else {
        cleaned.to_string()
    })
}

/// `dir/name~tag.ext` from `dir/name.ext`.
fn with_suffix(file: &str, tag: &str) -> String {
    let (dir, name) = file.rsplit_once('/').map_or(("", file), |(d, n)| (d, n));
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    let name = format!("{stem}~{tag}{ext}");
    if dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    }
}

/// Eight hex digits from a string (FNV-1a): short, stable tags for names.
fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// Fetch `url` into a temporary file next to `path` (hidden, so it is never
/// mistaken for the asset) and return that file; the caller moves it into
/// place.
fn download(url: &str, path: &Path, expected: u64, done: &AtomicU64) -> anyhow::Result<PathBuf> {
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Unique per process (two Runners on one machine may share the
    // folder) and per download (two versions of one asset may overlap).
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_file_name(format!(
        ".{}.{}-{}.part",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
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
    Ok(tmp)
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

    fn asset(id: &str, path: &str, size: u64) -> Asset {
        Asset {
            id: id.into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            size,
            ..Default::default()
        }
    }

    #[test]
    fn local_names_are_the_asset_managers_and_safe() {
        assert_eq!(
            local_name(&asset("a", "Optimized/Show A/intro.mov", 1)),
            "Optimized/Show A/intro.mov"
        );
        // Shows written before assets had a path: the name.
        let old = Asset {
            path: String::new(),
            ..asset("a", "Clip One.MP4", 1)
        };
        assert_eq!(local_name(&old), "Clip One.MP4");
        // Nothing can leave the cache folder, or be a name Windows refuses.
        for hostile in [
            "../../etc/passwd",
            "..\\..\\x.mov",
            "/abs/x.mov",
            "C:\\x.mov",
        ] {
            let name = local_name(&asset("a", hostile, 1));
            assert!(
                !name
                    .split('/')
                    .any(|c| c == ".." || c.is_empty() || c.contains(':')),
                "{hostile} → {name}"
            );
        }
        assert_eq!(local_name(&asset("a", "dir/CON.mov", 1)), "dir/_CON.mov");
        assert_eq!(local_name(&asset("a", "a?b.mov ", 1)), "a_b.mov");
        assert_eq!(local_name(&asset("a", ".hidden.mov", 1)), "hidden.mov");
        // Too long: shortened, and tagged so it stays unique.
        let long = format!("{}/x.mov", "d".repeat(90) + "/" + &"e".repeat(90));
        let name = local_name(&asset("a", &long, 1));
        assert!(name.len() < 80 && name.ends_with(".mov"), "{name}");
        assert_eq!(with_suffix("d/x.mov", "t"), "d/x~t.mov");
        assert_eq!(with_suffix("noext", "t"), "noext~t");
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
        let _ = std::fs::remove_dir_all(&dir);
        let cache = AssetCache::new(dir.clone());
        let asset = Asset {
            // Nothing listens here: a download attempt would fail.
            url: "http://127.0.0.1:1/none".into(),
            ..asset("id1", "Show/a.png", 3)
        };
        // Copied in by hand under its name: taken, and recorded.
        std::fs::create_dir_all(dir.join("Show")).unwrap();
        std::fs::write(dir.join("Show/a.png"), b"abc").unwrap();
        assert_eq!(cache.local_path(&asset), Some(dir.join("Show/a.png")));
        assert_eq!(cache.read_index()["id1"].file, "Show/a.png");

        // Same id, different size: not this file.
        let changed = Asset {
            size: 4,
            ..asset.clone()
        };
        assert_eq!(cache.local_path(&changed), None);
        // Same size, another URL (re-optimised): not this file either, now
        // that the index knows which version it is.
        let fresh = AssetCache::new(dir.clone());
        let moved = Asset {
            url: "http://127.0.0.1:1/none?v=2".into(),
            ..asset.clone()
        };
        assert_eq!(fresh.local_path(&moved), None);
        // While the known version still is.
        let fresh = AssetCache::new(dir.clone());
        assert_eq!(fresh.local_path(&asset), Some(dir.join("Show/a.png")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_taken_by_another_asset_gets_a_suffix_and_new_versions_replace_old_ones() {
        use std::io::{Read as _, Write as _};
        // Serves whatever body the path asks for: `/<body>`.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let mut s = s.unwrap();
                let mut req = [0u8; 1024];
                let n = s.read(&mut req).unwrap();
                let line = String::from_utf8_lossy(&req[..n]);
                let body = line
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .trim_start_matches('/')
                    .split('?')
                    .next()
                    .unwrap()
                    .to_string();
                write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let dir = std::env::temp_dir().join(format!("lcplay-assets-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = AssetCache::new(dir.clone());
        let wait = |a: &Asset| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(p) = cache.local_path(a) {
                    return p;
                }
                assert!(Instant::now() < deadline, "download never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let url = |body: &str| format!("http://127.0.0.1:{port}/{body}");
        // Two assets whose names Windows cannot tell apart once cleaned.
        let a = Asset {
            url: url("aaa"),
            ..asset("A", "x?.mov", 3)
        };
        let b = Asset {
            url: url("bbbb"),
            ..asset("B", "x*.mov", 4)
        };
        let pa = wait(&a);
        let pb = wait(&b);
        assert_eq!(pa, dir.join("x_.mov"));
        assert_ne!(pa, pb, "the second one gets a suffix");
        assert_eq!(std::fs::read(&pb).unwrap(), b"bbbb");
        // A new version of A (re-optimised): same name, new content, and
        // the index follows.
        let a2 = Asset {
            url: url("aaaaa?v=2"),
            size: 5,
            ..a.clone()
        };
        assert_eq!(wait(&a2), dir.join("x_.mov"));
        assert_eq!(std::fs::read(dir.join("x_.mov")).unwrap(), b"aaaaa");
        assert_eq!(cache.read_index()["A"].size, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
