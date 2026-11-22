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

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FgInterpUniform {
    // xy: 1.0 / fg_size, z: 1.0 when history was reset (teleport/cut), w: reserved.
    params: [f32; 4],
}

/// Pipelined main-thread state machine for 2x Frame Generation (30 real -> 60 display):
/// - `Empty`: no real frame cached yet; render Frame 0 synchronously -> `Primed`.
/// - `Primed`: `real_cache[prev_idx]` holds Frame 0; render Frame 1 + `interp(0.5)` synchronously -> `ReadyReal`.
/// - `ReadyReal` (Even Tick): present `real_cache[prev_idx]` (Frame N-1) IMMEDIATELY (<0.5ms), then render
///   Frame N + `interp(N-0.5)` offscreen in the same tick -> `ReadyInterp`.
/// - `ReadyInterp` (Odd Tick): present `interp_cache` (Frame N-0.5) + fresh HUD (<0.5ms), skipping all
///   3D world and Shader Pack work -> `ReadyReal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FgState {
    Empty,
    Primed,
    ReadyReal,
    ReadyInterp,
}

/// Per-frame camera and timing inputs for iOS/Metal Frame Generation.
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
    /// Motion-vector working resolution (capped at 720p vertical for <0.5ms mobile bandwidth).
    size: [u32; 2],
    window_size: [u32; 2],
    /// Ping-pong full-window real world frames (`self.format`, hand-free for clean interpolation).
    real_cache: [wgpu::Texture; 2],
    real_cache_views: [wgpu::TextureView; 2],
    real_cache_blit_groups: [wgpu::BindGroup; 2],
    /// Lazily allocated Vanilla display buffer when first-person hand is stamped over `real_cache`.
    real_display: Option<(wgpu::Texture, wgpu::TextureView, wgpu::BindGroup)>,
    has_real_display: bool,
    /// Full-window interpolated halfway frame `N-0.5` (`self.format`).
    interp_cache: wgpu::Texture,
    interp_cache_view: wgpu::TextureView,
    interp_cache_blit_group: wgpu::BindGroup,
    /// Pre-built `fs_fg_interp` bind groups for `[curr_idx = 0, curr_idx = 1]` using FG's own motion.
    interp_groups: [wgpu::BindGroup; 2],
    /// Cached `fs_fg_interp` bind groups sharing `targets.motion` + `targets.reactive` when
    /// MetalFX Temporal Upscaling + Frame Gen are both active (avoids duplicate `fs_motion`).
    temporal_interp_groups: Option<([u32; 2], [wgpu::BindGroup; 2])>,
    motion: wgpu::Texture,
    motion_view: wgpu::TextureView,
    reactive: wgpu::Texture,
    reactive_view: wgpu::TextureView,
    /// Index in `real_cache` holding the most recently rendered real frame `N`.
    prev_idx: usize,
}

#[allow(dead_code)]
pub struct MetalFx {
    pub mode: MetalFxMode,
    pub preset: MetalFxPreset,
    pub factor: MetalFxFactor,
    format: wgpu::TextureFormat,
    motion_pipeline: wgpu::RenderPipeline,
    fg_motion_pipeline: wgpu::RenderPipeline,
    motion_layout: wgpu::BindGroupLayout,
    motion_uniform: wgpu::Buffer,
    fg_motion_uniform: wgpu::Buffer,
    dummy_obj_motion_view: wgpu::TextureView,
    fg_interp_pipeline: wgpu::RenderPipeline,
    fg_interp_layout: wgpu::BindGroupLayout,
    fg_interp_uniform: wgpu::Buffer,
    blit_pipeline: wgpu::RenderPipeline,
    cas_pipeline: wgpu::RenderPipeline,
    input_copy_rgba8: wgpu::RenderPipeline,
    input_copy_rgba16: wgpu::RenderPipeline,
    blit_layout: wgpu::BindGroupLayout,
    linear: wgpu::Sampler,
    targets: Option<Targets>,
    fg_targets: Option<FrameGenTargets>,
    fg_state: FgState,
    fi_real_count: u32,
    fi_made_count: u32,
    fi_shown_since: std::time::Instant,
    fi_made_share: Option<f32>,
    frame_index: u32,
    previous_camera: Option<(DMat4, DVec3, bool, bool)>,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
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
        let fg_motion_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("MetalFX FG motion"),
            layout: Some(&motion_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_screen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_fg_motion"),
                compilation_options: Default::default(),
                targets: &[
                    Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rg16Float,
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
        let dummy_obj_motion = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("MetalFX dummy object motion"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_obj_motion_view = dummy_obj_motion.create_view(&Default::default());

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
        let fg_interp_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("MetalFX frame-gen interp uniform"),
            size: std::mem::size_of::<FgInterpUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let fg_tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let fg_interp_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("MetalFX frame-gen interp bindings"),
            entries: &[
                fg_tex_entry(0),
                fg_tex_entry(1),
                fg_tex_entry(2),
                fg_tex_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let fg_interp_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("MetalFX frame-gen interp"),
            bind_group_layouts: &[Some(&fg_interp_layout)],
            ..Default::default()
        });
        let fg_interp_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("MetalFX frame-gen interp"),
            layout: Some(&fg_interp_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_screen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_fg_interp"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: screen_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            mode: MetalFxMode::Off,
            preset: MetalFxPreset::P540,
            factor: MetalFxFactor::X2,
            format: screen_format,
            motion_pipeline,
            fg_motion_pipeline,
            motion_layout,
            motion_uniform,
            fg_motion_uniform,
            dummy_obj_motion_view,
            fg_interp_pipeline,
            fg_interp_layout,
            fg_interp_uniform,
            blit_pipeline,
            cas_pipeline,
            input_copy_rgba8,
            input_copy_rgba16,
            blit_layout,
            linear,
            targets: None,
            fg_targets: None,
            fg_state: FgState::Empty,
            fi_real_count: 0,
            fi_made_count: 0,
            fi_shown_since: std::time::Instant::now(),
            fi_made_share: None,
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

    /// Reprojection matrix from current unjittered clip space to previous frame's clip space.
    pub fn reproject_matrix(&self) -> Mat4 {
        self.reproject
    }

    /// Whether camera history was reset on this frame (teleport, F5 cut, mode switch).
    pub fn history_reset(&self) -> bool {
        self.reset_history
    }

    /// Releases GPU targets not currently needed (e.g. on iOS low-memory warning).
    pub fn release_unused(&mut self, fg_active: bool) {
        if !self.active() {
            self.targets = None;
        }
        if !fg_active {
            self.fg_targets = None;
            self.fg_state = FgState::Empty;
        } else if let Some(fg) = &mut self.fg_targets
            && !fg.has_real_display
        {
            fg.real_display = None;
        }
    }

    /// Fast ~1ms WGSL bidirectional motion-compensated Frame Generation is supported on all
    /// Apple Metal devices (iPhone & iPad, without requiring iOS 26's heavy 18ms neural FI).
    #[allow(unused_variables)]
    pub fn frame_gen_supported(&self, device: &wgpu::Device) -> bool {
        cfg!(any(target_os = "ios", target_os = "macos"))
    }

    /// Invalidates Frame Generation history and pipelined caches (e.g. menu open, resize, toggle off).
    pub fn reset_frame_gen(&mut self) {
        self.fg_state = FgState::Empty;
        self.fi_real_count = 0;
        self.fi_made_count = 0;
        self.fi_made_share = None;
        if let Some(fg) = &mut self.fg_targets {
            fg.has_real_display = false;
        }
    }

    /// Odd display tick: `interp_cache` holds Frame `N-0.5`, ready to present immediately while
    /// skipping all 3D world and Shader Pack rendering.
    pub fn fg_should_present_odd_tick(&self) -> bool {
        self.fg_state == FgState::ReadyInterp && self.fg_targets.is_some()
    }

    /// Even display tick (steady state): `real_cache[prev_idx]` holds Frame `N-1`, ready to
    /// present IMMEDIATELY (<0.5ms) BEFORE rendering Frame `N` + `interp(N-0.5)` offscreen.
    pub fn fg_has_pipelined_real(&self) -> bool {
        self.fg_state == FgState::ReadyReal && self.fg_targets.is_some()
    }

    /// Whether Frame Generation targets exist for the current window size (safe fallback view for
    /// offscreen pipelined ticks so `surface_frame` is never referenced after presentation).
    pub fn fg_fallback_screen_view(&self) -> Option<wgpu::TextureView> {
        self.fg_targets.as_ref().map(|fg| fg.interp_cache_view.clone())
    }

    /// Returns `(real_display_or_cache_view, interp_cache_view)` for offscreen post-world overlays
    /// such as `entity_outline` in Step B.
    pub fn offscreen_world_targets(&self) -> Option<(wgpu::TextureView, wgpu::TextureView)> {
        let fg = self.fg_targets.as_ref()?;
        let real = if fg.has_real_display
            && let Some((_, view, _)) = &fg.real_display
        {
            view.clone()
        } else {
            fg.real_cache_views[fg.prev_idx].clone()
        };
        Some((real, fg.interp_cache_view.clone()))
    }

    /// Lazily allocates `real_display`, copies `real_cache[prev_idx]` into it, and returns
    /// `(interp_cache_view, real_display_view)` so Vanilla first-person hand can be stamped onto
    /// both without polluting the hand-free `real_cache` used for the next frame's interpolation.
    pub fn prepare_vanilla_hand_targets(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Option<(wgpu::TextureView, wgpu::TextureView)> {
        let fg = self.fg_targets.as_mut()?;
        if fg.real_display.is_none() {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("MetalFX FG real display"),
                size: wgpu::Extent3d {
                    width: fg.window_size[0].max(1),
                    height: fg.window_size[1].max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            let blit_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("MetalFX FG real display blit"),
                layout: &self.blit_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.linear),
                    },
                ],
            });
            fg.real_display = Some((tex, view, blit_group));
        }
        let (_, real_display_view, _) = fg.real_display.as_ref().expect("allocated above");
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("MetalFX FG real display copy"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: real_display_view,
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
        rp.set_bind_group(0, &fg.real_cache_blit_groups[fg.prev_idx], &[]);
        rp.draw(0..3, 0..1);
        drop(rp);
        fg.has_real_display = true;
        Some((fg.interp_cache_view.clone(), real_display_view.clone()))
    }

    /// Clears `has_real_display` when Shader Packs draw the hand (or hand is hidden), so Step A
    /// blits directly from `real_cache[prev_idx]` with zero extra copy.
    pub fn clear_vanilla_hand_display(&mut self) {
        if let Some(fg) = &mut self.fg_targets {
            fg.has_real_display = false;
            fg.real_display = None;
        }
    }

    /// Share of real frames that produced an interpolated frame (for F3 readout).
    pub fn frame_gen_share(&self) -> Option<f32> {
        self.fi_made_share
    }

    /// Odd Tick fast path: blits `interp_cache` (Frame `N-0.5`) onto `screen`, runs optional
    /// inline `after` draws (e.g. screen vignette/tint) inside the same TBDR render pass to avoid
    /// an extra full-resolution swapchain Load+Store, transitions `ReadyInterp -> ReadyReal`, and
    /// records a generated tick.
    pub fn blit_pipelined_interp(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        screen: &wgpu::TextureView,
        after: impl FnOnce(&mut wgpu::RenderPass<'_>),
    ) -> bool {
        if self.fg_state != FgState::ReadyInterp {
            return false;
        }
        let Some(fg) = &self.fg_targets else {
            self.fg_state = FgState::Empty;
            return false;
        };
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FG odd present"),
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
            rp.set_bind_group(0, &fg.interp_cache_blit_group, &[]);
            rp.draw(0..3, 0..1);
            after(&mut rp);
        }
        self.fg_state = FgState::ReadyReal;
        self.record_fi_tick(true);
        true
    }

    /// Even Tick Step A: blits cached real Frame `N-1` onto `screen` IMMEDIATELY (<0.5ms GPU) so
    /// the current VSync lands right on time before Step B renders Frame `N` + `interp(N-0.5)`.
    /// Runs optional inline `after` draws (screen effects) in the same TBDR render pass.
    pub fn blit_pipelined_real(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        screen: &wgpu::TextureView,
        after: impl FnOnce(&mut wgpu::RenderPass<'_>),
    ) -> bool {
        if self.fg_state != FgState::ReadyReal {
            return false;
        }
        let Some(fg) = &self.fg_targets else {
            self.fg_state = FgState::Empty;
            return false;
        };
        let group = if fg.has_real_display
            && let Some((_, _, bg)) = &fg.real_display
        {
            bg
        } else {
            &fg.real_cache_blit_groups[fg.prev_idx]
        };
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FG even real present"),
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
            rp.set_bind_group(0, group, &[]);
            rp.draw(0..3, 0..1);
            after(&mut rp);
        }
        self.record_fi_tick(false);
        true
    }

    fn record_fi_tick(&mut self, made: bool) {
        if made {
            self.fi_made_count += 1;
        } else {
            self.fi_real_count += 1;
        }
        let now = std::time::Instant::now();
        if now.duration_since(self.fi_shown_since) >= std::time::Duration::from_millis(500) {
            if self.fi_real_count > 0 {
                self.fi_made_share = Some(self.fi_made_count as f32 / self.fi_real_count as f32);
            }
            self.fi_real_count = 0;
            self.fi_made_count = 0;
            self.fi_shown_since = now;
        }
    }

    /// Computes `(render_size, output_size)` for the active preset, mode, and factor on `window`.
    pub fn sizes(&self, window: [u32; 2]) -> ([u32; 2], [u32; 2]) {
        self.preset
            .sizes_for_window_with_factor(self.mode, self.factor, window)
    }

    /// Sub-pixel jitter matrix (Halton 2, 3) for `MetalFxMode::Temporal` and camera reprojection
    /// (shared with Frame Generation). Returns `Mat4::IDENTITY` when not Temporal, on the
    /// title screen, or when `apply_jitter` is false (shader packs supply their own projection).
    pub fn begin_frame(
        &mut self,
        window: [u32; 2],
        world_active: bool,
        apply_jitter: bool,
        first_person: bool,
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
            Some((prev_vp, prev_cam, prev_pack, prev_fp)) => {
                let delta = camera - prev_cam;
                let moved = delta.length_squared() > 100.0;
                if moved || prev_pack != has_pack || prev_fp != first_person {
                    (DMat4::IDENTITY, DMat4::IDENTITY, true)
                } else {
                    let current_inv = current.inverse();
                    let reproj_sky = prev_vp * current_inv;
                    // Detect abrupt camera cuts (e.g., 3rd-person back -> front F5 180° flip)
                    // without tripping on fast touchscreen camera swipes at 30 real FPS.
                    let center_prev = reproj_sky * glam::DVec4::new(0.0, 0.0, 0.0, 1.0);
                    let cut = center_prev.w <= 1e-4
                        || (center_prev.x / center_prev.w).abs() > 2.5
                        || (center_prev.y / center_prev.w).abs() > 2.5;
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
        self.previous_camera = Some((current, camera, has_pack, first_person));
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
        if let Some(fg) = &mut self.fg_targets {
            fg.temporal_interp_groups = None;
        }
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
    /// `object_motion`: optional per-entity motion vectors (`ObjectMotion`) for 3rd-person player/mobs.
    /// `gl_depth_flipped`: `true` for Shader Pack `depthtex0` (GL bottom-up forward-Z),
    /// `false` for Vanilla `world_depth` (top-down reverse-Z).
    /// Returns `true` when Temporal `fs_motion` populated `targets.motion_view` & `targets.reactive_view`.
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
        object_motion: Option<&wgpu::TextureView>,
        screen: &wgpu::TextureView,
        window: [u32; 2],
        gl_depth_flipped: bool,
        mut timing: Option<&mut super::pack_timing::PassTiming>,
    ) -> bool {
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
                timestamp_writes: timing.as_deref_mut().and_then(|t| t.writes("MetalFX input")),
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
        let mut ran_temporal_motion = false;
        #[cfg(not(any(target_os = "ios", target_os = "macos")))]
        let ran_temporal_motion = false;

        #[cfg(any(target_os = "ios", target_os = "macos"))]
        if let Some(scaler) = &targets.scaler {
            if matches!(scaler, NativeScaler::Temporal(_)) {
                let has_opaque = opaque_depth.is_some();
                let has_obj = object_motion.is_some();
                let u = MotionUniform {
                    reproject: self.reproject.to_cols_array_2d(),
                    reproject_sky: self.reproject_sky.to_cols_array_2d(),
                    size: [input_size[0] as f32, input_size[1] as f32],
                    jitter_px: self.jitter_px,
                    flags: [
                        if gl_depth_flipped { 1.0 } else { 0.0 },
                        if has_opaque { 1.0 } else { 0.0 },
                        if has_obj { 1.0 } else { 0.0 },
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
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(
                                object_motion.unwrap_or(&self.dummy_obj_motion_view),
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
                    timestamp_writes: timing.as_deref_mut().and_then(|t| t.writes("MetalFX motion")),
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
                ran_temporal_motion = true;
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
                timestamp_writes: timing.as_deref_mut().and_then(|t| t.writes("MetalFX present")),
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
                timestamp_writes: timing.as_deref_mut().and_then(|t| t.writes("MetalFX CAS")),
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
        ran_temporal_motion
    }

    fn ensure_frame_gen_targets(
        &mut self,
        device: &wgpu::Device,
        render_size: [u32; 2],
        window: [u32; 2],
    ) {
        // Motion-vector pass runs at <= 720p vertical (~0.4 ms on A14/A16); cached real frames
        // and the interpolated output stay at full window resolution in `self.format` so there is
        // zero color-space conversion mismatch and zero flicker.
        let fg_size = frame_gen_size(render_size);
        let valid = self
            .fg_targets
            .as_ref()
            .is_some_and(|t| t.size == fg_size && t.window_size == window);
        if valid {
            return;
        }
        self.fg_targets = None;
        self.fg_state = FgState::Empty;
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
        let rt_usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let rc0 = tex("MetalFX FG real 0", window, self.format, rt_usage);
        let rc1 = tex("MetalFX FG real 1", window, self.format, rt_usage);
        let rc0_view = rc0.create_view(&Default::default());
        let rc1_view = rc1.create_view(&Default::default());
        let make_blit_group = |label, view: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.blit_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.linear),
                    },
                ],
            })
        };
        let rc0_blit = make_blit_group("MetalFX FG real 0 blit", &rc0_view);
        let rc1_blit = make_blit_group("MetalFX FG real 1 blit", &rc1_view);

        let interp_cache = tex("MetalFX FG interp", window, self.format, rt_usage);
        let interp_cache_view = interp_cache.create_view(&Default::default());
        let interp_cache_blit_group =
            make_blit_group("MetalFX FG interp blit", &interp_cache_view);

        let motion = tex(
            "MetalFX FG motion",
            fg_size,
            wgpu::TextureFormat::Rg16Float,
            rt_usage,
        );
        let motion_view = motion.create_view(&Default::default());
        let reactive = tex(
            "MetalFX FG reactive",
            fg_size,
            wgpu::TextureFormat::R8Unorm,
            rt_usage,
        );
        let reactive_view = reactive.create_view(&Default::default());

        let make_interp_group = |label,
                                 curr: &wgpu::TextureView,
                                 prev: &wgpu::TextureView,
                                 m_view: &wgpu::TextureView,
                                 r_view: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.fg_interp_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(curr),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(prev),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(m_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(r_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Sampler(&self.linear),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: self.fg_interp_uniform.as_entire_binding(),
                    },
                ],
            })
        };
        let interp_group_0 = make_interp_group(
            "MetalFX FG interp curr=0",
            &rc0_view,
            &rc1_view,
            &motion_view,
            &reactive_view,
        );
        let interp_group_1 = make_interp_group(
            "MetalFX FG interp curr=1",
            &rc1_view,
            &rc0_view,
            &motion_view,
            &reactive_view,
        );

        log::info!(
            "Horizon: pipelined GPU Frame Generation active (motion {}x{}, display {}x{})",
            fg_size[0],
            fg_size[1],
            window[0],
            window[1],
        );

        self.fg_targets = Some(FrameGenTargets {
            size: fg_size,
            window_size: window,
            real_cache: [rc0, rc1],
            real_cache_views: [rc0_view, rc1_view],
            real_cache_blit_groups: [rc0_blit, rc1_blit],
            real_display: None,
            has_real_display: false,
            interp_cache,
            interp_cache_view,
            interp_cache_blit_group,
            interp_groups: [interp_group_0, interp_group_1],
            temporal_interp_groups: None,
            motion,
            motion_view,
            reactive,
            reactive_view,
            prev_idx: 0,
        });
    }

    /// Ensures Frame Generation targets exist and returns the destination `real_cache[curr_idx]`
    /// view so Vanilla `world_post.finish` (TAA/FXAA) can render directly into it with zero extra
    /// blit when MetalFX Upscaling is OFF and Frame Generation is ON.
    pub fn begin_frame_gen_real_target(
        &mut self,
        device: &wgpu::Device,
        render_size: [u32; 2],
        window: [u32; 2],
    ) -> wgpu::TextureView {
        self.ensure_frame_gen_targets(device, render_size, window);
        let curr_idx = if self.fg_state == FgState::Empty {
            0
        } else {
            1 - self.fg_targets.as_ref().expect("fg_targets").prev_idx
        };
        self.fg_targets
            .as_ref()
            .expect("fg_targets")
            .real_cache_views[curr_idx]
            .clone()
    }

    /// Runs MetalFX Upscaling (if enabled) and pipelined ~1ms WGSL motion-compensated Frame
    /// Generation (no background `Presenter` thread and no slow 18ms neural FI):
    /// - When `src_color` is `None` and MetalFX Upscaling is OFF, caller already rendered the
    ///   resolved real frame into `begin_frame_gen_real_target(...)` (Vanilla TAA/FXAA path).
    /// - Returns `true` when `interp_cache` holds a newly generated halfway frame `N-0.5`.
    #[allow(clippy::too_many_arguments, unused_variables)]
    pub fn finish_with_frame_gen(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pending_cmds: &mut Vec<wgpu::CommandBuffer>,
        src_color: Option<&wgpu::TextureView>,
        depth_view: &wgpu::TextureView,
        opaque_depth: Option<&wgpu::TextureView>,
        object_motion: Option<&wgpu::TextureView>,
        screen: Option<&wgpu::TextureView>,
        fg_params: Option<FrameGenParams>,
        window: [u32; 2],
        gl_depth_flipped: bool,
        mut timing: Option<&mut super::pack_timing::PassTiming>,
    ) -> bool {
        let upscale_on = self.active();
        let Some(fg_p) = fg_params else {
            self.fg_state = FgState::Empty;
            if upscale_on && let Some(screen) = screen {
                self.finish(
                    device,
                    queue,
                    encoder,
                    pending_cmds,
                    src_color,
                    depth_view,
                    opaque_depth,
                    object_motion,
                    screen,
                    window,
                    gl_depth_flipped,
                    timing,
                );
            }
            return false;
        };

        self.ensure_frame_gen_targets(device, fg_p.render_size, window);
        let curr_idx = if self.fg_state == FgState::Empty {
            0
        } else {
            1 - self.fg_targets.as_ref().expect("fg_targets").prev_idx
        };

        // 1. Render full-resolution real Frame `N` into `real_cache[curr_idx]`.
        let real_view = self
            .fg_targets
            .as_ref()
            .expect("fg_targets")
            .real_cache_views[curr_idx]
            .clone();
        let mut ran_temporal_motion = false;
        if upscale_on {
            ran_temporal_motion = self.finish(
                device,
                queue,
                encoder,
                pending_cmds,
                src_color,
                depth_view,
                opaque_depth,
                object_motion,
                &real_view,
                window,
                gl_depth_flipped,
                timing.as_deref_mut(),
            );
        } else if let Some(src) = src_color {
            self.blit_view(
                device,
                encoder,
                src,
                &real_view,
                "MetalFX FG real cache",
            );
        }

        // Frame 0 warmup: no previous frame yet, so display Frame 0 directly and prime history.
        if self.fg_state == FgState::Empty {
            let fg = self.fg_targets.as_mut().expect("fg_targets");
            fg.prev_idx = curr_idx;
            fg.has_real_display = false;
            if let Some(screen) = screen {
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("MetalFX FG prime present"),
                    timestamp_writes: timing.as_deref_mut().and_then(|t| t.writes("FG prime")),
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
                rp.set_bind_group(0, &fg.real_cache_blit_groups[curr_idx], &[]);
                rp.draw(0..3, 0..1);
            }
            self.fg_state = FgState::Primed;
            self.record_fi_tick(false);
            return false;
        }

        // 2. Build screen-space camera + per-entity motion vectors & reactive mask (~0.3 ms).
        // When MetalFX Temporal already ran `fs_motion` this frame, reuse its `motion_view` and
        // `reactive_view` directly and skip running a duplicate motion pass!
        let use_temporal_motion = ran_temporal_motion && self.targets.is_some();
        let motion_size = if use_temporal_motion {
            let targets = self.targets.as_ref().expect("targets");
            let t_size = targets.input_size;
            let fg = self.fg_targets.as_mut().expect("fg_targets");
            if fg
                .temporal_interp_groups
                .as_ref()
                .is_none_or(|(s, _)| *s != t_size)
            {
                let make_group = |label, curr: &wgpu::TextureView, prev: &wgpu::TextureView| {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some(label),
                        layout: &self.fg_interp_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(curr),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(prev),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(&targets.motion_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(
                                    &targets.reactive_view,
                                ),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::Sampler(&self.linear),
                            },
                            wgpu::BindGroupEntry {
                                binding: 5,
                                resource: self.fg_interp_uniform.as_entire_binding(),
                            },
                        ],
                    })
                };
                let g0 = make_group(
                    "MetalFX FG temporal interp curr=0",
                    &fg.real_cache_views[0],
                    &fg.real_cache_views[1],
                );
                let g1 = make_group(
                    "MetalFX FG temporal interp curr=1",
                    &fg.real_cache_views[1],
                    &fg.real_cache_views[0],
                );
                fg.temporal_interp_groups = Some((t_size, [g0, g1]));
            }
            t_size
        } else {
            let fg_size = self.fg_targets.as_ref().expect("fg_targets").size;
            let has_opaque = opaque_depth.is_some();
            let has_obj = object_motion.is_some();
            let u = MotionUniform {
                reproject: self.reproject.to_cols_array_2d(),
                reproject_sky: self.reproject_sky.to_cols_array_2d(),
                size: [fg_size[0] as f32, fg_size[1] as f32],
                jitter_px: self.jitter_px,
                flags: [
                    if gl_depth_flipped { 1.0 } else { 0.0 },
                    if has_opaque { 1.0 } else { 0.0 },
                    if has_obj { 1.0 } else { 0.0 },
                    0.0,
                ],
            };
            queue.write_buffer(&self.fg_motion_uniform, 0, bytemuck::bytes_of(&u));
            let motion_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("MetalFX FG motion"),
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
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(
                            object_motion.unwrap_or(&self.dummy_obj_motion_view),
                        ),
                    },
                ],
            });
            let fg = self.fg_targets.as_ref().expect("fg_targets");
            let clear = wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FG motion"),
                timestamp_writes: timing.as_deref_mut().and_then(|t| t.writes("FG motion")),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &fg.motion_view,
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
            rp.set_pipeline(&self.fg_motion_pipeline);
            rp.set_bind_group(0, &motion_group, &[]);
            rp.draw(0..3, 0..1);
            fg_size
        };

        // 3. Single-pass bidirectional motion-compensated interpolation -> `interp_cache` (~0.6 ms).
        let interp_u = FgInterpUniform {
            params: [
                1.0 / motion_size[0].max(1) as f32,
                1.0 / motion_size[1].max(1) as f32,
                if self.reset_history { 1.0 } else { 0.0 },
                0.0,
            ],
        };
        queue.write_buffer(&self.fg_interp_uniform, 0, bytemuck::bytes_of(&interp_u));
        {
            let fg = self.fg_targets.as_ref().expect("fg_targets");
            let interp_group = if use_temporal_motion
                && let Some((_, groups)) = &fg.temporal_interp_groups
            {
                &groups[curr_idx]
            } else {
                &fg.interp_groups[curr_idx]
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FG interpolate"),
                timestamp_writes: timing.as_deref_mut().and_then(|t| t.writes("FG interp")),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &fg.interp_cache_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.fg_interp_pipeline);
            rp.set_bind_group(0, interp_group, &[]);
            rp.draw(0..3, 0..1);
        }

        // 4. Advance ping-pong index & state machine.
        let fg = self.fg_targets.as_mut().expect("fg_targets");
        fg.prev_idx = curr_idx;
        fg.has_real_display = false;
        if let Some(screen) = screen {
            // Warmup Tick 1 (or synchronous screenshot): display interpolated Frame `N-0.5` now;
            // next tick enters pipelined steady state (`ReadyReal`).
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MetalFX FG sync present"),
                timestamp_writes: timing.and_then(|t| t.writes("FG present")),
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
            rp.set_bind_group(0, &fg.interp_cache_blit_group, &[]);
            rp.draw(0..3, 0..1);
            self.fg_state = FgState::ReadyReal;
            self.record_fi_tick(true);
        } else {
            // Pipelined Even Tick Step B: Step A already presented Frame `N-1`, and `interp_cache`
            // now holds Frame `N-0.5` for the upcoming Odd Tick.
            self.fg_state = FgState::ReadyInterp;
        }
        true
    }

    fn blit_view(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        dst: &wgpu::TextureView,
        label: &str,
    ) {
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
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
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
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

    #[test]
    fn metalfx_motion_wgsl_validates() {
        let module = naga::front::wgsl::parse_str(include_str!("metalfx_motion.wgsl")).unwrap();
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .unwrap();
    }
}

