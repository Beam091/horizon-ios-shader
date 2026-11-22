//! Dynamic Apple MetalFX bridge (`MTLFXTemporalScaler` and `MTLFXSpatialScaler`).
//! Loads `MetalFX.framework` lazily via `dlopen` so builds require no extra link flags
//! and older devices / iOS 15 gracefully return `None`.

use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, ProtocolObject},
};
use objc2_metal::{MTLCommandBuffer, MTLDevice, MTLTexture};

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
}

const RTLD_LAZY: c_int = 0x1;

fn ensure_loaded() {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let path = c"/System/Library/Frameworks/MetalFX.framework/MetalFX";
        unsafe {
            let _ = dlopen(path.as_ptr(), RTLD_LAZY);
        }
    });
}

/// Hardware `MTLFXTemporalScaler` wrapper.
#[derive(Debug)]
pub struct TemporalScaler {
    raw: Retained<AnyObject>,
    reactive_enabled: bool,
}

// SAFETY: MetalFX scalers are only encoded on one command buffer at a time while
// `Renderer` holds `&mut self`.
unsafe impl Send for TemporalScaler {}
unsafe impl Sync for TemporalScaler {}

impl TemporalScaler {
    pub fn is_supported(device: &ProtocolObject<dyn MTLDevice>) -> bool {
        ensure_loaded();
        let Some(cls) = AnyClass::get(c"MTLFXTemporalScalerDescriptor") else {
            return false;
        };
        unsafe { msg_send![cls, supportsDevice: device] }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        input_size: [u32; 2],
        output_size: [u32; 2],
        color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        reactive: Option<&ProtocolObject<dyn MTLTexture>>,
        output: &ProtocolObject<dyn MTLTexture>,
        auto_exposure: bool,
    ) -> Option<Self> {
        ensure_loaded();
        let cls = AnyClass::get(c"MTLFXTemporalScalerDescriptor")?;
        let supported: bool = unsafe { msg_send![cls, supportsDevice: device] };
        if !supported {
            return None;
        }
        let desc: Option<Retained<AnyObject>> = unsafe { msg_send![cls, new] };
        let desc = desc?;
        let mut reactive_enabled = false;
        unsafe {
            let _: () = msg_send![&*desc, setInputWidth: input_size[0] as usize];
            let _: () = msg_send![&*desc, setInputHeight: input_size[1] as usize];
            let _: () = msg_send![&*desc, setOutputWidth: output_size[0] as usize];
            let _: () = msg_send![&*desc, setOutputHeight: output_size[1] as usize];
            let _: () = msg_send![&*desc, setColorTextureFormat: color.pixelFormat().0];
            let _: () = msg_send![&*desc, setDepthTextureFormat: depth.pixelFormat().0];
            let _: () = msg_send![&*desc, setMotionTextureFormat: motion.pixelFormat().0];
            let _: () = msg_send![&*desc, setOutputTextureFormat: output.pixelFormat().0];
            let _: () = msg_send![&*desc, setAutoExposureEnabled: auto_exposure];
            // iOS 17.4+ / macOS 14.4+: enable reactive mask to prevent warping on translucent
            // water, first-person hand, and shader-pack post-processed pixels.
            if let Some(r_tex) = reactive {
                let supports_reactive: bool = msg_send![
                    cls,
                    instancesRespondToSelector: objc2::sel!(setReactiveMaskTextureEnabled:)
                ];
                if supports_reactive {
                    let _: () = msg_send![&*desc, setReactiveMaskTextureEnabled: true];
                    let _: () =
                        msg_send![&*desc, setReactiveMaskTextureFormat: r_tex.pixelFormat().0];
                    reactive_enabled = true;
                }
            }
        }
        let scaler: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*desc, newTemporalScalerWithDevice: device] };
        let raw = scaler?;
        // Verify our textures satisfy every MTLTextureUsage bit required by MetalFX.
        unsafe {
            let need_c: usize = msg_send![&*raw, colorTextureUsage];
            let need_d: usize = msg_send![&*raw, depthTextureUsage];
            let need_m: usize = msg_send![&*raw, motionTextureUsage];
            let need_o: usize = msg_send![&*raw, outputTextureUsage];
            if (color.usage().0 & need_c) != need_c
                || (depth.usage().0 & need_d) != need_d
                || (motion.usage().0 & need_m) != need_m
                || (output.usage().0 & need_o) != need_o
            {
                return None;
            }
            if reactive_enabled {
                if let Some(r_tex) = reactive {
                    let need_r: usize = msg_send![&*raw, reactiveTextureUsage];
                    if (r_tex.usage().0 & need_r) != need_r {
                        return None;
                    }
                }
            }
        }
        Some(Self {
            raw,
            reactive_enabled,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        cmd_buf: &ProtocolObject<dyn MTLCommandBuffer>,
        input_size: [u32; 2],
        color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        reactive: Option<&ProtocolObject<dyn MTLTexture>>,
        output: &ProtocolObject<dyn MTLTexture>,
        jitter_px: [f32; 2],
        depth_reversed: bool,
        reset: bool,
    ) {
        unsafe {
            let _: () = msg_send![&*self.raw, setColorTexture: color];
            let _: () = msg_send![&*self.raw, setDepthTexture: depth];
            let _: () = msg_send![&*self.raw, setMotionTexture: motion];
            if self.reactive_enabled {
                if let Some(r_tex) = reactive {
                    let _: () = msg_send![&*self.raw, setReactiveMaskTexture: r_tex];
                }
            }
            let _: () = msg_send![&*self.raw, setOutputTexture: output];
            let _: () = msg_send![&*self.raw, setInputContentWidth: input_size[0] as usize];
            let _: () = msg_send![&*self.raw, setInputContentHeight: input_size[1] as usize];
            let _: () = msg_send![&*self.raw, setPreExposure: 1.0f32];
            let _: () = msg_send![&*self.raw, setJitterOffsetX: jitter_px[0]];
            let _: () = msg_send![&*self.raw, setJitterOffsetY: jitter_px[1]];
            let _: () = msg_send![&*self.raw, setMotionVectorScaleX: 1.0f32];
            let _: () = msg_send![&*self.raw, setMotionVectorScaleY: 1.0f32];
            let _: () = msg_send![&*self.raw, setDepthReversed: depth_reversed];
            let _: () = msg_send![&*self.raw, setReset: reset];
            let _: () = msg_send![&*self.raw, encodeToCommandBuffer: cmd_buf];
        }
    }
}

/// Hardware `MTLFXSpatialScaler` wrapper.
#[derive(Debug)]
pub struct SpatialScaler {
    raw: Retained<AnyObject>,
}

unsafe impl Send for SpatialScaler {}
unsafe impl Sync for SpatialScaler {}

impl SpatialScaler {
    pub fn is_supported(device: &ProtocolObject<dyn MTLDevice>) -> bool {
        ensure_loaded();
        let Some(cls) = AnyClass::get(c"MTLFXSpatialScalerDescriptor") else {
            return false;
        };
        unsafe { msg_send![cls, supportsDevice: device] }
    }

    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        input_size: [u32; 2],
        output_size: [u32; 2],
        color: &ProtocolObject<dyn MTLTexture>,
        output: &ProtocolObject<dyn MTLTexture>,
        linear: bool,
    ) -> Option<Self> {
        ensure_loaded();
        let cls = AnyClass::get(c"MTLFXSpatialScalerDescriptor")?;
        let supported: bool = unsafe { msg_send![cls, supportsDevice: device] };
        if !supported {
            return None;
        }
        let desc: Option<Retained<AnyObject>> = unsafe { msg_send![cls, new] };
        let desc = desc?;
        // MTLFXSpatialScalerColorProcessingMode: 0 = Perceptual, 1 = Linear
        let mode: usize = usize::from(linear);
        unsafe {
            let _: () = msg_send![&*desc, setInputWidth: input_size[0] as usize];
            let _: () = msg_send![&*desc, setInputHeight: input_size[1] as usize];
            let _: () = msg_send![&*desc, setOutputWidth: output_size[0] as usize];
            let _: () = msg_send![&*desc, setOutputHeight: output_size[1] as usize];
            let _: () = msg_send![&*desc, setColorTextureFormat: color.pixelFormat().0];
            let _: () = msg_send![&*desc, setOutputTextureFormat: output.pixelFormat().0];
            let _: () = msg_send![&*desc, setColorProcessingMode: mode];
        }
        let scaler: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*desc, newSpatialScalerWithDevice: device] };
        let raw = scaler?;
        unsafe {
            let need_c: usize = msg_send![&*raw, colorTextureUsage];
            let need_o: usize = msg_send![&*raw, outputTextureUsage];
            if (color.usage().0 & need_c) != need_c || (output.usage().0 & need_o) != need_o {
                return None;
            }
        }
        Some(Self { raw })
    }

    pub fn encode(
        &self,
        cmd_buf: &ProtocolObject<dyn MTLCommandBuffer>,
        input_size: [u32; 2],
        color: &ProtocolObject<dyn MTLTexture>,
        output: &ProtocolObject<dyn MTLTexture>,
    ) {
        unsafe {
            let _: () = msg_send![&*self.raw, setColorTexture: color];
            let _: () = msg_send![&*self.raw, setOutputTexture: output];
            let _: () = msg_send![&*self.raw, setInputContentWidth: input_size[0] as usize];
            let _: () = msg_send![&*self.raw, setInputContentHeight: input_size[1] as usize];
            let _: () = msg_send![&*self.raw, encodeToCommandBuffer: cmd_buf];
        }
    }
}

/// Hardware `MTLFXFrameInterpolator` wrapper (iOS/iPadOS 26.0+, macOS 26.0+).
/// Loaded dynamically via `MTLFXFrameInterpolatorDescriptor` so older OS versions safely return `None`.
#[derive(Debug)]
pub struct FrameInterpolator {
    raw: Retained<AnyObject>,
}

// SAFETY: MetalFX interpolators are only encoded on one command buffer at a time while
// `Renderer` holds `&mut self`.
unsafe impl Send for FrameInterpolator {}
unsafe impl Sync for FrameInterpolator {}

impl FrameInterpolator {
    pub fn is_supported(device: &ProtocolObject<dyn MTLDevice>) -> bool {
        ensure_loaded();
        let Some(cls) = AnyClass::get(c"MTLFXFrameInterpolatorDescriptor") else {
            return false;
        };
        unsafe { msg_send![cls, supportsDevice: device] }
    }

    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        size: [u32; 2],
        color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        output: &ProtocolObject<dyn MTLTexture>,
    ) -> Option<Self> {
        if size[0] < 64 || size[1] < 64 {
            return None;
        }
        ensure_loaded();
        let cls = AnyClass::get(c"MTLFXFrameInterpolatorDescriptor")?;
        let supported: bool = unsafe { msg_send![cls, supportsDevice: device] };
        if !supported {
            return None;
        }
        let desc: Option<Retained<AnyObject>> = unsafe { msg_send![cls, new] };
        let desc = desc?;
        unsafe {
            let _: () = msg_send![&*desc, setInputWidth: size[0] as usize];
            let _: () = msg_send![&*desc, setInputHeight: size[1] as usize];
            let _: () = msg_send![&*desc, setOutputWidth: size[0] as usize];
            let _: () = msg_send![&*desc, setOutputHeight: size[1] as usize];
            let _: () = msg_send![&*desc, setColorTextureFormat: color.pixelFormat().0];
            let _: () = msg_send![&*desc, setOutputTextureFormat: output.pixelFormat().0];
            let _: () = msg_send![&*desc, setDepthTextureFormat: depth.pixelFormat().0];
            let _: () = msg_send![&*desc, setMotionTextureFormat: motion.pixelFormat().0];
        }
        let interp: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*desc, newFrameInterpolatorWithDevice: device] };
        let raw = interp?;
        // Verify our textures satisfy every MTLTextureUsage bit required by MetalFX.
        unsafe {
            let need_c: usize = msg_send![&*raw, colorTextureUsage];
            let need_d: usize = msg_send![&*raw, depthTextureUsage];
            let need_m: usize = msg_send![&*raw, motionTextureUsage];
            let need_o: usize = msg_send![&*raw, outputTextureUsage];
            if (color.usage().0 & need_c) != need_c
                || (depth.usage().0 & need_d) != need_d
                || (motion.usage().0 & need_m) != need_m
                || (output.usage().0 & need_o) != need_o
            {
                return None;
            }
        }
        Some(Self { raw })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        cmd_buf: &ProtocolObject<dyn MTLCommandBuffer>,
        color: &ProtocolObject<dyn MTLTexture>,
        prev_color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        output: &ProtocolObject<dyn MTLTexture>,
        jitter_px: [f32; 2],
        delta_time: f32,
        near_plane: f32,
        far_plane: f32,
        fov_degrees: f32,
        aspect_ratio: f32,
        depth_reversed: bool,
        reset: bool,
    ) {
        unsafe {
            let _: () = msg_send![&*self.raw, setColorTexture: color];
            let _: () = msg_send![&*self.raw, setPrevColorTexture: prev_color];
            let _: () = msg_send![&*self.raw, setDepthTexture: depth];
            let _: () = msg_send![&*self.raw, setMotionTexture: motion];
            let _: () = msg_send![&*self.raw, setOutputTexture: output];
            let _: () = msg_send![&*self.raw, setMotionVectorScaleX: 1.0f32];
            let _: () = msg_send![&*self.raw, setMotionVectorScaleY: 1.0f32];
            let _: () = msg_send![&*self.raw, setDeltaTime: delta_time];
            let _: () = msg_send![&*self.raw, setNearPlane: near_plane];
            let _: () = msg_send![&*self.raw, setFarPlane: far_plane];
            let _: () = msg_send![&*self.raw, setFieldOfView: fov_degrees];
            let _: () = msg_send![&*self.raw, setAspectRatio: aspect_ratio];
            let _: () = msg_send![&*self.raw, setJitterOffsetX: jitter_px[0]];
            let _: () = msg_send![&*self.raw, setJitterOffsetY: jitter_px[1]];
            let _: () = msg_send![&*self.raw, setDepthReversed: depth_reversed];
            let _: () = msg_send![&*self.raw, setShouldResetHistory: reset];
            let _: () = msg_send![&*self.raw, encodeToCommandBuffer: cmd_buf];
        }
    }
}

