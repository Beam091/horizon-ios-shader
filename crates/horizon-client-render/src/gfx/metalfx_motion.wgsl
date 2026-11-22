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
    // Rotation-only reprojection for sky pixels at infinity (avoids parallax distortion at low render distance).
    reproject_sky: mat4x4<f32>,
    size: vec2<f32>,
    // This frame's sub-pixel jitter in input pixels (+X right, +Y down); 0 when unjittered.
    jitter_px: vec2<f32>,
    // x: 1.0 for Shader Pack GL bottom-up forward-Z depth, 0.0 for Vanilla top-down reverse-Z.
    // y: 1.0 when `opaque_depth_in` carries Shader Pack `depthtex1` (opaque-only depth for water detection).
    // z, w: reserved.
    flags: vec4<f32>,
}

@group(0) @binding(0) var depth_in: texture_depth_2d;
@group(0) @binding(1) var<uniform> motion_u: MotionUniform;
@group(0) @binding(2) var opaque_depth_in: texture_depth_2d;

struct MotionOut {
    @location(0) motion: vec2<f32>,
    // Always normalized to reverse-Z (1 near, 0 far) so MetalFX runs with `depthReversed = YES`.
    @location(1) depth: f32,
    // Reactive mask (0..1) biasing MetalFX toward the current frame on hand/water/disocclusions.
    @location(2) reactive: f32,
}

fn load_rev_depth(tex: texture_depth_2d, p: vec2<i32>, dim: vec2<i32>, gl_mode: bool) -> f32 {
    let clamped = clamp(p, vec2<i32>(0), dim - vec2<i32>(1));
    let sample_p = select(clamped, vec2<i32>(clamped.x, dim.y - 1 - clamped.y), gl_mode);
    let d = textureLoad(tex, sample_p, 0);
    return select(d, 1.0 - d, gl_mode);
}

@fragment
fn fs_motion(v: Full) -> MotionOut {
    let dim = vec2<i32>(textureDimensions(depth_in));
    let p = clamp(vec2<i32>(v.position.xy), vec2<i32>(0), dim - vec2<i32>(1));
    let gl_mode = motion_u.flags.x > 0.5;
    let has_opaque = motion_u.flags.y > 0.5;

    let sample_p = select(p, vec2<i32>(p.x, dim.y - 1 - p.y), gl_mode);
    let d_raw = textureLoad(depth_in, sample_p, 0);
    let z_center = select(d_raw, 1.0 - d_raw, gl_mode);

    // Shader Pack first-person hand uses MC_HAND_DEPTH = 0.125, mapping GL depth into [0.4375, 0.5625].
    // The hand moves with the camera (zero screen-space world velocity), so reprojecting it with
    // world camera matrices would generate massive bogus motion vectors and warp the hand/edges.
    if gl_mode && d_raw > 0.0 && d_raw <= 0.565 {
        return MotionOut(vec2<f32>(0.0, 0.0), z_center, 0.95);
    }

    // 3x3 nearest-depth dilation (max in reverse-Z) so foreground silhouettes carry their own
    // motion vectors instead of dragging/warping with background motion.
    var best_z = z_center;
    var best_p = p;
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            let q = clamp(p + vec2<i32>(dx, dy), vec2<i32>(0), dim - vec2<i32>(1));
            let sp = select(q, vec2<i32>(q.x, dim.y - 1 - q.y), gl_mode);
            let dq = textureLoad(depth_in, sp, 0);
            if gl_mode && dq > 0.0 && dq <= 0.565 {
                continue;
            }
            let zq = select(dq, 1.0 - dq, gl_mode);
            if zq > best_z {
                best_z = zq;
                best_p = q;
            }
        }
    }

    // Remove this frame's sub-pixel jitter before unprojecting with the unjittered camera matrix.
    let uv_unjit = (vec2<f32>(best_p) + 0.5 - motion_u.jitter_px) / motion_u.size;
    let ndc = vec2<f32>(uv_unjit.x * 2.0 - 1.0, 1.0 - uv_unjit.y * 2.0);
    let is_sky = best_z <= 1e-6;
    let prev_clip = select(
        motion_u.reproject * vec4<f32>(ndc, best_z, 1.0),
        motion_u.reproject_sky * vec4<f32>(ndc, 0.0, 1.0),
        is_sky,
    );

    var mv = vec2<f32>(4096.0, 4096.0);
    var reactive = 0.0;
    if prev_clip.w > 1e-6 {
        let prev_ndc = prev_clip.xy / prev_clip.w;
        let prev_uv = vec2<f32>(prev_ndc.x * 0.5 + 0.5, 0.5 - prev_ndc.y * 0.5);
        // Apple MTLFXTemporalScaler convention (motionVectorScale = 1.0):
        // pixel offset from current pixel to previous frame's pixel.
        mv = clamp((prev_uv - uv_unjit) * motion_u.size, vec2<f32>(-4096.0), vec2<f32>(4096.0));
        if any(prev_uv < vec2<f32>(0.0)) || any(prev_uv > vec2<f32>(1.0)) {
            reactive = 1.0;
        }
    } else {
        // Point came from behind the previous camera: force history rejection instead of zero motion.
        reactive = 1.0;
    }

    // Translucent water/glass in Shader Packs (`depthtex0` != `depthtex1`) carries animated waves,
    // reflections and refraction whose optical flow diverges from surface depth.
    if gl_mode && has_opaque {
        let z_opaque = load_rev_depth(opaque_depth_in, p, dim, true);
        if abs(z_center - z_opaque) > 1e-4 {
            reactive = max(reactive, 0.90);
        }
    }

    // Shader Packs run their own post-processing / TAA / waving foliage / volumetric clouds
    // without MetalFX jitter; raise reactive weight under motion to prevent rubbery warping.
    if gl_mode {
        let speed = length(mv);
        let pack_reactive = clamp(0.32 + speed * 0.04, 0.32, 0.92);
        reactive = max(reactive, select(pack_reactive, max(pack_reactive, 0.55), is_sky));
    }

    return MotionOut(mv, z_center, reactive);
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
