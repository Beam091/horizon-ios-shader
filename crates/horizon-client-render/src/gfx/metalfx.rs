//! Apple MetalFX hardware upscaling (`MTLFXTemporalScaler` and `MTLFXSpatialScaler`)
//! with automatic x2 vertical resolution tiers (`360p->720p`, `480p->960p`, `540p->1080p`,
//! `720p->1440p/Max`) and a WGSL CAS sharpening fallback for older or non-Apple GPUs.
#![cfg_attr(any(target_os = "ios", target_os = "macos"), allow(unsafe_code))]

use bytemuck::{Pod, Zeroable};
use glam::{DMat4, DVec3, Mat4, Vec2};

use crate::graphics::{MetalFxMode, MetalFxPreset};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MotionUniform {
    reproject: [[f32; 4]; 4],
    size: [f32; 2],
    flip_y_and_gl_depth: f32,
    _pad: f32,
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
    /// `input_size` R32Float screen-aligned depth (for `MTLFXTemporalScaler`).
    depth_r32: wgpu::Texture,
    depth_r32_view: wgpu::TextureView,
    /// `output_size` Rgba16Float/Rgba8Unorm upscaled picture (`STORAGE_BINDING` required by MetalFX).
    output: wgpu::Texture,
    output_view: wgpu::TextureView,
    output_blit_group: wgpu::BindGroup,
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    scaler: Option<NativeScaler>,
    reset_next: bool,
}

#[allow(dead_code)]
pub struct MetalFx {
    pub mode: MetalFxMode,
    pub preset: MetalFxPreset,
    format: wgpu::TextureFormat,
    motion_pipeline: wgpu::RenderPipeline,
    motion_layout: wgpu::BindGroupLayout,
    motion_uniform: wgpu::Buffer,
    blit_pipeline: wgpu::RenderPipeline,
    cas_pipeline: wgpu::RenderPipeline,
    input_copy_rgba8: wgpu::RenderPipeline,
    input_copy_rgba16: wgpu::RenderPipeline,
    blit_layout: wgpu::BindGroupLayout,
    linear: wgpu::Sampler,
    targets: Option<Targets>,
    frame_index: u32,
    previous_camera: Option<(DMat4, DVec3)>,
    jitter_px: [f32; 2],
    reproject: Mat4,
    reset_history: bool,
    logged_backend: bool,
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

        Self {
            mode: MetalFxMode::Off,
            preset: MetalFxPreset::P540,
            format: screen_format,
            motion_pipeline,
            motion_layout,
            motion_uniform,
            blit_pipeline,
            cas_pipeline,
            input_copy_rgba8,
            input_copy_rgba16,
            blit_layout,
            linear,
            targets: None,
            frame_index: 0,
            previous_camera: None,
            jitter_px: [0.0; 2],
            reproject: Mat4::IDENTITY,
            reset_history: true,
            logged_backend: false,
        }
    }

    pub fn active(&self) -> bool {
        self.mode != MetalFxMode::Off
    }

    /// Computes `(render_size, output_size)` for the active x2 preset on `window`.
    pub fn sizes(&self, window: [u32; 2]) -> ([u32; 2], [u32; 2]) {
        self.preset.sizes_for_window(window)
    }

    /// Sub-pixel jitter matrix (Halton 2, 3) for `MetalFxMode::Temporal` and camera reprojection.
    /// Returns `Mat4::IDENTITY` when MetalFX is off, spatial, on the title screen, or when
    /// `apply_jitter` is false (shader packs supply their own projection matrix).
    pub fn begin_frame(
        &mut self,
        window: [u32; 2],
        world_active: bool,
        apply_jitter: bool,
        view_proj: Mat4,
        camera: DVec3,
    ) -> Mat4 {
        if !world_active || self.mode == MetalFxMode::Off {
            self.previous_camera = None;
            self.jitter_px = [0.0; 2];
            self.reset_history = true;
            return Mat4::IDENTITY;
        }
        let current = view_proj.as_dmat4();
        let (reproject, reset) = match self.previous_camera {
            Some((prev_vp, prev_cam)) => {
                let delta = camera - prev_cam;
                let moved = delta.length_squared() > 100.0;
                if moved {
                    (DMat4::IDENTITY, true)
                } else {
                    (
                        prev_vp * DMat4::from_translation(delta) * current.inverse(),
                        false,
                    )
                }
            }
            None => (DMat4::IDENTITY, true),
        };
        self.previous_camera = Some((current, camera));
        self.reproject = reproject.as_mat4();
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
            if !self.logged_backend {
                self.logged_backend = true;
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
                    log::info!(
                        "Horizon: MetalFX hardware unavailable on this device; using WGSL CAS 2x upscale ({}x{} -> {}x{})",
                        input_size[0],
                        input_size[1],
                        output_size[0],
                        output_size[1],
                    );
                }
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
    /// `gl_depth_flipped`: `true` for Shader Pack `depthtex0` (GL bottom-up forward-Z),
    /// `false` for Vanilla `world_depth` (top-down reverse-Z).
    #[allow(clippy::too_many_arguments, unused_variables)]
    pub fn finish(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        src_color: Option<&wgpu::TextureView>,
        depth_view: &wgpu::TextureView,
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

        // Run motion vector + screen-aligned depth pass for Temporal mode.
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let mut used_native = false;
        #[cfg(not(any(target_os = "ios", target_os = "macos")))]
        let used_native = false;

        #[cfg(any(target_os = "ios", target_os = "macos"))]
        if let Some(scaler) = &targets.scaler {
            if matches!(scaler, NativeScaler::Temporal(_)) {
                let u = MotionUniform {
                    reproject: self.reproject.to_cols_array_2d(),
                    size: [input_size[0] as f32, input_size[1] as f32],
                    flip_y_and_gl_depth: if gl_depth_flipped { 1.0 } else { 0.0 },
                    _pad: 0.0,
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
                    ],
                    ..Default::default()
                });
                rp.set_pipeline(&self.motion_pipeline);
                rp.set_bind_group(0, &motion_group, &[]);
                rp.draw(0..3, 0..1);
            }

            let reset = self.reset_history || targets.reset_next;
            targets.reset_next = false;
            let jitter_px = self.jitter_px;
            unsafe {
                encoder.as_hal_mut::<wgpu::hal::api::Metal, _, _>(|hal_enc| {
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
                            ts.encode(
                                cmd_buf,
                                c_tex.raw_handle(),
                                d_tex.raw_handle(),
                                m_tex.raw_handle(),
                                o_tex.raw_handle(),
                                jitter_px,
                                !gl_depth_flipped,
                                reset,
                            );
                            used_native = true;
                        }
                        NativeScaler::Spatial(ss) => {
                            ss.encode(cmd_buf, c_tex.raw_handle(), o_tex.raw_handle());
                            used_native = true;
                        }
                    }
                });
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
