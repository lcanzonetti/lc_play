//! The **show model**: the declarative description of a playback setup, loaded
//! from a JSON file.
//!
//! Coordinate system: the *canvas* (a.k.a. stage) is a 2D pixel space with the
//! origin at the top-left, `x` to the right and `y` downward. Every output and
//! every layer is positioned in this one shared space — which is exactly why a
//! layer can span several outputs: each output simply renders whatever slice of
//! the canvas it covers.

use std::path::Path;

use glam::{Mat3, Mat4, Vec2, Vec3};
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
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

/// Corner-pin (keystone) warp. Corners are normalized output coordinates
/// (0..1, top-left origin) in the order TL, TR, BR, BL. `None` = identity.
///
/// The mapping is a true perspective transform (homography), which is what a
/// projector hitting a flat surface off-axis produces: straight lines stay
/// straight and content is distributed with correct foreshortening.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Warp {
    #[serde(default)]
    pub corners: Option<[[f32; 2]; 4]>,
}

const IDENTITY_CORNERS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

impl Warp {
    pub fn corners_or_identity(&self) -> [[f32; 2]; 4] {
        self.corners.unwrap_or(IDENTITY_CORNERS)
    }

    /// Homography `H` mapping the unit square onto the corner quad:
    /// `H · (u, v, 1) = (X, Y, W)`, with the warped point at `(X/W, Y/W)`.
    /// (Heckbert's closed-form square-to-quad solution.)
    pub fn homography(&self) -> Mat3 {
        let [p0, p1, p2, p3] = self.corners_or_identity();
        let (x0, y0, x1, y1) = (p0[0], p0[1], p1[0], p1[1]);
        let (x2, y2, x3, y3) = (p2[0], p2[1], p3[0], p3[1]);

        let dx3 = x0 - x1 + x2 - x3;
        let dy3 = y0 - y1 + y2 - y3;
        let (a, b, c, d, e, f, g, h);
        if dx3.abs() < 1e-9 && dy3.abs() < 1e-9 {
            // Parallelogram: the mapping is affine.
            (a, b, c) = (x1 - x0, x2 - x1, x0);
            (d, e, f) = (y1 - y0, y2 - y1, y0);
            (g, h) = (0.0, 0.0);
        } else {
            let (dx1, dx2) = (x1 - x2, x3 - x2);
            let (dy1, dy2) = (y1 - y2, y3 - y2);
            let den = dx1 * dy2 - dx2 * dy1;
            g = (dx3 * dy2 - dx2 * dy3) / den;
            h = (dx1 * dy3 - dx3 * dy1) / den;
            (a, b, c) = (x1 - x0 + g * x1, x3 - x0 + h * x3, x0);
            (d, e, f) = (y1 - y0 + g * y1, y3 - y0 + h * y3, y0);
        }
        // Rows [a b c; d e f; g h 1], stored column-major.
        Mat3::from_cols(Vec3::new(a, d, g), Vec3::new(b, e, h), Vec3::new(c, f, 1.0))
    }
}

/// Soft-edge blend for overlapping projectors.
///
/// Edge widths are the overlap with the neighbor as a fraction of this output
/// (0..1). The ramp is complementary (`r(x) + r(1-x) = 1`), so two outputs whose
/// `canvas_rect`s overlap by exactly that width sum to full brightness.
/// `gamma` is the *projector's* gamma: output is already sRGB-encoded, so the
/// default 2.2 needs no extra correction; tune it if the seam reads light/dark.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Blend {
    #[serde(default)]
    pub left: f32,
    #[serde(default)]
    pub right: f32,
    #[serde(default)]
    pub top: f32,
    #[serde(default)]
    pub bottom: f32,
    #[serde(default = "projector_gamma")]
    pub gamma: f32,
}

impl Default for Blend {
    fn default() -> Self {
        Blend {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
            gamma: projector_gamma(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Copy, Deserialize)]
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
    /// Pre-recorded video, decoded by an `ffmpeg` child process.
    Video { path: String },
    /// Live input such as a capture card or NDI stream (not implemented).
    Live { kind: String },
    /// Content that cannot be shown right now — an asset still downloading,
    /// or one the show refers to but does not define. Drawn as a placeholder
    /// so the layout stays readable and the gap is visible.
    Missing { reason: String },
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
    /// Seconds over which opacity ramps up from zero after `start`.
    #[serde(default)]
    pub fade_in: f64,
    /// Seconds over which opacity ramps down to zero before `end`.
    #[serde(default)]
    pub fade_out: f64,
    /// Video: position in the file at `start`.
    #[serde(default)]
    pub media_in: f64,
    /// Video: start over from `media_in` when the file ends.
    #[serde(default)]
    pub media_loop: bool,
}

impl Timing {
    pub fn visible_at(&self, t: f64) -> bool {
        t >= self.start && self.end.map(|e| t < e).unwrap_or(true)
    }

    /// Fade factor (0..1) at show time `t`: the product of the fade-in ramp
    /// and the fade-out ramp, so a cue shorter than its two fades still
    /// starts and ends at zero.
    pub fn fade_at(&self, t: f64) -> f32 {
        let mut k = 1.0f64;
        if self.fade_in > 0.0 {
            k *= ((t - self.start) / self.fade_in).clamp(0.0, 1.0);
        }
        if let (Some(end), true) = (self.end, self.fade_out > 0.0) {
            k *= ((end - t) / self.fade_out).clamp(0.0, 1.0);
        }
        k as f32
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
fn projector_gamma() -> f32 {
    2.2
}

impl Show {
    /// Parse a show from JSON text and validate it, so mistakes surface as a
    /// clear message at load time instead of a garbled image (or a GPU panic).
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        let show: Show = serde_json::from_str(text)?;
        if show.outputs.is_empty() {
            anyhow::bail!("show has no outputs");
        }
        for o in &show.outputs {
            o.validate()?;
        }
        Ok(show)
    }
}

impl Output {
    /// Reject geometry the renderer cannot draw, with a message that names
    /// the output and the field.
    pub fn validate(&self) -> anyhow::Result<()> {
        {
            let o = self;
            let id = &o.id;
            if !matches!(o.rotation_degrees, 0 | 90 | 180 | 270) {
                anyhow::bail!(
                    "output '{id}': rotation_degrees {} must be 0/90/180/270",
                    o.rotation_degrees
                );
            }
            if !(o.canvas_rect.w > 0.0 && o.canvas_rect.h > 0.0) {
                anyhow::bail!("output '{id}': canvas_rect must have positive w and h");
            }
            let b = &o.blend;
            let edges = [b.left, b.right, b.top, b.bottom];
            if edges.iter().any(|e| !(0.0..=1.0).contains(e))
                || b.left + b.right > 1.0
                || b.top + b.bottom > 1.0
            {
                anyhow::bail!(
                    "output '{id}': blend edges must be 0..1 and opposite edges sum to <= 1"
                );
            }
            if b.gamma <= 0.0 {
                anyhow::bail!("output '{id}': blend gamma must be > 0");
            }
            if let Some(c) = o.warp.corners {
                if !is_convex(&c) {
                    anyhow::bail!(
                        "output '{id}': warp corners must form a convex quad in TL, TR, BR, BL order"
                    );
                }
            }
        }
        Ok(())
    }
}

impl Show {
    /// Rewrite relative media paths to be relative to `base` (the show file's
    /// folder), so a show and its media folder work from any directory.
    pub fn resolve_paths(&mut self, base: &Path) {
        for layer in &mut self.layers {
            if let Source::Image { path } | Source::Video { path } = &mut layer.source {
                if Path::new(path.as_str()).is_relative() {
                    *path = base.join(&*path).to_string_lossy().into_owned();
                }
            }
        }
    }
}

/// True if the four points form a strictly convex quad (either winding).
/// Non-convex or self-intersecting corners have no valid perspective mapping.
fn is_convex(c: &[[f32; 2]; 4]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b, d) = (c[i], c[(i + 1) % 4], c[(i + 2) % 4]);
        let cross = (b[0] - a[0]) * (d[1] - b[1]) - (b[1] - a[1]) * (d[0] - b[0]);
        if cross.abs() < 1e-9 || (sign != 0.0 && cross.signum() != sign) {
            return false;
        }
        sign = cross.signum();
    }
    true
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
    fn fades_ramp_from_and_to_zero() {
        let t = Timing {
            start: 10.0,
            end: Some(20.0),
            fade_in: 2.0,
            fade_out: 4.0,
            ..Default::default()
        };
        assert_eq!(t.fade_at(10.0), 0.0);
        assert_eq!(t.fade_at(11.0), 0.5);
        assert_eq!(t.fade_at(15.0), 1.0);
        assert_eq!(t.fade_at(18.0), 0.5);
        assert_eq!(t.fade_at(20.0), 0.0);
        // No fades configured: always fully on.
        assert_eq!(Timing::default().fade_at(123.0), 1.0);
    }

    #[test]
    fn timing_visibility_window() {
        let t = Timing {
            start: 2.0,
            end: Some(5.0),
            ..Default::default()
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
        assert_eq!(show.outputs[0].blend.gamma, 2.2);
    }

    #[test]
    fn bundled_examples_are_valid_with_media_and_consistent_blends() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let file = entry.unwrap().path();
            if file.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let name = file.display();
            let mut show = Show::from_json(&std::fs::read_to_string(&file).unwrap())
                .unwrap_or_else(|e| panic!("{name}: {e:#}"));
            show.resolve_paths(&dir);

            for layer in &show.layers {
                if let Source::Image { path } = &layer.source {
                    assert!(Path::new(path).exists(), "{name}: missing {path}");
                }
            }
            // A blend only works if the neighbors' slices overlap by exactly
            // the blend width.
            for a in &show.outputs {
                for b in &show.outputs {
                    let (ra, rb) = (a.canvas_rect, b.canvas_rect);
                    if a.blend.right > 0.0 && b.blend.left > 0.0 && rb.x > ra.x {
                        let overlap = ra.x + ra.w - rb.x;
                        assert_eq!(overlap, a.blend.right * ra.w, "{name}: {} right", a.id);
                        assert_eq!(overlap, b.blend.left * rb.w, "{name}: {} left", b.id);
                    }
                }
            }
            checked += 1;
        }
        assert!(
            checked >= 4,
            "expected the bundled templates, found {checked}"
        );
    }

    #[test]
    fn relative_media_paths_resolve_against_the_show_folder() {
        let mut show = Show::from_json(
            r#"{ "canvas": { "width": 10, "height": 10 },
                 "outputs": [ { "id": "o", "canvas_rect": { "x": 0, "y": 0, "w": 10, "h": 10 } } ],
                 "layers": [
                   { "id": "rel", "source": { "type": "image", "path": "media/a.png" },
                     "transform": { "position": [5,5], "size": [10,10] } },
                   { "id": "abs", "source": { "type": "image", "path": "/srv/b.png" },
                     "transform": { "position": [5,5], "size": [10,10] } } ] }"#,
        )
        .unwrap();
        show.resolve_paths(Path::new("/shows/demo"));
        let paths: Vec<_> = show
            .layers
            .iter()
            .map(|l| match &l.source {
                Source::Image { path } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, ["/shows/demo/media/a.png", "/srv/b.png"]);
    }

    fn warp(c: [[f32; 2]; 4]) -> Warp {
        Warp { corners: Some(c) }
    }

    fn apply(h: Mat3, u: f32, v: f32) -> Vec2 {
        let p = h * Vec3::new(u, v, 1.0);
        Vec2::new(p.x / p.z, p.y / p.z)
    }

    #[test]
    fn identity_homography_is_identity() {
        let h = Warp::default().homography();
        for (u, v) in [(0.0, 0.0), (0.3, 0.7), (1.0, 1.0)] {
            assert!(apply(h, u, v).distance(Vec2::new(u, v)) < 1e-6);
        }
    }

    #[test]
    fn homography_pins_all_four_corners() {
        let c = [[0.1, 0.05], [0.95, 0.0], [0.8, 0.9], [0.0, 1.0]];
        let h = warp(c).homography();
        let unit = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        for (i, (u, v)) in unit.into_iter().enumerate() {
            assert!(
                apply(h, u, v).distance(Vec2::from(c[i])) < 1e-5,
                "corner {i} misplaced"
            );
        }
    }

    #[test]
    fn homography_keeps_straight_lines_straight() {
        // A keystoned trapezoid: bilinear warping would bend the diagonal.
        let c = [[0.2, 0.0], [0.8, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let h = warp(c).homography();
        let (a, m, b) = (apply(h, 0.0, 0.0), apply(h, 0.37, 0.37), apply(h, 1.0, 1.0));
        let cross = (m - a).perp_dot(b - a);
        assert!(cross.abs() < 1e-5, "diagonal is not straight: {cross}");
    }

    #[test]
    fn rejects_non_convex_warp() {
        assert!(is_convex(&IDENTITY_CORNERS));
        // Mirrored (reversed winding) is still a valid convex quad.
        assert!(is_convex(&[[1.0, 0.0], [0.0, 0.0], [0.0, 1.0], [1.0, 1.0]]));
        // Bow-tie: TR and BR swapped.
        assert!(!is_convex(&[
            [0.0, 0.0],
            [1.0, 1.0],
            [1.0, 0.0],
            [0.0, 1.0]
        ]));
    }
}
