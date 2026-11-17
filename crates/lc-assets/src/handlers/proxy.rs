//! Preview proxies: small, seekable H.264 copies of video assets, for the
//! Producer's stage preview.
//!
//! The stage preview is played by the Producer's web view, which cannot
//! decode HAP or ProRes, and should not have to decode a 4K original on the
//! operator's laptop either. So the Asset Manager makes, once per file, a
//! 640-pixel-wide H.264 copy with a keyframe every half second (the preview
//! seeks constantly to follow the timeline), stores it in a hidden folder of
//! the media folder (`.lcplay-proxies`, not listed as media), and serves it
//! with `Range` support like any media file.
//!
//! The first request for a file waits while its proxy is made (a few seconds
//! for a short clip, longer for a long one); later ones are served at once.
//! A replaced file (new size) gets a new proxy.

use std::process::Stdio;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use tokio::process::Command;

use crate::error::AppError;
use crate::handlers::stream::serve_file;
use crate::library::MediaKind;
use crate::state::AppState;

/// Folder of the proxies, inside the media folder.
pub const PROXY_DIR: &str = ".lcplay-proxies";
/// Proxy width in pixels.
const PROXY_WIDTH: u32 = 640;

/// `GET /proxy/{id}` — the preview proxy of a video (made on first request).
pub async fn proxy_media(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    if item.kind != MediaKind::Video {
        return Err(AppError::NotFound);
    }
    let dir = state.config.media_dir.join(PROXY_DIR);
    let safe_id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(120)
        .collect();
    let path = dir.join(format!("{safe_id}-{}.mp4", item.size));

    if !path.exists() {
        // One maker per item; later requests wait for it, then serve.
        let job = state
            .proxy_jobs
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _making = job.lock().await;
        if !path.exists() {
            tokio::fs::create_dir_all(&dir).await?;
            // Shares the transcode budget with thumbnails and live
            // transcodes.
            let _slot = state
                .transcode_slots
                .acquire()
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?;
            make_proxy(&state.config.ffmpeg, &item.abs_path, &path).await?;
            tracing::info!("preview proxy made for '{}'", item.name);
        }
    }
    serve_file(&path, "video/mp4".into(), &headers).await
}

/// Encode the proxy, trying encoders in turn: x264 where ffmpeg has it,
/// else the operating system's H.264 encoder (Windows: Media Foundation).
async fn make_proxy(
    ffmpeg: &str,
    source: &std::path::Path,
    target: &std::path::Path,
) -> Result<(), AppError> {
    let tmp = target.with_extension("part.mp4");
    let mut last_error = String::new();
    for encoder in [
        &["-c:v", "libx264", "-preset", "veryfast", "-crf", "28"][..],
        &["-c:v", "h264_mf", "-b:v", "2M"][..],
        &["-c:v", "libopenh264", "-b:v", "2M"][..],
    ] {
        let out = Command::new(ffmpeg)
            .args(["-v", "error", "-nostdin", "-y", "-i"])
            .arg(source)
            .args(["-an", "-sn", "-vf"])
            .arg(format!("scale='min({PROXY_WIDTH},iw)':-2,format=yuv420p"))
            .args(encoder)
            // A keyframe every half second or so: the preview seeks a lot.
            .args(["-g", "12", "-movflags", "+faststart", "-f", "mp4"])
            .arg(&tmp)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| AppError::Internal(format!("running ffmpeg: {e}")))?;
        if out.status.success() {
            tokio::fs::rename(&tmp, target).await?;
            return Ok(());
        }
        last_error = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    let _ = tokio::fs::remove_file(&tmp).await;
    Err(AppError::Internal(format!(
        "no preview proxy for '{}': {last_error}",
        source.display()
    )))
}
