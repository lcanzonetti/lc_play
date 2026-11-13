//! Direct byte streaming with HTTP Range support.
//!
//! This is the zero-overhead path: the file is streamed straight from disk in
//! bounded chunks. `Range` requests get a `206 Partial Content` response, which
//! is what enables seeking in players.
//!
//! Caching: item IDs are path-based, so a file replaced under the same name
//! keeps its URL. Every response therefore carries validators built from the
//! file's size and modification time (`ETag`, `Last-Modified`) plus an
//! explicit `Cache-Control`, and the handler honours `If-None-Match` /
//! `If-Modified-Since` (→ `304`) and `If-Range` (a resumed download of a file
//! that changed gets the whole new file, never old and new bytes stitched).

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::error::AppError;
use crate::state::AppState;

/// Read size for streamed bodies. `ReaderStream`'s 4 KiB default means one
/// syscall and one body frame per 4 KiB — far too fine-grained for media.
pub(crate) const STREAM_CHUNK: usize = 64 * 1024;

/// `GET /stream/{id}` — stream the original file, honouring `Range`.
pub async fn stream_media(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;

    let file = tokio::fs::File::open(&item.abs_path).await?;
    let meta = file.metadata().await?;
    let total = meta.len();
    let validators = Validators::new(total, meta.modified().ok(), state.config.cache_max_age);

    if validators.not_modified(&headers) {
        return Ok((StatusCode::NOT_MODIFIED, validators.headers()).into_response());
    }

    let content_type = item.mime.clone();

    // A Range is only honoured if the client's copy is still current.
    let range = if validators.if_range_holds(&headers) {
        headers.get(header::RANGE)
    } else {
        None
    };

    match parse_range(range, total)? {
        // Whole file: 200 OK.
        None => {
            let stream = ReaderStream::with_capacity(file, STREAM_CHUNK);
            let body = Body::from_stream(stream);
            Ok((
                validators.headers(),
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_LENGTH, total.to_string()),
                ],
                body,
            )
                .into_response())
        }
        // Partial content: 206.
        Some((start, end)) => {
            let len = end - start + 1;
            let mut file = file;
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let stream = ReaderStream::with_capacity(file.take(len), STREAM_CHUNK);
            let body = Body::from_stream(stream);

            Ok((
                StatusCode::PARTIAL_CONTENT,
                validators.headers(),
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_LENGTH, len.to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    ),
                ],
                body,
            )
                .into_response())
        }
    }
}

/// Cache validators for one file version.
struct Validators {
    /// Strong `ETag`: `"<size hex>-<mtime ns hex>"`. `None` if the filesystem
    /// reports no modification time (then only `Cache-Control` is sent).
    etag: Option<String>,
    /// Modification time, truncated to whole seconds as HTTP dates are.
    last_modified: Option<SystemTime>,
    /// Whether `last_modified` is a *strong* validator (RFC 9110 §8.8.2.2):
    /// the file was last changed at least a second ago, so two versions can't
    /// share the same one-second timestamp. Only then may a date `If-Range`
    /// match.
    date_is_strong: bool,
    cache_control: String,
}

impl Validators {
    fn new(size: u64, modified: Option<SystemTime>, max_age: u64) -> Self {
        let since_epoch = modified.and_then(|m| m.duration_since(UNIX_EPOCH).ok());
        Validators {
            etag: since_epoch.map(|d| format!("\"{size:x}-{:x}\"", d.as_nanos())),
            last_modified: since_epoch
                .map(|d| UNIX_EPOCH + std::time::Duration::from_secs(d.as_secs())),
            date_is_strong: modified
                .is_some_and(|m| m + std::time::Duration::from_secs(1) <= SystemTime::now()),
            cache_control: format!("public, max-age={max_age}, must-revalidate"),
        }
    }

    fn headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CACHE_CONTROL, self.cache_control.parse().unwrap());
        if let Some(etag) = &self.etag {
            h.insert(header::ETAG, etag.parse().unwrap());
        }
        if let Some(lm) = self.last_modified {
            h.insert(
                header::LAST_MODIFIED,
                httpdate::fmt_http_date(lm).parse().unwrap(),
            );
        }
        h
    }

    /// `true` when the client's cached copy is current (→ `304`).
    /// `If-None-Match` takes precedence over `If-Modified-Since` (RFC 9110 §13.2.2).
    fn not_modified(&self, req: &HeaderMap) -> bool {
        if let Some(inm) = req.get(header::IF_NONE_MATCH) {
            let Some(etag) = &self.etag else { return false };
            // Weak comparison: ignore any W/ prefix on either side.
            let ours = etag.trim_start_matches("W/");
            return inm.to_str().is_ok_and(|v| {
                v.split(',')
                    .map(str::trim)
                    .any(|t| t == "*" || t.trim_start_matches("W/") == ours)
            });
        }
        match (req.get(header::IF_MODIFIED_SINCE), self.last_modified) {
            (Some(ims), Some(lm)) => ims
                .to_str()
                .ok()
                .and_then(|v| httpdate::parse_http_date(v).ok())
                .is_some_and(|since| lm <= since),
            _ => false,
        }
    }

    /// `true` if a `Range` may be honoured: there is no `If-Range`, or it
    /// still matches this version (strong ETag, or exact `Last-Modified` when
    /// that date is a strong validator).
    fn if_range_holds(&self, req: &HeaderMap) -> bool {
        let Some(v) = req.get(header::IF_RANGE) else {
            return true;
        };
        let Ok(v) = v.to_str() else { return false };
        let v = v.trim();
        if v.starts_with('"') || v.starts_with("W/") {
            // Strong comparison: a weak tag never matches.
            return self.etag.as_deref() == Some(v);
        }
        match (httpdate::parse_http_date(v), self.last_modified) {
            (Ok(date), Some(lm)) => self.date_is_strong && date == lm,
            _ => false,
        }
    }
}

/// Parse a single-range `Range: bytes=start-end` header.
///
/// Returns:
///   * `Ok(None)` → serve the whole file with `200`. That covers no header, an
///     unknown unit, a syntactically invalid range, and multi-range requests
///     (`bytes=0-99,200-299`; no `multipart/byteranges` support). RFC 9110
///     §14.2 lets a server ignore a Range it can't or won't honour.
///   * `Ok(Some((start, end)))` for a satisfiable inclusive byte range,
///   * `Err(RangeNotSatisfiable)` for a valid range outside the file (`416`).
fn parse_range(
    value: Option<&axum::http::HeaderValue>,
    total: u64,
) -> Result<Option<(u64, u64)>, AppError> {
    let Some(value) = value else {
        return Ok(None);
    };
    // Anything we can't parse is ignored (whole file), never a 400.
    let Ok(value) = value.to_str() else {
        return Ok(None);
    };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Ok(None); // unsupported unit
    };
    if spec.contains(',') {
        return Ok(None); // multi-range: not supported, serve the whole file
    }
    let Some((start_s, end_s)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let num = |s: &str| s.parse::<u64>().ok();

    let (start, end) = match (start_s.trim(), end_s.trim()) {
        ("", "") => return Ok(None),
        // "bytes=-N" -> last N bytes
        ("", n) => {
            let Some(n) = num(n) else { return Ok(None) };
            if n == 0 || total == 0 {
                // A zero-length suffix, or any suffix of an empty file.
                return Err(AppError::RangeNotSatisfiable { size: total });
            }
            (total - n.min(total), total - 1)
        }
        // "bytes=N-" -> from N to end
        (s, "") => {
            let Some(s) = num(s) else { return Ok(None) };
            (s, total.saturating_sub(1))
        }
        // "bytes=S-E"
        (s, e) => {
            let (Some(s), Some(e)) = (num(s), num(e)) else {
                return Ok(None);
            };
            if e < s {
                return Ok(None); // invalid per RFC 9110 §14.1.1: ignore
            }
            (s, e.min(total.saturating_sub(1)))
        }
    };

    if total == 0 || start > end || start >= total {
        return Err(AppError::RangeNotSatisfiable { size: total });
    }

    Ok(Some((start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    fn req(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), hv(v));
        }
        h
    }

    fn validators() -> Validators {
        let mtime = UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        Validators::new(1000, Some(mtime), 0)
    }

    #[test]
    fn matching_etag_or_date_is_not_modified() {
        let v = validators();
        let etag = v.etag.clone().unwrap();
        assert!(v.not_modified(&req(&[(header::IF_NONE_MATCH, &etag)])));
        assert!(v.not_modified(&req(&[(header::IF_NONE_MATCH, &format!("W/{etag}"))])));
        assert!(!v.not_modified(&req(&[(header::IF_NONE_MATCH, "\"other\"")])));
        let lm = httpdate::fmt_http_date(v.last_modified.unwrap());
        assert!(v.not_modified(&req(&[(header::IF_MODIFIED_SINCE, &lm)])));
        // If-None-Match wins over a matching date.
        assert!(!v.not_modified(&req(&[
            (header::IF_NONE_MATCH, "\"other\""),
            (header::IF_MODIFIED_SINCE, &lm)
        ])));
    }

    #[test]
    fn if_range_only_holds_for_the_same_version() {
        let v = validators();
        let etag = v.etag.clone().unwrap();
        assert!(v.if_range_holds(&HeaderMap::new()));
        assert!(v.if_range_holds(&req(&[(header::IF_RANGE, &etag)])));
        assert!(!v.if_range_holds(&req(&[(header::IF_RANGE, "\"stale\"")])));
        assert!(!v.if_range_holds(&req(&[(header::IF_RANGE, &format!("W/{etag}"))])));
        let lm = httpdate::fmt_http_date(v.last_modified.unwrap());
        assert!(v.if_range_holds(&req(&[(header::IF_RANGE, &lm)])));
    }

    #[test]
    fn date_if_range_needs_a_strong_date() {
        // Modified "now": another change could land within the same second,
        // so a date can't prove the client's bytes are from this version.
        let v = Validators::new(1000, Some(SystemTime::now()), 0);
        let lm = httpdate::fmt_http_date(v.last_modified.unwrap());
        assert!(!v.if_range_holds(&req(&[(header::IF_RANGE, &lm)])));
        // The ETag (which has nanoseconds) still works.
        let etag = v.etag.clone().unwrap();
        assert!(v.if_range_holds(&req(&[(header::IF_RANGE, &etag)])));
    }

    #[test]
    fn etag_changes_with_mtime() {
        let a = Validators::new(1000, Some(UNIX_EPOCH + std::time::Duration::new(5, 1)), 0);
        let b = Validators::new(1000, Some(UNIX_EPOCH + std::time::Duration::new(5, 2)), 0);
        assert_ne!(a.etag, b.etag);
    }

    #[test]
    fn no_header_serves_whole_file() {
        assert_eq!(parse_range(None, 1000).unwrap(), None);
    }

    #[test]
    fn open_ended_range() {
        assert_eq!(
            parse_range(Some(&hv("bytes=500-")), 1000).unwrap(),
            Some((500, 999))
        );
    }

    #[test]
    fn closed_range_is_inclusive() {
        assert_eq!(
            parse_range(Some(&hv("bytes=0-499")), 1000).unwrap(),
            Some((0, 499))
        );
    }

    #[test]
    fn end_is_clamped_to_size() {
        assert_eq!(
            parse_range(Some(&hv("bytes=0-99999")), 1000).unwrap(),
            Some((0, 999))
        );
    }

    #[test]
    fn suffix_range_returns_last_n_bytes() {
        assert_eq!(
            parse_range(Some(&hv("bytes=-200")), 1000).unwrap(),
            Some((800, 999))
        );
    }

    #[test]
    fn suffix_larger_than_file_is_whole_file() {
        assert_eq!(
            parse_range(Some(&hv("bytes=-5000")), 1000).unwrap(),
            Some((0, 999))
        );
    }

    #[test]
    fn start_past_end_is_not_satisfiable() {
        assert!(matches!(
            parse_range(Some(&hv("bytes=1000-1100")), 1000),
            Err(AppError::RangeNotSatisfiable { .. })
        ));
    }

    #[test]
    fn unknown_unit_serves_whole_file() {
        assert_eq!(parse_range(Some(&hv("items=0-1")), 1000).unwrap(), None);
    }

    #[test]
    fn multi_range_serves_whole_file() {
        assert_eq!(
            parse_range(Some(&hv("bytes=0-99,200-299")), 1000).unwrap(),
            None
        );
    }

    #[test]
    fn malformed_ranges_are_ignored_not_rejected() {
        for bad in ["bytes=abc", "bytes=5", "bytes=-", "bytes=9-3", "bytes=x-9"] {
            assert_eq!(parse_range(Some(&hv(bad)), 1000).unwrap(), None, "{bad}");
        }
    }

    #[test]
    fn any_range_of_an_empty_file_is_not_satisfiable() {
        for r in ["bytes=0-", "bytes=-5", "bytes=0-0"] {
            assert!(matches!(
                parse_range(Some(&hv(r)), 0),
                Err(AppError::RangeNotSatisfiable { .. })
            ));
        }
    }
}
