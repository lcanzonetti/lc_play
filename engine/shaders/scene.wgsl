// Scene pass: draw one layer (a unit quad) into the output's canvas-slice
// target. The MVP maps the layer's unit quad through its canvas-space model
// matrix and the output's view-projection, so a layer that overlaps several
// outputs is drawn (clipped) correctly in each output's pass.

struct SceneUniform {
    mvp: mat4x4<f32>,
    opacity: f32,
    _pad: vec3<f32>,
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

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    var c = textureSample(tex, samp, in.uv);
    c.a = c.a * u.opacity;
    return c;
}
