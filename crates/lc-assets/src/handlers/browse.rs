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
    /// Small JPEG preview.
    pub thumb_url: String,
    /// What to play: the optimised version when there is one, else the
    /// original (`stream_url`, `size`, `path`).
    pub play_url: String,
    pub play_size: u64,
    /// Relative to the media folder.
    pub play_path: String,
    /// Videos: where the optimiser is with this file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optimized: Option<crate::optimize::Status>,
    /// Videos and sound files: their sound, as Runners play it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<crate::audio::Status>,
    /// Pixel size and duration, when `ffprobe` could read the file.
    #[serde(flatten)]
    pub probe: Option<crate::handlers::manage::Probe>,
}

impl From<MediaItem> for ItemDto {
    fn from(m: MediaItem) -> Self {
        ItemDto {
            stream_url: format!("/stream/{}", m.id),
            transcode_url: format!("/transcode/{}", m.id),
            thumb_url: format!("/thumb/{}", m.id),
            probe: None,
            play_url: format!("/stream/{}", m.id),
            play_size: m.size,
            play_path: m.path.clone(),
            optimized: None,
            audio: None,
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
    // Probe results are cached, so only new files cost an `ffprobe` run.
    let items = futures::future::join_all(
        state
            .library
            .list()
            .into_iter()
            .map(|item| crate::handlers::manage::describe(&state, item)),
    )
    .await;
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
    let item = state.library.get(&id).ok_or(AppError::NotFound)?;
    Ok(Json(crate::handlers::manage::describe(&state, item).await))
}
