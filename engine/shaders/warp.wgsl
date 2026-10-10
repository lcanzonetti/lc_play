// Warp + edge-blend pass: sample the output's rendered canvas slice through a
// warp mesh (vertex positions already distorted + oriented on the CPU) and
// multiply by a soft-edge blend mask so overlapping projectors fade seamlessly.

struct BlendUniform {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    gamma: f32,
    _pad: vec3<f32>,
};

@group(0) @binding(0) var<uniform> b: BlendUniform;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VSOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@location(0) ndc: vec2<f32>, @location(1) uv: vec2<f32>) -> VSOut {
    var out: VSOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = uv;
    return out;
}

// Linear ramp from 0 at the edge to 1 at distance `w`. `w <= 0` disables.
fn edge(x: f32, w: f32) -> f32 {
    if (w <= 0.0) {
        return 1.0;
    }
    return clamp(x / w, 0.0, 1.0);
}

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, samp, in.uv);
    var f = edge(in.uv.x, b.left)
          * edge(1.0 - in.uv.x, b.right)
          * edge(in.uv.y, b.top)
          * edge(1.0 - in.uv.y, b.bottom);
    f = pow(f, b.gamma);
    return vec4<f32>(c.rgb * f, c.a);
}
