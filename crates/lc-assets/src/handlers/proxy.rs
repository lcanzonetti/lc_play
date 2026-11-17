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
//! * The first request for a file starts its encode, in its own task, and
//!   is answered at once (503, retry later; `?status` tells when it is
//!   ready). No request waits on an encode, a client that goes away does not
//!   abandon it half-written, and there is at most one encode per file.
//! * Proxies have their own small concurrency limit, so a show full of new
//!   videos does not hold up thumbnails and probes for minutes.
//! * A proxy is named after the asset, its size and modification time: a
//!   file replaced in place gets a new one, and the old one is removed.
//! * A file that cannot be made into a proxy (no H.264 encoder in ffmpeg, a
//!   read-only media folder) is remembered and answered at once with 404;
//!   the Producer then plays the original if it can.

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::process::Command;

use crate::error::AppError;
use crate::handlers::stream::serve_file;
use crate::library::MediaKind;
use crate::state::AppState;

/// Folder of the proxies, inside the media folder.
pub const PROXY_DIR: &str = ".lcplay-proxies";
/// Proxy width in pixels.
const PROXY_WIDTH: u32 = 640;
/// Longest a proxy encode may take before it is abandoned.
const PROXY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Proxies made at the same time.
pub const PROXY_JOBS: usize = 2;

#[derive(Debug, Deserialize)]
pub struct ProxyParams {
    /// Answer with the proxy's state (`{"state": "ready" | "making" |
    /// "failed"}`) instead of the proxy.
    #[serde(default)]
    status: Option<String>,
}

/// `GET /proxy/{id}` — the preview proxy of a video. If it does not exist
/// yet, its encode is started in the background and the answer is
/// `503 Service Unavailable` with `Retry-After`: nothing waits on an encode,
/// so the browser's few connections to this server (six per host) stay free
/// for listings, thumbnails and uploads. `?status` reports progress.
pub async fn proxy_media(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ProxyParams>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    if item.kind != MediaKind::Video {
        return Err(AppError::NotFound);
    }
    let modified = tokio::fs::metadata(&item.abs_path)
        .await?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let version = (item.size, modified);
    let stem = format!("{:016x}", fnv1a(id.as_bytes()));
    let path = state
        .config
        .media_dir
        .join(PROXY_DIR)
        .join(format!("{stem}-{}-{modified}.mp4", item.size));

    let ready = tokio::fs::try_exists(&path).await.unwrap_or(false);
    let failed = !ready && state.proxy_failed.lock().unwrap().get(&id) == Some(&version);
    if !ready && !failed {
        start_job(
            &state,
            &id,
            &item.name,
            &item.abs_path,
            &path,
            &stem,
            version,
        );
    }
    if params.status.is_some() {
        let s = if ready {
            "ready"
        } else if failed {
            "failed"
        } else {
            "making"
        };
        return Ok(axum::Json(serde_json::json!({ "state": s })).into_response());
    }
    if ready {
        return serve_file(&path, "video/mp4".into(), &headers).await;
    }
    if failed {
        return Err(AppError::NotFound);
    }
    Ok((
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "5")],
        "preview proxy being made",
    )
        .into_response())
}

/// Start the encode of a proxy, unless one for this item is running. It
/// runs to the end whatever happens to the request that started it.
fn start_job(
    state: &AppState,
    id: &str,
    name: &str,
    source: &std::path::Path,
    target: &std::path::Path,
    stem: &str,
    version: (u64, u64),
) {
    if !state.proxy_jobs.lock().unwrap().insert(id.to_string()) {
        return; // already being made
    }
    let (state, id, name) = (state.clone(), id.to_string(), name.to_string());
    let (source, target, stem) = (source.to_path_buf(), target.to_path_buf(), stem.to_string());
    tokio::spawn(async move {
        let result = async {
            tokio::fs::create_dir_all(target.parent().expect("in the proxy folder")).await?;
            let _slot = state
                .proxy_slots
                .acquire()
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?;
            remove_older_versions(&target, &stem).await;
            make_proxy(&state.config.ffmpeg, &source, &target).await
        }
        .await;
        match result {
            Ok(()) => tracing::info!("preview proxy made for '{name}'"),
            Err(e) => {
                tracing::warn!("{e:?}");
                state
                    .proxy_failed
                    .lock()
                    .unwrap()
                    .insert(id.clone(), version);
            }
        }
        state.proxy_jobs.lock().unwrap().remove(&id);
    });
}

/// Remove the proxies of earlier versions of the same asset.
async fn remove_older_versions(current: &std::path::Path, stem: &str) {
    let Some(dir) = current.parent() else { return };
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{stem}-")) && e.path() != current {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

/// Remove every proxy of an asset (it was deleted).
pub async fn remove_proxies(media_dir: &std::path::Path, id: &str) {
    let stem = format!("{:016x}", fnv1a(id.as_bytes()));
    remove_older_versions(&media_dir.join(PROXY_DIR).join(format!("{stem}-")), &stem).await;
}

/// A stable 64-bit hash (FNV-1a) of the asset id, for the proxy's name.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Encode the proxy, trying encoders in turn: x264 where ffmpeg has it,
/// else the operating system's H.264 encoder (Windows: Media Foundation).
/// Each attempt writes its own temporary file, removed on failure.
async fn make_proxy(
    ffmpeg: &str,
    source: &std::path::Path,
    target: &std::path::Path,
) -> Result<(), AppError> {
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    let mut last_error = String::new();
    for encoder in [
        &["-c:v", "libx264", "-preset", "veryfast", "-crf", "28"][..],
        &["-c:v", "h264_mf", "-b:v", "2M"][..],
        &["-c:v", "libopenh264", "-b:v", "2M"][..],
    ] {
        let n = ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let tmp = target.with_extension(format!("{}-{n}.part.mp4", std::process::id()));
        let child = Command::new(ffmpeg)
            .args(["-v", "error", "-nostdin", "-y", "-i"])
            .arg(source)
            .args(["-an", "-sn", "-vf"])
            .arg(format!("scale='min({PROXY_WIDTH},iw)':-2,format=yuv420p"))
            .args(encoder)
            // A keyframe every half second or so: the preview seeks a lot.
            .args(["-g", "12", "-movflags", "+faststart", "-f", "mp4"])
            .arg(&tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AppError::Internal(format!("running ffmpeg: {e}")))?;
        let result = tokio::time::timeout(PROXY_TIMEOUT, child.wait_with_output()).await;
        match result {
            Ok(Ok(out)) if out.status.success() => {
                tokio::fs::rename(&tmp, target).await?;
                return Ok(());
            }
            Ok(Ok(out)) => last_error = String::from_utf8_lossy(&out.stderr).trim().to_string(),
            Ok(Err(e)) => last_error = e.to_string(),
            Err(_) => last_error = "timed out".into(),
        }
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    Err(AppError::Internal(format!(
        "no preview proxy for '{}': {last_error}",
        source.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_names_tell_long_similar_ids_apart() {
        let a = "6c6f6e672f666f6c6465722f".repeat(10) + "61";
        let b = "6c6f6e672f666f6c6465722f".repeat(10) + "62";
        assert_ne!(fnv1a(a.as_bytes()), fnv1a(b.as_bytes()));
    }
}
