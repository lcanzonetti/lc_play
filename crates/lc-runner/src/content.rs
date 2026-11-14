//! Layer content: turns a [`Source`] into something the compositor can draw —
//! a GPU texture plus a bind group for the scene shader.
//!
//! Phase 1 implements `Image` and `Color`. `Video` and `Live` resolve to a
//! clearly-labeled placeholder texture so a layout still composes and runs;
//! their real implementations arrive behind the `video` / `live` features.

use anyhow::Context as _;
use wgpu::util::DeviceExt as _;

use crate::show::Source;

/// Anything that can supply a texture to the scene pipeline for a given time.
pub trait Content {
    /// Advance to show time `t`, uploading a new frame if the source is dynamic.
    /// Static content ignores this.
    fn update(&mut self, _t: f64, _queue: &wgpu::Queue) {}

    /// Bind group for scene-shader group(1): `{ texture, sampler }`.
    fn bind_group(&self) -> &wgpu::BindGroup;
}

/// A single immutable texture (images, solid colors, placeholders).
pub struct StaticContent {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
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
    /// Bind group layout for scene group(1) (texture + sampler).
    pub layout: &'a wgpu::BindGroupLayout,
    pub sampler: &'a wgpu::Sampler,
    /// Largest texture edge the device accepts.
    pub max_dimension: u32,
}

impl TextureFactory<'_> {
    /// Upload sRGB-encoded, **premultiplied** RGBA8 pixels.
    fn upload_rgba8(&self, label: &str, width: u32, height: u32, rgba: &[u8]) -> StaticContent {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture_with_data(
            self.queue,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size,
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
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(self.sampler),
                },
            ],
        });
        StaticContent {
            _texture: texture,
            bind_group,
        }
    }
}

/// Build the content for a layer source.
pub fn build(source: &Source, factory: &TextureFactory) -> anyhow::Result<Box<dyn Content>> {
    match source {
        Source::Image { path } => {
            let mut img = image::open(path)
                .with_context(|| format!("loading image '{path}'"))?
                .to_rgba8();
            let (w, h) = img.dimensions();
            let max = factory.max_dimension;
            if w > max || h > max {
                anyhow::bail!("image '{path}' is {w}x{h}, larger than this GPU's {max}px limit");
            }
            premultiply_srgb(&mut img);
            Ok(Box::new(factory.upload_rgba8(path, w, h, &img)))
        }
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
        Source::Video { path } => build_video(path, factory),
        Source::Live { kind } => {
            tracing::warn!("live source '{kind}' not yet implemented; showing placeholder");
            Ok(placeholder(factory))
        }
    }
}

/// The magenta checkerboard shown for unimplemented or failed sources.
pub fn placeholder(factory: &TextureFactory) -> Box<dyn Content> {
    Box::new(factory.upload_rgba8(
        "placeholder",
        PLACEHOLDER_SIZE,
        PLACEHOLDER_SIZE,
        &placeholder_pixels(),
    ))
}

#[cfg(not(feature = "video"))]
fn build_video(path: &str, factory: &TextureFactory) -> anyhow::Result<Box<dyn Content>> {
    tracing::warn!(
        "video '{path}' needs the `video` feature (cargo run --features video); showing placeholder"
    );
    Ok(placeholder(factory))
}

#[cfg(feature = "video")]
fn build_video(path: &str, factory: &TextureFactory) -> anyhow::Result<Box<dyn Content>> {
    // Phase 2: decode with ffmpeg-next, upload frames in Content::update.
    // Until that lands, fail loudly so the feature build doesn't silently no-op.
    let _ = (path, factory);
    anyhow::bail!("video decoding (Phase 2) is not implemented yet")
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
fn srgb8(linear: f32) -> u8 {
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
