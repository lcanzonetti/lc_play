//! On-the-fly transcoding by streaming `ffmpeg`'s stdout.
//!
//! We do progressive transcoding: `ffmpeg` reads the source, encodes to a
//! fragmented container that plays while it downloads, and writes to `pipe:1`,
//! which we forward straight to the HTTP response. Nothing is buffered to disk.
//!
//! Scalability / safety properties:
//!   * a [`Semaphore`] caps concurrent jobs (see [`AppState::transcode_slots`]);
//!   * `kill_on_drop` + holding the child in the response stream means a client
//!     disconnect tears down the `ffmpeg` process promptly (no zombies);
//!   * input-side `-ss` seeking makes `?t=` cheap, so a client can jump ahead
//!     without re-encoding everything before the seek point.

use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures::Stream;
use pin_project_lite::pin_project;
use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::ReaderStream;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct TranscodeParams {
    /// Output container/codec profile: `mp4` (default) or `webm`.
    #[serde(default)]
    format: Option<String>,
    /// Seek offset in seconds before encoding begins.
    #[serde(default)]
    t: Option<f64>,
    /// Scale to this output height (width auto, keeping aspect). e.g. `720`.
    #[serde(default)]
    height: Option<u32>,
    /// x264 CRF quality (0-51, lower = better). Ignored for `copy`.
    #[serde(default)]
    crf: Option<u32>,
    /// Audio bitrate in kbps.
    #[serde(default)]
    abitrate: Option<u32>,
    /// If set, remux only (`-c copy`) — no re-encode. Fast, but only works when
    /// the source codecs already fit the container.
    #[serde(default)]
    copy: Option<bool>,
}

/// `GET /transcode/{id}` — transcode/remux and stream the result.
pub async fn transcode_media(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<TranscodeParams>,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;

    // Acquire a transcode slot. We hold this permit for the whole stream so the
    // number of live `ffmpeg` processes never exceeds `max_transcodes`.
    let permit = state
        .transcode_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AppError::Internal("transcode semaphore closed".into()))?;

    let format = params
        .format
        .as_deref()
        .unwrap_or("mp4")
        .to_ascii_lowercase();
    let (content_type, args) = build_ffmpeg_args(&format, &item.abs_path, &params)?;

    let mut cmd = Command::new(&state.config.ffmpeg);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        AppError::Internal(format!(
            "failed to start ffmpeg ({}): {e}",
            state.config.ffmpeg
        ))
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Internal("ffmpeg produced no stdout".into()))?;

    tracing::debug!(item = %item.path, %format, "transcode started");

    let stream = TranscodeStream {
        inner: ReaderStream::new(stdout),
        _child: child,
        _permit: permit,
    };

    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            // Transcoded output has no known length and is not range-seekable;
            // use `?t=` to seek instead.
            (header::ACCEPT_RANGES, "none".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

/// Build the `ffmpeg` argument vector and the response content type.
fn build_ffmpeg_args(
    format: &str,
    input: &std::path::Path,
    params: &TranscodeParams,
) -> Result<(&'static str, Vec<String>), AppError> {
    let mut args: Vec<String> = Vec::new();

    args.push("-hide_banner".into());
    args.push("-loglevel".into());
    args.push("error".into());

    // Input-side seek (fast): place `-ss` before `-i`.
    if let Some(t) = params.t {
        if t.is_finite() && t > 0.0 {
            args.push("-ss".into());
            args.push(format!("{t:.3}"));
        }
    }

    args.push("-i".into());
    args.push(input.to_string_lossy().to_string());

    let copy = params.copy.unwrap_or(false);
    let scale_filter = params
        .height
        .map(|h| format!("scale=-2:{}", h.clamp(144, 4320)));

    let content_type: &'static str = match format {
        "mp4" => {
            if copy {
                args.extend(["-c".into(), "copy".into()]);
            } else {
                args.extend([
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    "veryfast".into(),
                    "-crf".into(),
                    params.crf.unwrap_or(23).clamp(0, 51).to_string(),
                    "-c:a".into(),
                    "aac".into(),
                    "-b:a".into(),
                    format!("{}k", params.abitrate.unwrap_or(128)),
                ]);
                if let Some(f) = &scale_filter {
                    args.extend(["-vf".into(), f.clone()]);
                }
            }
            // Fragmented MP4 so the stream is playable while still being written.
            args.extend([
                "-movflags".into(),
                "frag_keyframe+empty_moov+default_base_moof".into(),
                "-f".into(),
                "mp4".into(),
            ]);
            "video/mp4"
        }
        "webm" => {
            if copy {
                args.extend(["-c".into(), "copy".into()]);
            } else {
                args.extend([
                    "-c:v".into(),
                    "libvpx-vp9".into(),
                    "-deadline".into(),
                    "realtime".into(),
                    "-cpu-used".into(),
                    "5".into(),
                    "-crf".into(),
                    params.crf.unwrap_or(32).clamp(0, 63).to_string(),
                    "-b:v".into(),
                    "0".into(),
                    "-c:a".into(),
                    "libopus".into(),
                    "-b:a".into(),
                    format!("{}k", params.abitrate.unwrap_or(128)),
                ]);
                if let Some(f) = &scale_filter {
                    args.extend(["-vf".into(), f.clone()]);
                }
            }
            args.extend(["-f".into(), "webm".into()]);
            "video/webm"
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "unsupported transcode format '{other}' (use 'mp4' or 'webm')"
            )));
        }
    };

    // Write to stdout.
    args.push("pipe:1".into());
    Ok((content_type, args))
}

pin_project! {
    /// Wraps `ffmpeg`'s stdout and keeps the child process and the semaphore
    /// permit alive for exactly as long as the response body is being read.
    /// When the body is dropped (completion or client disconnect), the child is
    /// dropped too — and with `kill_on_drop` set, killed — and the permit is
    /// released back to the pool.
    struct TranscodeStream {
        #[pin]
        inner: ReaderStream<tokio::process::ChildStdout>,
        _child: Child,
        _permit: OwnedSemaphorePermit,
    }
}

impl Stream for TranscodeStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}
