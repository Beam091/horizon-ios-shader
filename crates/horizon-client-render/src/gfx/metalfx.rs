//! Apple MetalFX hardware upscaling (`MTLFXTemporalScaler` and `MTLFXSpatialScaler`)
//! with automatic x2 vertical resolution tiers (`360p->720p`, `480p->960p`, `540p->1080p`,
//! `720p->1440p/Max`) and a WGSL CAS sharpening fallback for older or non-Apple GPUs.
#![cfg_attr(any(target_os = "ios", target_os = "macos"), allow(unsafe_code))]

use bytemuck::{Pod, Zeroable};
use glam::{DMat4, DVec3, Mat4, Vec2};

use crate::graphics::{MetalFxFactor, MetalFxMode, MetalFxPreset};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MotionUniform {
    reproject: [[f32; 4]; 4],
    reproject_sky: [[f32; 4]; 4],
    size: [f32; 2],
    jitter_px: [f32; 2],
    flags: [f32; 4],
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
enum NativeScaler {
    Temporal(wgpu::hal::metal::metalfx::TemporalScaler),
    Spatial(wgpu::hal::metal::metalfx::SpatialScaler),
}

#[allow(dead_code)]
struct Targets {
    mode: MetalFxMode,
    input_size: [u32; 2],
    output_size: [u32; 2],
    color_format: wgpu::TextureFormat,
    /// Intermediate `input_size` color target (when input isn't already a standalone `wgpu::Texture`).
    input_color: wgpu::Texture,
    input_color_view: wgpu::TextureView,
    /// `input_size` Rg16Float motion vectors (for `MTLFXTemporalScaler`).
    motion: wgpu::Texture,
    motion_view: wgpu::TextureView,
    /// `input_size` R32Float screen-aligned reverse-Z depth (for `MTLFXTemporalScaler`).
    depth_r32: wgpu::Texture,
    depth_r32_view: wgpu::TextureView,
    /// `input_size` R8Unorm reactive mask (iOS 17.4+ / macOS 14.4+ `MTLFXTemporalScaler`).
    reactive: wgpu::Texture,
    reactive_view: wgpu::TextureView,
    /// `output_size` Rgba16Float/Rgba8Unorm upscaled picture (`STORAGE_BINDING` required by MetalFX).
    output: wgpu::Texture,
    output_view: wgpu::TextureView,
    output_blit_group: wgpu::BindGroup,
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    scaler: Option<NativeScaler>,
    reset_next: bool,
}

/// Per-frame camera and timing inputs for Apple `MTLFXFrameInterpolator`.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct FrameGenParams {
    pub delta_time: f32,
    pub near: f32,
    pub far: f32,
    pub fov_degrees: f32,
    pub render_size: [u32; 2],
}

#[allow(dead_code)]
struct FrameGenTargets {
    size: [u32; 2],
    color_format: wgpu::TextureFormat,
    /// Ping-pong current/previous color buffers (`curr_idx` and `1 - curr_idx`) matching `size`.
    history: [wgpu::Texture; 2],
    history_views: [wgpu::TextureView; 2],
    motion: wgpu::Texture,
    motion_view: wgpu::TextureView,
    depth_r32: wgpu::Texture,
    depth_r32_view: wgpu::TextureView,
    reactive: wgpu::Texture,
    reactive_view: wgpu::TextureView,
    output: wgpu::Texture,
    output_view: wgpu::TextureView,
    output_blit_group: wgpu::BindGroup,
    curr_idx: usize,
    has_prev: bool,
    reset_next: bool,
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    interpolator: Option<wgpu::hal::metal::metalfx::FrameInterpolator>,
}

#[allow(dead_code)]
pub struct MetalFx {
    pub mode: MetalFxMode,
    pub preset: MetalFxPreset,
    pub factor: MetalFxFactor,
    format: wgpu::TextureFormat,
    motion_pipeline: wgpu::RenderPipeline,
    motion_layout: wgpu::BindGroupLayout,
    motion_uniform: wgpu::Buffer,
    fg_motion_uniform: wgpu::Buffer,
    blit_pipeline: wgpu::RenderPipeline,
    cas_pipeline: wgpu::RenderPipeline,
    input_copy_rgba8: wgpu::RenderPipeline,
    input_copy_rgba16: wgpu::RenderPipeline,
    blit_layout: wgpu::BindGroupLayout,
    linear: wgpu::Sampler,
    targets: Option<Targets>,
    fg_targets: Option<FrameGenTargets>,
    fg_supported_cache: std::sync::OnceLock<bool>,
    frame_index: u32,
    previous_camera: Option<(DMat4, DVec3, bool)>,
    jitter_px: [f32; 2],
    reproject: Mat4,
    reproject_sky: Mat4,
    reset_history: bool,
}

impl MetalFx {
    pub fn new(device: &wgpu::Device, screen_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("MetalFX helpers"),
            source: wgpu::ShaderSource::Wgsl(include_str!("metalfx_motion.wgsl").into()),
        });
        let motion_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("MetalFX motion bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let motion_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("MetalFX motion"),
            bind_group_layouts: &[Some(&motion_layout)],
            ..Default::default()
        });
        let motion_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("MetalFX motion"),
            layout: Some(&motion_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_screen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_motion"),
                compilation_options: Default::default(),
                targets: &[
                    Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rg16Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                    Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::R32Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                    Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::R8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                ],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("MetalFX blit bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let blit_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("MetalFX blit"),
            bind_group_layouts: &[Some(&blit_layout)],
            ..Default::default()
        });
        let make_blit = |entry: &str, label: &str, target_format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&blit_pl),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_screen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let blit_pipeline = make_blit("fs_blit", "MetalFX blit", screen_format);
        let cas_pipeline = make_blit("fs_cas", "MetalFX CAS fallback", screen_format);
        let input_copy_rgba8 = make_blit(
            "fs_blit",
            "MetalFX input rgba8",
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let input_copy_rgba16 = make_blit(
            "fs_blit",
            "MetalFX input rgba16f",
            wgpu::TextureFormat::Rgba16Float,
        );
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("MetalFX linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let motion_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("MetalFX motion uniform"),
            size: std::mem::size_of::<MotionUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let fg_motion_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("MetalFX frame-gen motion uniform"),
            size: std::mem::size_of::<MotionUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            mode: MetalFxMode::Off,
            preset: MetalFxPreset::P540,
            factor: MetalFxFactor::X2,
            format: screen_format,
            motion_pipeline,
            motion_layout,
            motion_uniform,
            fg_motion_uniform,
            blit_pipeline,
            cas_pipeline,
            input_copy_rgba8,
            input_copy_rgba16,
            blit_layout,
            linear,
            targets: None,
            fg_targets: None,
            fg_supported_cache: std::sync::OnceLock::new(),
            frame_index: 0,
            previous_camera: None,
            jitter_px: [0.0; 2],
            reproject: Mat4::IDENTITY,
            reproject_sky: Mat4::IDENTITY,
            reset_history: true,
        }
    }

    pub fn active(&self) -> bool {
        self.mode != MetalFxMode::Off
    }

    /// Queries once whether this Metal device & OS support `MTLFXFrameInterpolator` (iOS 26+).
    #[allow(unused_variables)]
    pub fn frame_gen_supported(&self, device: &wgpu::Device) -> bool {
        *self.fg_supported_cache.get_or_init(|| {
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                let supported = unsafe {
                    device
                        .as_hal::<wgpu::hal::api::Metal>()
                        .is_some_and(|hal_dev| {
                            wgpu::hal::metal::metalfx::FrameInterpolator::is_supported(
                                hal_dev.raw_device(),
                            )
                        })
                };
                log::info!("Horizon: MetalFX Frame Interpolation supported = {supported}");
                supported
            }
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            {
                false
            }
        })
    }

    /// Invalidates MetalFX Frame Interpolation history (e.g. menu open, resize, world switch).
    pub fn reset_frame_gen(&mut self) {
        if let Some(fg) = &mut self.fg_targets {
            fg.has_prev = false;
            fg.reset_next = true;
        }
    }

    /// Computes `(render_size, output_size)` for the active preset, mode, and factor on `window`.
    pub fn sizes(&self, window: [u32; 2]) -> ([u32; 2], [u32; 2]) {
        self.preset
            .sizes_for_window_with_factor(self.mode, self.factor, window)
    }

    /// Sub-pixel jitter matrix (Halton 2, 3) for `MetalFxMode::Temporal` and camera reprojection
    /// (shared with `MTLFXFrameInterpolator`). Returns `Mat4::IDENTITY` when not Temporal, on the
    /// title screen, or when `apply_jitter` is false (shader packs supply their own projection).
    pub fn begin_frame(
        &mut self,
        window: [u32; 2],
        world_active: bool,
        apply_jitter: bool,
        view_proj: Mat4,
        camera: DVec3,
    ) -> Mat4 {
        if !world_active {
            self.previous_camera = None;
            self.jitter_px = [0.0; 2];
            self.reproject = Mat4::IDENTITY;
            self.reproject_sky = Mat4::IDENTITY;
            self.reset_history = true;
            return Mat4::IDENTITY;
        }
        let has_pack = !apply_jitter;
        let current = view_proj.as_dmat4();
        let (reproject, reproject_sky, reset) = match self.previous_camera {
            Some((prev_vp, prev_cam, prev_pack)) => {
                let delta = camera - prev_cam;
                let moved = delta.length_squared() > 100.0;
                if moved || prev_pack != has_pack {
                    (DMat4::IDENTITY, DMat4::IDENTITY, true)
                } else {
                    let current_inv = current.inverse();
                    let reproj_sky = prev_vp * current_inv;
                    // Detect abrupt camera cuts (e.g., F5 perspective flip) where screen center
                    // jumps far off-axis in a single frame.
                    let center_prev = reproj_sky * glam::DVec4::new(0.0, 0.0, 0.0, 1.0);
                    let cut = center_prev.w <= 1e-4
                        || (center_prev.x / center_prev.w).abs() > 1.5
                        || (center_prev.y / center_prev.w).abs() > 1.5;
                    if cut {
                        (DMat4::IDENTITY, DMat4::IDENTITY, true)
                    } else {
                        let reproj = prev_vp * DMat4::from_translation(delta) * current_inv;
                        (reproj, reproj_sky, false)
                    }
                }
            }
            None => (DMat4::IDENTITY, DMat4::IDENTITY, true),
        };
        self.previous_camera = Some((current, camera, has_pack));
        self.reproject = reproject.as_mat4();
        self.reproject_sky = reproject_sky.as_mat4();
        self.reset_history = reset;

        if self.mode != MetalFxMode::Temporal || !apply_jitter {
            self.jitter_px = [0.0; 2];
            return Mat4::IDENTITY;
        }
        let (render_size, output_size) = self.sizes(window);
        let ratio = (output_size[1] as f32 / render_size[1].max(1) as f32).clamp(1.0, 3.0);
        let phases = (8.0 * ratio * ratio).ceil() as u32;
        self.frame_index = self.frame_index.wrapping_add(1);
        let i = self.frame_index % phases.max(8) + 1;
        let offset = Vec2::new(halton(i, 2), halton(i, 3)) - 0.5;
        // MetalFX jitterOffset is in pixel units [-0.5, 0.5], +X right, +Y down.
        self.jitter_px = [offset.x, offset.y];
        let clip_jitter = Vec2::new(
            offset.x * 2.0 / render_size[0].max(1) as f32,
            -offset.y * 2.0 / render_size[1].max(1) as f32,
        );
        Mat4::from_translation(clip_jitter.extend(0.0))
    }

    /// Ensures internal textures and native MetalFX scaler exist for `(render_size, output_size)`,
    /// returning the `input_color_view` for the caller (e.g., `pack_post` final pass) to render into.
    pub fn ensure_input_target(
        &mut self,
        device: &wgpu::Device,
        window: [u32; 2],
    ) -> &wgpu::TextureView {
        let (input_size, output_size) = self.sizes(window);
        self.ensure_targets(device, input_size, output_size);
        &self.targets.as_ref().expect("targets").input_color_view
    }

    fn ensure_targets(&mut self, device: &wgpu::Device, input_size: [u32; 2], output_size: [u32; 2]) {
        // Use Rgba16Float for HDR or Rgba8Unorm for SDR so STORAGE_BINDING (MTLTextureUsageShaderWrite)
        // is universally supported by MetalFX on every Apple Silicon GPU.
        let color_format = if matches!(
            self.format,
            wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rgb10a2Unorm
        ) {
            wgpu::TextureFormat::Rgba16Float
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let valid = self.targets.as_ref().is_some_and(|t| {
            t.mode == self.mode
                && t.input_size == input_size
                && t.output_size == output_size
                && t.color_format == color_format
        });
        if valid {
            return;
        }
        // Drop old scaler/textures first to keep peak GPU memory low on mobile.
        self.targets = None;
        let tex = |label, size: [u32; 2], format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size[0].max(1),
                    height: size[1].max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let input_color = tex(
            "MetalFX input color",
            input_size,
            color_format,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let input_color_view = input_color.create_view(&Default::default());
        let motion = tex(
            "MetalFX motion",
            input_size,
            wgpu::TextureFormat::Rg16Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let motion_view = motion.create_view(&Default::default());
        let depth_r32 = tex(
            "MetalFX depth",
            input_size,
            wgpu::TextureFormat::R32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let depth_r32_view = depth_r32.create_view(&Default::default());
        let reactive = tex(
            "MetalFX reactive",
            input_size,
            wgpu::TextureFormat::R8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let reactive_view = reactive.create_view(&Default::default());
        let output = tex(
            "MetalFX output",
            output_size,
            color_format,
            wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let output_view = output.create_view(&Default::default());
        let output_blit_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("MetalFX output blit"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&output_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.linear),
                },
            ],
        });

        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let scaler = {
            let linear = color_format == wgpu::TextureFormat::Rgba16Float;
            let built = unsafe {
                device
                    .as_hal::<wgpu::hal::api::Metal>()
                    .and_then(|hal_dev| {
                        let raw_dev = hal_dev.raw_device();
                        let c_tex = input_color.as_hal::<wgpu::hal::api::Metal>()?;
                        let d_tex = depth_r32.as_hal::<wgpu::hal::api::Metal>()?;
                        let m_tex = motion.as_hal::<wgpu::hal::api::Metal>()?;
                        let r_tex = reactive.as_hal::<wgpu::hal::api::Metal>();
                        let o_tex = output.as_hal::<wgpu::hal::api::Metal>()?;
                        match self.mode {
                            MetalFxMode::Temporal => {
                                wgpu::hal::metal::metalfx::TemporalScaler::new(
                                    raw_dev,
                                    input_size,
                                    output_size,
                                    c_tex.raw_handle(),
                                    d_tex.raw_handle(),
                                    m_tex.raw_handle(),
                                    r_tex.as_ref().map(|t| t.raw_handle()),
                                    o_tex.raw_handle(),
                                    linear,
                                )
                                .map(NativeScaler::Temporal)
                            }
                            MetalFxMode::Spatial => {
                                wgpu::hal::metal::metalfx::SpatialScaler::new(
                                    raw_dev,
                                    input_size,
                                    output_size,
                                    c_tex.raw_handle(),
                                    o_tex.raw_handle(),
                                    linear,
                                )
                                .map(NativeScaler::Spatial)
                            }
                            MetalFxMode::Off => None,
                        }
                    })
            };
            if built.is_some() {
                log::info!(
                    "Horizon: MetalFX {:?} active ({}x{} -> {}x{})",
                    self.mode,
                    input_size[0],
                    input_size[1],
                    output_size[0],
                    output_size[1],
                );
            } else {
                log::warn!(
                    "Horizon: MetalFX {:?} unavailable for {}x{} -> {}x{}; falling back to WGSL CAS",
                    self.mode,
                    input_size[0],
                    input_size[1],
                    output_size[0],
                    output_size[1],
                );
            }
            built
        };

        self.targets = Some(Targets {
            mode: self.mode,
            input_size,
            output_size,
            color_format,
            input_color,
            input_color_view,
            motion,
            motion_view,
            depth_r32,
            depth_r32_view,
            reactive,
            reactive_view,
            output,
            output_view,
            output_blit_group,
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            scaler,
            reset_next: true,
        });
    }

    /// Runs MetalFX (Temporal or Spatial, or WGSL CAS fallback) from `src_color` (or internal
    /// `input_color` when `src_color` is `None`) and `depth_view` onto `screen`.
    /// `opaque_depth`: optional Shader Pack `depthtex1` (pre-translucent depth) for water detection.
    /// `gl_depth_flipped`: `true` for Shader Pack `depthtex0` (GL bottom-up forward-Z),
    /// `false` for Vanilla `world_depth` (top-down reverse-Z).
    #[allow(clippy::too_many_arguments, unused_variables)]
    pub fn finish(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pending_cmds: &mut Vec<wgpu::CommandBuffer>,
        src_color: Option<&wgpu::TextureView>,
        depth_view: &wgpu::TextureView,
        opaque_depth: Option<&wgpu::TextureView>,
        screen: &wgpu::TextureView,
        window: [u32; 2],
        gl_depth_flipped: bool,
    ) {
        let (input_size, output_size) = self.sizes(window);
        self.ensure_targets(device, input_size, output_size);
        let targets = self.targets.as_mut().expect("targets");

        // If caller rendered to its own color view (e.g. Vanilla world target), copy/convert
        // into `targets.input_color` so MetalFX sees exact matching pixelFormat and usage.
        if let Some(src) = src_color {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("MetalFX input copy"),
                layout: &self.blit_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(src),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.linear),
                    },
                ],
            });
            let copy_pl = if targets.color_format == wgpu::TextureFormat::Rgba16Float {
                &self.input_copy_rgba16
            } else {
                &self.input_copy_rgba8
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX input"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.input_color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(copy_pl);
            rp.set_bind_group(0, &group, &[]);
            rp.draw(0..3, 0..1);
        }

        // Run motion vector + reverse-Z depth + reactive mask pass for Temporal mode.
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let mut used_native = false;
        #[cfg(not(any(target_os = "ios", target_os = "macos")))]
        let used_native = false;

        #[cfg(any(target_os = "ios", target_os = "macos"))]
        if let Some(scaler) = &targets.scaler {
            if matches!(scaler, NativeScaler::Temporal(_)) {
                let has_opaque = opaque_depth.is_some();
                let u = MotionUniform {
                    reproject: self.reproject.to_cols_array_2d(),
                    reproject_sky: self.reproject_sky.to_cols_array_2d(),
                    size: [input_size[0] as f32, input_size[1] as f32],
                    jitter_px: self.jitter_px,
                    flags: [
                        if gl_depth_flipped { 1.0 } else { 0.0 },
                        if has_opaque { 1.0 } else { 0.0 },
                        0.0,
                        0.0,
                    ],
                };
                queue.write_buffer(&self.motion_uniform, 0, bytemuck::bytes_of(&u));
                let motion_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("MetalFX motion"),
                    layout: &self.motion_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(depth_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: self.motion_uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(
                                opaque_depth.unwrap_or(depth_view),
                            ),
                        },
                    ],
                });
                let clear = wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                };
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("MetalFX motion"),
                    color_attachments: &[
                        Some(wgpu::RenderPassColorAttachment {
                            view: &targets.motion_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: clear,
                        }),
                        Some(wgpu::RenderPassColorAttachment {
                            view: &targets.depth_r32_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: clear,
                        }),
                        Some(wgpu::RenderPassColorAttachment {
                            view: &targets.reactive_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: clear,
                        }),
                    ],
                    ..Default::default()
                });
                rp.set_pipeline(&self.motion_pipeline);
                rp.set_bind_group(0, &motion_group, &[]);
                rp.draw(0..3, 0..1);
            }

            // wgpu-core forbids mixing wgpu render passes and `as_hal_mut` on the same
            // CommandEncoder (`EncodingApi::Wgpu` vs `EncodingApi::Raw`). Finish the prior
            // encoder, encode MetalFX into a brand-new untouched CommandEncoder, and swap in
            // a fresh wgpu encoder for the present pass + HUD so GPU execution stays strictly
            // ordered in one `queue.submit` batch.
            let prev = std::mem::replace(
                encoder,
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Horizon post-MetalFX"),
                }),
            );
            pending_cmds.push(prev.finish());

            let mut mfx_enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("MetalFX"),
            });
            let reset = self.reset_history || targets.reset_next;
            targets.reset_next = false;
            let jitter_px = self.jitter_px;
            unsafe {
                mfx_enc.as_hal_mut::<wgpu::hal::api::Metal, _, _>(|hal_enc| {
                    let Some(hal_enc) = hal_enc else {
                        return;
                    };
                    hal_enc.end_pending_encoders();
                    let Some(cmd_buf) = hal_enc.raw_command_buffer() else {
                        return;
                    };
                    let Some(c_tex) = targets.input_color.as_hal::<wgpu::hal::api::Metal>() else {
                        return;
                    };
                    let Some(o_tex) = targets.output.as_hal::<wgpu::hal::api::Metal>() else {
                        return;
                    };
                    match scaler {
                        NativeScaler::Temporal(ts) => {
                            let Some(d_tex) = targets.depth_r32.as_hal::<wgpu::hal::api::Metal>()
                            else {
                                return;
                            };
                            let Some(m_tex) = targets.motion.as_hal::<wgpu::hal::api::Metal>()
                            else {
                                return;
                            };
                            let r_tex = targets.reactive.as_hal::<wgpu::hal::api::Metal>();
                            // `fs_motion` always outputs reverse-Z depth (1 near, 0 far), so
                            // `depth_reversed` is always `true` for both Vanilla and Shader Packs.
                            ts.encode(
                                cmd_buf,
                                input_size,
                                c_tex.raw_handle(),
                                d_tex.raw_handle(),
                                m_tex.raw_handle(),
                                r_tex.as_ref().map(|t| t.raw_handle()),
                                o_tex.raw_handle(),
                                jitter_px,
                                true,
                                reset,
                            );
                            used_native = true;
                        }
                        NativeScaler::Spatial(ss) => {
                            ss.encode(cmd_buf, input_size, c_tex.raw_handle(), o_tex.raw_handle());
                            used_native = true;
                        }
                    }
                });
            }
            if used_native {
                pending_cmds.push(mfx_enc.finish());
            }
        }

        // Composite upscaled MetalFX output onto the swapchain `screen` (or run WGSL CAS fallback).
        if used_native {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: screen,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.blit_pipeline);
            rp.set_bind_group(0, &targets.output_blit_group, &[]);
            rp.draw(0..3, 0..1);
        } else {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("MetalFX CAS fallback"),
                layout: &self.blit_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            src_color.unwrap_or(&targets.input_color_view),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.linear),
                    },
                ],
            });
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX CAS fallback"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: screen,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.cas_pipeline);
            rp.set_bind_group(0, &group, &[]);
            rp.draw(0..3, 0..1);
        }
    }

    fn ensure_frame_gen_targets(&mut self, device: &wgpu::Device, render_size: [u32; 2]) {
        // Cap MetalFX Frame Interpolation at 720p vertical so mobile GPUs (A14/A16) stay well
        // within a ~2-4 ms interpolation budget while real frames retain full resolution.
        let fg_size = frame_gen_size(render_size);
        let color_format = if matches!(
            self.format,
            wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rgb10a2Unorm
        ) {
            wgpu::TextureFormat::Rgba16Float
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let valid = self
            .fg_targets
            .as_ref()
            .is_some_and(|t| t.size == fg_size && t.color_format == color_format);
        if valid {
            return;
        }
        self.fg_targets = None;
        let tex = |label, format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: fg_size[0].max(1),
                    height: fg_size[1].max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let color_usage = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST;
        let h0 = tex("MetalFX FI history 0", color_format, color_usage);
        let h1 = tex("MetalFX FI history 1", color_format, color_usage);
        let h0_view = h0.create_view(&Default::default());
        let h1_view = h1.create_view(&Default::default());
        let motion = tex(
            "MetalFX FI motion",
            wgpu::TextureFormat::Rg16Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING,
        );
        let motion_view = motion.create_view(&Default::default());
        let depth_r32 = tex(
            "MetalFX FI depth",
            wgpu::TextureFormat::R32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING,
        );
        let depth_r32_view = depth_r32.create_view(&Default::default());
        let reactive = tex(
            "MetalFX FI reactive",
            wgpu::TextureFormat::R8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let reactive_view = reactive.create_view(&Default::default());
        let output = tex(
            "MetalFX FI output",
            color_format,
            wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC,
        );
        let output_view = output.create_view(&Default::default());
        let output_blit_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("MetalFX FI output blit"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&output_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.linear),
                },
            ],
        });

        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let interpolator = {
            let built = unsafe {
                device
                    .as_hal::<wgpu::hal::api::Metal>()
                    .and_then(|hal_dev| {
                        let raw_dev = hal_dev.raw_device();
                        let c_tex = h0.as_hal::<wgpu::hal::api::Metal>()?;
                        let d_tex = depth_r32.as_hal::<wgpu::hal::api::Metal>()?;
                        let m_tex = motion.as_hal::<wgpu::hal::api::Metal>()?;
                        let o_tex = output.as_hal::<wgpu::hal::api::Metal>()?;
                        wgpu::hal::metal::metalfx::FrameInterpolator::new(
                            raw_dev,
                            fg_size,
                            c_tex.raw_handle(),
                            d_tex.raw_handle(),
                            m_tex.raw_handle(),
                            o_tex.raw_handle(),
                        )
                    })
            };
            if built.is_some() {
                log::info!(
                    "Horizon: MetalFX Frame Interpolation active ({}x{})",
                    fg_size[0],
                    fg_size[1],
                );
            } else {
                log::warn!(
                    "Horizon: MetalFX Frame Interpolation unavailable for {}x{}",
                    fg_size[0],
                    fg_size[1],
                );
            }
            built
        };

        self.fg_targets = Some(FrameGenTargets {
            size: fg_size,
            color_format,
            history: [h0, h1],
            history_views: [h0_view, h1_view],
            motion,
            motion_view,
            depth_r32,
            depth_r32_view,
            reactive,
            reactive_view,
            output,
            output_view,
            output_blit_group,
            curr_idx: 0,
            has_prev: false,
            reset_next: true,
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            interpolator,
        });
    }

    /// Runs MetalFX Upscaling (if enabled) and/or Apple `MTLFXFrameInterpolator` (when `generated`
    /// and `fg_params` are provided). Writes the real frame onto `screen` and the interpolated
    /// frame onto `generated`, returning `true` when an interpolated frame was produced.
    #[allow(clippy::too_many_arguments, unused_variables)]
    pub fn finish_with_frame_gen(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pending_cmds: &mut Vec<wgpu::CommandBuffer>,
        src_color: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        opaque_depth: Option<&wgpu::TextureView>,
        screen: &wgpu::TextureView,
        generated: Option<&wgpu::TextureView>,
        fg_params: Option<FrameGenParams>,
        window: [u32; 2],
        gl_depth_flipped: bool,
    ) -> bool {
        let upscale_on = self.active();
        let Some((gen_view, fg_p)) = generated.zip(fg_params) else {
            if upscale_on {
                self.finish(
                    device,
                    queue,
                    encoder,
                    pending_cmds,
                    Some(src_color),
                    depth_view,
                    opaque_depth,
                    screen,
                    window,
                    gl_depth_flipped,
                );
            }
            return false;
        };

        // 1. Real frame: run MetalFX upscaler if enabled, else blit `src_color` directly to `screen`.
        if upscale_on {
            self.finish(
                device,
                queue,
                encoder,
                pending_cmds,
                Some(src_color),
                depth_view,
                opaque_depth,
                screen,
                window,
                gl_depth_flipped,
            );
        } else {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("MetalFX FI real copy"),
                layout: &self.blit_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(src_color),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.linear),
                    },
                ],
            });
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FI real"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: screen,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.blit_pipeline);
            rp.set_bind_group(0, &group, &[]);
            rp.draw(0..3, 0..1);
        }

        // 2. Interpolated frame: copy current world into ping-pong history + build motion/depth.
        self.ensure_frame_gen_targets(device, fg_p.render_size);
        let fg = self.fg_targets.as_mut().expect("fg_targets");
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let has_interp = fg.interpolator.is_some();
        #[cfg(not(any(target_os = "ios", target_os = "macos")))]
        let has_interp = false;
        if !has_interp {
            return false;
        }

        let curr_idx = fg.curr_idx;
        let copy_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("MetalFX FI input copy"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(src_color),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.linear),
                },
            ],
        });
        let copy_pl = if fg.color_format == wgpu::TextureFormat::Rgba16Float {
            &self.input_copy_rgba16
        } else {
            &self.input_copy_rgba8
        };
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FI history"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &fg.history_views[curr_idx],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(copy_pl);
            rp.set_bind_group(0, &copy_group, &[]);
            rp.draw(0..3, 0..1);
        }

        let has_opaque = opaque_depth.is_some();
        let u = MotionUniform {
            reproject: self.reproject.to_cols_array_2d(),
            reproject_sky: self.reproject_sky.to_cols_array_2d(),
            size: [fg.size[0] as f32, fg.size[1] as f32],
            jitter_px: self.jitter_px,
            flags: [
                if gl_depth_flipped { 1.0 } else { 0.0 },
                if has_opaque { 1.0 } else { 0.0 },
                0.0,
                0.0,
            ],
        };
        queue.write_buffer(&self.fg_motion_uniform, 0, bytemuck::bytes_of(&u));
        let motion_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("MetalFX FI motion"),
            layout: &self.motion_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(depth_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.fg_motion_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(
                        opaque_depth.unwrap_or(depth_view),
                    ),
                },
            ],
        });
        {
            let clear = wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FI motion"),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &fg.motion_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: clear,
                    }),
                    Some(wgpu::RenderPassColorAttachment {
                        view: &fg.depth_r32_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: clear,
                    }),
                    Some(wgpu::RenderPassColorAttachment {
                        view: &fg.reactive_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: clear,
                    }),
                ],
                ..Default::default()
            });
            rp.set_pipeline(&self.motion_pipeline);
            rp.set_bind_group(0, &motion_group, &[]);
            rp.draw(0..3, 0..1);
        }

        // 3. Encode `MTLFXFrameInterpolator` into a dedicated untouched CommandEncoder.
        #[allow(unused_mut)]
        let mut made = false;
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        if let Some(interp) = &fg.interpolator {
            let prev = std::mem::replace(
                encoder,
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Horizon post-MetalFX-FI"),
                }),
            );
            pending_cmds.push(prev.finish());

            let mut fi_enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("MetalFX FI"),
            });
            let prev_idx = if fg.has_prev { 1 - curr_idx } else { curr_idx };
            let reset = self.reset_history || fg.reset_next || !fg.has_prev;
            let jitter_px = self.jitter_px;
            let aspect = fg.size[0] as f32 / fg.size[1].max(1) as f32;
            let mut encoded = false;
            unsafe {
                fi_enc.as_hal_mut::<wgpu::hal::api::Metal, _, _>(|hal_enc| {
                    let Some(hal_enc) = hal_enc else {
                        return;
                    };
                    hal_enc.end_pending_encoders();
                    let Some(cmd_buf) = hal_enc.raw_command_buffer() else {
                        return;
                    };
                    let Some(c_tex) = fg.history[curr_idx].as_hal::<wgpu::hal::api::Metal>() else {
                        return;
                    };
                    let Some(p_tex) = fg.history[prev_idx].as_hal::<wgpu::hal::api::Metal>() else {
                        return;
                    };
                    let Some(d_tex) = fg.depth_r32.as_hal::<wgpu::hal::api::Metal>() else {
                        return;
                    };
                    let Some(m_tex) = fg.motion.as_hal::<wgpu::hal::api::Metal>() else {
                        return;
                    };
                    let Some(o_tex) = fg.output.as_hal::<wgpu::hal::api::Metal>() else {
                        return;
                    };
                    interp.encode(
                        cmd_buf,
                        c_tex.raw_handle(),
                        p_tex.raw_handle(),
                        d_tex.raw_handle(),
                        m_tex.raw_handle(),
                        o_tex.raw_handle(),
                        jitter_px,
                        fg_p.delta_time.clamp(0.001, 0.25),
                        fg_p.near.max(0.01),
                        fg_p.far.max(1.0),
                        fg_p.fov_degrees.clamp(10.0, 150.0),
                        aspect,
                        true,
                        reset,
                    );
                    encoded = true;
                });
            }
            if encoded {
                pending_cmds.push(fi_enc.finish());
                fg.has_prev = true;
                fg.reset_next = false;
                fg.curr_idx = 1 - curr_idx;
                made = !reset;
            }
        }

        // 4. Composite interpolated output onto `generated` (CAS-sharpened when upscaling to display).
        if made {
            let pl = if fg.size != window {
                &self.cas_pipeline
            } else {
                &self.blit_pipeline
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FI present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: gen_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(pl);
            rp.set_bind_group(0, &fg.output_blit_group, &[]);
            rp.draw(0..3, 0..1);
        }
        made
    }
}

/// Computes the working resolution for `MTLFXFrameInterpolator` (even dimensions, capped at 720p
/// height to keep mobile GPU overhead around ~2–4 ms on iPhone/iPad while preserving aspect ratio).
fn frame_gen_size(render_size: [u32; 2]) -> [u32; 2] {
    let w = render_size[0].max(64);
    let h = render_size[1].max(64);
    if h <= 720 {
        return [(w & !1).max(64), (h & !1).max(64)];
    }
    let out_h = 720u32;
    let out_w = ((w as f64 * f64::from(out_h) / h as f64).round() as u32 & !1).max(64);
    [out_w, out_h]
}

fn halton(mut index: u32, base: u32) -> f32 {
    let mut f = 1.0f32;
    let mut r = 0.0f32;
    while index > 0 {
        f /= base as f32;
        r += f * (index % base) as f32;
        index /= base;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{DVec4, Vec3};

    #[test]
    fn motion_vector_points_from_current_to_previous_pixel_in_metalfx_convention() {
        // Object at (0, 0, -10); camera turns right (yaw increases) so object moves LEFT on screen.
        // MetalFX expects mv = prev_px - curr_px, which must be POSITIVE in X (pointing right
        // back to where the object was in the previous frame).
        let size = Vec2::new(640.0, 360.0);
        let proj = Mat4::perspective_rh(70f32.to_radians(), size.x / size.y, 512.0, 0.05).as_dmat4();
        let prev_vp = proj * crate::camera::view_matrix(0.0, 0.0).as_dmat4();
        let curr_vp = proj * crate::camera::view_matrix(5.0, 0.0).as_dmat4();
        let cam = DVec3::new(0.0, 64.0, 0.0);
        // Look direction at yaw=0 is +Z in Minecraft (`view_matrix` rotates by PI - yaw).
        let block = DVec3::new(0.0, 64.0, 10.0);

        let project_uv = |vp: DMat4| -> (Vec2, f64) {
            let c = vp * (block - cam).extend(1.0);
            let ndc = c.truncate() / c.w;
            (
                Vec2::new((ndc.x * 0.5 + 0.5) as f32, (0.5 - ndc.y * 0.5) as f32),
                ndc.z,
            )
        };
        let (prev_uv, _) = project_uv(prev_vp);
        let (curr_uv, curr_z) = project_uv(curr_vp);
        let reproj = prev_vp * curr_vp.inverse();
        let curr_ndc = DVec4::new(
            f64::from(curr_uv.x) * 2.0 - 1.0,
            1.0 - f64::from(curr_uv.y) * 2.0,
            curr_z,
            1.0,
        );
        let p = reproj * curr_ndc;
        let recovered_prev = Vec2::new(
            ((p.x / p.w) * 0.5 + 0.5) as f32,
            (0.5 - (p.y / p.w) * 0.5) as f32,
        );
        let mv = (recovered_prev - curr_uv) * size;
        let expected_mv = (prev_uv - curr_uv) * size;
        assert!((mv - expected_mv).length() < 1e-3);
        assert!(
            mv.x > 10.0,
            "turning right moves object left, so prev - curr must point right (+X), got {mv:?}"
        );
    }

    #[test]
    fn sky_reprojection_ignores_camera_translation_parallax() {
        let proj = Mat4::perspective_rh(70f32.to_radians(), 16.0 / 9.0, 64.0, 0.05).as_dmat4();
        let vp = proj * crate::camera::view_matrix(0.0, 0.0).as_dmat4();
        let reproj_sky = vp * vp.inverse();
        let p = reproj_sky * DVec4::new(0.2, -0.3, 0.0, 1.0);
        let ndc = Vec2::new((p.x / p.w) as f32, (p.y / p.w) as f32);
        assert!((ndc - Vec2::new(0.2, -0.3)).length() < 1e-6);
    }

    #[test]
    fn shader_pack_gl_depth_converts_to_horizon_reverse_z_and_isolates_hand_depth() {
        let (gl_proj, _) = super::super::pack_uniforms::pack_projection(70.0, 16.0 / 9.0, 512.0);
        let rev_proj = Mat4::perspective_rh(70f32.to_radians(), 16.0 / 9.0, 512.0, 0.05);
        for dist in [0.25f32, 1.0, 10.0, 100.0, 500.0] {
            let eye = Vec3::new(0.0, 0.0, -dist).extend(1.0);
            let gl_c = gl_proj * eye;
            let d_gl = ((gl_c.z + gl_c.w) * 0.5) / gl_c.w;
            let rev_c = rev_proj * eye;
            let d_rev = rev_c.z / rev_c.w;
            assert!(
                ((1.0 - d_gl) - d_rev).abs() < 1e-5,
                "dist={dist}: 1-d_gl={} vs d_rev={d_rev}",
                1.0 - d_gl
            );
            assert!(
                d_gl > 0.565,
                "world block at dist={dist} must stay above hand depth threshold 0.565, got {d_gl}"
            );
        }
        // First-person hand uses pack_hand::projection (HAND_DEPTH = 0.125), mapping strictly <= 0.5625.
        let hand_proj = super::super::pack_hand::projection(70.0, 16.0 / 9.0, 512.0);
        for dist in [0.1f32, 0.5, 2.0, 50.0] {
            let c = hand_proj * Vec3::new(0.0, 0.0, -dist).extend(1.0);
            let d_hand = ((c.z + c.w) * 0.5) / c.w;
            assert!(
                d_hand <= 0.565,
                "hand at dist={dist} must fall inside <= 0.565 mask, got {d_hand}"
            );
        }
    }
}
