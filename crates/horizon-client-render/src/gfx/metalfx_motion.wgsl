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
    // z: 1.0 when `obj_motion_in` carries per-entity ObjectMotion vectors (3rd-person player, mobs, items).
    // w: reserved.
    flags: vec4<f32>,
}

@group(0) @binding(0) var depth_in: texture_depth_2d;
@group(0) @binding(1) var<uniform> motion_u: MotionUniform;
@group(0) @binding(2) var opaque_depth_in: texture_depth_2d;
@group(0) @binding(3) var obj_motion_in: texture_2d<f32>;

struct MotionOut {
    @location(0) motion: vec2<f32>,
    // Always normalized to reverse-Z (1 near, 0 far) so MetalFX runs with `depthReversed = YES`.
    @location(1) depth: f32,
    // Reactive mask (0..1) biasing MetalFX toward the current frame on hand/water/disocclusions.
    @location(2) reactive: f32,
}

struct FgMotionOut {
    @location(0) motion: vec2<f32>,
    @location(1) reactive: f32,
}

fn load_rev_depth(tex: texture_depth_2d, p: vec2<i32>, dim: vec2<i32>, gl_mode: bool) -> f32 {
    let clamped = clamp(p, vec2<i32>(0), dim - vec2<i32>(1));
    let sample_p = select(clamped, vec2<i32>(clamped.x, dim.y - 1 - clamped.y), gl_mode);
    let d = textureLoad(tex, sample_p, 0);
    return select(d, 1.0 - d, gl_mode);
}

fn compute_motion(v: Full) -> MotionOut {
    let dim = vec2<i32>(textureDimensions(depth_in));
    let out_uv = v.position.xy / max(motion_u.size, vec2<f32>(1.0));
    let p = clamp(vec2<i32>(out_uv * vec2<f32>(dim)), vec2<i32>(0), dim - vec2<i32>(1));
    let gl_mode = motion_u.flags.x > 0.5;
    let has_opaque = motion_u.flags.y > 0.5;
    let has_obj_motion = motion_u.flags.z > 0.5;

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

    // Per-entity motion vectors (3rd-person player, mobs, dropped items) override static-world
    // camera reprojection so moving entities interpolate smoothly without backward snapping.
    if has_obj_motion {
        let obj_dim = vec2<i32>(textureDimensions(obj_motion_in));
        let obj_uv = (vec2<f32>(best_p) + 0.5) / vec2<f32>(dim);
        let obj_p = clamp(vec2<i32>(obj_uv * vec2<f32>(obj_dim)), vec2<i32>(0), obj_dim - vec2<i32>(1));
        let obj = textureLoad(obj_motion_in, obj_p, 0);
        if obj.z > 0.5 {
            let obj_mv = clamp(obj.xy * motion_u.size, vec2<f32>(-4096.0), vec2<f32>(4096.0));
            return MotionOut(obj_mv, z_center, 0.05);
        }
    }

    // Remove this frame's sub-pixel jitter before unprojecting with the unjittered camera matrix.
    let uv_unjit = (vec2<f32>(best_p) + 0.5) / vec2<f32>(dim) - motion_u.jitter_px / max(motion_u.size, vec2<f32>(1.0));
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

@fragment
fn fs_motion(v: Full) -> MotionOut {
    return compute_motion(v);
}

// 2-target variant for WGSL Frame Generation (omits unused `depth_r32` to save ~3.7 MB VRAM & bandwidth).
@fragment
fn fs_fg_motion(v: Full) -> FgMotionOut {
    let out = compute_motion(v);
    return FgMotionOut(out.motion, out.reactive);
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

// Fast single-pass bidirectional motion-compensated frame interpolation (~0.6 ms on Apple A14/A16).
// Generates halfway frame `N-0.5` from `real_cache[N-1]` (`fg_prev_tex`) and `real_cache[N]`
// (`fg_curr_tex`) using screen-space camera motion vectors (`fg_motion_tex`) and `fg_reactive_tex`.
struct FgInterpUniform {
    // xy: 1.0 / fg_size, z: 1.0 when history was reset (teleport/cut), w: reserved.
    params: vec4<f32>,
}

@group(0) @binding(0) var fg_curr_tex: texture_2d<f32>;
@group(0) @binding(1) var fg_prev_tex: texture_2d<f32>;
@group(0) @binding(2) var fg_motion_tex: texture_2d<f32>;
@group(0) @binding(3) var fg_reactive_tex: texture_2d<f32>;
@group(0) @binding(4) var fg_sampler: sampler;
@group(0) @binding(5) var<uniform> fg_u: FgInterpUniform;

@fragment
fn fs_fg_interp(v: Full) -> @location(0) vec4<f32> {
    let uv = v.uv;
    if fg_u.params.z > 0.5 {
        return textureSampleLevel(fg_curr_tex, fg_sampler, uv, 0.0);
    }

    // `fs_motion` stores pixel offset from current pixel (t=1) to previous pixel (t=0):
    // mv_uv = prev_uv - curr_uv.
    let inv_fg_size = fg_u.params.xy;
    let mv_px0 = textureSampleLevel(fg_motion_tex, fg_sampler, uv, 0.0).xy;
    let mv0 = clamp(mv_px0 * inv_fg_size, vec2<f32>(-0.35), vec2<f32>(0.35));

    // One-step backward refinement: look up motion at the Frame N location that lands on `uv` at t=0.5.
    let probe_uv = clamp(uv - 0.5 * mv0, vec2<f32>(0.001), vec2<f32>(0.999));
    let mv_px1 = textureSampleLevel(fg_motion_tex, fg_sampler, probe_uv, 0.0).xy;
    let mv1 = clamp(mv_px1 * inv_fg_size, vec2<f32>(-0.35), vec2<f32>(0.35));

    let mv_diff = length(mv1 - mv0);
    let mv = select(mv1, mv0, mv_diff > 0.08);
    let half_mv = 0.5 * mv;

    let uv_curr = uv - half_mv;
    let uv_prev = uv + half_mv;

    let curr_in = all(uv_curr >= vec2<f32>(0.0)) && all(uv_curr <= vec2<f32>(1.0));
    let prev_in = all(uv_prev >= vec2<f32>(0.0)) && all(uv_prev <= vec2<f32>(1.0));

    let c_curr = textureSampleLevel(
        fg_curr_tex,
        fg_sampler,
        clamp(uv_curr, vec2<f32>(0.0), vec2<f32>(1.0)),
        0.0,
    );
    let c_prev = textureSampleLevel(
        fg_prev_tex,
        fg_sampler,
        clamp(uv_prev, vec2<f32>(0.0), vec2<f32>(1.0)),
        0.0,
    );

    if !prev_in {
        return c_curr;
    }
    if !curr_in {
        return c_prev;
    }

    // Disocclusion / independent-object anti-ghosting blend:
    // Matching warped colors get a clean 50/50 blend; sharp mismatches (moving mobs, particles,
    // water waves, newly exposed edges) smoothly bias toward `c_curr` with zero flicker.
    let reactive = max(
        textureSampleLevel(fg_reactive_tex, fg_sampler, uv, 0.0).r,
        textureSampleLevel(fg_reactive_tex, fg_sampler, probe_uv, 0.0).r,
    );
    let color_diff = length(c_curr.rgb - c_prev.rgb);
    let disocc = smoothstep(0.18, 0.55, color_diff);
    let edge_mismatch = smoothstep(0.03, 0.12, mv_diff);
    let bias_curr = clamp(
        max(disocc * 0.85, max(edge_mismatch * 0.75, reactive * 0.65)),
        0.0,
        0.92,
    );
    let w_curr = mix(0.5, 1.0, bias_curr);
    return vec4<f32>(mix(c_prev.rgb, c_curr.rgb, w_curr), 1.0);
}

