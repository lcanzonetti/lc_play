// Scene pass: draw one layer (a unit quad) into the output's canvas-slice
// target. The MVP maps the layer's unit quad through its canvas-space model
// matrix and the output's view-projection, so a layer that overlaps several
// outputs is drawn (clipped) correctly in each output's pass.

// Must match `SceneUniform` in render.rs byte-for-byte (80 bytes). Pad with
// scalars: a vec3 pad would be 16-byte aligned and grow the struct to 96.
struct SceneUniform {
    mvp: mat4x4<f32>,
    opacity: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> u: SceneUniform;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VSOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VSOut {
    var out: VSOut;
    out.clip = u.mvp * vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

// Textures hold premultiplied alpha (see content.rs), so opacity scales all
// four channels and blending uses the premultiplied "over" operator. This
// avoids the dark fringes straight alpha produces under linear filtering.
@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv) * u.opacity;
}
