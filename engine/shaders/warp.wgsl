// Warp + edge-blend pass: sample the output's rendered canvas slice through the
// warp mesh and multiply by a soft-edge blend mask so overlapping projectors
// fade seamlessly.
//
// Vertices arrive as homogeneous clip positions (x·w, y·w, 0, w) computed on the
// CPU from the corner-pin homography, so the rasterizer's perspective-correct
// interpolation applies the keystone exactly — no mesh-resolution error.

// Must match `BlendUniform` in render.rs byte-for-byte (32 bytes). Pad with
// scalars: a vec3 pad would be 16-byte aligned and grow the struct to 48.
struct BlendUniform {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    // 2.2 / projector_gamma (1.0 for a standard 2.2 projector).
    gamma_exp: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> b: BlendUniform;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VSOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@location(0) clip: vec4<f32>, @location(1) uv: vec2<f32>) -> VSOut {
    var out: VSOut;
    out.clip = clip;
    out.uv = uv;
    return out;
}

// Complementary ramp across a blend zone of width `w`: ramp(t) + ramp(1-t) = 1,
// so this edge and the neighbor's mirrored edge always sum to full light.
// `w <= 0` disables the edge.
fn edge(d: f32, w: f32) -> f32 {
    if (w <= 0.0) {
        return 1.0;
    }
    return smoothstep(0.0, 1.0, d / w);
}

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, samp, in.uv);
    var f = edge(in.uv.x, b.left)
          * edge(1.0 - in.uv.x, b.right)
          * edge(in.uv.y, b.top)
          * edge(1.0 - in.uv.y, b.bottom);
    // Output is sRGB-encoded (≈ gamma 2.2); compensate for the projector's own
    // gamma so the *light* (not the signal) sums to 1 across the overlap.
    f = pow(f, b.gamma_exp);
    return vec4<f32>(c.rgb * f, 1.0);
}
