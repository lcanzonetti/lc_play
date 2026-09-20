//! Central error type that renders cleanly as an HTTP response.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    /// The requested media item does not exist in the index.
    NotFound,
    /// The client sent something invalid (bad id, bad range, bad query).
    BadRequest(String),
    /// Requested range cannot be satisfied.
    RangeNotSatisfiable { size: u64 },
    /// An unexpected server-side failure.
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "media item not found".to_string()),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::RangeNotSatisfiable { size } => {
                // Advertise the valid range so clients can retry correctly.
                let mut resp = (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    Json(json!({ "error": "requested range not satisfiable" })),
                )
                    .into_response();
                resp.headers_mut().insert(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes */{size}").parse().unwrap(),
                );
                return resp;
            }
            AppError::Internal(m) => {
                tracing::error!("internal error: {m}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };

        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => AppError::NotFound,
            _ => AppError::Internal(e.to_string()),
        }
    }
}
