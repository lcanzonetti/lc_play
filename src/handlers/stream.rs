//! Direct byte streaming with HTTP Range support.
//!
//! This is the zero-overhead path: the file is streamed straight from disk in
//! bounded chunks. `Range` requests get a `206 Partial Content` response, which
//! is what enables seeking in players and makes responses CDN-cacheable.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::error::AppError;
use crate::state::AppState;

/// `GET /stream/{id}` — stream the original file, honouring `Range`.
pub async fn stream_media(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;

    let file = tokio::fs::File::open(&item.abs_path).await?;
    let total = file.metadata().await?.len();

    let content_type = item.mime.clone();

    match parse_range(headers.get(header::RANGE), total)? {
        // Whole file: 200 OK.
        None => {
            let stream = ReaderStream::new(file);
            let body = Body::from_stream(stream);
            Ok((
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
            let stream = ReaderStream::new(file.take(len));
            let body = Body::from_stream(stream);

            Ok((
                StatusCode::PARTIAL_CONTENT,
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

/// Parse a single-range `Range: bytes=start-end` header.
///
/// Returns:
///   * `Ok(None)` when there is no range header (serve the whole file),
///   * `Ok(Some((start, end)))` for a satisfiable inclusive byte range,
///   * `Err(RangeNotSatisfiable)` / `Err(BadRequest)` otherwise.
///
/// Multi-range requests are intentionally not supported; we serve the whole
/// file instead, which is valid per RFC 7233.
fn parse_range(
    value: Option<&axum::http::HeaderValue>,
    total: u64,
) -> Result<Option<(u64, u64)>, AppError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| AppError::BadRequest("invalid Range header".into()))?;

    let Some(spec) = value.strip_prefix("bytes=") else {
        return Ok(None); // unsupported unit; ignore and serve whole file
    };

    // Only the first range of a possible list is honoured.
    let spec = spec.split(',').next().unwrap_or("").trim();
    let (start_s, end_s) = spec
        .split_once('-')
        .ok_or_else(|| AppError::BadRequest("malformed Range".into()))?;

    if total == 0 {
        return Err(AppError::RangeNotSatisfiable { size: total });
    }

    let (start, end) = match (start_s.trim(), end_s.trim()) {
        // "bytes=-N" -> last N bytes
        ("", n) => {
            let n: u64 = n
                .parse()
                .map_err(|_| AppError::BadRequest("malformed Range".into()))?;
            let n = n.min(total);
            (total - n, total - 1)
        }
        // "bytes=N-" -> from N to end
        (s, "") => {
            let s: u64 = s
                .parse()
                .map_err(|_| AppError::BadRequest("malformed Range".into()))?;
            (s, total - 1)
        }
        // "bytes=S-E"
        (s, e) => {
            let s: u64 = s
                .parse()
                .map_err(|_| AppError::BadRequest("malformed Range".into()))?;
            let e: u64 = e
                .parse()
                .map_err(|_| AppError::BadRequest("malformed Range".into()))?;
            (s, e.min(total - 1))
        }
    };

    if start > end || start >= total {
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

    #[test]
    fn no_header_serves_whole_file() {
        assert_eq!(parse_range(None, 1000).unwrap(), None);
    }

    #[test]
    fn open_ended_range() {
        assert_eq!(parse_range(Some(&hv("bytes=500-")), 1000).unwrap(), Some((500, 999)));
    }

    #[test]
    fn closed_range_is_inclusive() {
        assert_eq!(parse_range(Some(&hv("bytes=0-499")), 1000).unwrap(), Some((0, 499)));
    }

    #[test]
    fn end_is_clamped_to_size() {
        assert_eq!(parse_range(Some(&hv("bytes=0-99999")), 1000).unwrap(), Some((0, 999)));
    }

    #[test]
    fn suffix_range_returns_last_n_bytes() {
        assert_eq!(parse_range(Some(&hv("bytes=-200")), 1000).unwrap(), Some((800, 999)));
    }

    #[test]
    fn suffix_larger_than_file_is_whole_file() {
        assert_eq!(parse_range(Some(&hv("bytes=-5000")), 1000).unwrap(), Some((0, 999)));
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
    fn only_first_range_of_a_set_is_used() {
        assert_eq!(
            parse_range(Some(&hv("bytes=0-99,200-299")), 1000).unwrap(),
            Some((0, 99))
        );
    }
}
