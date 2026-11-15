//! Managing the library from the Producer: upload, delete, and the metadata
//! and thumbnails its asset panel shows.
//!
//! Everything here works on files in the media directory; the in-memory
//! library is simply rescanned after a change. Media metadata comes from
//! `ffprobe`, thumbnails from `ffmpeg` — both cached in memory per file
//! version, so each file costs one run of each.

use std::path::Path as FsPath;
use std::process::Stdio;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::error::AppError;
use crate::handlers::browse::ItemDto;
use crate::library::{classify, MediaItem, MediaKind};
use crate::state::AppState;

/// Width of generated thumbnails, in pixels.
const THUMB_WIDTH: u32 = 240;

/// What `ffprobe` says about a file's first video stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Probe {
    pub width: u32,
    pub height: u32,
    /// Seconds; zero for stills.
    pub duration_s: f64,
}

/// An item as the API reports it: the indexed facts plus probe metadata.
pub async fn describe(state: &AppState, item: MediaItem) -> ItemDto {
    let probe = match item.kind {
        MediaKind::Audio => None,
        MediaKind::Video | MediaKind::Image => probe(state, &item).await,
    };
    let mut dto = ItemDto::from(item);
    dto.probe = probe;
    dto
}

async fn probe(state: &AppState, item: &MediaItem) -> Option<Probe> {
    if let Some((size, probe)) = state.probes.lock().unwrap().get(&item.id) {
        if *size == item.size {
            return *probe;
        }
    }
    let probe = run_ffprobe(state, item).await;
    // Failures are remembered too (until the file changes): the Producer
    // lists the library every few seconds.
    state
        .probes
        .lock()
        .unwrap()
        .insert(item.id.clone(), (item.size, probe));
    probe
}

async fn run_ffprobe(state: &AppState, item: &MediaItem) -> Option<Probe> {
    // Shares the ffmpeg budget: the first listing of a large library must
    // not start one process per file all at once.
    let _slot = state.transcode_slots.acquire().await.ok()?;
    let out = Command::new(&state.config.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height:format=duration"])
        .args(["-of", "json"])
        .arg(&item.abs_path)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let stream = json["streams"].get(0)?;
    let probe = Probe {
        width: stream["width"].as_u64()? as u32,
        height: stream["height"].as_u64()? as u32,
        duration_s: match item.kind {
            // ffprobe reports a nominal 0.04 s for a still; that is not a duration.
            MediaKind::Image => 0.0,
            _ => json["format"]["duration"]
                .as_str()
                .and_then(|d| d.parse::<f64>().ok())
                .filter(|d| d.is_finite() && *d > 0.0)
                .unwrap_or(0.0),
        },
    };
    Some(probe)
}

#[derive(Deserialize)]
pub struct UploadParams {
    /// File name to store the upload under.
    name: String,
}

/// Reduce a client-supplied name to a plain file name inside the media
/// directory. `None` if nothing usable is left.
fn safe_file_name(name: &str) -> Option<String> {
    // Whatever path the browser sent, keep only its last component…
    let base = name.rsplit(['/', '\\']).next()?.trim();
    // …and refuse names that are special to the file system or to us
    // (dot-files are in-progress uploads).
    if base.is_empty() || base.starts_with('.') || base.contains('\0') || base.len() > 200 {
        return None;
    }
    Some(base.to_string())
}

/// `POST /api/upload?name=<file name>` — store the request body as a media
/// file and index it. The body is the raw file (not a multipart form), so it
/// can be streamed to disk without buffering and sent from a browser with a
/// plain `fetch(url, { method: "POST", body: file })`.
pub async fn upload(
    State(state): State<AppState>,
    Query(params): Query<UploadParams>,
    body: Body,
) -> Result<Json<ItemDto>, AppError> {
    let name = safe_file_name(&params.name)
        .ok_or_else(|| AppError::BadRequest("unusable file name".into()))?;
    if classify(FsPath::new(&name)).is_none() {
        return Err(AppError::BadRequest(format!(
            "'{name}' is not a supported media type"
        )));
    }

    let root = state.library.root().to_path_buf();
    tokio::fs::create_dir_all(&root).await?;
    // Written under a hidden temporary name (the scanner skips dot-files)
    // and renamed when complete, so a half-received file is never served.
    // The counter keeps two simultaneous uploads of the same name from
    // writing into each other; the last one to finish wins the rename.
    static UPLOAD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = UPLOAD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = root.join(format!(".{name}.{seq}.uploading"));
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| std::io::Error::other(format!("upload interrupted: {e}")))?;
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        tokio::fs::rename(&tmp, root.join(&name)).await
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(AppError::Internal(format!("storing upload '{name}': {e}")));
    }

    rescan(&state).await;
    let item = state
        .library
        .get_by_path(&name)
        .ok_or_else(|| AppError::Internal(format!("'{name}' was stored but not indexed")))?;
    tracing::info!(name = %item.name, size = item.size, "asset uploaded");
    Ok(Json(describe(&state, item).await))
}

/// `DELETE /api/items/{id}` — remove a file from the media directory.
pub async fn delete_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Only paths present in the index can be deleted: an arbitrary id
    // resolves to nothing, so this cannot reach outside the media directory.
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    tokio::fs::remove_file(&item.abs_path).await?;
    rescan(&state).await;
    tracing::info!(name = %item.name, "asset deleted");
    Ok(Json(serde_json::json!({ "deleted": item.name })))
}

async fn rescan(state: &AppState) {
    let library = state.library.clone();
    let _ = tokio::task::spawn_blocking(move || library.rescan()).await;
}

/// `GET /thumb/{id}` — a small PNG of the image, or of the video's first
/// frame.
pub async fn thumbnail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    if item.kind == MediaKind::Audio {
        return Err(AppError::NotFound);
    }
    let cached = state
        .thumbs
        .lock()
        .unwrap()
        .get(&id)
        .filter(|(size, _)| *size == item.size)
        .map(|(_, jpeg)| jpeg.clone());
    let jpeg = match cached {
        Some(jpeg) => jpeg,
        None => {
            // Shares the transcode budget: a panel full of new files must
            // not start dozens of ffmpeg processes at once.
            let _slot = state
                .transcode_slots
                .acquire()
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?;
            let out = Command::new(&state.config.ffmpeg)
                .args(["-v", "error", "-nostdin", "-i"])
                .arg(&item.abs_path)
                .args(["-frames:v", "1", "-vf"])
                .arg(format!("scale={THUMB_WIDTH}:-2"))
                // PNG rather than JPEG: recent ffmpeg versions refuse to
                // encode MJPEG from ordinary (limited-range) video without
                // extra flags, which made thumbnails fail silently.
                .args(["-f", "image2pipe", "-c:v", "png", "pipe:1"])
                .stdin(Stdio::null())
                .output()
                .await
                .map_err(|e| AppError::Internal(format!("running ffmpeg: {e}")))?;
            if !out.status.success() || out.stdout.is_empty() {
                return Err(AppError::Internal(format!(
                    "thumbnail for '{}' failed: {}",
                    item.name,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let jpeg = Bytes::from(out.stdout);
            state
                .thumbs
                .lock()
                .unwrap()
                .insert(id, (item.size, jpeg.clone()));
            jpeg
        }
    };
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        jpeg,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_names_cannot_leave_the_media_directory() {
        assert_eq!(safe_file_name("clip.mp4").as_deref(), Some("clip.mp4"));
        assert_eq!(
            safe_file_name("../../etc/passwd.mp4").as_deref(),
            Some("passwd.mp4")
        );
        assert_eq!(
            safe_file_name("C:\\Users\\me\\My Clip.mov").as_deref(),
            Some("My Clip.mov")
        );
        assert_eq!(safe_file_name(".hidden.mp4"), None);
        assert_eq!(safe_file_name("dir/"), None);
        assert_eq!(safe_file_name(""), None);
    }
}
