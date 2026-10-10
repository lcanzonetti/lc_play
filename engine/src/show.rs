//! The **show model**: the declarative description of a playback setup, loaded
//! from a JSON file.
//!
//! Coordinate system: the *canvas* (a.k.a. stage) is a 2D pixel space with the
//! origin at the top-left, `x` to the right and `y` downward. Every output and
//! every layer is positioned in this one shared space — which is exactly why a
//! layer can span several outputs: each output simply renders whatever slice of
//! the canvas it covers.

use glam::{Mat4, Vec2, Vec3};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Show {
    pub canvas: Canvas,
    pub outputs: Vec<Output>,
    #[serde(default)]
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub playback: Playback,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Canvas {
    pub width: f32,
    pub height: f32,
    /// Clear color behind all layers, linear RGBA 0..1.
    #[serde(default = "black")]
    pub background: [f32; 4],
}

/// A physical display, defined by the slice of canvas it shows plus its own
/// geometry correction (warp + edge-blend + orientation).
#[derive(Debug, Clone, Deserialize)]
pub struct Output {
    pub id: String,
    /// The region of the canvas this output renders.
    pub canvas_rect: Rect,
    /// Window placement on the host machine.
    #[serde(default)]
    pub window: WindowPlacement,
    /// Physical display rotation applied to the final image: 0/90/180/270.
    #[serde(default)]
    pub rotation_degrees: u16,
    #[serde(default)]
    pub warp: Warp,
    #[serde(default)]
    pub blend: Blend,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WindowPlacement {
    /// Make the window borderless-fullscreen on its monitor.
    #[serde(default)]
    pub fullscreen: bool,
    /// Index into the OS monitor list (for multi-display machines).
    #[serde(default)]
    pub monitor: Option<usize>,
    /// Top-left position in desktop pixels (windowed mode).
    #[serde(default)]
    pub position: Option<[i32; 2]>,
    /// Window size in pixels. Defaults to the canvas_rect size when omitted.
    #[serde(default)]
    pub size: Option<[u32; 2]>,
}

/// Corner warp. Corners are normalized output coordinates (0..1, top-left
/// origin) in the order TL, TR, BR, BL. `None` means an identity (no warp).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Warp {
    #[serde(default)]
    pub corners: Option<[[f32; 2]; 4]>,
}

/// Soft-edge blend widths per edge, as a fraction of the output (0..1), plus a
/// blend gamma. Used to overlap neighboring projectors seamlessly.
#[derive(Debug, Clone, Deserialize)]
pub struct Blend {
    #[serde(default)]
    pub left: f32,
    #[serde(default)]
    pub right: f32,
    #[serde(default)]
    pub top: f32,
    #[serde(default)]
    pub bottom: f32,
    #[serde(default = "one")]
    pub gamma: f32,
}

impl Default for Blend {
    fn default() -> Self {
        Blend {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
            gamma: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Layer {
    pub id: String,
    pub source: Source,
    pub transform: Transform,
    #[serde(default)]
    pub timing: Timing,
}

/// Content source for a layer. Tagged by `"type"` in JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Source {
    /// Still image loaded from disk.
    Image { path: String },
    /// Solid color (linear RGBA 0..1) — handy for testing a layout with no assets.
    Color { rgba: [f32; 4] },
    /// Pre-recorded video (Phase 2; requires the `video` feature).
    Video { path: String },
    /// Live input such as a capture card or NDI stream (Phase 3).
    Live { kind: String },
}

/// Placement of a layer on the canvas.
#[derive(Debug, Clone, Deserialize)]
pub struct Transform {
    /// Canvas-space point (pixels) the layer's anchor is pinned to.
    pub position: [f32; 2],
    /// Layer size in canvas pixels before `scale`.
    pub size: [f32; 2],
    /// Normalized anchor within the layer (0..1). Default is the center.
    #[serde(default = "center")]
    pub anchor: [f32; 2],
    /// Uniform/non-uniform multiplier applied to `size`.
    #[serde(default = "one_vec")]
    pub scale: [f32; 2],
    /// Rotation in degrees about the anchor (positive = clockwise on screen).
    #[serde(default)]
    pub rotation: f32,
    /// 0..1 opacity.
    #[serde(default = "one")]
    pub opacity: f32,
    /// Draw order; higher is on top.
    #[serde(default)]
    pub z: f32,
}

impl Transform {
    /// Model matrix mapping the unit quad `[0,1]²` into canvas space.
    pub fn model_matrix(&self) -> Mat4 {
        let pos = Vec2::from(self.position);
        let size = Vec2::from(self.size) * Vec2::from(self.scale);
        let anchor = Vec2::from(self.anchor);

        // world = T(position) · R(rotation) · S(size) · T(-anchor)
        let t_pos = Mat4::from_translation(Vec3::new(pos.x, pos.y, 0.0));
        let rot = Mat4::from_rotation_z(self.rotation.to_radians());
        let scale = Mat4::from_scale(Vec3::new(size.x, size.y, 1.0));
        let t_anchor = Mat4::from_translation(Vec3::new(-anchor.x, -anchor.y, 0.0));
        t_pos * rot * scale * t_anchor
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Timing {
    /// Show-clock time (seconds) at which the layer appears.
    #[serde(default)]
    pub start: f64,
    /// Show-clock time at which it disappears. `None` = until the end.
    #[serde(default)]
    pub end: Option<f64>,
}

impl Timing {
    pub fn visible_at(&self, t: f64) -> bool {
        t >= self.start && self.end.map(|e| t < e).unwrap_or(true)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playback {
    #[serde(default = "true_")]
    pub autoplay: bool,
    #[serde(default, rename = "loop")]
    pub looping: bool,
    /// Total show duration in seconds, used for looping. `None` = run forever.
    #[serde(default)]
    pub duration: Option<f64>,
}

impl Default for Playback {
    fn default() -> Self {
        Playback {
            autoplay: true,
            looping: false,
            duration: None,
        }
    }
}

impl Rect {
    /// Orthographic view-projection mapping this canvas rect to clip space,
    /// honoring the canvas y-down convention.
    pub fn view_projection(&self) -> Mat4 {
        Mat4::orthographic_rh(
            self.x,          // left
            self.x + self.w, // right
            self.y + self.h, // bottom (larger canvas-y)
            self.y,          // top    (smaller canvas-y)
            -1.0,
            1.0,
        )
    }
}

// ---- serde defaults ----
fn black() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}
fn one() -> f32 {
    1.0
}
fn one_vec() -> [f32; 2] {
    [1.0, 1.0]
}
fn center() -> [f32; 2] {
    [0.5, 0.5]
}
fn true_() -> bool {
    true
}

impl Show {
    /// Parse a show from JSON text, with light validation.
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        let show: Show = serde_json::from_str(text)?;
        if show.outputs.is_empty() {
            anyhow::bail!("show has no outputs");
        }
        for o in &show.outputs {
            if !matches!(o.rotation_degrees, 0 | 90 | 180 | 270) {
                anyhow::bail!(
                    "output '{}' has invalid rotation_degrees {} (must be 0/90/180/270)",
                    o.id,
                    o.rotation_degrees
                );
            }
        }
        Ok(show)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_center_keeps_position_fixed_under_rotation() {
        let t = Transform {
            position: [100.0, 100.0],
            size: [50.0, 50.0],
            anchor: [0.5, 0.5],
            scale: [1.0, 1.0],
            rotation: 90.0,
            opacity: 1.0,
            z: 0.0,
        };
        let m = t.model_matrix();
        // The anchor (local 0.5,0.5) must map to the position regardless of rotation.
        let p = m * glam::Vec4::new(0.5, 0.5, 0.0, 1.0);
        assert!((p.x - 100.0).abs() < 1e-3, "x={}", p.x);
        assert!((p.y - 100.0).abs() < 1e-3, "y={}", p.y);
    }

    #[test]
    fn timing_visibility_window() {
        let t = Timing {
            start: 2.0,
            end: Some(5.0),
        };
        assert!(!t.visible_at(1.9));
        assert!(t.visible_at(2.0));
        assert!(t.visible_at(4.9));
        assert!(!t.visible_at(5.0));
    }

    #[test]
    fn parses_minimal_show() {
        let json = r#"{
            "canvas": { "width": 1920, "height": 1080 },
            "outputs": [ { "id": "A", "canvas_rect": { "x": 0, "y": 0, "w": 960, "h": 1080 } } ],
            "layers": [ { "id": "bg", "source": { "type": "color", "rgba": [1,0,0,1] },
                          "transform": { "position": [960,540], "size": [1920,1080] } } ]
        }"#;
        let show = Show::from_json(json).unwrap();
        assert_eq!(show.outputs.len(), 1);
        assert_eq!(show.layers.len(), 1);
        assert!(show.playback.autoplay);
    }
}
