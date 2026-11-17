//! Layer content: turns a [`Source`] into something the compositor can draw —
//! one or more GPU textures plus a bind group for the scene shader, and a
//! description of how the shader must read them ([`Sampling`]).
//!
//! Loading never blocks the render loop. Decoding a large image or probing a
//! video file can take a second or more; that happens on a worker thread,
//! and the layer is simply transparent until its content is ready. A frame
//! that waited for a file instead would freeze every output on the machine
//! (and on Windows, after a few seconds, make the windows "not responding").
//!
//! | Source | Content | Decoding |
//! |---|---|---|
//! | `color` | [`StaticContent`] | — |
//! | `image` | [`ImageContent`] | worker thread, once |
//! | `video`, HAP in a QuickTime file | [`crate::hap::HapContent`] | the GPU (BCn textures); CPU only unpacks |
//! | `video`, anything else | [`crate::video::VideoContent`] | `ffmpeg` (hardware-accelerated where possible) → YUV planes → GPU converts |

use std::sync::mpsc::{Receiver, TryRecvError};

use anyhow::Context as _;
use wgpu::util::DeviceExt as _;

use crate::render::plane_entries;
use crate::show::{Layer, Source, Timing};

/// How the scene shader reads a layer's texture planes. Mirrors the `mode`
/// switch in `shaders/scene.wgsl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum Encoding {
    /// RGBA with premultiplied alpha in an sRGB texture.
    #[default]
    Premultiplied = 0,
    /// RGBA with straight alpha in an sRGB texture (HAP, HAP Alpha, HAP R).
    Straight = 1,
    /// HAP Q: scaled YCoCg in a DXT5 texture.
    ScaledYCoCg = 2,
    /// Three 8-bit planes, Y U V, BT.709 limited range.
    Yuv709 = 3,
    /// Three 8-bit planes, Y U V, BT.601 limited range.
    Yuv601 = 4,
    /// RGB in an sRGB texture whose alpha is to be ignored (HAP DXT1).
    Opaque = 5,
    /// No texture: the shader draws the sweeping-lines test pattern (see
    /// `testpattern.rs`), positioned by `Sampling::param`.
    Sweep = 6,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sampling {
    pub mode: Encoding,
    /// Fraction of the texture that holds the picture (compressed textures
    /// are padded to whole 4×4 blocks).
    pub uv_scale: [f32; 2],
    /// Extra values for shader-drawn content (the sweep test pattern).
    pub param: [f32; 4],
}

impl Default for Sampling {
    fn default() -> Self {
        Sampling {
            mode: Encoding::Premultiplied,
            uv_scale: [1.0, 1.0],
            param: [0.0; 4],
        }
    }
}

/// Anything that can supply a texture to the scene pipeline for a given time.
pub trait Content {
    /// Advance to time `t` (seconds on the layer's timeline, or cluster time
    /// for a free-running layer), uploading a new frame if the source is
    /// dynamic, and finishing any loading that has completed. Must return
    /// quickly: it runs once per layer per rendered frame.
    fn update(&mut self, _t: f64, _gpu: &TextureFactory) {}

    /// The layer's timing changed (it was moved on the timeline) while its
    /// source stayed the same. Dynamic content adopts the new timing instead
    /// of being rebuilt — which for video would mean a new decoder.
    fn retime(&mut self, _timing: &Timing) {}

    /// The layer is off screen and not about to appear: release whatever is
    /// only needed while playing (for video, the decoder).
    fn idle(&mut self) {}

    /// Bind group for scene-shader group(1): planes and sampler.
    fn bind_group(&self) -> &wgpu::BindGroup;

    /// Whether the textures hold a picture yet. Video textures are blank
    /// until the first decoded frame arrives — and blank YUV decodes to
    /// green — so until then the layer shows nothing.
    fn ready(&self) -> bool {
        true
    }

    /// Whether the picture in the textures is older than the frame last
    /// asked for (the decoder did not have it in time). Counted per output
    /// as a late decode: a frame where this layer was behind the others.
    fn late(&self) -> bool {
        false
    }

    /// How the shader must interpret the planes.
    fn sampling(&self) -> Sampling {
        Sampling::default()
    }
}

/// A single immutable texture (images, solid colors, placeholders).
pub struct StaticContent {
    /// Kept alive for the bind group that samples it.
    pub(crate) _texture: wgpu::Texture,
    pub(crate) bind_group: wgpu::BindGroup,
}

impl Content for StaticContent {
    fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }
}

/// Everything the content layer needs from the GPU side to build textures.
pub struct TextureFactory<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    /// Bind group layout for scene group(1) (planes + sampler).
    pub layout: &'a wgpu::BindGroupLayout,
    pub sampler: &'a wgpu::Sampler,
    /// Largest texture edge the device accepts.
    pub max_dimension: u32,
    /// The device samples BCn-compressed textures (needed for GPU HAP).
    pub supports_bc: bool,
}

impl TextureFactory<'_> {
    /// An empty texture to be filled later (`write_texture`).
    pub fn texture(
        &self,
        label: &str,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    /// A bind group over up to three planes (repeat the first for content
    /// with fewer).
    pub fn bind(&self, label: &str, planes: [&wgpu::Texture; 3]) -> wgpu::BindGroup {
        let views = planes.map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: self.layout,
            entries: &plane_entries([&views[0], &views[1], &views[2]], self.sampler),
        })
    }

    /// Upload sRGB-encoded, **premultiplied** RGBA8 pixels.
    pub(crate) fn upload_rgba8(
        &self,
        label: &str,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> StaticContent {
        let texture = self.device.create_texture_with_data(
            self.queue,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            rgba,
        );
        let bind_group = self.bind(label, [&texture, &texture, &texture]);
        StaticContent {
            _texture: texture,
            bind_group,
        }
    }

    /// A 1×1 fully transparent texture: what a layer shows while loading.
    pub fn transparent(&self) -> StaticContent {
        self.upload_rgba8("loading", 1, 1, &[0, 0, 0, 0])
    }
}

/// Identity of a layer's content for reuse across show edits: two layers
/// with the same key can share (or keep) the same loaded content. Video is
/// keyed per layer, since each layer needs its own decoder position.
pub fn cache_key(layer: &Layer) -> String {
    match &layer.source {
        Source::Image { path } => format!("image:{path}"),
        Source::Color { rgba } => format!("color:{rgba:?}"),
        // Keyed per layer, like video: each follows its own cue's timing.
        Source::SyncTest { fps } => format!("sync-test:{}:{fps}", layer.id),
        Source::SweepTest { fps } => format!("sweep-test:{}:{fps}", layer.id),
        Source::Video { path } => format!("video:{}:{path}", layer.id),
        Source::Live { kind } => format!("live:{kind}"),
        Source::Missing { reason } => format!("missing:{reason}"),
    }
}

/// Build the content for a layer. Never blocks on files: anything slow is
/// started here and finished by the content's `update`.
pub fn build(layer: &Layer, factory: &TextureFactory) -> anyhow::Result<Box<dyn Content>> {
    match &layer.source {
        Source::Image { path } => Ok(Box::new(ImageContent::start(path, factory))),
        Source::Color { rgba } => {
            let a = rgba[3].clamp(0.0, 1.0);
            let px = [
                srgb8(rgba[0] * a),
                srgb8(rgba[1] * a),
                srgb8(rgba[2] * a),
                (a * 255.0).round() as u8,
            ];
            Ok(Box::new(factory.upload_rgba8("color", 1, 1, &px)))
        }
        Source::Video { path } => Ok(crate::hap::open_video(path, &layer.timing, factory)),
        Source::SyncTest { fps } => Ok(Box::new(crate::testpattern::SyncTestContent::new(
            *fps,
            &layer.timing,
            factory,
        ))),
        Source::SweepTest { fps } => Ok(Box::new(crate::testpattern::SweepContent::new(
            *fps, factory,
        ))),
        Source::Missing { reason } => {
            tracing::debug!(layer = %layer.id, "{reason}; showing placeholder");
            Ok(placeholder(factory))
        }
        Source::Live { kind } => {
            tracing::warn!("live source '{kind}' not yet implemented; showing placeholder");
            Ok(placeholder(factory))
        }
    }
}

/// Width, height and premultiplied pixels — or why not.
type DecodedImage = anyhow::Result<(u32, u32, Vec<u8>)>;

/// A still image, decoded on a worker thread.
pub struct ImageContent {
    current: StaticContent,
    pending: Option<Receiver<DecodedImage>>,
    path: String,
}

impl ImageContent {
    fn start(path: &str, factory: &TextureFactory) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (owned, max) = (path.to_string(), factory.max_dimension);
        let spawned = std::thread::Builder::new()
            .name("lc-image".into())
            .spawn(move || {
                crate::priority::background_thread();
                let _ = tx.send(decode_image(&owned, max));
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start an image loader: {e}");
        }
        ImageContent {
            current: factory.transparent(),
            pending: Some(rx),
            path: path.to_string(),
        }
    }
}

/// Read, check and premultiply an image file. Runs off the render thread.
fn decode_image(path: &str, max: u32) -> DecodedImage {
    let mut img = image::open(path)
        .with_context(|| format!("loading image '{path}'"))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    if w > max || h > max {
        anyhow::bail!("image '{path}' is {w}x{h}, larger than this GPU's {max}px limit");
    }
    premultiply_srgb(&mut img);
    Ok((w, h, img.into_raw()))
}

impl Content for ImageContent {
    fn update(&mut self, _t: f64, gpu: &TextureFactory) {
        let Some(rx) = &self.pending else {
            return;
        };
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            Ok(Ok((w, h, rgba))) => {
                self.current = gpu.upload_rgba8(&self.path, w, h, &rgba);
                self.pending = None;
            }
            Ok(Err(e)) => {
                tracing::error!("{e:#}; showing placeholder");
                self.current = placeholder_static(gpu);
                self.pending = None;
            }
            Err(TryRecvError::Disconnected) => {
                self.current = placeholder_static(gpu);
                self.pending = None;
            }
        }
    }

    fn bind_group(&self) -> &wgpu::BindGroup {
        &self.current.bind_group
    }
}

/// The magenta checkerboard shown for unimplemented or failed sources.
pub fn placeholder(factory: &TextureFactory) -> Box<dyn Content> {
    Box::new(placeholder_static(factory))
}

pub(crate) fn placeholder_static(factory: &TextureFactory) -> StaticContent {
    factory.upload_rgba8(
        "placeholder",
        PLACEHOLDER_SIZE,
        PLACEHOLDER_SIZE,
        &placeholder_pixels(),
    )
}

const PLACEHOLDER_SIZE: u32 = 64;

/// A magenta/black checkerboard so unimplemented sources are obvious on screen.
fn placeholder_pixels() -> Vec<u8> {
    let mut px = Vec::with_capacity((PLACEHOLDER_SIZE * PLACEHOLDER_SIZE * 4) as usize);
    for y in 0..PLACEHOLDER_SIZE {
        for x in 0..PLACEHOLDER_SIZE {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            let c = if on {
                [255, 0, 255, 255]
            } else {
                [20, 20, 20, 255]
            };
            px.extend_from_slice(&c);
        }
    }
    px
}

/// Convert straight-alpha sRGB pixels to premultiplied alpha, multiplying in
/// *linear* light (decode → × alpha → re-encode). Premultiplying the encoded
/// bytes directly would make semi-transparent edges far too dark. Opaque
/// pixels — usually the vast majority — are skipped.
fn premultiply_srgb(rgba: &mut [u8]) {
    let decode: [f32; 256] = std::array::from_fn(|i| srgb_to_linear(i as f32 / 255.0));
    for px in rgba.as_chunks_mut::<4>().0 {
        match px[3] {
            255 => {}
            0 => px[..3].fill(0),
            a => {
                let a = a as f32 / 255.0;
                for c in &mut px[..3] {
                    *c = srgb8(decode[*c as usize] * a);
                }
            }
        }
    }
}

pub(crate) fn srgb_to_linear(s: f32) -> f32 {
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear → sRGB 8-bit encoding, so values given in linear space round-trip
/// correctly through an sRGB texture.
pub(crate) fn srgb8(linear: f32) -> u8 {
    let l = linear.clamp(0.0, 1.0);
    let s = if l <= 0.0031308 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_round_trips() {
        for b in [0u8, 1, 50, 128, 200, 255] {
            assert_eq!(srgb8(srgb_to_linear(b as f32 / 255.0)), b);
        }
    }

    #[test]
    fn premultiply_is_linear_correct() {
        // White at 50% alpha must become 50% *linear* light (sRGB byte ~188),
        // not byte 128 (which is only ~21% linear light).
        let mut px = [255, 255, 255, 128, 10, 20, 30, 255, 99, 99, 99, 0];
        premultiply_srgb(&mut px);
        assert!((186..=189).contains(&px[0]), "got {}", px[0]);
        assert_eq!(&px[4..8], &[10, 20, 30, 255], "opaque pixels untouched");
        assert_eq!(&px[8..12], &[0, 0, 0, 0], "transparent pixels zeroed");
    }
}
