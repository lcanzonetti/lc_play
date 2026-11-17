//! The synchronisation test pattern (`sync_test` cue source).
//!
//! Drawn by the Runner itself — no file, no decoder — so that what it shows
//! depends only on *which frame the Runner chose*, which is exactly what is
//! being tested. Filmed with a fast camera across two outputs (or two
//! machines), any disagreement is readable directly in frames:
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │ ■ □ ■ ■ □ □ ■ □ ■ □ □ ■ □ ■ ■ □  binary  │  frame number, 16 bits, MSB left
//! │                                          │
//! │            1 2 3 4 5                     │  frame number, 7-segment digits
//! │                                          │
//! │ ▮ . . . . . . . . . . . . . . . . . . .  │  marker: one step per frame
//! └──────────────────────────────────────────┘
//! ```
//!
//! On the first frame of every second the whole picture is white: a flash a
//! photodiode or the eye can catch. The picture is small (it is stretched to
//! the cue's size) and is rebuilt only when the frame number changes.

use crate::content::{Content, TextureFactory};
use crate::show::Timing;
use crate::video::media_frame;

const W: u32 = 320;
const H: u32 = 180;
/// Positions of the stepping marker; a whole cycle is this many frames.
const MARKER_STEPS: i64 = 20;

pub struct SyncTestContent {
    fps: f64,
    timing: Timing,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    shown: Option<i64>,
    pixels: Vec<u8>,
}

impl SyncTestContent {
    pub fn new(fps: f64, timing: &Timing, factory: &TextureFactory) -> Self {
        let texture = factory.texture("sync-test", W, H, wgpu::TextureFormat::Rgba8UnormSrgb);
        let bind_group = factory.bind("sync-test", [&texture, &texture, &texture]);
        SyncTestContent {
            fps: fps.clamp(1.0, 1000.0),
            timing: timing.clone(),
            texture,
            bind_group,
            shown: None,
            pixels: vec![0; (W * H * 4) as usize],
        }
    }
}

impl Content for SyncTestContent {
    fn update(&mut self, t: f64, gpu: &TextureFactory) {
        // The same frame arithmetic as video, so the pattern behaves like a
        // clip of infinite length (in point, free run and all).
        let frame = media_frame(&self.timing, self.fps, None, t);
        if self.shown == Some(frame) {
            return;
        }
        draw(&mut self.pixels, frame, self.fps);
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &self.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(W * 4),
                rows_per_image: Some(H),
            },
            wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
        );
        self.shown = Some(frame);
    }

    fn retime(&mut self, timing: &Timing) {
        self.timing = timing.clone();
        self.shown = None;
    }

    fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    fn ready(&self) -> bool {
        self.shown.is_some()
    }
}

/// Render frame `frame` of the pattern into `px` (RGBA8, W×H, opaque).
pub(crate) fn draw(px: &mut [u8], frame: i64, fps: f64) {
    let per_second = fps.round().max(1.0) as i64;
    let flash = frame.rem_euclid(per_second) == 0;
    let (bg, fg) = if flash { (255, 0) } else { (0, 255) };
    px.chunks_exact_mut(4)
        .for_each(|p| p.copy_from_slice(&[bg, bg, bg, 255]));

    let mut rect = |x0: u32, y0: u32, w: u32, h: u32, v: u8| {
        for y in y0..(y0 + h).min(H) {
            for x in x0..(x0 + w).min(W) {
                let i = ((y * W + x) * 4) as usize;
                px[i..i + 3].copy_from_slice(&[v, v, v]);
            }
        }
    };

    // Binary frame number: 16 cells, most significant bit on the left, each
    // with an outline so that "0" cells are visible too.
    let bits = frame.rem_euclid(1 << 16) as u32;
    let cell = W / 16;
    for b in 0..16 {
        let x = b * cell + 2;
        rect(x, 8, cell - 4, 28, fg / 3 + bg / 3 * 2);
        if bits & (1 << (15 - b)) != 0 {
            rect(x + 2, 10, cell - 8, 24, fg);
        } else {
            rect(x + 2, 10, cell - 8, 24, bg);
        }
    }

    // Seven-segment digits of the frame number (up to 8 digits), centred.
    let text = frame.rem_euclid(100_000_000).to_string();
    let (dw, dh, gap, thick) = (24u32, 48u32, 8u32, 6u32);
    let total = text.len() as u32 * (dw + gap) - gap;
    let mut x = (W - total) / 2;
    for ch in text.bytes() {
        let segments = SEGMENTS[(ch - b'0') as usize];
        let y = 56;
        let horizontal =
            |rect: &mut dyn FnMut(u32, u32, u32, u32, u8), yy: u32| rect(x, yy, dw, thick, fg);
        // a, b, c, d, e, f, g
        if segments & 0b100_0000 != 0 {
            horizontal(&mut rect, y);
        }
        if segments & 0b010_0000 != 0 {
            rect(x + dw - thick, y, thick, dh / 2, fg);
        }
        if segments & 0b001_0000 != 0 {
            rect(x + dw - thick, y + dh / 2, thick, dh / 2, fg);
        }
        if segments & 0b000_1000 != 0 {
            horizontal(&mut rect, y + dh - thick);
        }
        if segments & 0b000_0100 != 0 {
            rect(x, y + dh / 2, thick, dh / 2, fg);
        }
        if segments & 0b000_0010 != 0 {
            rect(x, y, thick, dh / 2, fg);
        }
        if segments & 0b000_0001 != 0 {
            horizontal(&mut rect, y + dh / 2 - thick / 2);
        }
        x += dw + gap;
    }

    // Stepping marker: one of MARKER_STEPS slots lit, advancing every frame.
    let slot = W / MARKER_STEPS as u32;
    for s in 0..MARKER_STEPS as u32 {
        rect(s * slot + slot / 2 - 1, 150, 2, 2, fg);
    }
    let lit = frame.rem_euclid(MARKER_STEPS) as u32;
    rect(lit * slot + 2, 128, slot - 4, 40, fg);
}

/// Segments a..g (MSB = a) of the digits 0–9.
const SEGMENTS: [u8; 10] = [
    0b111_1110, 0b011_0000, 0b110_1101, 0b111_1001, 0b011_0011, 0b101_1011, 0b101_1111, 0b111_0000,
    0b111_1111, 0b111_1011,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(px: &[u8], x: u32, y: u32) -> u8 {
        px[((y * W + x) * 4) as usize]
    }

    #[test]
    fn the_binary_cells_encode_the_frame_number() {
        let mut px = vec![0; (W * H * 4) as usize];
        draw(&mut px, 0b1010_0000_0000_0001, 60.0);
        let cell = W / 16;
        let lit = |b: u32| pixel(&px, b * cell + cell / 2, 20) == 255;
        assert!(lit(0) && !lit(1) && lit(2) && !lit(3) && lit(15));
    }

    #[test]
    fn the_first_frame_of_each_second_flashes() {
        let mut px = vec![0; (W * H * 4) as usize];
        draw(&mut px, 120, 60.0);
        assert_eq!(pixel(&px, 1, H - 2), 255, "background is white on a flash");
        draw(&mut px, 121, 60.0);
        assert_eq!(pixel(&px, 1, H - 2), 0);
    }

    #[test]
    fn the_marker_steps_one_slot_per_frame() {
        let mut px = vec![0; (W * H * 4) as usize];
        let slot = W / MARKER_STEPS as u32;
        draw(&mut px, 3, 60.0);
        assert_eq!(pixel(&px, 3 * slot + slot / 2, 140), 255);
        assert_eq!(pixel(&px, 4 * slot + slot / 2, 140), 0);
    }
}

/// Two lines sweeping across the layer: a vertical one left to right, a
/// horizontal one top to bottom, one step per frame at `fps`, white on
/// transparent so they can be watched over the content. A smooth sweep means
/// every frame reached the screen once, in order; a judder or a jump, a
/// repeated or dropped frame. Drawn by the scene shader (mode 6): sharp at
/// any resolution, nothing to upload.
pub struct SweepContent {
    fps: f64,
    frame: i64,
    /// The shader samples a texture whatever the mode: a 1×1 one.
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Frames for one sweep across the display, per second of `fps`: the
/// vertical line crosses in 2 s, the horizontal one in 3 s (different, so
/// the two do not move as one).
const SWEEP_X_SECONDS: f64 = 2.0;
const SWEEP_Y_SECONDS: f64 = 3.0;
/// Line width, output pixels.
const SWEEP_LINE_PX: f32 = 3.0;

impl SweepContent {
    pub fn new(fps: f64, factory: &TextureFactory) -> Self {
        let blank = factory.transparent();
        SweepContent {
            fps: fps.clamp(1.0, 1000.0),
            frame: 0,
            _texture: blank._texture,
            bind_group: blank.bind_group,
        }
    }

    /// Where the lines are on frame `frame`, 0..1 across the layer.
    pub(crate) fn positions(frame: i64, fps: f64) -> (f32, f32) {
        let steps = |seconds: f64| ((seconds * fps).round() as i64).max(2);
        let at = |n: i64| (frame.rem_euclid(n) as f64 + 0.5) / n as f64;
        (
            at(steps(SWEEP_X_SECONDS)) as f32,
            at(steps(SWEEP_Y_SECONDS)) as f32,
        )
    }
}

impl Content for SweepContent {
    fn update(&mut self, t: f64, _gpu: &TextureFactory) {
        // `t` is cluster time (seconds) for a display's test pattern: every
        // display on the same step at the same instant.
        self.frame = (t.max(0.0) * self.fps + 1e-6).floor() as i64;
    }

    fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    fn sampling(&self) -> crate::content::Sampling {
        let (x, y) = Self::positions(self.frame, self.fps);
        crate::content::Sampling {
            mode: crate::content::Encoding::Sweep,
            uv_scale: [1.0, 1.0],
            param: [x, y, SWEEP_LINE_PX, 0.0],
        }
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::*;

    #[test]
    fn the_lines_step_once_per_frame_and_wrap() {
        let fps = 50.0;
        let (x0, y0) = SweepContent::positions(0, fps);
        let (x1, _) = SweepContent::positions(1, fps);
        assert!(
            (x1 - x0 - 1.0 / 100.0).abs() < 1e-6,
            "100 steps across at 50 fps"
        );
        assert_eq!(SweepContent::positions(100, fps).0, x0, "wraps after 2 s");
        assert_eq!(SweepContent::positions(150, fps).1, y0, "and 3 s");
        assert!(x0 > 0.0 && x0 < 1.0);
    }
}
