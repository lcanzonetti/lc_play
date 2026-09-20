//! JSON browse API: list the library and fetch item metadata.

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::error::AppError;
use crate::library::{MediaItem, MediaKind};
use crate::state::AppState;

#[derive(Serialize)]
pub struct LibraryResponse {
    pub count: usize,
    pub items: Vec<ItemDto>,
}

/// Public shape of a media item (drops internal fields like the absolute path).
#[derive(Serialize)]
pub struct ItemDto {
    pub id: String,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub mime: String,
    pub kind: MediaKind,
    /// Direct (no-transcode) stream URL.
    pub stream_url: String,
    /// On-the-fly transcode URL (defaults to fragmented MP4).
    pub transcode_url: String,
}

impl From<MediaItem> for ItemDto {
    fn from(m: MediaItem) -> Self {
        ItemDto {
            stream_url: format!("/stream/{}", m.id),
            transcode_url: format!("/transcode/{}", m.id),
            id: m.id,
            name: m.name,
            path: m.path,
            size: m.size,
            mime: m.mime,
            kind: m.kind,
        }
    }
}

/// `GET /api/library` — list every indexed item.
pub async fn list_library(State(state): State<AppState>) -> Json<LibraryResponse> {
    let items: Vec<ItemDto> = state
        .library
        .list()
        .into_iter()
        .map(ItemDto::from)
        .collect();
    Json(LibraryResponse {
        count: items.len(),
        items,
    })
}

/// `GET /api/items/{id}` — metadata for a single item.
pub async fn get_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ItemDto>, AppError> {
    state
        .library
        .get(&id)
        .map(|m| Json(ItemDto::from(m)))
        .ok_or(AppError::NotFound)
}
