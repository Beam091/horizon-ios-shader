// MetalFX support shaders:
// 1. `fs_motion`: generates screen-space motion vectors (Rg16Float, pixels) + screen-aligned
//    R32Float depth for Apple `MTLFXTemporalScaler` from either Vanilla reverse-Z depth or
//    Shader Pack GL bottom-up depth (`depthtex0`).
// 2. `fs_blit`: copies the MetalFX output texture smoothly onto the display swapchain.
// 3. `fs_cas`: Contrast Adaptive Sharpening upscale fallback when MetalFX hardware is unavailable.

struct Full {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_screen(@builtin(vertex_index) i: u32) -> Full {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return Full(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

struct MotionUniform {
    reproject: mat4x4<f32>,
    size: vec2<f32>,
    // 1.0 for Shader Pack GL bottom-up depth (forward-Z 0..1), 0.0 for Vanilla top-down reverse-Z.
    flip_y_and_gl_depth: f32,
    _pad: f32,
}

@group(0) @binding(0) var depth_in: texture_depth_2d;
@group(0) @binding(1) var<uniform> motion_u: MotionUniform;

struct MotionOut {
    @location(0) motion: vec2<f32>,
    @location(1) depth: f32,
}

@fragment
fn fs_motion(v: Full) -> MotionOut {
    let dim = vec2<i32>(textureDimensions(depth_in));
    let p = clamp(vec2<i32>(v.position.xy), vec2<i32>(0), dim - vec2<i32>(1));
    let gl_mode = motion_u.flip_y_and_gl_depth > 0.5;
    let sample_p = select(p, vec2<i32>(p.x, dim.y - 1 - p.y), gl_mode);
    let d = textureLoad(depth_in, sample_p, 0);
    // `motion_u.reproject` operates in Horizon's reverse-Z clip space (1 near, 0 far).
    let z_clip = select(d, 1.0 - d, gl_mode);
    let ndc = vec2<f32>(v.uv.x * 2.0 - 1.0, 1.0 - v.uv.y * 2.0);
    let prev_clip = motion_u.reproject * vec4<f32>(ndc, z_clip, 1.0);
    var mv = vec2<f32>(0.0, 0.0);
    if prev_clip.w > 1e-6 {
        let prev_ndc = prev_clip.xy / prev_clip.w;
        let prev_uv = vec2<f32>(prev_ndc.x * 0.5 + 0.5, 0.5 - prev_ndc.y * 0.5);
        // Apple MTLFXTemporalScaler convention (motionVectorScale = 1.0):
        // pixel offset from current pixel to previous frame's pixel.
        mv = clamp((prev_uv - v.uv) * motion_u.size, vec2<f32>(-4096.0), vec2<f32>(4096.0));
    }
    return MotionOut(mv, d);
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;

@fragment
fn fs_blit(v: Full) -> @location(0) vec4<f32> {
    return textureSampleLevel(src_tex, src_sampler, v.uv, 0.0);
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.299, 0.587, 0.114));
}

// Lightweight Contrast Adaptive Sharpening upscale fallback for devices without MetalFX.
@fragment
fn fs_cas(v: Full) -> @location(0) vec4<f32> {
    let inv = 1.0 / vec2<f32>(textureDimensions(src_tex));
    let c = textureSampleLevel(src_tex, src_sampler, v.uv, 0.0);
    let n = textureSampleLevel(src_tex, src_sampler, v.uv + vec2<f32>(0.0, -inv.y), 0.0).rgb;
    let s = textureSampleLevel(src_tex, src_sampler, v.uv + vec2<f32>(0.0, inv.y), 0.0).rgb;
    let e = textureSampleLevel(src_tex, src_sampler, v.uv + vec2<f32>(inv.x, 0.0), 0.0).rgb;
    let w = textureSampleLevel(src_tex, src_sampler, v.uv + vec2<f32>(-inv.x, 0.0), 0.0).rgb;
    let mn = min(c.rgb, min(min(n, s), min(e, w)));
    let mx = max(c.rgb, max(max(n, s), max(e, w)));
    let amp = clamp(min(mn, 1.0 - mx) / max(mx, vec3<f32>(1e-4)), vec3<f32>(0.0), vec3<f32>(1.0));
    let weight = -0.14 * sqrt(amp);
    let sharpened = (c.rgb + weight * (n + s + e + w)) / (1.0 + 4.0 * weight);
    return vec4<f32>(clamp(sharpened, vec3<f32>(0.0), vec3<f32>(100.0)), c.a);
}
