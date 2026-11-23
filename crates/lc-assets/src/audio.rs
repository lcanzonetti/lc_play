//! Sound extraction: each file's audio, once, as the PCM file Runners play.
//!
//! Runners never decode audio at show time. For every video or audio file
//! in the library that has a sound track, the Asset Manager writes its
//! first audio stream as a WAV file — 32-bit float, 48 kHz
//! (`lc_proto::audio::SAMPLE_RATE`), all of its channels, RF64 when over
//! 4 GB — which a Runner downloads like any media and reads straight from a
//! memory mapping.
//!
//! * **Where**: next to the optimised files, `Optimized/<source path>.wav`
//!   (the source's own extension kept, so `clip.mp4` and `clip.mov` do not
//!   collide). The library does not list that folder.
//! * **Independent of optimisation**: sound is extracted whether or not
//!   video optimisation is on, and from the *original* (the optimised HAP
//!   files carry no sound).
//! * **Alignment**: the sound's first sample is not always at the video's
//!   first frame (encoder delay, edit lists). The offset between the two
//!   streams' start times is recorded, and Runners apply it: lip-sync is
//!   kept to the sample.
//! * The manifest (`.lcplay/audio.json`) records, per file, the version
//!   (size, mtime) a sidecar was made from, and files without sound or
//!   that failed, so nothing is redone in a loop.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::error::AppError;
use crate::library::{MediaItem, MediaKind};
use crate::optimize::{mtime_of, OWN_DIR};
use crate::state::AppState;

const MANIFEST: &str = "audio.json";
/// A source that changed this recently may still be being copied in.
const SETTLE: Duration = Duration::from_secs(10);
/// Longest an extraction may take (a two-hour 64-channel file is well
/// under this on any disk).
const TIMEOUT: Duration = Duration::from_secs(1800);

/// What the manifest remembers about one source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Record {
    /// The source version it was made from.
    pub size: u64,
    pub mtime: u64,
    /// The sidecar, relative to the media folder; `None`: no sound.
    pub output: Option<String>,
    pub output_size: u64,
    pub channels: u32,
    pub frames: u64,
    /// Media time of the sidecar's first sample (see the module notes).
    pub offset_s: f64,
    pub error: Option<String>,
    /// When it was made (ms since 1970): part of its URL.
    pub made: u64,
}

/// An item's sound, as the library listing reports it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    /// `pending`, `extracting`, `done`, `none` (no sound track), `failed`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set when `done`: what a show's `Asset::audio` holds (the URL
    /// relative to the Asset Manager).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_s: Option<f64>,
}

pub struct Extractor {
    media_dir: PathBuf,
    ffmpeg: String,
    ffprobe: String,
    manifest: Mutex<BTreeMap<String, Record>>,
    /// The item being extracted now.
    running: Mutex<Option<String>>,
    /// Item path → (size, when first seen at that size).
    seen: Mutex<HashMap<String, (u64, std::time::Instant)>>,
}

impl Extractor {
    pub fn new(media_dir: &Path, ffmpeg: &str, ffprobe: &str) -> std::sync::Arc<Self> {
        let manifest = std::fs::read(media_dir.join(OWN_DIR).join(MANIFEST))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        std::sync::Arc::new(Extractor {
            media_dir: media_dir.to_path_buf(),
            ffmpeg: ffmpeg.to_string(),
            ffprobe: ffprobe.to_string(),
            manifest: Mutex::new(manifest),
            running: Mutex::new(None),
            seen: Mutex::new(HashMap::new()),
        })
    }

    /// The record of `item` at version `mtime`, if it is current.
    fn record(&self, item: &MediaItem, mtime: u64) -> Option<Record> {
        self.manifest
            .lock()
            .unwrap()
            .get(&item.path)
            .filter(|r| r.size == item.size && r.mtime == mtime)
            .cloned()
    }

    pub fn status(&self, item: &MediaItem, mtime: u64) -> Option<Status> {
        if !matches!(item.kind, MediaKind::Video | MediaKind::Audio) {
            return None;
        }
        let blank = Status {
            state: "pending",
            error: None,
            url: None,
            size: None,
            path: None,
            channels: None,
            frames: None,
            offset_s: None,
        };
        if self.running.lock().unwrap().as_deref() == Some(item.path.as_str()) {
            return Some(Status {
                state: "extracting",
                ..blank
            });
        }
        let Some(r) = self.record(item, mtime) else {
            return Some(blank);
        };
        if let Some(e) = r.error {
            return Some(Status {
                state: "failed",
                error: Some(e),
                ..blank
            });
        }
        let Some(out) = r.output else {
            return Some(Status {
                state: "none",
                ..blank
            });
        };
        // Still there, and whole? Otherwise it is made again.
        if !std::fs::metadata(self.media_dir.join(&out)).is_ok_and(|m| m.len() == r.output_size) {
            return Some(blank);
        }
        Some(Status {
            state: "done",
            url: Some(format!("/audio/{}?v={}", item.id, r.made)),
            size: Some(r.output_size),
            path: Some(out),
            channels: Some(r.channels),
            frames: Some(r.frames),
            offset_s: Some(r.offset_s),
            ..blank
        })
    }

    fn save(&self) {
        let m = self.manifest.lock().unwrap().clone();
        let path = self.media_dir.join(OWN_DIR).join(MANIFEST);
        let tmp = path.with_extension("tmp");
        let result = std::fs::create_dir_all(self.media_dir.join(OWN_DIR))
            .and_then(|()| {
                std::fs::write(&tmp, serde_json::to_vec_pretty(&m).expect("serialisable"))
            })
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = result {
            tracing::error!("cannot save the audio manifest: {e}");
        }
    }

    /// Forget a deleted source and remove its sidecar.
    pub fn forget(&self, path: &str) {
        let record = self.manifest.lock().unwrap().remove(path);
        if let Some(out) = record.and_then(|r| r.output) {
            let _ = std::fs::remove_file(self.media_dir.join(out));
        }
        self.save();
    }

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

/// `<output folder>/<source path>.wav`.
pub fn sidecar_path(out_dir: &str, path: &str) -> String {
    format!("{out_dir}/{}.wav", path.replace('\\', "/"))
}

/// Extract what needs it, one file at a time, in the background.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            step(&state).await;
        }
    });
}

async fn step(state: &AppState) {
    let ex = state.audio.clone();
    // Sources gone (deleted outside the Asset Manager): their sidecars too.
    let gone: Vec<String> = ex
        .manifest
        .lock()
        .unwrap()
        .keys()
        .filter(|p| state.library.get_by_path(p).is_none() && !ex.media_dir.join(p).exists())
        .cloned()
        .collect();
    for path in gone {
        ex.forget(&path);
    }
    for item in state.library.list() {
        if !matches!(item.kind, MediaKind::Video | MediaKind::Audio) || !ex.settled(&item) {
            continue;
        }
        let mtime = mtime_of(&item.abs_path).await;
        if ex.status(&item, mtime).is_none_or(|s| s.state != "pending") {
            continue;
        }
        *ex.running.lock().unwrap() = Some(item.path.clone());
        let record = extract(state, &item, mtime).await;
        *ex.running.lock().unwrap() = None;
        // Changed while it ran: the result is of no use.
        if mtime_of(&item.abs_path).await != mtime
            || tokio::fs::metadata(&item.abs_path)
                .await
                .map_or(true, |m| m.len() != item.size)
        {
            if let Some(out) = &record.output {
                let _ = std::fs::remove_file(ex.media_dir.join(out));
            }
            return;
        }
        let previous = ex
            .manifest
            .lock()
            .unwrap()
            .insert(item.path.clone(), record.clone());
        if let Some(old) = previous.and_then(|r| r.output) {
            if Some(&old) != record.output.as_ref() {
                let _ = std::fs::remove_file(ex.media_dir.join(old));
            }
        }
        ex.save();
        return; // one per step: the library may have changed
    }
}

/// The streams' facts `ffprobe` gives.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Streams {
    /// The first audio stream: channels and start time.
    audio: Option<(u32, f64)>,
    /// The first video stream's start time.
    video_start: Option<f64>,
}

fn parse_streams(json: &serde_json::Value) -> Streams {
    let start = |s: &serde_json::Value| {
        s["start_time"]
            .as_str()
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|t| t.is_finite())
            .unwrap_or(0.0)
    };
    let streams = json["streams"].as_array().cloned().unwrap_or_default();
    let of = |kind: &str| streams.iter().find(|s| s["codec_type"] == kind);
    Streams {
        audio: of("audio").and_then(|s| {
            let ch = s["channels"].as_u64().filter(|&c| c > 0)? as u32;
            Some((ch, start(s)))
        }),
        // Cover art (an attached picture) is not a video stream.
        video_start: streams
            .iter()
            .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
            .map(start),
    }
}

/// Sample frames in a WAV file of `len` bytes whose header is `head`.
fn wav_frames(head: &[u8], len: u64, channels: u32) -> Option<u64> {
    let mut at = 12usize;
    while at + 8 <= head.len() {
        let id = &head[at..at + 4];
        let size = u32::from_le_bytes(head[at + 4..at + 8].try_into().ok()?) as usize;
        if id == b"data" {
            return Some((len - (at + 8) as u64) / (4 * channels as u64));
        }
        at += 8 + size + (size & 1);
    }
    None
}

async fn extract(state: &AppState, item: &MediaItem, mtime: u64) -> Record {
    let ex = &state.audio;
    let mut record = Record {
        size: item.size,
        mtime,
        ..Default::default()
    };
    let probe = Command::new(&ex.ffprobe)
        .args(["-v", "error", "-show_entries"])
        .args(["stream=codec_type,channels,start_time:stream_disposition=attached_pic"])
        .args(["-of", "json"])
        .arg(&item.abs_path)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    let streams = match probe {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout)
            .map(|j| parse_streams(&j))
            .ok(),
        _ => None,
    };
    let Some(streams) = streams else {
        record.error = Some("ffprobe cannot read the file".into());
        return record;
    };
    let Some((channels, audio_start)) = streams.audio else {
        return record; // no sound
    };
    if channels as usize > lc_proto::audio::MAX_CUE_CHANNELS {
        record.error = Some(format!(
            "{channels} channels: at most {} are played",
            lc_proto::audio::MAX_CUE_CHANNELS
        ));
        return record;
    }
    let out_rel = sidecar_path(crate::optimize::output_dir_name(), &item.path);
    let out = ex.media_dir.join(&out_rel);
    let tmp = out.with_file_name(format!(
        ".{}.part.wav",
        out.file_stem().unwrap_or_default().to_string_lossy()
    ));
    if let Some(dir) = tmp.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    tracing::info!("extracting the sound of '{}'", item.path);
    let run = Command::new(&ex.ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(&item.abs_path)
        .args(["-map", "0:a:0", "-vn", "-sn", "-dn"])
        .args(["-c:a", "pcm_f32le", "-ar"])
        .arg(lc_proto::audio::SAMPLE_RATE.to_string())
        // No encoder tag (a LIST chunk) in the header; RF64 when needed.
        .args(["-fflags", "+bitexact", "-rf64", "auto", "-f", "wav"])
        .arg(&tmp)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let result: anyhow::Result<u64> = async {
        let o = tokio::time::timeout(TIMEOUT, run)
            .await
            .map_err(|_| anyhow::anyhow!("ffmpeg took too long"))??;
        anyhow::ensure!(
            o.status.success(),
            "ffmpeg: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
        tokio::fs::rename(&tmp, &out).await?;
        Ok(tokio::fs::metadata(&out).await?.len())
    }
    .await;
    match result {
        Ok(size) => {
            let mut head = vec![0u8; 4096];
            let n = std::fs::File::open(&out)
                .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
                .unwrap_or(0);
            record.frames = wav_frames(&head[..n], size, channels).unwrap_or(0);
            record.output = Some(out_rel);
            record.output_size = size;
            record.channels = channels;
            record.offset_s = streams.video_start.map_or(0.0, |v| audio_start - v);
            record.made = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            tracing::info!(
                "sound of '{}': {channels} channels, {:.1} s, offset {:+.1} ms",
                item.path,
                record.frames as f64 / lc_proto::audio::SAMPLE_RATE as f64,
                record.offset_s * 1000.0
            );
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!("extracting the sound of '{}' failed: {e:#}", item.path);
            record.error = Some(format!("{e:#}"));
        }
    }
    record
}

/// `GET /audio/{id}` — an item's sound file.
pub async fn serve(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    let mtime = mtime_of(&item.abs_path).await;
    // Only a path from the manifest, for an item in the index.
    let path = state
        .audio
        .status(&item, mtime)
        .and_then(|s| s.path)
        .ok_or(AppError::NotFound)?;
    crate::handlers::stream::serve_file(
        &state.audio.media_dir.join(path),
        "audio/wav".into(),
        &headers,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offset_is_between_the_audio_and_the_video_start() {
        let json = serde_json::json!({ "streams": [
            { "codec_type": "video", "start_time": "0.000000", "disposition": { "attached_pic": 0 } },
            { "codec_type": "audio", "channels": 6, "start_time": "0.021333" }
        ] });
        let s = parse_streams(&json);
        assert_eq!(s.audio.map(|a| a.0), Some(6));
        assert!((s.audio.unwrap().1 - s.video_start.unwrap() - 0.021333).abs() < 1e-9);

        // An mp3 with cover art: audio only.
        let json = serde_json::json!({ "streams": [
            { "codec_type": "audio", "channels": 2, "start_time": "0.025057" },
            { "codec_type": "video", "start_time": "0.025057", "disposition": { "attached_pic": 1 } }
        ] });
        assert_eq!(parse_streams(&json).video_start, None);

        let silent = serde_json::json!({ "streams": [ { "codec_type": "video" } ] });
        assert_eq!(parse_streams(&silent).audio, None);
    }

    #[test]
    fn sidecars_keep_the_source_extension() {
        assert_eq!(
            sidecar_path("Optimized", "Show A/clip.mp4"),
            "Optimized/Show A/clip.mp4.wav"
        );
        assert_eq!(
            sidecar_path("Optimized", "Show A\\x.wav"),
            "Optimized/Show A/x.wav.wav"
        );
    }

    #[tokio::test]
    async fn ffmpeg_writes_what_the_runner_reads() {
        // Skipped where ffmpeg is not installed.
        let dir = std::env::temp_dir().join(format!("lcplay-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("tone.mp4");
        let made = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:duration=1:sample_rate=44100",
            ])
            .args(["-f", "lavfi", "-i", "color=c=black:s=64x64:d=1"])
            .args(["-ac", "2", "-c:a", "aac", "-c:v", "libx264", "-shortest"])
            .arg(&src)
            .output()
            .await;
        if !made.is_ok_and(|o| o.status.success()) {
            eprintln!("ffmpeg with x264/aac unavailable: skipped");
            return;
        }
        let out = dir.join("tone.wav");
        let o = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&src)
            .args(["-map", "0:a:0", "-vn", "-c:a", "pcm_f32le", "-ar", "48000"])
            .args(["-fflags", "+bitexact", "-rf64", "auto", "-f", "wav"])
            .arg(&out)
            .output()
            .await
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let bytes = std::fs::read(&out).unwrap();
        let frames = wav_frames(&bytes, bytes.len() as u64, 2).unwrap();
        // About a second at 48 kHz (AAC pads a little).
        assert!((46_000..=50_000).contains(&frames), "{frames}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
