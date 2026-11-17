//! Asset optimisation: converting media to the playback codec, in the
//! background, by rules set per source codec (like WATCHOUT 7's Asset
//! Manager).
//!
//! * **Rules** (`.lcplay/optimize.json` in the media folder, edited from the
//!   Producer): for each kind of source — H.264, HEVC, ProRes, ProRes with
//!   alpha, each HAP variant, NotchLC, DXV, anything else — the output
//!   (keep as is, re-chunk losslessly, or HAP / HAP Alpha / HAP Q), the
//!   number of chunks and whether chunks are Snappy-compressed.
//! * **Output**: `Optimized/` in the media folder, same sub-folders and file
//!   names as the originals (extension `.mov`). Originals are kept, so files
//!   can be optimised again with other settings. The library does not list
//!   `Optimized/`; an item's optimised version is a property of the item.
//! * **Jobs**: one at a time, in the background. HAP sources whose output is
//!   "re-chunk" go through `lc_hap::rechunk` (lossless: the texture is kept
//!   bit for bit); everything else through ffmpeg's HAP encoder. A HAP file
//!   already stored as asked (chunks, Snappy) is used as it is.
//! * **When**: new and changed files are optimised as they appear. Changing
//!   a rule does not touch what is done (as in WATCHOUT); "re-optimise" on an
//!   item, or "apply to existing files", does.
//! * The manifest (`.lcplay/optimized.json`) records, per file, the version
//!   (size, mtime) and rule an output was made from, and failures, so
//!   nothing is redone or retried in a loop.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt as _;
use tokio::process::Command;

use crate::library::{MediaItem, MediaKind};
use crate::state::AppState;

/// Folder of the optimised files, in the media folder — unless the media
/// folder already had an `Optimized` folder of its own (see
/// [`choose_output_dir`]).
pub const OPTIMIZED_DIR: &str = "Optimized";
/// The name used when `Optimized` is already taken by the user's media.
pub const OPTIMIZED_DIR_ALT: &str = "Optimized (LC Play)";

/// The output folder this Asset Manager uses, chosen once at start-up (the
/// library scan must leave it out from the first scan on).
static OUTPUT_DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The name of the folder optimised files go in.
pub fn output_dir_name() -> &'static str {
    OUTPUT_DIR.get().map_or(OPTIMIZED_DIR, String::as_str)
}

/// Pick the output folder: `Optimized`, unless the media folder already has
/// a top-level folder of that name (in any letter case — Windows does not
/// tell them apart) holding the user's own media from before LC Play made
/// one. The choice is stored, so it never changes afterwards.
fn choose_output_dir(media_dir: &Path) -> String {
    let stored = media_dir.join(OWN_DIR).join("output-dir");
    if let Ok(name) = std::fs::read_to_string(&stored) {
        let name = name.trim();
        if name == OPTIMIZED_DIR || name == OPTIMIZED_DIR_ALT {
            return name.to_string();
        }
    }
    let users_own = std::fs::read_dir(media_dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(OPTIMIZED_DIR)
                && std::fs::read_dir(e.path()).is_ok_and(|mut d| d.next().is_some())
        });
    // A manifest means the folder is ours (made before this choice was
    // stored).
    let ours = media_dir.join(OWN_DIR).join("optimized.json").exists();
    let name = if users_own && !ours {
        tracing::warn!(
            "the media folder already has an '{OPTIMIZED_DIR}' folder: optimised files go in \
             '{OPTIMIZED_DIR_ALT}' instead"
        );
        OPTIMIZED_DIR_ALT
    } else {
        OPTIMIZED_DIR
    };
    if let Err(e) = std::fs::create_dir_all(media_dir.join(OWN_DIR))
        .and_then(|()| std::fs::write(&stored, name))
    {
        tracing::warn!("cannot store the optimisation folder name: {e}");
    }
    name.to_string()
}

/// A source that changed this recently may still be being copied in: wait.
const SETTLE: Duration = Duration::from_secs(10);
/// ffmpeg reports progress twice a second: this long without a report and
/// it is taken as hung, and stopped.
const STALL: Duration = Duration::from_secs(300);
/// Folder of the Asset Manager's own files (settings, manifest).
pub const OWN_DIR: &str = ".lcplay";

/// What a source file is, for choosing its rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    H264,
    Hevc,
    Prores,
    ProresAlpha,
    Hap,
    HapAlpha,
    HapQ,
    HapR,
    HapQAlpha,
    Notchlc,
    Dxv,
    Other,
}

impl Source {
    pub const ALL: [Source; 12] = [
        Source::H264,
        Source::Hevc,
        Source::Prores,
        Source::ProresAlpha,
        Source::Hap,
        Source::HapAlpha,
        Source::HapQ,
        Source::HapR,
        Source::HapQAlpha,
        Source::Notchlc,
        Source::Dxv,
        Source::Other,
    ];

    /// From ffprobe's codec name, codec tag and pixel format. The HAP tags
    /// come first: ffprobe gives no codec name for variants its decoder does
    /// not know (HAP R, `Hap7`).
    pub fn classify(codec: &str, tag: &str, pix_fmt: &str) -> Source {
        match tag {
            "Hap1" => return Source::Hap,
            "Hap5" | "HapA" => return Source::HapAlpha,
            "HapY" => return Source::HapQ,
            "Hap7" => return Source::HapR,
            "HapM" => return Source::HapQAlpha,
            _ => {}
        }
        match codec {
            "h264" => Source::H264,
            "hevc" => Source::Hevc,
            "prores" if pix_fmt.contains("yuva") => Source::ProresAlpha,
            "prores" => Source::Prores,
            "hap" => match tag {
                "Hap5" | "HapA" => Source::HapAlpha,
                "HapY" => Source::HapQ,
                "Hap7" => Source::HapR,
                "HapM" => Source::HapQAlpha,
                _ => Source::Hap,
            },
            "notchlc" => Source::Notchlc,
            "dxv" => Source::Dxv,
            _ => Source::Other,
        }
    }

    fn is_hap(self) -> bool {
        matches!(
            self,
            Source::Hap | Source::HapAlpha | Source::HapQ | Source::HapR | Source::HapQAlpha
        )
    }
}

/// What a source becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// Played as it is.
    Keep,
    /// HAP sources: same variant, re-chunked losslessly.
    Rechunk,
    Hap,
    HapAlpha,
    HapQ,
}

/// One row of the codec map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub output: Output,
    /// Chunks per frame, 1–64: how many CPU cores a Runner can unpack a
    /// frame on at once.
    pub chunks: u32,
    /// Snappy-compress the chunks (smaller files, CPU to unpack) or not
    /// (no unpacking at all, about twice the size).
    pub snappy: bool,
}

/// The codec map, and whether optimisation runs at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub enabled: bool,
    pub rules: BTreeMap<Source, Rule>,
}

/// Chunks by default: enough for the cores of a typical show machine.
pub const DEFAULT_CHUNKS: u32 = 8;

impl Default for Settings {
    fn default() -> Self {
        let rule = |output| Rule {
            output,
            chunks: DEFAULT_CHUNKS,
            snappy: true,
        };
        let rules = Source::ALL
            .iter()
            .map(|&s| {
                let output = match s {
                    Source::ProresAlpha => Output::HapAlpha,
                    // HAP Q Alpha has two textures: ffmpeg cannot write it
                    // and the re-chunker does not split it.
                    Source::HapQAlpha => Output::Keep,
                    s if s.is_hap() => Output::Rechunk,
                    _ => Output::HapQ,
                };
                (s, rule(output))
            })
            .collect();
        Settings {
            enabled: true,
            rules,
        }
    }
}

impl Settings {
    pub fn rule(&self, source: Source) -> Rule {
        self.rules
            .get(&source)
            .copied()
            .unwrap_or_else(|| Settings::default().rules[&source])
    }

    /// Bring values from the network or a file within bounds.
    pub fn sanitize(&mut self) {
        let defaults = Settings::default();
        for s in Source::ALL {
            let rule = self.rules.entry(s).or_insert(defaults.rules[&s]);
            rule.chunks = rule.chunks.clamp(1, 64);
            let rechunk_ok = s.is_hap() && s != Source::HapQAlpha;
            if rule.output == Output::Rechunk && !rechunk_ok {
                rule.output = if s.is_hap() {
                    Output::Keep
                } else {
                    Output::HapQ
                };
            }
        }
    }
}

/// What the manifest remembers about one file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The source version it was made from.
    pub size: u64,
    pub mtime: u64,
    pub source: Source,
    pub rule: Rule,
    /// The optimised file, relative to the media folder; `None` = the
    /// original is played (kept, or already as asked).
    pub output: Option<String>,
    pub output_size: u64,
    pub error: Option<String>,
    /// When the output was made (milliseconds since 1970): part of its URL, so a
    /// re-optimised file is a new version to Runners even at the same size.
    #[serde(default)]
    pub made: u64,
    /// Asked to be made again (re-optimise, or a changed rule). Until the
    /// new output is there, the old one stays in use: a show does not fall
    /// back to the original, and Runners do not download twice.
    #[serde(default)]
    pub stale: bool,
}

/// The optimiser's state, shared with the HTTP handlers.
pub struct Optimizer {
    media_dir: PathBuf,
    ffmpeg: String,
    settings: Mutex<Settings>,
    manifest: Mutex<BTreeMap<String, Record>>,
    /// Item path → progress in ‰, while its job runs.
    running: Mutex<HashMap<String, Arc<AtomicU32>>>,
    /// Item path → (size, when first seen at that size): a file is
    /// optimised once its size has stood still for [`SETTLE`].
    seen: Mutex<HashMap<String, (u64, std::time::Instant)>>,
    /// The output folder (see [`choose_output_dir`]).
    out_dir: String,
}

/// An item's optimisation, as the library listing reports it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    /// `original` (not a video, or optimisation off), `pending`,
    /// `optimizing`, `done` (`output` set: an optimised file; unset: the
    /// original is used as it is), `failed`.
    pub state: &'static str,
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The optimised file, relative to the media folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_size: Option<u64>,
    /// Version of the output (when it was made).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

impl Optimizer {
    pub fn new(media_dir: &Path, ffmpeg: &str) -> Arc<Self> {
        let own = media_dir.join(OWN_DIR);
        let mut settings: Settings = read_json(&own.join("optimize.json")).unwrap_or_default();
        settings.sanitize();
        let out_dir = choose_output_dir(media_dir);
        let _ = OUTPUT_DIR.set(out_dir.clone());
        let manifest = read_json(&own.join("optimized.json")).unwrap_or_default();
        Arc::new(Optimizer {
            seen: Mutex::new(HashMap::new()),
            out_dir,
            media_dir: media_dir.to_path_buf(),
            ffmpeg: ffmpeg.to_string(),
            settings: Mutex::new(settings),
            manifest: Mutex::new(manifest),
            running: Mutex::new(HashMap::new()),
        })
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn set_settings(&self, mut settings: Settings) -> std::io::Result<()> {
        settings.sanitize();
        write_json(
            &self.media_dir.join(OWN_DIR).join("optimize.json"),
            &settings,
        )?;
        *self.settings.lock().unwrap() = settings;
        Ok(())
    }

    /// Optimise `path` again (with the current rules). `false`: there is
    /// no record of it yet (it is pending anyway).
    pub fn redo(&self, path: &str) -> bool {
        let found = self
            .manifest
            .lock()
            .unwrap()
            .get_mut(path)
            .map(|r| r.stale = true)
            .is_some();
        self.save_manifest();
        found
    }

    /// Optimise again every file whose record was made with another rule
    /// than its source's current one, and retry failures. Returns how many.
    pub fn apply_to_existing(&self) -> usize {
        let settings = self.settings();
        let mut n = 0;
        for r in self.manifest.lock().unwrap().values_mut() {
            if !r.stale && (r.error.is_some() || r.rule != settings.rule(r.source)) {
                r.stale = true;
                n += 1;
            }
        }
        self.save_manifest();
        n
    }

    /// Forget a deleted file and remove its optimised version.
    pub fn forget(&self, path: &str) {
        let record = self.manifest.lock().unwrap().remove(path);
        if let Some(out) = record.and_then(|r| r.output) {
            let _ = std::fs::remove_file(self.media_dir.join(out));
        }
        self.save_manifest();
    }

    /// The status of `item`, whose file has modification time `mtime`.
    pub fn status(&self, item: &MediaItem, mtime: u64, source: Option<Source>) -> Status {
        let blank = Status {
            state: "original",
            source,
            progress: None,
            rule: None,
            error: None,
            output: None,
            output_size: None,
            version: None,
        };
        if item.kind != MediaKind::Video {
            return blank;
        }
        let progress = self
            .running
            .lock()
            .unwrap()
            .get(&item.path)
            .map(|p| p.load(Ordering::Relaxed) as f32 / 1000.0);
        // A record of another version of the file says nothing about this
        // one.
        let record = self
            .manifest
            .lock()
            .unwrap()
            .get(&item.path)
            .filter(|r| r.size == item.size && r.mtime == mtime)
            .cloned();
        let enabled = self.settings.lock().unwrap().enabled;
        let Some(r) = record else {
            return match (progress, enabled) {
                (Some(p), _) => Status {
                    state: "optimizing",
                    progress: Some(p),
                    ..blank
                },
                (None, true) => Status {
                    state: "pending",
                    ..blank
                },
                (None, false) => blank,
            };
        };
        // The output must still be there, and whole: if not, it is made
        // again (and the original played meanwhile).
        let intact = r.output.as_ref().is_none_or(|o| {
            std::fs::metadata(self.media_dir.join(o)).is_ok_and(|m| m.len() == r.output_size)
        });
        let (output, output_size, version) = match (&r.output, intact, r.error.is_none()) {
            (Some(o), true, true) => (Some(o.clone()), Some(r.output_size), Some(r.made)),
            _ => (None, None, None),
        };
        let state = if progress.is_some() {
            "optimizing"
        } else if !intact || (r.stale && enabled) {
            "pending"
        } else if r.error.is_some() {
            "failed"
        } else {
            "done"
        };
        Status {
            state,
            progress,
            rule: Some(r.rule),
            error: r.error.filter(|_| state == "failed"),
            output,
            output_size,
            version,
            ..blank
        }
    }

    fn save_manifest(&self) {
        let m = self.manifest.lock().unwrap().clone();
        if let Err(e) = write_json(&self.media_dir.join(OWN_DIR).join("optimized.json"), &m) {
            tracing::error!("cannot save the optimisation manifest: {e}");
        }
    }

    /// Where the optimised version of `path` goes: the same path under the
    /// output folder, with `.mov` — and, if another file already owns that
    /// name (`clip.mp4` and `clip.mov` side by side; or names differing only
    /// in letter case, one file on Windows), a tag from the source path.
    fn output_for(&self, path: &str) -> String {
        let base = output_path(&self.out_dir, path);
        let taken = self.manifest.lock().unwrap().iter().any(|(other, r)| {
            other != path
                && r.output
                    .as_deref()
                    .is_some_and(|o| o.to_lowercase() == base.to_lowercase())
        });
        if taken {
            let stem = base.strip_suffix(".mov").unwrap_or(&base);
            format!("{stem}~{:08x}.mov", fnv1a(path))
        } else {
            base
        }
    }

    /// Drop the records (and outputs) of sources that are gone — deleted
    /// outside the Asset Manager, e.g. in Explorer.
    fn prune(&self, library: &crate::library::Library) {
        let gone: Vec<String> = self
            .manifest
            .lock()
            .unwrap()
            .keys()
            .filter(|p| library.get_by_path(p).is_none() && !self.media_dir.join(p).exists())
            .cloned()
            .collect();
        for path in gone {
            tracing::info!("'{path}' is gone: removing its optimised version");
            self.forget(&path);
        }
    }

    /// `true` once `item`'s size has not changed for [`SETTLE`].
    fn settled(&self, item: &MediaItem) -> bool {
        let mut seen = self.seen.lock().unwrap();
        let now = std::time::Instant::now();
        let entry = seen.entry(item.path.clone()).or_insert((item.size, now));
        if entry.0 != item.size {
            *entry = (item.size, now);
        }
        now.duration_since(entry.1) >= SETTLE
    }
}

/// `<output folder>/<path with .mov>`.
fn output_path(out_dir: &str, path: &str) -> String {
    let p = Path::new(path).with_extension("mov");
    format!("{out_dir}/{}", p.to_string_lossy().replace('\\', "/"))
}

fn fnv1a(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    })
}

/// Watch the library and optimise what needs it, one file at a time.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = step(&state).await {
                tracing::warn!("optimisation: {e:#}");
            }
        }
    });
}

/// Optimise the next file that needs it, if any (and wait for it).
async fn step(state: &AppState) -> anyhow::Result<()> {
    let opt = state.optimizer.clone();
    opt.prune(&state.library);
    if !opt.settings.lock().unwrap().enabled {
        return Ok(());
    }
    for item in state.library.list() {
        if item.kind != MediaKind::Video || !opt.settled(&item) {
            continue;
        }
        let mtime = mtime_of(&item.abs_path).await;
        let probe = crate::handlers::manage::probe(state, &item).await;
        let Some((source, duration)) = probe.and_then(|p| Some((p.source?, p.duration_s))) else {
            continue;
        };
        if opt.status(&item, mtime, Some(source)).state != "pending" {
            continue;
        }
        let rule = opt.settings().rule(source);
        run_job(state, &item, mtime, source, rule, duration).await;
        return Ok(()); // one per step: the library may have changed
    }
    Ok(())
}

/// Optimise one file and record the result.
async fn run_job(
    state: &AppState,
    item: &MediaItem,
    mtime: u64,
    source: Source,
    rule: Rule,
    duration: f64,
) {
    let opt = state.optimizer.clone();
    let mut record = Record {
        size: item.size,
        mtime,
        source,
        rule,
        output: None,
        output_size: 0,
        error: None,
        made: 0,
        stale: false,
    };
    // Nothing to make: kept as it is, or HAP already stored as asked.
    let fits = match rule.output {
        Output::Keep => true,
        Output::Rechunk => {
            let path = item.abs_path.clone();
            tokio::task::spawn_blocking(move || lc_hap::rechunk::file_layout(&path))
                .await
                .ok()
                .and_then(Result::ok)
                .is_some_and(|(_, l)| l.chunks == rule.chunks as usize && l.snappy == rule.snappy)
        }
        _ => false,
    };
    if !fits {
        let progress = Arc::new(AtomicU32::new(0));
        opt.running
            .lock()
            .unwrap()
            .insert(item.path.clone(), progress.clone());
        let out_rel = opt.output_for(&item.path);
        let out = opt.media_dir.join(&out_rel);
        let tmp = out.with_file_name(format!(
            ".{}.part.mov",
            out.file_stem().unwrap_or_default().to_string_lossy()
        ));
        tracing::info!(
            "optimising '{}' ({source:?} → {:?}, {} chunks)",
            item.path,
            rule.output,
            rule.chunks
        );
        let result = make(&opt, item, rule, duration, &tmp, progress).await;
        opt.running.lock().unwrap().remove(&item.path);
        match result.and_then(|()| {
            std::fs::rename(&tmp, &out)?;
            Ok(std::fs::metadata(&out)?.len())
        }) {
            Ok(size) => {
                tracing::info!("optimised '{}' → {out_rel}", item.path);
                record.output = Some(out_rel);
                record.output_size = size;
                record.made = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                tracing::warn!("optimising '{}' failed: {e:#}", item.path);
                record.error = Some(format!("{e:#}"));
            }
        }
    }
    // Deleted (or replaced) while the job ran: the result is of no use.
    let current = tokio::fs::metadata(&item.abs_path).await.ok();
    if current.is_none_or(|m| m.len() != item.size) || mtime_of(&item.abs_path).await != mtime {
        if let Some(out) = &record.output {
            let _ = std::fs::remove_file(opt.media_dir.join(out));
        }
        tracing::info!(
            "'{}' changed while it was optimised: will be redone",
            item.path
        );
        return;
    }
    // An older output of this file (other rule) is not needed any more.
    let previous = opt
        .manifest
        .lock()
        .unwrap()
        .insert(item.path.clone(), record.clone());
    if let Some(old) = previous.and_then(|r| r.output) {
        if Some(&old) != record.output.as_ref() {
            let _ = std::fs::remove_file(opt.media_dir.join(old));
        }
    }
    opt.save_manifest();
}

/// Produce the optimised file at `tmp`.
async fn make(
    opt: &Optimizer,
    item: &MediaItem,
    rule: Rule,
    duration: f64,
    tmp: &Path,
    progress: Arc<AtomicU32>,
) -> anyhow::Result<()> {
    if let Some(dir) = tmp.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    if rule.output == Output::Rechunk {
        let (src, dst) = (item.abs_path.clone(), tmp.to_path_buf());
        return tokio::task::spawn_blocking(move || {
            lc_hap::rechunk::rechunk(&src, &dst, rule.chunks as usize, rule.snappy, &mut |p| {
                progress.store((p * 1000.0) as u32, Ordering::Relaxed)
            })
        })
        .await?;
    }
    let format = match rule.output {
        Output::Hap => "hap",
        Output::HapAlpha => "hap_alpha",
        _ => "hap_q",
    };
    // HAP needs width and height in multiples of 4: pad (transparent for
    // HAP Alpha).
    let pad = if rule.output == Output::HapAlpha {
        "format=rgba,pad=ceil(iw/4)*4:ceil(ih/4)*4:color=black@0"
    } else {
        "pad=ceil(iw/4)*4:ceil(ih/4)*4"
    };
    let mut child = Command::new(&opt.ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(&item.abs_path)
        .args([
            "-map", "0:v:0", "-an", "-sn", "-vf", pad, "-c:v", "hap", "-format", format,
        ])
        .args(["-chunks", &rule.chunks.to_string()])
        .args(["-compressor", if rule.snappy { "snappy" } else { "none" }])
        .args(["-progress", "pipe:1", "-nostats", "-f", "mov"])
        .arg(tmp)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    // Read ffmpeg's messages as they come, keeping the last few KB for the
    // error report: unread, a damaged source that logs an error per frame
    // fills the pipe and ffmpeg (and with it every job) stops.
    let stderr = child.stderr.take();
    let messages = tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let mut kept = Vec::new();
        if let Some(mut stderr) = stderr {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut buf).await {
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > 8192 {
                    kept.drain(..kept.len() - 8192);
                }
            }
        }
        String::from_utf8_lossy(&kept).trim().to_string()
    });
    if let Some(stdout) = child.stdout.take() {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        loop {
            let line = match tokio::time::timeout(STALL, lines.next_line()).await {
                Ok(Ok(Some(line))) => line,
                Ok(_) => break,
                Err(_) => {
                    let _ = child.kill().await;
                    anyhow::bail!(
                        "ffmpeg made no progress for {} minutes and was stopped",
                        STALL.as_secs() / 60
                    );
                }
            };
            if let Some(us) = line
                .strip_prefix("out_time_us=")
                .and_then(|v| v.parse::<f64>().ok())
            {
                let p = if duration > 0.0 {
                    (us / 1e6 / duration).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                progress.store((p * 1000.0) as u32, Ordering::Relaxed);
            }
        }
    }
    let status = child.wait().await?;
    let messages = messages.await.unwrap_or_default();
    anyhow::ensure!(status.success(), "ffmpeg: {messages}");
    Ok(())
}

/// Modification time of a file, in seconds.
pub async fn mtime_of(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Write via a temporary file, so a crash leaves the old or the new one.
fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(value).expect("serialisable"),
    )?;
    std::fs::rename(tmp, path)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

use axum::extract::{Path as UrlPath, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;

use crate::error::AppError;

/// `GET /api/optimize` — the codec map.
pub async fn get_settings(State(state): State<AppState>) -> Json<Settings> {
    Json(state.optimizer.settings())
}

/// `PUT /api/optimize` — replace the codec map. Applies to files optimised
/// from now on; `POST /api/optimize/apply` re-does the existing ones.
pub async fn put_settings(
    State(state): State<AppState>,
    Json(settings): Json<Settings>,
) -> Result<Json<Settings>, AppError> {
    state
        .optimizer
        .set_settings(settings)
        .map_err(|e| AppError::Internal(format!("saving the optimisation settings: {e}")))?;
    Ok(Json(state.optimizer.settings()))
}

/// `POST /api/optimize/apply` — optimise again the files made with another
/// rule than the current one, and retry failures.
pub async fn apply(State(state): State<AppState>) -> Json<serde_json::Value> {
    let queued = state.optimizer.apply_to_existing();
    Json(serde_json::json!({ "queued": queued }))
}

/// `POST /api/items/{id}/optimize` — optimise one file again.
pub async fn redo_item(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    if item.kind != MediaKind::Video {
        return Err(AppError::BadRequest("only videos are optimised".into()));
    }
    state.optimizer.redo(&item.path);
    Ok(Json(serde_json::json!({ "queued": item.path })))
}

/// `GET /optimized/{id}` — the optimised version of an item (with `Range`).
pub async fn serve(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    let mtime = mtime_of(&item.abs_path).await;
    let status = state.optimizer.status(&item, mtime, None);
    // Only a path from the manifest, for an item in the index: nothing
    // outside the media folder can be named.
    let out = status.output.ok_or(AppError::NotFound)?;
    crate::handlers::stream::serve_file(
        &state.optimizer.media_dir.join(out),
        "video/quicktime".into(),
        &headers,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_classified_from_ffprobe() {
        assert_eq!(Source::classify("h264", "avc1", "yuv420p"), Source::H264);
        assert_eq!(
            Source::classify("prores", "ap4h", "yuva444p12le"),
            Source::ProresAlpha
        );
        assert_eq!(
            Source::classify("prores", "apcn", "yuv422p10le"),
            Source::Prores
        );
        assert_eq!(Source::classify("hap", "HapY", ""), Source::HapQ);
        assert_eq!(Source::classify("hap", "HapM", ""), Source::HapQAlpha);
        // ffprobe gives no codec name for HAP R.
        assert_eq!(Source::classify("", "Hap7", ""), Source::HapR);
        assert_eq!(Source::classify("vp9", "", ""), Source::Other);
    }

    #[test]
    fn settings_are_kept_within_bounds() {
        let mut s = Settings::default();
        s.rules.insert(
            Source::H264,
            Rule {
                output: Output::Rechunk,
                chunks: 0,
                snappy: true,
            },
        );
        s.rules.insert(
            Source::HapQAlpha,
            Rule {
                output: Output::Rechunk,
                chunks: 500,
                snappy: false,
            },
        );
        s.rules.remove(&Source::Dxv);
        s.sanitize();
        assert_eq!(
            s.rule(Source::H264).output,
            Output::HapQ,
            "re-chunk is for HAP only"
        );
        assert_eq!(s.rule(Source::H264).chunks, 1);
        assert_eq!(
            s.rule(Source::HapQAlpha),
            Rule {
                output: Output::Keep,
                chunks: 64,
                snappy: false
            }
        );
        assert!(
            s.rules.contains_key(&Source::Dxv),
            "missing rows get the default"
        );
    }

    #[test]
    fn optimised_files_keep_their_names_under_optimized() {
        assert_eq!(
            output_path("Optimized", "Show A/intro.mp4"),
            "Optimized/Show A/intro.mov"
        );
        assert_eq!(output_path("Optimized", "clip.mov"), "Optimized/clip.mov");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lcplay-opt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn two_sources_with_one_stem_get_two_outputs() {
        let dir = scratch("stem");
        let opt = Optimizer::new(&dir, "ffmpeg");
        let record = |output: &str| Record {
            size: 1,
            mtime: 1,
            source: Source::H264,
            rule: Settings::default().rule(Source::H264),
            output: Some(output.into()),
            output_size: 1,
            error: None,
            made: 1,
            stale: false,
        };
        opt.manifest
            .lock()
            .unwrap()
            .insert("clip.mp4".into(), record("Optimized/clip.mov"));
        // Its own output stays its own…
        assert_eq!(opt.output_for("clip.mp4"), "Optimized/clip.mov");
        // …and another source gets another name, in any letter case.
        let other = opt.output_for("clip.mov");
        assert!(
            other.starts_with("Optimized/clip~") && other.ends_with(".mov"),
            "{other}"
        );
        assert_ne!(
            opt.output_for("CLIP.mov").to_lowercase(),
            "optimized/clip.mov"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_users_own_optimized_folder_is_left_alone() {
        let dir = scratch("own");
        std::fs::create_dir_all(dir.join("optimized")).unwrap();
        std::fs::write(dir.join("optimized/mine.mp4"), b"x").unwrap();
        assert_eq!(choose_output_dir(&dir), OPTIMIZED_DIR_ALT);
        // The choice sticks.
        std::fs::remove_dir_all(dir.join("optimized")).unwrap();
        assert_eq!(choose_output_dir(&dir), OPTIMIZED_DIR_ALT);

        let dir = scratch("fresh");
        assert_eq!(choose_output_dir(&dir), OPTIMIZED_DIR);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
