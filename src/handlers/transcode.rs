//! On-the-fly transcoding by streaming `ffmpeg`'s stdout.
//!
//! We do progressive transcoding: `ffmpeg` reads the source, encodes to a
//! fragmented container that plays while it downloads, and writes to `pipe:1`,
//! which we forward straight to the HTTP response. Nothing is buffered to disk.
//!
//! Request flow, in order:
//!   1. validate the query and build the `ffmpeg` arguments (bad input gets a
//!      400 at once, without touching a slot);
//!   2. answer `HEAD` with headers only — never spawn `ffmpeg` for it;
//!   3. take a slot from a [`Semaphore`] (see [`AppState::transcode_slots`]),
//!      waiting at most [`SLOT_WAIT`], else `503` + `Retry-After`;
//!   4. spawn `ffmpeg` and wait for its **first output chunk** plus a short
//!      grace window ([`STARTUP_GRACE`]) before sending the status line. If it
//!      exits non-zero by then, the client gets a real error (422 for a failed
//!      `copy=true` remux, else 500) instead of `200` with a stub body;
//!   5. stream the rest. If `ffmpeg` exits non-zero mid-stream, the body ends
//!      with an error so the client sees a truncated transfer, not a clean end.
//!
//! The child and the permit live inside the body stream: when the body is
//! dropped (completion or client disconnect) `kill_on_drop` tears `ffmpeg`
//! down and the slot is released. Input-side `-ss` makes `?t=` seeks cheap.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, Method};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::ReaderStream;

use crate::error::AppError;
use crate::handlers::stream::STREAM_CHUNK;
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

/// How long a request waits for a free transcode slot before getting `503`.
/// Long enough to cover a player switching quality (the old stream's slot is
/// released a moment after it disconnects), short enough not to hang clients.
const SLOT_WAIT: Duration = Duration::from_secs(5);

/// After the first output chunk, how long to keep watching for an early
/// `ffmpeg` failure before committing to a `200`. Adds at most this much to
/// startup latency.
const STARTUP_GRACE: Duration = Duration::from_millis(300);

/// Stop the grace window early once this much output is buffered, so a fast
/// remux can't pile up memory before the response starts.
const STARTUP_BUFFER: usize = 1024 * 1024;

/// Lines of `ffmpeg` stderr kept for the log when a transcode fails.
const STDERR_TAIL: usize = 8;

/// `GET`/`HEAD /transcode/{id}` — transcode/remux and stream the result.
pub async fn transcode_media(
    State(state): State<AppState>,
    method: Method,
    Path(id): Path<String>,
    Query(params): Query<TranscodeParams>,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;

    // Validate before anything else, so a bad request never waits for a slot.
    let format = params
        .format
        .as_deref()
        .unwrap_or("mp4")
        .to_ascii_lowercase();
    let (content_type, args) = build_ffmpeg_args(&format, &item.abs_path, &params)?;

    if method == Method::HEAD {
        // Crawlers and link previews send HEAD; don't burn a slot and an
        // encoder on them. Use a body with *no* size hint: `Body::empty()`
        // would advertise `content-length: 0`, but a HEAD must carry the same
        // headers as the GET, which is streamed with no length.
        let unsized_body = futures::stream::empty::<std::io::Result<Bytes>>();
        return Ok((
            response_headers(content_type),
            Body::from_stream(unsized_body),
        )
            .into_response());
    }

    // Acquire a transcode slot. We hold this permit for the whole stream so the
    // number of live `ffmpeg` processes never exceeds `max_transcodes`.
    let permit = match tokio::time::timeout(
        SLOT_WAIT,
        state.transcode_slots.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err(AppError::Internal("transcode semaphore closed".into())),
        Err(_) => {
            return Err(AppError::Busy {
                retry_after_secs: SLOT_WAIT.as_secs(),
            })
        }
    };

    let mut cmd = Command::new(&state.config.ffmpeg);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    // Forward ffmpeg's diagnostics (`-loglevel error`, so only real problems)
    // to our logs, and keep the last few lines for the failure message.
    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
    let stderr_task = child.stderr.take().map(|stderr| {
        let source = item.path.clone();
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(item = %source, "ffmpeg: {line}");
                let mut tail = tail.lock().unwrap();
                if tail.len() == STDERR_TAIL {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        })
    });

    tracing::debug!(item = %item.path, %format, "transcode started");

    // Hold the status line until ffmpeg has produced something *and* has
    // survived a short grace window. Some failures (e.g. `copy=true` into a
    // container that can't hold the codecs) write a few hundred bytes of
    // container header before exiting non-zero, so the first chunk alone
    // proves nothing. If the client disconnects meanwhile, this future is
    // dropped and so is the child (`kill_on_drop`). A slow source (or a far
    // `-ss` seek) simply waits for its first chunk.
    let mut out = ReaderStream::with_capacity(stdout, STREAM_CHUNK);
    let mut prefix: VecDeque<Bytes> = VecDeque::new();
    let mut buffered = 0usize;
    let mut eof = false;
    match out.next().await {
        Some(Ok(chunk)) => {
            buffered += chunk.len();
            prefix.push_back(chunk);
        }
        Some(Err(e)) => return Err(AppError::Internal(format!("reading ffmpeg output: {e}"))),
        None => eof = true,
    }
    if !eof {
        let grace = tokio::time::sleep(STARTUP_GRACE);
        tokio::pin!(grace);
        // Keep draining stdout during the window so ffmpeg never blocks on a
        // full pipe; stop early once STARTUP_BUFFER is held (fast remuxes).
        while buffered < STARTUP_BUFFER {
            tokio::select! {
                _ = &mut grace => break,
                next = out.next() => match next {
                    Some(Ok(chunk)) => {
                        buffered += chunk.len();
                        prefix.push_back(chunk);
                    }
                    Some(Err(e)) => {
                        return Err(AppError::Internal(format!("reading ffmpeg output: {e}")))
                    }
                    None => {
                        eof = true;
                        break;
                    }
                },
            }
        }
    }
    if eof {
        // ffmpeg already finished. A clean exit (tiny output) is served as
        // is; a failure becomes a real error status instead of a 200.
        let status = child.wait().await;
        if !matches!(&status, Ok(s) if s.success()) {
            if let Some(task) = stderr_task {
                // stderr closes when ffmpeg exits; bounded just in case.
                let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
            }
            let tail: Vec<String> = stderr_tail.lock().unwrap().iter().cloned().collect();
            let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            tracing::error!(item = %item.path, %format, %status, stderr = ?tail, "transcode failed at startup");
            // ffmpeg's text includes absolute paths, so it stays in the log.
            return Err(if params.copy.unwrap_or(false) {
                AppError::Unprocessable(format!(
                    "remux to {format} failed: the source codecs probably don't fit the \
                     container; retry without copy=true"
                ))
            } else {
                AppError::Internal(format!("ffmpeg failed ({status}): {}", tail.join(" | ")))
            });
        }
    }

    let body = TranscodeBody {
        prefix,
        out,
        child,
        _permit: permit,
        source: item.path.clone(),
    };
    Ok((
        response_headers(content_type),
        Body::from_stream(body.into_stream()),
    )
        .into_response())
}

fn response_headers(content_type: &'static str) -> [(header::HeaderName, &'static str); 3] {
    [
        (header::CONTENT_TYPE, content_type),
        // Transcoded output has no known length and is not range-seekable;
        // use `?t=` to seek instead.
        (header::ACCEPT_RANGES, "none"),
        (header::CACHE_CONTROL, "no-store"),
    ]
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

    // `file:` forces ffmpeg's local-file protocol, so a name that looks like a
    // URL or protocol spec (`http:…`, `concat:…`) is never interpreted as one.
    args.push("-i".into());
    args.push(format!("file:{}", input.to_string_lossy()));

    // Clamp like crf/height: an absurd value would only make ffmpeg fail.
    let abitrate = params.abitrate.unwrap_or(128).clamp(32, 512);

    let copy = params.copy.unwrap_or(false);
    // Target height: clamped, and rounded down to even because libx264
    // (4:2:0) rejects odd frame sizes. `min(ih, …)` never upscales a smaller
    // source, and `trunc(…/2)*2` keeps that result even too (`-2` already
    // does the same for the width). The comma inside `min()` is escaped for
    // ffmpeg's filter-graph parser; no shell is involved.
    let scale_filter = params.height.map(|h| {
        let h = h.clamp(144, 4320) & !1;
        format!("scale=-2:trunc(min(ih\\,{h})/2)*2")
    });

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
                    format!("{abitrate}k"),
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
                    format!("{abitrate}k"),
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

/// Response body state: `ffmpeg`'s stdout plus the child process and the
/// semaphore permit, kept alive for exactly as long as the body is read. When
/// the body is dropped (completion or client disconnect) the child is dropped
/// too — and with `kill_on_drop` set, killed — and the permit is released.
struct TranscodeBody {
    /// Output read before the headers were sent.
    prefix: VecDeque<Bytes>,
    out: ReaderStream<ChildStdout>,
    child: Child,
    _permit: OwnedSemaphorePermit,
    source: String,
}

impl TranscodeBody {
    fn into_stream(self) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
        futures::stream::unfold(Some(self), |state| async move {
            let mut st = state?;
            if let Some(chunk) = st.prefix.pop_front() {
                return Some((Ok(chunk), Some(st)));
            }
            match st.out.next().await {
                Some(Ok(chunk)) => Some((Ok(chunk), Some(st))),
                Some(Err(e)) => Some((Err(e), None)),
                // stdout closed: end cleanly only if ffmpeg succeeded, so a
                // mid-stream failure reaches the client as a broken transfer.
                None => match st.child.wait().await {
                    Ok(status) if status.success() => None,
                    Ok(status) => {
                        tracing::error!(item = %st.source, %status, "transcode failed mid-stream");
                        Some((
                            Err(std::io::Error::other(format!(
                                "ffmpeg exited with {status}"
                            ))),
                            None,
                        ))
                    }
                    Err(e) => Some((Err(e), None)),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(query: &str) -> TranscodeParams {
        serde_json::from_str(query).unwrap()
    }

    #[test]
    fn input_is_forced_to_the_file_protocol() {
        let (_, args) = build_ffmpeg_args(
            "mp4",
            std::path::Path::new("/m/concat:x.mp4"),
            &params("{}"),
        )
        .unwrap();
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "file:/m/concat:x.mp4");
    }

    #[test]
    fn audio_bitrate_is_clamped() {
        let (_, args) = build_ffmpeg_args(
            "webm",
            std::path::Path::new("/m/a.mkv"),
            &params(r#"{"abitrate": 100000}"#),
        )
        .unwrap();
        assert!(args.contains(&"512k".to_string()), "{args:?}");
    }

    #[test]
    fn scale_height_is_even_and_never_upscales() {
        let (_, args) = build_ffmpeg_args(
            "mp4",
            std::path::Path::new("/m/a.mkv"),
            &params(r#"{"height": 145}"#),
        )
        .unwrap();
        let vf = &args[args.iter().position(|a| a == "-vf").unwrap() + 1];
        assert_eq!(vf, r"scale=-2:trunc(min(ih\,144)/2)*2");
    }

    #[test]
    fn unknown_format_is_a_bad_request() {
        let r = build_ffmpeg_args("avi", std::path::Path::new("/m/a.mkv"), &params("{}"));
        assert!(matches!(r, Err(AppError::BadRequest(_))));
    }
}
