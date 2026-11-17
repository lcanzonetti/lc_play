// Scene pass: draw one layer (a unit quad) into the output's canvas-slice
// target. The MVP maps the layer's unit quad through its canvas-space model
// matrix and the output's view-projection, so a layer that overlaps several
// outputs is drawn (clipped) correctly in each output's pass.
//
// A layer's pixels can arrive in several encodings, chosen by `u.mode`, so
// that video stays in the form it was decoded to and the GPU does the
// conversion (this is where the playback performance comes from):
//
//   0  RGBA, premultiplied alpha, sRGB texture (images, colours, RGBA video)
//   1  RGBA, straight alpha, sRGB texture (HAP / HAP Alpha / HAP R: BCn)
//   2  scaled YCoCg in a DXT5 texture, not sRGB (HAP Q)
//   3  YUV 4:2:0, three R8 planes, BT.709 limited range (decoded video)
//   4  YUV 4:2:0, three R8 planes, BT.601 limited range (SD video)
//   5  RGB, alpha ignored, sRGB texture (HAP DXT1, which is RGB only)
//   6  no texture: the sweeping-lines test pattern, drawn here
//
// Whatever the encoding, the result is premultiplied linear-light RGBA, which
// is what the (sRGB) scene target blends in.

// Must match `SceneUniform` in render.rs byte-for-byte (96 bytes).
struct SceneUniform {
    mvp: mat4x4<f32>,
    opacity: f32,
    mode: u32,
    // Fraction of the texture holding the picture: compressed textures are
    // padded to a multiple of 4 pixels.
    uv_scale: vec2<f32>,
    // Mode 6: x = vertical line position, y = horizontal line position
    // (0..1 across the layer), z = line width in output pixels.
    param: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: SceneUniform;
@group(1) @binding(0) var tex: texture_2d<f32>;    // RGBA, or the Y plane
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var tex_u: texture_2d<f32>;  // U plane (YUV only)
@group(1) @binding(3) var tex_v: texture_2d<f32>;  // V plane (YUV only)

struct VSOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VSOut {
    var out: VSOut;
    out.clip = u.mvp * vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv * u.uv_scale;
    return out;
}

// sRGB transfer function, exact (not a 2.2 power): encoded → linear.
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    // All samples are taken unconditionally (sampling inside non-uniform
    // control flow is not allowed); unused planes are bound to the main
    // texture and cost little.
    let s = textureSample(tex, samp, in.uv);
    let su = textureSample(tex_u, samp, in.uv).r;
    let sv = textureSample(tex_v, samp, in.uv).r;

    // Mode 6 needs screen-space derivatives, which must also be taken in
    // uniform control flow: here, for every mode.
    let px = fwidth(in.uv);

    var rgba: vec4<f32>;
    switch u.mode {
        case 6u: {
            // A vertical and a horizontal line, `param.z` output pixels
            // wide, wherever the layer is shown (fitted to its display).
            let half = 0.5 * u.param.z;
            let dx = abs(in.uv.x - u.param.x) / max(px.x, 1e-6);
            let dy = abs(in.uv.y - u.param.y) / max(px.y, 1e-6);
            let on = f32(dx <= half || dy <= half);
            rgba = vec4<f32>(on, on, on, on);
        }
        case 5u: {
            // DXT1 punch-through texels read as transparent black; in HAP
            // they mean black.
            rgba = vec4<f32>(s.rgb, 1.0);
        }
        case 1u: {
            // Straight alpha: premultiply here.
            rgba = vec4<f32>(s.rgb * s.a, s.a);
        }
        case 2u: {
            // HAP Q, "scaled YCoCg" (reference: Vidvox ScaledCoCgYToRGBA.frag).
            // R = Co, G = Cg, B = scale, A = Y; all offset by 128/255.
            let off = 0.50196078431373;
            let scale = (s.b * (255.0 / 8.0)) + 1.0;
            let co = (s.r - off) / scale;
            let cg = (s.g - off) / scale;
            let y = s.a;
            let rgb = vec3<f32>(y + co - cg, y + cg, y - co - cg);
            rgba = vec4<f32>(srgb_to_linear(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
        }
        case 3u, 4u: {
            // Limited ("TV") range: Y 16–235, chroma 16–240.
            let y = (s.r - 16.0 / 255.0) * (255.0 / 219.0);
            let cb = (su - 128.0 / 255.0) * (255.0 / 224.0);
            let cr = (sv - 128.0 / 255.0) * (255.0 / 224.0);
            var rgb: vec3<f32>;
            if u.mode == 3u {
                // BT.709
                rgb = vec3<f32>(y + 1.5748 * cr, y - 0.1873 * cb - 0.4681 * cr, y + 1.8556 * cb);
            } else {
                // BT.601
                rgb = vec3<f32>(y + 1.402 * cr, y - 0.344136 * cb - 0.714136 * cr, y + 1.772 * cb);
            }
            rgba = vec4<f32>(srgb_to_linear(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
        }
        default: {
            // Already premultiplied (and linearised by the sRGB texture).
            rgba = s;
        }
    }
    return rgba * u.opacity;
}
