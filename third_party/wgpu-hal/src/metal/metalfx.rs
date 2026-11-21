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

    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        input_size: [u32; 2],
        output_size: [u32; 2],
        color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
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
        }
        Some(Self { raw })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        cmd_buf: &ProtocolObject<dyn MTLCommandBuffer>,
        input_size: [u32; 2],
        color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        output: &ProtocolObject<dyn MTLTexture>,
        jitter_px: [f32; 2],
        depth_reversed: bool,
        reset: bool,
    ) {
        unsafe {
            let _: () = msg_send![&*self.raw, setColorTexture: color];
            let _: () = msg_send![&*self.raw, setDepthTexture: depth];
            let _: () = msg_send![&*self.raw, setMotionTexture: motion];
            let _: () = msg_send![&*self.raw, setOutputTexture: output];
            let _: () = msg_send![&*self.raw, setInputContentWidth: input_size[0] as usize];
            let _: () = msg_send![&*self.raw, setInputContentHeight: input_size[1] as usize];
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
