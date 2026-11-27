//! iOS controllers through Apple's GameController framework: gilrs has no iOS backend, so a
//! Bluetooth pad (Xbox, DualSense, DualShock 4, Switch Pro, MFi) paired in Settings is read here
//! as `GCController`'s extended gamepad into the same `PadState` gilrs' pads give elsewhere.
//! Like the clipboard, the one place besides it that calls UIKit-side Objective-C.
#![allow(unsafe_code)]
use crate::gamepad::{Button, GlyphStyle, PadState};
use objc2::{
    class, msg_send,
    rc::Retained,
    runtime::AnyObject,
};
use std::ffi::CStr;

#[link(name = "GameController", kind = "framework")]
unsafe extern "C" {}

/// A connected controller with an extended profile, and what it is doing now.
#[derive(Clone, Debug)]
pub struct Pad {
    pub name: String,
    pub style: GlyphStyle,
    pub state: PadState,
}

type Obj = Retained<AnyObject>;

/// `object.name` for a property that gives an object.
macro_rules! get {
    ($object:expr, $name:ident) => {{
        let value: Option<Obj> = unsafe { msg_send![&*$object, $name] };
        value
    }};
}

fn string(object: &AnyObject, read: impl Fn(&AnyObject) -> Option<Obj>) -> Option<String> {
    let text = read(object)?;
    let utf8: *const std::ffi::c_char = unsafe { msg_send![&*text, UTF8String] };
    (!utf8.is_null()).then(|| unsafe { CStr::from_ptr(utf8) }.to_string_lossy().into_owned())
}

/// A `GCControllerButtonInput`: whether it is pressed, and how far (a trigger).
fn pressed(button: Option<Obj>) -> bool {
    button.is_some_and(|b| unsafe { msg_send![&*b, isPressed] })
}
fn value(button: Option<Obj>) -> f32 {
    button.map_or(0.0, |b| unsafe { msg_send![&*b, value] })
}

/// A `GCControllerDirectionPad`'s stick position (up positive).
fn stick(pad: Option<Obj>) -> [f32; 2] {
    let Some(pad) = pad else {
        return [0.0; 2];
    };
    let x = get!(pad, xAxis).map_or(0.0, |a| unsafe { msg_send![&*a, value] });
    let y = get!(pad, yAxis).map_or(0.0, |a| unsafe { msg_send![&*a, value] });
    [x, y]
}

fn style_of(category: &str) -> GlyphStyle {
    let category = category.to_ascii_lowercase();
    if category.contains("dualsense") || category.contains("dualshock") || category.contains("ps")
    {
        GlyphStyle::PlayStation
    } else if category.contains("switch") || category.contains("joy-con") {
        GlyphStyle::Nintendo
    } else {
        GlyphStyle::Xbox
    }
}

/// One extended gamepad's buttons, sticks and triggers now.
fn state_of(gamepad: &Obj) -> PadState {
    let mut state = PadState::default();
    let map = [
        (Button::South, get!(gamepad, buttonA)),
        (Button::East, get!(gamepad, buttonB)),
        (Button::West, get!(gamepad, buttonX)),
        (Button::North, get!(gamepad, buttonY)),
        (Button::LeftBumper, get!(gamepad, leftShoulder)),
        (Button::RightBumper, get!(gamepad, rightShoulder)),
        (Button::Start, get!(gamepad, buttonMenu)),
        (Button::Select, get!(gamepad, buttonOptions)),
        (Button::LeftStick, get!(gamepad, leftThumbstickButton)),
        (Button::RightStick, get!(gamepad, rightThumbstickButton)),
    ];
    for (button, input) in map {
        state.set(button, pressed(input));
    }
    if let Some(dpad) = get!(gamepad, dpad) {
        state.set(Button::DPadUp, pressed(get!(dpad, up)));
        state.set(Button::DPadDown, pressed(get!(dpad, down)));
        state.set(Button::DPadLeft, pressed(get!(dpad, left)));
        state.set(Button::DPadRight, pressed(get!(dpad, right)));
    }
    state.left = stick(get!(gamepad, leftThumbstick));
    state.right = stick(get!(gamepad, rightThumbstick));
    state.triggers = [
        value(get!(gamepad, leftTrigger)),
        value(get!(gamepad, rightTrigger)),
    ];
    state
}

/// Every connected game controller, in the order the system lists them.
pub fn connected() -> Vec<Pad> {
    let mut pads = Vec::new();
    // SAFETY: `+controllers` takes nothing and returns an NSArray of GCController (or nil);
    // `count`, `objectAtIndex:` and the properties read are plain getters on those objects.
    let controllers: Option<Obj> = unsafe { msg_send![class!(GCController), controllers] };
    let Some(list) = controllers else {
        return pads;
    };
    let count: usize = unsafe { msg_send![&*list, count] };
    for i in 0..count {
        let controller: Option<Obj> = unsafe { msg_send![&*list, objectAtIndex: i] };
        let Some(controller) = controller else {
            continue;
        };
        // A pad with no extended profile (a remote) is not a game controller.
        let Some(gamepad) = get!(controller, extendedGamepad) else {
            continue;
        };
        pads.push(Pad {
            name: string(&controller, |c| get!(c, vendorName))
                .unwrap_or_else(|| "Game controller".to_owned()),
            style: style_of(&string(&controller, |c| get!(c, productCategory)).unwrap_or_default()),
            state: state_of(&gamepad),
        });
    }
    pads
}

use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Weak};
use objc2::runtime::{AnyClass, Bool, ClassBuilder, Imp, NSObject, Sel};
use objc2::{ClassType, define_class, sel};
use objc2_foundation::NSString;
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};
use winit::keyboard::KeyCode;

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {
    static UIWindowSceneSessionRoleApplication: *const AnyObject;
    static UIPointerLockStateDidChangeNotification: *const AnyObject;
    static UIPointerLockStateSceneUserInfoKey: *const AnyObject;
    fn UIAccessibilityIsAssistiveTouchRunning() -> Bool;
}

/// State of a hardware keyboard (`GCKeyboard`) and mouse/trackpad (`GCMouse`) connected to iOS/iPadOS.
#[derive(Clone, Debug, Default)]
pub struct HardwareInput {
    pub keyboard_connected: bool,
    pub mouse_connected: bool,
    pub keys: Vec<KeyCode>,
    pub mouse_buttons: [bool; 3],
    pub mouse_delta: (f32, f32),
    pub mouse_scroll: (f32, f32),
    /// Native iPadOS pointer position in physical pixels from `UIHoverGestureRecognizer`, when moved.
    pub hover_pos: Option<(f32, f32)>,
}

static MOUSE_DX: AtomicU32 = AtomicU32::new(0);
static MOUSE_DY: AtomicU32 = AtomicU32::new(0);
static MOUSE_SCROLL_X: AtomicU32 = AtomicU32::new(0);
static MOUSE_SCROLL_Y: AtomicU32 = AtomicU32::new(0);
static INSTALLED_MOUSE_INPUT: AtomicUsize = AtomicUsize::new(0);
static MOUSE_CONNECTED: AtomicBool = AtomicBool::new(false);

static HOVER_X: AtomicU32 = AtomicU32::new(0);
static HOVER_Y: AtomicU32 = AtomicU32::new(0);
static HOVER_DIRTY: AtomicBool = AtomicBool::new(false);
static HOVER_INSTALLED_VIEW: AtomicUsize = AtomicUsize::new(0);
static HOVER_RECOGNIZER: AtomicUsize = AtomicUsize::new(0);
static POINTER_INTERACTION: AtomicUsize = AtomicUsize::new(0);
static VIEW_INTERACTIONS_LOCKED: AtomicI8 = AtomicI8::new(-1);

fn atomic_add_f32(atom: &AtomicU32, delta: f32) {
    if !delta.is_finite() || delta == 0.0 {
        return;
    }
    let mut cur = atom.load(Ordering::Relaxed);
    loop {
        let next = (f32::from_bits(cur) + delta).to_bits();
        match atom.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }
}

fn atomic_take_f32(atom: &AtomicU32) -> f32 {
    f32::from_bits(atom.swap(0.0f32.to_bits(), Ordering::Relaxed))
}

#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
}

#[repr(C)]
struct StaticBlock {
    isa: *const std::ffi::c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*const StaticBlock, *mut AnyObject, f32, f32),
    descriptor: *const BlockDescriptor,
}

unsafe impl Sync for StaticBlock {}

unsafe extern "C" {
    static _NSConcreteGlobalBlock: std::ffi::c_void;
}

unsafe extern "C" fn on_mouse_moved(
    _block: *const StaticBlock,
    _mouse: *mut AnyObject,
    dx: f32,
    dy: f32,
) {
    atomic_add_f32(&MOUSE_DX, dx);
    // GCMouseInput reports +dy for upward movement; screen and camera look use +dy downward.
    atomic_add_f32(&MOUSE_DY, -dy);
}

unsafe extern "C" fn on_mouse_scroll(
    _block: *const StaticBlock,
    _dpad: *mut AnyObject,
    sx: f32,
    sy: f32,
) {
    // Apple iOS/iPadOS GameController quirk (matches SDL3's SDL_uikitevents.m):
    // `GCMouseInput.scroll.valueChangedHandler` delivers vertical wheel scroll on the first
    // axis (`sx`, negative = wheel up, positive = wheel down) and horizontal scroll on `sy`.
    let horizontal = sy;
    let vertical = -sx;
    if horizontal != 0.0 {
        atomic_add_f32(&MOUSE_SCROLL_X, horizontal);
    }
    if vertical != 0.0 {
        atomic_add_f32(&MOUSE_SCROLL_Y, vertical);
    }
}

static BLOCK_DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<StaticBlock>(),
};

static MOUSE_MOVED_BLOCK: StaticBlock = StaticBlock {
    isa: unsafe { &_NSConcreteGlobalBlock },
    flags: 1 << 28,
    reserved: 0,
    invoke: on_mouse_moved,
    descriptor: &BLOCK_DESCRIPTOR,
};

static MOUSE_SCROLL_BLOCK: StaticBlock = StaticBlock {
    isa: unsafe { &_NSConcreteGlobalBlock },
    flags: 1 << 28,
    reserved: 0,
    invoke: on_mouse_scroll,
    descriptor: &BLOCK_DESCRIPTOR,
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: f64,
    y: f64,
}

// SAFETY: `CGPoint` layout on 64-bit iOS (`CGFloat` = `f64`).
unsafe impl Encode for Point {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Size {
    width: f64,
    height: f64,
}

// SAFETY: `CGSize` layout on 64-bit iOS (`CGFloat` = `f64`).
unsafe impl Encode for Size {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    origin: Point,
    size: Size,
}

// SAFETY: `CGRect` layout on 64-bit iOS (`CGPoint` + `CGSize`).
unsafe impl Encode for Rect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[Point::ENCODING, Size::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

// SAFETY: `UIEdgeInsets` layout on 64-bit iOS (`CGFloat` = `f64`).
unsafe impl Encode for EdgeInsets {
    const ENCODING: Encoding = Encoding::Struct(
        "UIEdgeInsets",
        &[f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING],
    );
}

/// Wrapper for Obj-C objects accessed strictly from UIKit main-thread callbacks.
struct SendObj(Obj);
unsafe impl Send for SendObj {}

struct SendWeak(Weak<AnyObject>);
unsafe impl Send for SendWeak {}

struct SceneLifecycleState {
    scene: Option<SendObj>,
    delegate: Option<SendObj>,
    window: Option<SendObj>,
    tracked_windows: Vec<SendWeak>,
}

impl SceneLifecycleState {
    const fn new() -> Self {
        Self {
            scene: None,
            delegate: None,
            window: None,
            tracked_windows: Vec::new(),
        }
    }
}

static SCENE_STATE: Mutex<SceneLifecycleState> = Mutex::new(SceneLifecycleState::new());
static APP_DELEGATE_PTR: AtomicUsize = AtomicUsize::new(0);
static HORIZON_ROOT_VC_CLS: OnceLock<&'static AnyClass> = OnceLock::new();
static SCENE_CONNECTED_COUNT: AtomicU32 = AtomicU32::new(0);
static SCENE_INIT_WS_COUNT: AtomicU32 = AtomicU32::new(0);
static SCENE_ADOPTED_COUNT: AtomicU32 = AtomicU32::new(0);
static POINTER_LOCK_NOTIFY_COUNT: AtomicU32 = AtomicU32::new(0);
static SCENE_LIFECYCLE_INSTALLED: Once = Once::new();
static POINTER_LOCK_OBSERVER_INSTALLED: Once = Once::new();
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static IN_INIT_WITH_SCENE: AtomicBool = AtomicBool::new(false);

type AppInitFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject;
type InitWithFrameFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, Rect) -> *mut AnyObject;
type SetRootVcFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);
type MakeKeyAndVisibleFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);
static ORIG_APP_INIT: OnceLock<AppInitFn> = OnceLock::new();
static ORIG_INIT_WITH_FRAME: OnceLock<InitWithFrameFn> = OnceLock::new();
static ORIG_SET_ROOT_VC: OnceLock<SetRootVcFn> = OnceLock::new();
static ORIG_MAKE_KEY_AND_VISIBLE: OnceLock<MakeKeyAndVisibleFn> = OnceLock::new();

/// Landscape mask (`UIInterfaceOrientationMaskLandscapeLeft | Right`).
const LANDSCAPE_ORIENTATION_MASK: usize = (1 << 3) | (1 << 4);
/// All screen edges (`UIRectEdgeAll`).
const RECT_EDGE_ALL: usize = 0x0F;
/// `UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight`.
const VIEW_AUTORESIZING_FLEXIBLE_WH: usize = (1 << 1) | (1 << 4);

/// Safely replaces or overrides an instance method on `cls` without ever mutating an inherited
/// superclass method implementation.
unsafe fn swizzle_instance_method(
    cls: &AnyClass,
    sel: Sel,
    new_imp: Imp,
    types: &CStr,
) -> Option<Imp> {
    let cls_ptr = (cls as *const AnyClass).cast_mut().cast();
    let method = unsafe { objc2::ffi::class_getInstanceMethod(cls_ptr, sel) };
    if method.is_null() {
        return None;
    }
    let orig_imp = unsafe { objc2::ffi::method_getImplementation(method) }?;
    let super_cls = unsafe { objc2::ffi::class_getSuperclass(cls_ptr) };
    let super_method = if super_cls.is_null() {
        std::ptr::null()
    } else {
        unsafe { objc2::ffi::class_getInstanceMethod(super_cls, sel) }
    };
    if std::ptr::eq(method, super_method) {
        let added = unsafe { objc2::ffi::class_addMethod(cls_ptr, sel, new_imp, types.as_ptr()) };
        if !added.as_bool() {
            return None;
        }
    } else {
        let _ = unsafe { objc2::ffi::method_setImplementation(method.cast_mut(), new_imp) };
    }
    Some(orig_imp)
}

unsafe fn is_winit_window_ptr(this: *const AnyObject) -> bool {
    if this.is_null() {
        return false;
    }
    let Some(winit_cls) = AnyClass::get(c"WinitUIWindow") else {
        return false;
    };
    let obj_cls = unsafe { objc2::ffi::object_getClass(this.cast_mut().cast()) };
    std::ptr::eq(obj_cls, (winit_cls as *const AnyClass).cast())
}

unsafe fn is_winit_vc_ptr(vc: *const AnyObject) -> bool {
    if vc.is_null() {
        return false;
    }
    let Some(winit_cls) = AnyClass::get(c"WinitUIViewController") else {
        return false;
    };
    unsafe { msg_send![vc, isKindOfClass: winit_cls] }
}

unsafe fn is_horizon_root_vc_ptr(vc: *const AnyObject) -> bool {
    if vc.is_null() {
        return false;
    }
    let Some(root_cls) = HORIZON_ROOT_VC_CLS.get().copied() else {
        return false;
    };
    unsafe { msg_send![vc, isKindOfClass: root_cls] }
}

unsafe fn track_window_ptr(this: *const AnyObject) {
    let mut st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.tracked_windows.retain(|w| w.0.load().is_some());
    let already_tracked = st.tracked_windows.iter().any(|w| {
        w.0.load()
            .is_some_and(|obj| std::ptr::eq(&*obj as *const AnyObject, this))
    });
    if !already_tracked {
        st.tracked_windows
            .push(SendWeak(Weak::new(unsafe { &*this })));
    }
}

unsafe fn configure_scene_restrictions(scene: &AnyObject) {
    let sr_sel = sel!(sizeRestrictions);
    let has_sr: bool = unsafe { msg_send![scene, respondsToSelector: sr_sel] };
    if has_sr && let Some(sr) = get!(scene, sizeRestrictions) {
        let allow_fs_sel = sel!(setAllowsFullScreen:);
        let has_allow_fs: bool = unsafe { msg_send![&*sr, respondsToSelector: allow_fs_sel] };
        if has_allow_fs {
            unsafe {
                let _: () = msg_send![&*sr, setAllowsFullScreen: Bool::YES];
            }
        }
    }
    // Match DELOSApp.swift: ask iPadOS 16+/26+ explicitly for landscape scene geometry.
    let req_sel = sel!(requestGeometryUpdateWithPreferences:errorHandler:);
    let has_req: bool = unsafe { msg_send![scene, respondsToSelector: req_sel] };
    if has_req && let Some(pref_cls) = AnyClass::get(c"UIWindowSceneGeometryPreferencesIOS") {
        let alloc: Allocated<AnyObject> = unsafe { msg_send![pref_cls, alloc] };
        let prefs: Option<Obj> = unsafe {
            msg_send![alloc, initWithInterfaceOrientations: LANDSCAPE_ORIENTATION_MASK]
        };
        if let Some(prefs) = prefs {
            unsafe {
                let _: () = msg_send![
                    scene,
                    requestGeometryUpdateWithPreferences: &*prefs,
                    errorHandler: std::ptr::null::<std::ffi::c_void>()
                ];
            }
        }
    }
}

unsafe fn is_app_window_scene(scene: &AnyObject) -> bool {
    let Some(ws_cls) = AnyClass::get(c"UIWindowScene") else {
        return false;
    };
    let is_ws: bool = unsafe { msg_send![scene, isKindOfClass: ws_cls] };
    if !is_ws {
        return false;
    }
    let Some(session) = get!(scene, session) else {
        return false;
    };
    let Some(role) = get!(session, role) else {
        return false;
    };
    let app_role = unsafe { UIWindowSceneSessionRoleApplication };
    if app_role.is_null() {
        return true;
    }
    unsafe { msg_send![&*role, isEqualToString: app_role] }
}

unsafe fn window_needs_scene(win: *const AnyObject) -> bool {
    let cur_scene: Option<Obj> = unsafe { msg_send![win, windowScene] };
    match cur_scene {
        None => true,
        Some(scene) => {
            let del: Option<Obj> = get!(scene, delegate);
            del.is_none()
        }
    }
}

unsafe fn attach_window_inner(
    win: *const AnyObject,
    scene: &AnyObject,
    delegate: Option<&AnyObject>,
) {
    let cur_scene: Option<Obj> = unsafe { msg_send![win, windowScene] };
    let already = cur_scene
        .as_ref()
        .is_some_and(|s| std::ptr::eq(&**s as *const AnyObject, scene as *const AnyObject));
    if !already {
        let set_sel = sel!(setWindowScene:);
        let responds: bool = unsafe { msg_send![win, respondsToSelector: set_sel] };
        if responds {
            unsafe {
                let _: () = msg_send![win, setWindowScene: scene];
            }
            SCENE_ADOPTED_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
    unsafe { configure_scene_restrictions(scene) };
    if let Some(del) = delegate {
        unsafe {
            let _: () = msg_send![del, setWindow: win];
        }
    }
}

unsafe extern "C" fn hvc_prefers_pointer_locked(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    if !PROBING_PREFERS.load(Ordering::Relaxed) {
        PREFERS_CB_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    Bool::new(PREFERS_POINTER_LOCKED.load(Ordering::Relaxed))
}

unsafe extern "C" fn hvc_child_for_pointer_lock(_this: *mut AnyObject, _cmd: Sel) -> *mut AnyObject {
    std::ptr::null_mut()
}

unsafe extern "C" fn hvc_first_child(this: *mut AnyObject, _cmd: Sel) -> *mut AnyObject {
    if this.is_null() {
        return std::ptr::null_mut();
    }
    let children: Option<Obj> = unsafe { msg_send![this, childViewControllers] };
    children.map_or(std::ptr::null_mut(), |arr| unsafe {
        msg_send![&*arr, firstObject]
    })
}

unsafe extern "C" fn hvc_supported_orientations(_this: *mut AnyObject, _cmd: Sel) -> usize {
    LANDSCAPE_ORIENTATION_MASK
}

unsafe extern "C" fn hvc_yes(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    Bool::YES
}

unsafe extern "C" fn hvc_edges(_this: *mut AnyObject, _cmd: Sel) -> usize {
    RECT_EDGE_ALL
}

/// Registers a genuine `UIViewController` subclass (`HorizonRootViewController`) before any window
/// is created, matching `DELOSApp.swift`'s `OrientationLockedContainer`: it owns
/// `prefersPointerLocked` and `prefersInterfaceOrientationLocked` directly on a real Obj-C
/// subclass while forwarding status-bar / home-indicator / edge-gesture preferences to winit's
/// child `WinitUIViewController`.
unsafe fn register_horizon_root_vc_class() -> Option<&'static AnyClass> {
    if let Some(&cls) = HORIZON_ROOT_VC_CLS.get() {
        return Some(cls);
    }
    if let Some(existing) = AnyClass::get(c"HorizonRootViewController") {
        let _ = HORIZON_ROOT_VC_CLS.set(existing);
        return Some(existing);
    }
    let ui_vc_cls = AnyClass::get(c"UIViewController")?;
    let mut builder = ClassBuilder::new(c"HorizonRootViewController", ui_vc_cls)?;
    unsafe {
        builder.add_method(
            sel!(prefersPointerLocked),
            hvc_prefers_pointer_locked as unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
        );
        builder.add_method(
            sel!(childViewControllerForPointerLock),
            hvc_child_for_pointer_lock
                as unsafe extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject,
        );
        builder.add_method(
            sel!(childViewControllerForStatusBarHidden),
            hvc_first_child as unsafe extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject,
        );
        builder.add_method(
            sel!(childViewControllerForHomeIndicatorAutoHidden),
            hvc_first_child as unsafe extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject,
        );
        builder.add_method(
            sel!(childViewControllerForScreenEdgesDeferringSystemGestures),
            hvc_first_child as unsafe extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject,
        );
        builder.add_method(
            sel!(supportedInterfaceOrientations),
            hvc_supported_orientations as unsafe extern "C" fn(*mut AnyObject, Sel) -> usize,
        );
        builder.add_method(
            sel!(prefersInterfaceOrientationLocked),
            hvc_yes as unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
        );
        builder.add_method(
            sel!(prefersStatusBarHidden),
            hvc_yes as unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
        );
        builder.add_method(
            sel!(prefersHomeIndicatorAutoHidden),
            hvc_yes as unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
        );
        builder.add_method(
            sel!(preferredScreenEdgesDeferringSystemGestures),
            hvc_edges as unsafe extern "C" fn(*mut AnyObject, Sel) -> usize,
        );
    }
    let cls = builder.register();
    let _ = HORIZON_ROOT_VC_CLS.set(cls);
    Some(cls)
}

/// Wraps a `WinitUIViewController` inside a native `HorizonRootViewController` container.
unsafe fn wrap_in_horizon_root_vc(child_vc: &AnyObject) -> Option<Obj> {
    if unsafe { is_horizon_root_vc_ptr(child_vc) } {
        return unsafe { Retained::retain((child_vc as *const AnyObject).cast_mut()) };
    }
    if let Some(parent) = get!(child_vc, parentViewController)
        && unsafe { is_horizon_root_vc_ptr(&*parent) }
    {
        return Some(parent);
    }
    let root_cls = unsafe { register_horizon_root_vc_class() }?;
    let container: Option<Obj> = unsafe { msg_send![root_cls, new] };
    let container = container?;
    unsafe {
        let _: () = msg_send![&*container, addChildViewController: child_vc];
    }
    let cview: Option<Obj> = get!(container, view);
    let wview: Option<Obj> = get!(child_vc, view);
    unsafe {
        if let (Some(cview), Some(wview)) = (cview, wview) {
            let bounds: Rect = msg_send![&*cview, bounds];
            let _: () = msg_send![&*wview, setFrame: bounds];
            let _: () = msg_send![&*wview, setAutoresizingMask: VIEW_AUTORESIZING_FLEXIBLE_WH];
            let _: () = msg_send![&*cview, addSubview: &*wview];
        }
        let _: () = msg_send![child_vc, didMoveToParentViewController: &*container];
        let lock_orient_sel = sel!(setNeedsUpdateOfPrefersInterfaceOrientationLocked);
        let has_lock_orient: bool = msg_send![&*container, respondsToSelector: lock_orient_sel];
        if has_lock_orient {
            let _: () = msg_send![&*container, setNeedsUpdateOfPrefersInterfaceOrientationLocked];
        }
    }
    Some(container)
}

fn make_scene_configuration(session: &AnyObject) -> Option<Retained<AnyObject>> {
    let role: Option<Obj> = get!(session, role);
    let cfg_cls = AnyClass::get(c"UISceneConfiguration")?;
    let alloc: Allocated<AnyObject> = unsafe { msg_send![cfg_cls, alloc] };
    let name = NSString::from_str("Default Configuration");
    let role_ptr = role
        .as_deref()
        .map_or(std::ptr::null::<AnyObject>(), |r| r as *const AnyObject);
    let cfg: Option<Obj> =
        unsafe { msg_send![alloc, initWithName: &*name, sessionRole: role_ptr] };
    if let Some(cfg) = cfg {
        let is_app = role.as_deref().is_some_and(|r| {
            let app_role = unsafe { UIWindowSceneSessionRoleApplication };
            app_role.is_null() || unsafe { msg_send![r, isEqualToString: app_role] }
        });
        if is_app {
            unsafe {
                let _: () = msg_send![&*cfg, setDelegateClass: HorizonSceneDelegate::class()];
            }
        }
        return Some(cfg);
    }
    None
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `HorizonAppDelegate` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "HorizonAppDelegate"]
    struct HorizonAppDelegate;

    impl HorizonAppDelegate {
        #[unsafe(method_id(window))]
        fn window(&self) -> Option<Retained<AnyObject>> {
            SCENE_STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .window
                .as_ref()
                .map(|w| w.0.clone())
        }

        #[unsafe(method(setWindow:))]
        fn set_window(&self, window: Option<&AnyObject>) {
            let next = window.and_then(|w| {
                unsafe { Retained::retain((w as *const AnyObject).cast_mut()) }.map(SendObj)
            });
            let prev = {
                let mut st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::replace(&mut st.window, next)
            };
            drop(prev);
        }

        #[unsafe(method(application:didFinishLaunchingWithOptions:))]
        fn did_finish_launching(
            &self,
            _application: &AnyObject,
            _options: Option<&AnyObject>,
        ) -> Bool {
            Bool::YES
        }

        #[unsafe(method(application:supportedInterfaceOrientationsForWindow:))]
        fn supported_orientations(
            &self,
            _application: &AnyObject,
            _window: Option<&AnyObject>,
        ) -> usize {
            LANDSCAPE_ORIENTATION_MASK
        }

        #[unsafe(method_id(application:configurationForConnectingSceneSession:options:))]
        fn scene_configuration(
            &self,
            _application: &AnyObject,
            session: &AnyObject,
            _options: &AnyObject,
        ) -> Option<Retained<AnyObject>> {
            make_scene_configuration(session)
        }
    }
);

unsafe fn install_app_delegate_on(app: *const AnyObject) {
    if app.is_null() {
        return;
    }
    let existing: Option<Obj> = unsafe { msg_send![app, delegate] };
    if existing.is_some() {
        return;
    }
    let mut ptr = APP_DELEGATE_PTR.load(Ordering::Acquire);
    if ptr == 0 {
        let del: Retained<HorizonAppDelegate> =
            unsafe { msg_send![HorizonAppDelegate::class(), new] };
        let raw = Retained::into_raw(del) as usize;
        match APP_DELEGATE_PTR.compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => ptr = raw,
            Err(existing_ptr) => {
                // Another path initialized it first; release our duplicate.
                let _ = unsafe { Retained::from_raw(raw as *mut HorizonAppDelegate) };
                ptr = existing_ptr;
            }
        }
    }
    if ptr != 0 {
        let del_obj = ptr as *const AnyObject;
        unsafe {
            let _: () = msg_send![app, setDelegate: del_obj];
        }
    }
}

unsafe fn ensure_app_delegate() {
    let app: Option<Obj> = unsafe { msg_send![class!(UIApplication), sharedApplication] };
    if let Some(app) = app {
        unsafe { install_app_delegate_on(&*app) };
    }
}

/// Gives `UIApplication` a genuine `UIApplicationDelegate` (`HorizonAppDelegate`) during
/// `UIApplicationMain`, since `winit 0.30` intentionally passes `nil` for `delegateClassName`.
unsafe extern "C-unwind" fn swizzled_ui_app_init(
    this: *mut AnyObject,
    cmd: Sel,
) -> *mut AnyObject {
    let res = if let Some(orig) = ORIG_APP_INIT.get() {
        unsafe { orig(this, cmd) }
    } else {
        this
    };
    if !res.is_null() {
        unsafe { install_app_delegate_on(res) };
    }
    res
}

/// Redirects `[[WinitUIWindow alloc] initWithFrame:]` to `initWithWindowScene:` when our
/// `HorizonSceneDelegate` already connected an application `UIWindowScene` (matching SDL3),
/// so winit's window is created natively on the scene from birth.
unsafe extern "C-unwind" fn swizzled_init_with_frame(
    this: *mut AnyObject,
    cmd: Sel,
    frame: Rect,
) -> *mut AnyObject {
    if unsafe { is_winit_window_ptr(this) } && !IN_INIT_WITH_SCENE.load(Ordering::Relaxed) {
        let (scene, delegate) = {
            let st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
            (
                st.scene.as_ref().map(|s| s.0.clone()),
                st.delegate.as_ref().map(|d| d.0.clone()),
            )
        };
        if let Some(scene) = scene {
            IN_INIT_WITH_SCENE.store(true, Ordering::Relaxed);
            let res: *mut AnyObject = unsafe { msg_send![this, initWithWindowScene: &*scene] };
            IN_INIT_WITH_SCENE.store(false, Ordering::Relaxed);
            if !res.is_null() {
                SCENE_INIT_WS_COUNT.fetch_add(1, Ordering::Relaxed);
                unsafe {
                    track_window_ptr(res);
                    configure_scene_restrictions(&scene);
                    if let Some(del) = delegate.as_deref() {
                        let _: () = msg_send![del, setWindow: res];
                    }
                }
            }
            return res;
        }
    }
    if let Some(orig) = ORIG_INIT_WITH_FRAME.get() {
        unsafe { orig(this, cmd, frame) }
    } else {
        this
    }
}

/// Wraps `WinitUIViewController` inside a genuine `HorizonRootViewController` subclass BEFORE
/// `-[UIWindow setRootViewController:]` attaches it to `WinitUIWindow`, so iPadOS queries
/// `prefersPointerLocked` on a native `UIViewController` subclass from the first frame.
unsafe extern "C-unwind" fn swizzled_set_root_vc(
    this: *mut AnyObject,
    cmd: Sel,
    vc: *mut AnyObject,
) {
    if !vc.is_null() && unsafe { is_winit_window_ptr(this) } {
        unsafe {
            if !is_horizon_root_vc_ptr(vc) && is_winit_vc_ptr(vc) {
                let _ = ensure_pointer_lock_methods(&*vc);
                if let Some(container) = wrap_in_horizon_root_vc(&*vc) {
                    if let Some(orig) = ORIG_SET_ROOT_VC.get() {
                        orig(this, cmd, Retained::as_ptr(&container).cast_mut());
                    }
                    return;
                }
            } else if !is_horizon_root_vc_ptr(vc) {
                let _ = ensure_pointer_lock_methods(&*vc);
            }
        }
    }
    if let Some(orig) = ORIG_SET_ROOT_VC.get() {
        unsafe { orig(this, cmd, vc) };
    }
}

unsafe extern "C-unwind" fn swizzled_make_key_and_visible(this: *mut AnyObject, cmd: Sel) {
    let is_winit = unsafe { is_winit_window_ptr(this) };
    if is_winit {
        unsafe { track_window_ptr(this) };
        if unsafe { window_needs_scene(this) } {
            let (scene, delegate) = {
                let st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                (
                    st.scene.as_ref().map(|s| s.0.clone()),
                    st.delegate.as_ref().map(|d| d.0.clone()),
                )
            };
            if let Some(scene) = scene {
                unsafe { attach_window_inner(this, &scene, delegate.as_deref()) };
            } else {
                // Keep the window off-screen until `scene:willConnectToSession:options:` connects
                // the real application UIWindowScene and attaches + shows it.
                return;
            }
        }
    }
    if let Some(orig) = ORIG_MAKE_KEY_AND_VISIBLE.get() {
        unsafe { orig(this, cmd) };
    }
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `PointerLockObserver` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "HorizonPointerLockObserver"]
    struct PointerLockObserver;

    impl PointerLockObserver {
        #[unsafe(method(pointerLockDidChange:))]
        fn pointer_lock_did_change(&self, notification: &AnyObject) {
            handle_pointer_lock_notification(notification);
        }
    }
);

fn handle_pointer_lock_notification(notification: &AnyObject) {
    POINTER_LOCK_NOTIFY_COUNT.fetch_add(1, Ordering::Relaxed);
    let user_info: Option<Obj> = get!(notification, userInfo);
    let key = unsafe { UIPointerLockStateSceneUserInfoKey };
    let scene: Option<Obj> = if !key.is_null() {
        user_info.and_then(|info| unsafe { msg_send![&*info, objectForKey: key] })
    } else {
        None
    };
    if let Some(scene) = scene
        && let Some(pls) = get!(scene, pointerLockState)
    {
        let locked: bool = unsafe { msg_send![&*pls, isLocked] };
        SYS_POINTER_LOCKED.store(locked, Ordering::Relaxed);
        if let Ok(mut st) = POINTER_LOCK_STATE.try_lock() {
            st.sys_locked = locked;
            st.last_probe_at = None;
        }
        unsafe { invalidate_pointer_interaction() };
    }
}

fn ensure_pointer_lock_observer() {
    POINTER_LOCK_OBSERVER_INSTALLED.call_once(|| unsafe {
        let name = UIPointerLockStateDidChangeNotification;
        if name.is_null() {
            return;
        }
        let center: Option<Obj> = msg_send![class!(NSNotificationCenter), defaultCenter];
        let Some(center) = center else {
            return;
        };
        let observer: Retained<PointerLockObserver> =
            msg_send![PointerLockObserver::class(), new];
        let _: () = msg_send![
            &*center,
            addObserver: &*observer,
            selector: sel!(pointerLockDidChange:),
            name: name,
            object: std::ptr::null::<AnyObject>()
        ];
        std::mem::forget(observer);
    });
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `HorizonSceneDelegate` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "HorizonSceneDelegate"]
    struct HorizonSceneDelegate;

    impl HorizonSceneDelegate {
        #[unsafe(method_id(window))]
        fn window(&self) -> Option<Retained<AnyObject>> {
            SCENE_STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .window
                .as_ref()
                .map(|w| w.0.clone())
        }

        #[unsafe(method(setWindow:))]
        fn set_window(&self, window: Option<&AnyObject>) {
            let next = window.and_then(|w| {
                unsafe { Retained::retain((w as *const AnyObject).cast_mut()) }.map(SendObj)
            });
            let prev = {
                let mut st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::replace(&mut st.window, next)
            };
            drop(prev);
        }

        #[unsafe(method(scene:willConnectToSession:options:))]
        fn scene_will_connect(
            &self,
            scene: &AnyObject,
            _session: &AnyObject,
            _options: &AnyObject,
        ) {
            if !unsafe { is_app_window_scene(scene) } {
                return;
            }
            unsafe {
                ensure_app_delegate();
                configure_scene_restrictions(scene);
            }
            ensure_pointer_lock_observer();
            SCENE_CONNECTED_COUNT.fetch_add(1, Ordering::Relaxed);
            let scene_ret = unsafe { Retained::retain((scene as *const AnyObject).cast_mut()) };
            let self_ret =
                unsafe { Retained::retain(self as *const HorizonSceneDelegate as *mut AnyObject) };
            let (windows, old_scene, old_del) = {
                let mut st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                let old_scene = std::mem::replace(&mut st.scene, scene_ret.map(SendObj));
                let old_del = std::mem::replace(&mut st.delegate, self_ret.map(SendObj));
                st.tracked_windows.retain(|w| w.0.load().is_some());
                let wins: Vec<Obj> = st
                    .tracked_windows
                    .iter()
                    .filter_map(|w| w.0.load())
                    .collect();
                (wins, old_scene, old_del)
            };
            drop(old_scene);
            drop(old_del);
            let del_obj = self as *const HorizonSceneDelegate as *const AnyObject;
            for win in windows {
                unsafe {
                    attach_window_inner(&*win, scene, Some(&*del_obj));
                    let _: () = msg_send![&*win, makeKeyAndVisible];
                }
            }
            REFRESH_PENDING.store(true, Ordering::Release);
        }

        #[unsafe(method(sceneDidBecomeActive:))]
        fn scene_did_become_active(&self, scene: &AnyObject) {
            if unsafe { is_app_window_scene(scene) } {
                unsafe { ensure_app_delegate() };
                let scene_ret =
                    unsafe { Retained::retain((scene as *const AnyObject).cast_mut()) };
                let self_ret = unsafe {
                    Retained::retain(self as *const HorizonSceneDelegate as *mut AnyObject)
                };
                let (windows, old_scene, old_del) = {
                    let mut st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                    let old_scene = std::mem::replace(&mut st.scene, scene_ret.map(SendObj));
                    let old_del = std::mem::replace(&mut st.delegate, self_ret.map(SendObj));
                    st.tracked_windows.retain(|w| w.0.load().is_some());
                    let wins: Vec<Obj> = st
                        .tracked_windows
                        .iter()
                        .filter_map(|w| w.0.load())
                        .collect();
                    (wins, old_scene, old_del)
                };
                drop(old_scene);
                drop(old_del);
                let del_obj = self as *const HorizonSceneDelegate as *const AnyObject;
                for win in windows {
                    if unsafe { window_needs_scene(&*win) } {
                        unsafe {
                            attach_window_inner(&*win, scene, Some(&*del_obj));
                            let _: () = msg_send![&*win, makeKeyAndVisible];
                        }
                    }
                }
            }
            REFRESH_PENDING.store(true, Ordering::Release);
        }

        #[unsafe(method(sceneWillResignActive:))]
        fn scene_will_resign_active(&self, _scene: &AnyObject) {
            REFRESH_PENDING.store(true, Ordering::Release);
        }

        #[unsafe(method(sceneDidDisconnect:))]
        fn scene_did_disconnect(&self, scene: &AnyObject) {
            let (old_scene, old_del, old_win) = {
                let mut st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                let matches = st
                    .scene
                    .as_ref()
                    .is_some_and(|s| std::ptr::eq(&*s.0 as *const AnyObject, scene));
                if matches {
                    (st.scene.take(), st.delegate.take(), st.window.take())
                } else {
                    (None, None, None)
                }
            };
            drop(old_scene);
            drop(old_del);
            drop(old_win);
        }
    }
);

/// Registers `HorizonAppDelegate`, `HorizonSceneDelegate`, and `HorizonRootViewController` and
/// installs `UIScene` window adoption for `winit 0.30` before `UIApplicationMain` runs.
/// Must be called on the main thread at process startup.
pub fn install_scene_lifecycle() {
    SCENE_LIFECYCLE_INSTALLED.call_once(|| unsafe {
        let app_del_cls = HorizonAppDelegate::class();
        let app_del_ptr = (app_del_cls as *const AnyClass).cast_mut().cast();
        let app_proto = objc2::ffi::objc_getProtocol(c"UIApplicationDelegate".as_ptr());
        if !app_proto.is_null() {
            let _ = objc2::ffi::class_addProtocol(app_del_ptr, app_proto);
        }

        let cls = HorizonSceneDelegate::class();
        let cls_ptr = (cls as *const AnyClass).cast_mut().cast();
        for proto_name in [c"UISceneDelegate", c"UIWindowSceneDelegate"] {
            let proto = objc2::ffi::objc_getProtocol(proto_name.as_ptr());
            if !proto.is_null() {
                let _ = objc2::ffi::class_addProtocol(cls_ptr, proto);
            }
        }
        let _ = register_horizon_root_vc_class();
        ensure_pointer_lock_observer();

        // Give `UIApplication` a genuine `UIApplicationDelegate` during `UIApplicationMain`.
        if let Some(ui_app_cls) = AnyClass::get(c"UIApplication") {
            let new_imp: Imp = std::mem::transmute::<AppInitFn, Imp>(swizzled_ui_app_init);
            if let Some(orig_imp) = swizzle_instance_method(ui_app_cls, sel!(init), new_imp, c"@@:")
            {
                let orig_fn: AppInitFn = std::mem::transmute::<Imp, AppInitFn>(orig_imp);
                let _ = ORIG_APP_INIT.set(orig_fn);
            }
        }

        // Only swizzle `UIWindow` when Info.plist declares a `UIApplicationSceneManifest`.
        let bundle: Option<Obj> = msg_send![class!(NSBundle), mainBundle];
        let manifest_key = NSString::from_str("UIApplicationSceneManifest");
        let has_manifest = bundle
            .and_then(|b| {
                let val: Option<Obj> = msg_send![&*b, objectForInfoDictionaryKey: &*manifest_key];
                val
            })
            .is_some();
        if !has_manifest {
            return;
        }
        let Some(ui_window_cls) = AnyClass::get(c"UIWindow") else {
            return;
        };
        // 1. Redirect `[[WinitUIWindow alloc] initWithFrame:]` to `initWithWindowScene:` when
        //    our scene delegate already connected the app scene.
        let init_imp: Imp = std::mem::transmute::<InitWithFrameFn, Imp>(swizzled_init_with_frame);
        if let Some(orig_imp) = swizzle_instance_method(
            ui_window_cls,
            sel!(initWithFrame:),
            init_imp,
            c"@@:{CGRect={CGPoint=dd}{CGSize=dd}}",
        ) {
            let orig_fn: InitWithFrameFn = std::mem::transmute::<Imp, InitWithFrameFn>(orig_imp);
            let _ = ORIG_INIT_WITH_FRAME.set(orig_fn);
        }
        // 2. Wrap `WinitUIViewController` inside a genuine `HorizonRootViewController` subclass
        //    BEFORE `setRootViewController:` attaches it to `WinitUIWindow`.
        let root_imp: Imp = std::mem::transmute::<SetRootVcFn, Imp>(swizzled_set_root_vc);
        if let Some(orig_imp) = swizzle_instance_method(
            ui_window_cls,
            sel!(setRootViewController:),
            root_imp,
            c"v@:@",
        ) {
            let orig_fn: SetRootVcFn = std::mem::transmute::<Imp, SetRootVcFn>(orig_imp);
            let _ = ORIG_SET_ROOT_VC.set(orig_fn);
        }
        // 3. Fallback adoption if winit created its window before the scene connected.
        let key_imp: Imp =
            std::mem::transmute::<MakeKeyAndVisibleFn, Imp>(swizzled_make_key_and_visible);
        if let Some(orig_imp) = swizzle_instance_method(
            ui_window_cls,
            sel!(makeKeyAndVisible),
            key_imp,
            c"v@:",
        ) {
            let orig_fn: MakeKeyAndVisibleFn =
                std::mem::transmute::<Imp, MakeKeyAndVisibleFn>(orig_imp);
            let _ = ORIG_MAKE_KEY_AND_VISIBLE.set(orig_fn);
        }
    });
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `HoverWatch` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "HorizonHoverWatch"]
    struct HoverWatch;

    impl HoverWatch {
        #[unsafe(method(hoverChanged:))]
        fn hover_changed(&self, recognizer: &AnyObject) {
            handle_hover(recognizer);
        }

        #[unsafe(method(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:))]
        fn should_recognize_simultaneously(
            &self,
            _gr: &AnyObject,
            _other: &AnyObject,
        ) -> Bool {
            Bool::YES
        }

        #[unsafe(method(gestureRecognizer:shouldRequireFailureOfGestureRecognizer:))]
        fn should_require_failure(&self, _gr: &AnyObject, _other: &AnyObject) -> Bool {
            Bool::NO
        }

        #[unsafe(method(gestureRecognizer:shouldBeRequiredToFailByGestureRecognizer:))]
        fn should_be_required_to_fail(&self, _gr: &AnyObject, _other: &AnyObject) -> Bool {
            Bool::NO
        }
    }
);

static POINTER_STYLE_CALLS: AtomicU32 = AtomicU32::new(0);

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `PointerStyleDelegate` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "HorizonPointerStyleDelegate"]
    struct PointerStyleDelegate;

    impl PointerStyleDelegate {
        #[unsafe(method_id(pointerInteraction:regionForRequest:defaultRegion:))]
        fn region_for_request(
            &self,
            interaction: &AnyObject,
            _request: &AnyObject,
            default_region: &AnyObject,
        ) -> Option<Retained<AnyObject>> {
            // Match SDL3 / bcd-games: return a single continuous region spanning the whole view
            // bounds so hidden pointer style never lapses between per-location default regions.
            let full_region = get!(interaction, view).and_then(|view| {
                let region_cls = AnyClass::get(c"UIPointerRegion")?;
                let bounds: Rect = unsafe { msg_send![&*view, bounds] };
                unsafe {
                    msg_send![
                        region_cls,
                        regionWithRect: bounds,
                        identifier: std::ptr::null::<AnyObject>()
                    ]
                }
            });
            full_region.or_else(|| unsafe {
                Retained::retain((default_region as *const AnyObject).cast_mut())
            })
        }

        #[unsafe(method_id(pointerInteraction:styleForRegion:))]
        fn style_for_region(
            &self,
            _interaction: &AnyObject,
            _region: &AnyObject,
        ) -> Option<Retained<AnyObject>> {
            POINTER_STYLE_CALLS.fetch_add(1, Ordering::Relaxed);
            if WANT_POINTER_LOCK.load(Ordering::Relaxed)
                && !SYS_POINTER_LOCKED.load(Ordering::Relaxed)
            {
                AnyClass::get(c"UIPointerStyle")
                    .and_then(|style_cls| unsafe { msg_send![style_cls, hiddenPointerStyle] })
            } else {
                None
            }
        }
    }
);

fn handle_hover(recognizer: &AnyObject) {
    // UIGestureRecognizerState: 1 = Began, 2 = Changed.
    let state: isize = unsafe { msg_send![recognizer, state] };
    if state != 1 && state != 2 {
        return;
    }
    let view: Option<Obj> = unsafe { msg_send![recognizer, view] };
    let Some(view) = view else {
        return;
    };
    let pt: Point = unsafe { msg_send![recognizer, locationInView: &*view] };
    let scale: f64 = get!(view, window)
        .and_then(|w| get!(w, screen))
        .map(|s| unsafe { msg_send![&*s, scale] })
        .unwrap_or_else(|| unsafe { msg_send![&*view, contentScaleFactor] });
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    if pt.x.is_finite() && pt.y.is_finite() {
        HOVER_X.store(((pt.x * scale) as f32).to_bits(), Ordering::Relaxed);
        HOVER_Y.store(((pt.y * scale) as f32).to_bits(), Ordering::Relaxed);
        HOVER_DIRTY.store(true, Ordering::Release);
    }
}

unsafe fn invalidate_pointer_interaction() {
    let inter_ptr = POINTER_INTERACTION.load(Ordering::Relaxed) as *const AnyObject;
    if !inter_ptr.is_null() {
        unsafe {
            let _: () = msg_send![inter_ptr, invalidate];
        }
    }
}

unsafe fn ensure_view_interactions(vc: &AnyObject, locked: bool) {
    let view: Option<Obj> = unsafe { msg_send![vc, view] };
    let Some(view) = view else {
        return;
    };
    let view_addr = &*view as *const AnyObject as usize;
    if HOVER_INSTALLED_VIEW.load(Ordering::Relaxed) != view_addr {
        if let Some(hover_cls) = AnyClass::get(c"UIHoverGestureRecognizer") {
            unsafe {
                let gr_proto = objc2::ffi::objc_getProtocol(c"UIGestureRecognizerDelegate".as_ptr());
                if !gr_proto.is_null() {
                    let cls_ptr = (HoverWatch::class() as *const AnyClass).cast_mut().cast();
                    let _ = objc2::ffi::class_addProtocol(cls_ptr, gr_proto);
                }
                let watch: Retained<HoverWatch> = msg_send![HoverWatch::class(), new];
                let alloc: Allocated<AnyObject> = msg_send![hover_cls, alloc];
                let rec: Option<Obj> = msg_send![
                    alloc,
                    initWithTarget: &*watch,
                    action: sel!(hoverChanged:)
                ];
                if let Some(rec) = rec {
                    // Match SDL3: restrict hover tracking to indirect pointer devices
                    // (`UITouchTypeIndirectPointer == 3`).
                    let set_types = sel!(setAllowedTouchTypes:);
                    let has_types: bool = msg_send![&*rec, respondsToSelector: set_types];
                    if has_types {
                        let num: Option<Obj> =
                            msg_send![class!(NSNumber), numberWithInteger: 3isize];
                        if let Some(num) = num {
                            let arr: Option<Obj> =
                                msg_send![class!(NSArray), arrayWithObject: &*num];
                            if let Some(arr) = arr {
                                let _: () = msg_send![&*rec, setAllowedTouchTypes: &*arr];
                            }
                        }
                    }
                    let _: () = msg_send![&*rec, setCancelsTouchesInView: Bool::NO];
                    let _: () = msg_send![&*rec, setDelaysTouchesBegan: Bool::NO];
                    let _: () = msg_send![&*rec, setDelaysTouchesEnded: Bool::NO];
                    let _: () = msg_send![&*rec, setDelegate: &*watch];
                    let _: () = msg_send![&*view, addGestureRecognizer: &*rec];
                    // Keep both target (unretained by UIGestureRecognizer) and recognizer alive.
                    HOVER_RECOGNIZER.store(Retained::into_raw(rec) as usize, Ordering::Relaxed);
                    std::mem::forget(watch);
                }
            }
        }
        if let Some(inter_cls) = AnyClass::get(c"UIPointerInteraction") {
            unsafe {
                // Declare formal UIPointerInteractionDelegate conformance before instantiating.
                let proto = objc2::ffi::objc_getProtocol(c"UIPointerInteractionDelegate".as_ptr());
                if !proto.is_null() {
                    let cls_ptr = (PointerStyleDelegate::class() as *const AnyClass)
                        .cast_mut()
                        .cast();
                    let _ = objc2::ffi::class_addProtocol(cls_ptr, proto);
                }
                let delegate: Retained<PointerStyleDelegate> =
                    msg_send![PointerStyleDelegate::class(), new];
                let alloc: Allocated<AnyObject> = msg_send![inter_cls, alloc];
                let inter: Option<Obj> = msg_send![alloc, initWithDelegate: &*delegate];
                if let Some(inter) = inter {
                    let add_sel = sel!(addInteraction:);
                    let responds: bool = msg_send![&*view, respondsToSelector: add_sel];
                    if responds {
                        let _: () = msg_send![&*view, addInteraction: &*inter];
                        POINTER_INTERACTION
                            .store(Retained::into_raw(inter) as usize, Ordering::Relaxed);
                        // UIPointerInteraction holds its delegate weakly, so keep it alive.
                        std::mem::forget(delegate);
                    }
                }
            }
        }
        HOVER_INSTALLED_VIEW.store(view_addr, Ordering::Relaxed);
        VIEW_INTERACTIONS_LOCKED.store(-1, Ordering::Relaxed);
    }

    let wanted = i8::from(locked);
    if VIEW_INTERACTIONS_LOCKED.swap(wanted, Ordering::Relaxed) != wanted {
        let rec_ptr = HOVER_RECOGNIZER.load(Ordering::Relaxed) as *const AnyObject;
        if !rec_ptr.is_null() {
            unsafe {
                let _: () = msg_send![rec_ptr, setEnabled: Bool::new(!locked)];
            }
        }
        // Keep UIPointerInteraction always enabled so in-view pointer updates immediately pick up
        // `hiddenPointerStyle` (in-world) or `nil` (menus) on `invalidate` without requiring the
        // pointer to leave and re-enter the view.
        let inter_ptr = POINTER_INTERACTION.load(Ordering::Relaxed) as *const AnyObject;
        if !inter_ptr.is_null() {
            unsafe {
                let _: () = msg_send![inter_ptr, invalidate];
            }
        }
    }
}

/// USB HID Usage IDs (Keyboard/Keypad Page 0x07) used by Apple's `GCKeyCode`.
const GC_KEY_MAP: &[(isize, KeyCode)] = &[
    (4, KeyCode::KeyA),
    (5, KeyCode::KeyB),
    (6, KeyCode::KeyC),
    (7, KeyCode::KeyD),
    (8, KeyCode::KeyE),
    (9, KeyCode::KeyF),
    (10, KeyCode::KeyG),
    (11, KeyCode::KeyH),
    (12, KeyCode::KeyI),
    (13, KeyCode::KeyJ),
    (14, KeyCode::KeyK),
    (15, KeyCode::KeyL),
    (16, KeyCode::KeyM),
    (17, KeyCode::KeyN),
    (18, KeyCode::KeyO),
    (19, KeyCode::KeyP),
    (20, KeyCode::KeyQ),
    (21, KeyCode::KeyR),
    (22, KeyCode::KeyS),
    (23, KeyCode::KeyT),
    (24, KeyCode::KeyU),
    (25, KeyCode::KeyV),
    (26, KeyCode::KeyW),
    (27, KeyCode::KeyX),
    (28, KeyCode::KeyY),
    (29, KeyCode::KeyZ),
    (30, KeyCode::Digit1),
    (31, KeyCode::Digit2),
    (32, KeyCode::Digit3),
    (33, KeyCode::Digit4),
    (34, KeyCode::Digit5),
    (35, KeyCode::Digit6),
    (36, KeyCode::Digit7),
    (37, KeyCode::Digit8),
    (38, KeyCode::Digit9),
    (39, KeyCode::Digit0),
    (40, KeyCode::Enter),
    (41, KeyCode::Escape),
    (42, KeyCode::Backspace),
    (43, KeyCode::Tab),
    (44, KeyCode::Space),
    (45, KeyCode::Minus),
    (46, KeyCode::Equal),
    (47, KeyCode::BracketLeft),
    (48, KeyCode::BracketRight),
    (49, KeyCode::Backslash),
    (51, KeyCode::Semicolon),
    (52, KeyCode::Quote),
    (53, KeyCode::Backquote),
    (54, KeyCode::Comma),
    (55, KeyCode::Period),
    (56, KeyCode::Slash),
    (57, KeyCode::CapsLock),
    (58, KeyCode::F1),
    (59, KeyCode::F2),
    (60, KeyCode::F3),
    (61, KeyCode::F4),
    (62, KeyCode::F5),
    (63, KeyCode::F6),
    (64, KeyCode::F7),
    (65, KeyCode::F8),
    (66, KeyCode::F9),
    (67, KeyCode::F10),
    (68, KeyCode::F11),
    (69, KeyCode::F12),
    (70, KeyCode::PrintScreen),
    (71, KeyCode::ScrollLock),
    (72, KeyCode::Pause),
    (73, KeyCode::Insert),
    (74, KeyCode::Home),
    (75, KeyCode::PageUp),
    (76, KeyCode::Delete),
    (77, KeyCode::End),
    (78, KeyCode::PageDown),
    (79, KeyCode::ArrowRight),
    (80, KeyCode::ArrowLeft),
    (81, KeyCode::ArrowDown),
    (82, KeyCode::ArrowUp),
    (83, KeyCode::NumLock),
    (84, KeyCode::NumpadDivide),
    (85, KeyCode::NumpadMultiply),
    (86, KeyCode::NumpadSubtract),
    (87, KeyCode::NumpadAdd),
    (88, KeyCode::NumpadEnter),
    (89, KeyCode::Numpad1),
    (90, KeyCode::Numpad2),
    (91, KeyCode::Numpad3),
    (92, KeyCode::Numpad4),
    (93, KeyCode::Numpad5),
    (94, KeyCode::Numpad6),
    (95, KeyCode::Numpad7),
    (96, KeyCode::Numpad8),
    (97, KeyCode::Numpad9),
    (98, KeyCode::Numpad0),
    (99, KeyCode::NumpadDecimal),
    (224, KeyCode::ControlLeft),
    (225, KeyCode::ShiftLeft),
    (226, KeyCode::AltLeft),
    (227, KeyCode::SuperLeft),
    (228, KeyCode::ControlRight),
    (229, KeyCode::ShiftRight),
    (230, KeyCode::AltRight),
    (231, KeyCode::SuperRight),
];

/// Returns true if any physical button on `GCMouse.current` is pressed right now.
pub fn is_hardware_mouse_button_down() -> bool {
    let mouse: Option<Obj> = unsafe { msg_send![class!(GCMouse), current] };
    let Some(m) = mouse else {
        return false;
    };
    let Some(input) = get!(m, mouseInput) else {
        return false;
    };
    pressed(get!(input, leftButton))
        || pressed(get!(input, rightButton))
        || pressed(get!(input, middleButton))
}

/// Discards accumulated hardware mouse motion and scroll deltas (used when switching between menus and world).
pub fn flush_hardware_mouse_deltas() {
    let _ = atomic_take_f32(&MOUSE_DX);
    let _ = atomic_take_f32(&MOUSE_DY);
    let _ = atomic_take_f32(&MOUSE_SCROLL_X);
    let _ = atomic_take_f32(&MOUSE_SCROLL_Y);
}

/// Polls any connected hardware keyboard (`GCKeyboard.coalescedKeyboard`) and mouse (`GCMouse.current`).
pub fn poll_hardware_input() -> HardwareInput {
    let mut out = HardwareInput::default();
    let keyboard: Option<Obj> = unsafe { msg_send![class!(GCKeyboard), coalescedKeyboard] };
    if let Some(kb) = keyboard
        && let Some(input) = get!(kb, keyboardInput)
    {
        out.keyboard_connected = true;
        for &(usage, code) in GC_KEY_MAP {
            let button: Option<Obj> = unsafe { msg_send![&*input, buttonForKeyCode: usage] };
            if pressed(button) {
                out.keys.push(code);
            }
        }
    }
    let mouse: Option<Obj> = unsafe { msg_send![class!(GCMouse), current] };
    if let Some(m) = mouse
        && let Some(input) = get!(m, mouseInput)
    {
        out.mouse_connected = true;
        MOUSE_CONNECTED.store(true, Ordering::Relaxed);
        let ptr = &*input as *const AnyObject as usize;
        if INSTALLED_MOUSE_INPUT.swap(ptr, Ordering::Relaxed) != ptr {
            let move_block = (&MOUSE_MOVED_BLOCK as *const StaticBlock).cast::<AnyObject>();
            let _: () = unsafe { msg_send![&*input, setMouseMovedHandler: move_block] };
            if let Some(scroll) = get!(input, scroll) {
                let scroll_block = (&MOUSE_SCROLL_BLOCK as *const StaticBlock).cast::<AnyObject>();
                let _: () = unsafe { msg_send![&*scroll, setValueChangedHandler: scroll_block] };
            }
            refresh_pointer_lock();
        }
        out.mouse_buttons = [
            pressed(get!(input, leftButton)),
            pressed(get!(input, rightButton)),
            pressed(get!(input, middleButton)),
        ];
    } else {
        MOUSE_CONNECTED.store(false, Ordering::Relaxed);
        INSTALLED_MOUSE_INPUT.store(0, Ordering::Relaxed);
    }
    out.mouse_delta = (atomic_take_f32(&MOUSE_DX), atomic_take_f32(&MOUSE_DY));
    out.mouse_scroll = (
        atomic_take_f32(&MOUSE_SCROLL_X),
        atomic_take_f32(&MOUSE_SCROLL_Y),
    );
    if HOVER_DIRTY.swap(false, Ordering::AcqRel) {
        out.hover_pos = Some((
            f32::from_bits(HOVER_X.load(Ordering::Relaxed)),
            f32::from_bits(HOVER_Y.load(Ordering::Relaxed)),
        ));
    }
    out
}

/// Whether the game currently wants pointer lock (in-world); drives `UIPointerInteraction`
/// fallback cursor hiding continuously while unlocked.
static WANT_POINTER_LOCK: AtomicBool = AtomicBool::new(false);
/// The exact value returned by `-[WinitUIViewController prefersPointerLocked]`.
static PREFERS_POINTER_LOCKED: AtomicBool = AtomicBool::new(false);
/// Last observed `UIWindowScene.pointerLockState.isLocked` reported by iPadOS.
static SYS_POINTER_LOCKED: AtomicBool = AtomicBool::new(false);
static POINTER_LOCK_INSTALLED_CLS: AtomicUsize = AtomicUsize::new(0);
/// Number of times UIKit itself queried `-[WinitUIViewController prefersPointerLocked]`.
static PREFERS_CB_CALLS: AtomicU32 = AtomicU32::new(0);
static PROBING_PREFERS: AtomicBool = AtomicBool::new(false);

static OS_VERSION_CACHE: OnceLock<String> = OnceLock::new();

unsafe fn ios_system_version() -> &'static str {
    OS_VERSION_CACHE
        .get_or_init(|| {
            let dev: Option<Obj> = unsafe { msg_send![class!(UIDevice), currentDevice] };
            dev.and_then(|d| string(&d, |o| get!(o, systemVersion)))
                .unwrap_or_else(|| "?".to_owned())
        })
        .as_str()
}

unsafe fn obj_class_name(obj: &AnyObject) -> String {
    let cls = unsafe { objc2::ffi::object_getClass((obj as *const AnyObject).cast_mut().cast()) };
    if cls.is_null() {
        return "nil".to_owned();
    }
    let cname = unsafe { objc2::ffi::class_getName(cls) };
    if cname.is_null() {
        return "?".to_owned();
    }
    unsafe { CStr::from_ptr(cname) }
        .to_string_lossy()
        .into_owned()
}

struct PointerLockState {
    want_lock: bool,
    applied: Option<bool>,
    next_retry_at: Option<Instant>,
    auto_retries: u8,
    last_probe_at: Option<Instant>,
    sys_locked: bool,
    app_has_delegate: bool,
    scene_key_matches: bool,
    win_scene_kind: u8,
    win_is_key: bool,
    has_pls: bool,
    scene_fullscreen: bool,
    sa_top: i32,
    sa_bottom: i32,
    resizing_restrictions: isize,
    view_first_responder: bool,
    assistive_touch: bool,
    mouse_count: usize,
    scene_window_count: usize,
    activation_state: isize,
    vc_prefers: bool,
    vc_class: String,
    windows_summary: String,
}

impl PointerLockState {
    const fn new() -> Self {
        Self {
            want_lock: false,
            applied: None,
            next_retry_at: None,
            auto_retries: 0,
            last_probe_at: None,
            sys_locked: false,
            app_has_delegate: false,
            scene_key_matches: false,
            win_scene_kind: 0,
            win_is_key: false,
            has_pls: false,
            scene_fullscreen: false,
            sa_top: 0,
            sa_bottom: 0,
            resizing_restrictions: -1,
            view_first_responder: false,
            assistive_touch: false,
            mouse_count: 0,
            scene_window_count: 0,
            activation_state: -99,
            vc_prefers: false,
            vc_class: String::new(),
            windows_summary: String::new(),
        }
    }
}

static POINTER_LOCK_STATE: Mutex<PointerLockState> = Mutex::new(PointerLockState::new());

/// Returns whether iPadOS currently reports the scene pointer as locked (`UIPointerLockState.isLocked`).
pub fn is_pointer_locked() -> bool {
    SYS_POINTER_LOCKED.load(Ordering::Relaxed)
}

/// Marks the iPadOS pointer-lock state dirty so the next `set_pointer_locked()` call immediately
/// re-requests `setNeedsUpdateOfPrefersPointerLocked` (e.g. after returning from background,
/// Control Center, window resize/maximize, click-up while unlocked, or regaining focus).
pub fn refresh_pointer_lock() {
    REFRESH_PENDING.store(true, Ordering::Release);
    if let Ok(mut st) = POINTER_LOCK_STATE.try_lock() {
        st.applied = None;
        st.next_retry_at = None;
        st.auto_retries = 0;
        st.last_probe_at = None;
    }
}

/// Concise F3 debug readout showing live UIKit pointer-lock diagnostics on iPadOS.
pub fn pointer_lock_debug_status() -> String {
    let st = POINTER_LOCK_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let vc = if st.vc_class.is_empty() {
        "none"
    } else {
        st.vc_class.as_str()
    };
    let os = unsafe { ios_system_version() };
    format!(
        "Pointer: lock={} want={} pref={} cb={} os={} ad={} sd={}/{}/{} ws={} key={} sk={} act={} pls={} full={} sa={}/{} rr={} at={} m={} n={} nw={} st={} vc={}",
        if st.sys_locked { "ON" } else { "OFF" },
        if st.want_lock { "ON" } else { "OFF" },
        u8::from(st.vc_prefers),
        PREFERS_CB_CALLS.load(Ordering::Relaxed),
        os,
        u8::from(st.app_has_delegate),
        SCENE_CONNECTED_COUNT.load(Ordering::Relaxed),
        SCENE_INIT_WS_COUNT.load(Ordering::Relaxed),
        SCENE_ADOPTED_COUNT.load(Ordering::Relaxed),
        st.win_scene_kind,
        u8::from(st.win_is_key),
        u8::from(st.scene_key_matches),
        st.activation_state,
        u8::from(st.has_pls),
        u8::from(st.scene_fullscreen),
        st.sa_top,
        st.sa_bottom,
        st.resizing_restrictions,
        u8::from(st.assistive_touch),
        st.mouse_count,
        POINTER_LOCK_NOTIFY_COUNT.load(Ordering::Relaxed),
        st.scene_window_count,
        POINTER_STYLE_CALLS.load(Ordering::Relaxed),
        vc,
    )
}

static BUNDLE_PLIST_DIAG: OnceLock<String> = OnceLock::new();

unsafe fn bundle_plist_diag() -> &'static str {
    BUNDLE_PLIST_DIAG
        .get_or_init(|| {
            let bundle: Option<Obj> = unsafe { msg_send![class!(NSBundle), mainBundle] };
            let Some(bundle) = bundle else {
                return "sdk=none".to_owned();
            };
            let str_key = |k: &str| -> Option<String> {
                let key = NSString::from_str(k);
                let val: Option<Obj> =
                    unsafe { msg_send![&*bundle, objectForInfoDictionaryKey: &*key] };
                let val = val?;
                let desc: Option<Retained<NSString>> = unsafe { msg_send![&*val, description] };
                desc.map(|s| s.to_string())
            };
            let bool_key = |k: &str| -> u8 {
                let key = NSString::from_str(k);
                let val: Option<Obj> =
                    unsafe { msg_send![&*bundle, objectForInfoDictionaryKey: &*key] };
                val.map_or(0, |v| {
                    let sel_bool = sel!(boolValue);
                    let has: bool = unsafe { msg_send![&*v, respondsToSelector: sel_bool] };
                    if has {
                        let b: bool = unsafe { msg_send![&*v, boolValue] };
                        u8::from(b)
                    } else {
                        1
                    }
                })
            };
            let sdk = str_key("DTSDKName").unwrap_or_else(|| "none".to_owned());
            let ind = bool_key("UIApplicationSupportsIndirectInputEvents");
            let rfs = bool_key("UIRequiresFullScreen");
            let gm = bool_key("GCSupportsGameMode");
            format!("sdk={sdk} ind={ind} rfs={rfs} gm={gm}")
        })
        .as_str()
}

/// Second F3 debug line showing runtime `Info.plist` SDK/pointer flags and all scene windows.
pub fn pointer_lock_windows_status() -> String {
    let diag = unsafe { bundle_plist_diag() };
    let st = POINTER_LOCK_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if st.windows_summary.is_empty() {
        format!("SceneWin: {diag} | none")
    } else {
        format!("SceneWin: {diag} | {}", st.windows_summary)
    }
}

unsafe extern "C" fn prefers_pointer_locked(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    if !PROBING_PREFERS.load(Ordering::Relaxed) {
        PREFERS_CB_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    Bool::new(PREFERS_POINTER_LOCKED.load(Ordering::Relaxed))
}

unsafe extern "C" fn child_vc_for_pointer_lock(
    _this: *mut AnyObject,
    _cmd: Sel,
) -> *mut AnyObject {
    std::ptr::null_mut()
}

/// Installs `prefersPointerLocked` and `childViewControllerForPointerLock` on non-native VC
/// classes once (fallback when `HorizonRootViewController` is not the root VC).
unsafe fn ensure_pointer_lock_methods(vc: &AnyObject) -> bool {
    if unsafe { is_horizon_root_vc_ptr(vc) } {
        return false;
    }
    let cls = unsafe { objc2::ffi::object_getClass((vc as *const AnyObject).cast_mut().cast()) };
    if cls.is_null() {
        return false;
    }
    let cls_addr = cls as usize;
    let installed_now = POINTER_LOCK_INSTALLED_CLS.swap(cls_addr, Ordering::Relaxed) != cls_addr;
    if installed_now {
        let pref_imp: Imp = unsafe {
            std::mem::transmute::<unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool, Imp>(
                prefers_pointer_locked,
            )
        };
        unsafe {
            objc2::ffi::class_replaceMethod(
                cls.cast_mut(),
                sel!(prefersPointerLocked),
                pref_imp,
                c"B@:".as_ptr(),
            );
        }
        let child_imp: Imp = unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject,
                Imp,
            >(child_vc_for_pointer_lock)
        };
        unsafe {
            objc2::ffi::class_replaceMethod(
                cls.cast_mut(),
                sel!(childViewControllerForPointerLock),
                child_imp,
                c"@@:".as_ptr(),
            );
        }
    }
    installed_now
}

unsafe fn root_view_controller() -> Option<Obj> {
    let app: Option<Obj> = unsafe { msg_send![class!(UIApplication), sharedApplication] };
    let app = app?;
    let horizon_vc_cls = HORIZON_ROOT_VC_CLS.get().copied();
    let winit_vc_cls = AnyClass::get(c"WinitUIViewController");
    let mut primary_vc: Option<Obj> = None;
    let mut key_vc: Option<Obj> = None;
    let mut any_vc: Option<Obj> = None;

    fn inspect_window(
        win: &AnyObject,
        horizon_vc_cls: Option<&AnyClass>,
        winit_vc_cls: Option<&AnyClass>,
        primary_vc: &mut Option<Obj>,
        key_vc: &mut Option<Obj>,
        any_vc: &mut Option<Obj>,
    ) -> Option<Obj> {
        let vc: Option<Obj> = unsafe { msg_send![win, rootViewController] };
        let vc = vc?;
        let is_key: bool = unsafe { msg_send![win, isKeyWindow] };
        let is_horizon =
            horizon_vc_cls.is_some_and(|cls| unsafe { msg_send![&*vc, isKindOfClass: cls] });
        let is_winit =
            winit_vc_cls.is_some_and(|cls| unsafe { msg_send![&*vc, isKindOfClass: cls] });
        let is_ours = is_horizon || is_winit;
        if is_ours && is_key {
            return Some(vc);
        }
        if is_ours && primary_vc.is_none() {
            *primary_vc = Some(vc);
        } else if is_key && key_vc.is_none() {
            *key_vc = Some(vc);
        } else if any_vc.is_none() {
            *any_vc = Some(vc);
        }
        None
    }

    let scenes: Option<Obj> = unsafe { msg_send![&*app, connectedScenes] };
    if let Some(scenes) = scenes {
        let all: Option<Obj> = unsafe { msg_send![&*scenes, allObjects] };
        if let Some(all) = all {
            let count: usize = unsafe { msg_send![&*all, count] };
            let window_scene_cls = class!(UIWindowScene);
            for i in 0..count {
                let scene: Option<Obj> = unsafe { msg_send![&*all, objectAtIndex: i] };
                let Some(scene) = scene else { continue };
                let is_ws: bool = unsafe { msg_send![&*scene, isKindOfClass: window_scene_cls] };
                if !is_ws {
                    continue;
                }
                let windows: Option<Obj> = unsafe { msg_send![&*scene, windows] };
                let Some(windows) = windows else { continue };
                let wcount: usize = unsafe { msg_send![&*windows, count] };
                for j in 0..wcount {
                    let win: Option<Obj> = unsafe { msg_send![&*windows, objectAtIndex: j] };
                    let Some(win) = win else { continue };
                    if let Some(found) = inspect_window(
                        &win,
                        horizon_vc_cls,
                        winit_vc_cls,
                        &mut primary_vc,
                        &mut key_vc,
                        &mut any_vc,
                    ) {
                        return Some(found);
                    }
                }
            }
        }
    }
    if let Some(vc) = primary_vc {
        return Some(vc);
    }
    let legacy_windows: Option<Obj> = unsafe { msg_send![&*app, windows] };
    if let Some(windows) = legacy_windows {
        let wcount: usize = unsafe { msg_send![&*windows, count] };
        for j in 0..wcount {
            let win: Option<Obj> = unsafe { msg_send![&*windows, objectAtIndex: j] };
            let Some(win) = win else { continue };
            if let Some(found) = inspect_window(
                &win,
                horizon_vc_cls,
                winit_vc_cls,
                &mut primary_vc,
                &mut key_vc,
                &mut any_vc,
            ) {
                return Some(found);
            }
        }
    }
    if let Some(vc) = primary_vc.or(key_vc).or(any_vc) {
        return Some(vc);
    }
    let win: Option<Obj> = unsafe { msg_send![&*app, keyWindow] };
    win.and_then(|w| unsafe { msg_send![&*w, rootViewController] })
}

struct SceneLockInfo {
    /// `0` = no `windowScene`, `1` = delegate-less fallback scene, `2` = delegated `UIWindowScene`.
    win_scene_kind: u8,
    app_has_delegate: bool,
    scene_key_matches: bool,
    win_is_key: bool,
    is_locked: bool,
    has_pls: bool,
    scene_fullscreen: bool,
    sa_top: i32,
    sa_bottom: i32,
    resizing_restrictions: isize,
    view_first_responder: bool,
    assistive_touch: bool,
    mouse_count: usize,
    scene_window_count: usize,
    activation_state: isize,
    windows_summary: String,
}

/// Inspects live `UIWindowScene` / `UIPointerLockState` diagnostics, repairing a detached or
/// delegate-less fallback `UIWindow.windowScene` or non-key active window if needed so
/// `setNeedsUpdateOfPrefersPointerLocked` reaches the foreground `UIWindowScene`.
unsafe fn read_scene_lock_state(vc: &AnyObject) -> SceneLockInfo {
    let app: Option<Obj> = unsafe { msg_send![class!(UIApplication), sharedApplication] };
    let app_has_delegate = app.as_ref().is_some_and(|a| get!(a, delegate).is_some());
    let assistive_touch = unsafe { UIAccessibilityIsAssistiveTouchRunning() }.as_bool();
    let mice: Option<Obj> = unsafe { msg_send![class!(GCMouse), mice] };
    let mouse_count: usize = mice.map_or(0, |list| unsafe { msg_send![&*list, count] });

    let view: Option<Obj> = get!(vc, view);
    let view_first_responder = view
        .as_ref()
        .is_some_and(|v| unsafe { msg_send![&**v, isFirstResponder] });
    let win: Option<Obj> = view.as_ref().and_then(|v| get!(v, window));
    let (sa_top, sa_bottom) = win.as_ref().map_or((0, 0), |w| {
        let sa_sel = sel!(safeAreaInsets);
        let has_sa: bool = unsafe { msg_send![&**w, respondsToSelector: sa_sel] };
        if has_sa {
            let insets: EdgeInsets = unsafe { msg_send![&**w, safeAreaInsets] };
            (insets.top.round() as i32, insets.bottom.round() as i32)
        } else {
            (0, 0)
        }
    });
    let mut win_is_key = win
        .as_ref()
        .is_some_and(|w| unsafe { msg_send![&**w, isKeyWindow] });
    let mut scene: Option<Obj> = win.as_ref().and_then(|w| get!(w, windowScene));
    let needs_attach = scene
        .as_ref()
        .is_none_or(|s| get!(s, delegate).is_none());

    if needs_attach {
        let (tracked_scene, tracked_del) = {
            let st = SCENE_STATE.lock().unwrap_or_else(|e| e.into_inner());
            (
                st.scene.as_ref().map(|s| s.0.clone()),
                st.delegate.as_ref().map(|d| d.0.clone()),
            )
        };
        let target_scene = tracked_scene.or_else(|| {
            let app = app.as_ref()?;
            let scenes = get!(app, connectedScenes)?;
            let all = get!(scenes, allObjects)?;
            let count: usize = unsafe { msg_send![&*all, count] };
            let mut fallback: Option<Obj> = None;
            for i in 0..count {
                let s: Option<Obj> = unsafe { msg_send![&*all, objectAtIndex: i] };
                let Some(s) = s else { continue };
                if !unsafe { is_app_window_scene(&s) } {
                    continue;
                }
                let act: isize = unsafe { msg_send![&*s, activationState] };
                if act == 0 && get!(s, delegate).is_some() {
                    return Some(s);
                }
                if fallback.is_none() {
                    fallback = Some(s);
                }
            }
            fallback
        });
        if let (Some(w), Some(s)) = (win.as_ref(), target_scene.as_ref()) {
            unsafe {
                attach_window_inner(&**w, &**s, tracked_del.as_deref());
                let _: () = msg_send![&**w, makeKeyAndVisible];
            }
            scene = get!(w, windowScene);
            win_is_key = unsafe { msg_send![&**w, isKeyWindow] };
        } else if scene.is_none() {
            scene = target_scene;
        }
    }

    let win_scene_kind = match win.as_ref().and_then(|w| get!(w, windowScene)) {
        None => 0,
        Some(s) if get!(s, delegate).is_some() => 2,
        Some(_) => 1,
    };

    let Some(scene) = scene else {
        return SceneLockInfo {
            win_scene_kind,
            app_has_delegate,
            scene_key_matches: false,
            win_is_key,
            is_locked: false,
            has_pls: false,
            scene_fullscreen: false,
            sa_top,
            sa_bottom,
            resizing_restrictions: -1,
            view_first_responder,
            assistive_touch,
            mouse_count,
            scene_window_count: 0,
            activation_state: -99,
            windows_summary: String::new(),
        };
    };

    let act: isize = unsafe { msg_send![&*scene, activationState] };
    if act == 0 && !win_is_key && let Some(w) = win.as_ref() {
        unsafe {
            let _: () = msg_send![&**w, makeKeyWindow];
        }
        win_is_key = unsafe { msg_send![&**w, isKeyWindow] };
    }

    let scene_key_matches = {
        let kw_sel = sel!(keyWindow);
        let has_kw: bool = unsafe { msg_send![&*scene, respondsToSelector: kw_sel] };
        if has_kw
            && let (Some(skw), Some(w)) = (get!(scene, keyWindow), win.as_ref())
        {
            std::ptr::eq(&*skw as *const AnyObject, &**w as *const AnyObject)
        } else {
            false
        }
    };

    let (scene_window_count, windows_summary) = if let Some(wins) = get!(scene, windows) {
        let wcount: usize = unsafe { msg_send![&*wins, count] };
        let mut parts = Vec::with_capacity(wcount.min(4));
        for idx in 0..wcount.min(4) {
            let w: Option<Obj> = unsafe { msg_send![&*wins, objectAtIndex: idx] };
            let Some(w) = w else { continue };
            let wcls = unsafe { obj_class_name(&w) };
            let is_k: bool = unsafe { msg_send![&*w, isKeyWindow] };
            let is_h: bool = unsafe { msg_send![&*w, isHidden] };
            let rvc_name = get!(w, rootViewController)
                .map(|r| unsafe { obj_class_name(&r) })
                .unwrap_or_else(|| "nil".to_owned());
            parts.push(format!(
                "{wcls}({}{},vc={rvc_name})",
                if is_k { "K" } else { "-" },
                if is_h { "H" } else { "V" },
            ));
        }
        (wcount, parts.join(" | "))
    } else {
        (0, String::new())
    };
    let scene_fullscreen = if let (Some(space), Some(screen)) =
        (get!(scene, coordinateSpace), get!(scene, screen))
    {
        let sb: Rect = unsafe { msg_send![&*space, bounds] };
        let scr: Rect = unsafe { msg_send![&*screen, bounds] };
        ((sb.size.width - scr.size.width).abs() <= 1.5
            && (sb.size.height - scr.size.height).abs() <= 1.5)
            || ((sb.size.width - scr.size.height).abs() <= 1.5
                && (sb.size.height - scr.size.width).abs() <= 1.5)
    } else {
        false
    };
    let resizing_restrictions = {
        let eg_sel = sel!(effectiveGeometry);
        let has_eg: bool = unsafe { msg_send![&*scene, respondsToSelector: eg_sel] };
        if has_eg && let Some(geom) = get!(scene, effectiveGeometry) {
            let rr_sel = sel!(resizingRestrictions);
            let has_rr: bool = unsafe { msg_send![&*geom, respondsToSelector: rr_sel] };
            if has_rr {
                unsafe { msg_send![&*geom, resizingRestrictions] }
            } else {
                -1
            }
        } else {
            -1
        }
    };

    let lock_sel = sel!(pointerLockState);
    let responds: bool = unsafe { msg_send![&*scene, respondsToSelector: lock_sel] };
    if !responds {
        return SceneLockInfo {
            win_scene_kind,
            app_has_delegate,
            scene_key_matches,
            win_is_key,
            is_locked: false,
            has_pls: false,
            scene_fullscreen,
            sa_top,
            sa_bottom,
            resizing_restrictions,
            view_first_responder,
            assistive_touch,
            mouse_count,
            scene_window_count,
            activation_state: act,
            windows_summary,
        };
    }
    let state: Option<Obj> = get!(scene, pointerLockState);
    let has_pls = state.is_some();
    let is_locked = state.is_some_and(|st| unsafe { msg_send![&*st, isLocked] });
    SceneLockInfo {
        win_scene_kind,
        app_has_delegate,
        scene_key_matches,
        win_is_key,
        is_locked,
        has_pls,
        scene_fullscreen,
        sa_top,
        sa_bottom,
        resizing_restrictions,
        view_first_responder,
        assistive_touch,
        mouse_count,
        scene_window_count,
        activation_state: act,
        windows_summary,
    }
}

unsafe fn request_pointer_lock_update(vc: &AnyObject) {
    let update_sel = sel!(setNeedsUpdateOfPrefersPointerLocked);
    let responds: bool = unsafe { msg_send![vc, respondsToSelector: update_sel] };
    if responds {
        unsafe {
            let _: () = msg_send![vc, setNeedsUpdateOfPrefersPointerLocked];
        }
    }
}

/// Locks and hides the iPadOS/iOS hardware mouse pointer while in-world (`prefersPointerLocked`),
/// unlocks/restores it when a menu or inventory screen is open, and toggles
/// `UIHoverGestureRecognizer` and `UIPointerInteraction` (`hiddenPointerStyle` fallback).
/// Keeps `prefersPointerLocked == YES` steadily while in-world (matching SDL3 / vkQuake-ios)
/// without flipping back to `NO` on re-asks.
pub fn set_pointer_locked(locked: bool) {
    WANT_POINTER_LOCK.store(locked, Ordering::Relaxed);
    let now = Instant::now();
    let mut st = POINTER_LOCK_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if REFRESH_PENDING.swap(false, Ordering::AcqRel) {
        st.applied = None;
        st.next_retry_at = None;
        st.auto_retries = 0;
        st.last_probe_at = None;
    }
    unsafe {
        let Some(vc) = root_view_controller() else {
            st.applied = None;
            return;
        };
        ensure_view_interactions(&vc, locked);
        let installed_now = ensure_pointer_lock_methods(&vc);
        let cur_vc_name = obj_class_name(&vc);
        if st.vc_class != cur_vc_name {
            st.vc_class = cur_vc_name;
        }

        let need_probe = installed_now
            || st.want_lock != locked
            || st.applied != Some(locked)
            || st
                .last_probe_at
                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(120));
        if need_probe {
            let info = read_scene_lock_state(&vc);
            let was_sys_locked = st.sys_locked;
            st.win_scene_kind = info.win_scene_kind;
            st.app_has_delegate = info.app_has_delegate;
            st.scene_key_matches = info.scene_key_matches;
            st.win_is_key = info.win_is_key;
            st.sys_locked = info.is_locked;
            st.has_pls = info.has_pls;
            st.scene_fullscreen = info.scene_fullscreen;
            st.sa_top = info.sa_top;
            st.sa_bottom = info.sa_bottom;
            st.resizing_restrictions = info.resizing_restrictions;
            st.view_first_responder = info.view_first_responder;
            st.assistive_touch = info.assistive_touch;
            st.mouse_count = info.mouse_count;
            st.scene_window_count = info.scene_window_count;
            st.activation_state = info.activation_state;
            st.windows_summary = info.windows_summary;
            st.last_probe_at = Some(now);
            SYS_POINTER_LOCKED.store(info.is_locked, Ordering::Relaxed);
            if was_sys_locked != info.is_locked {
                invalidate_pointer_interaction();
            }
            PROBING_PREFERS.store(true, Ordering::Relaxed);
            let vc_pref: bool = msg_send![&*vc, prefersPointerLocked];
            PROBING_PREFERS.store(false, Ordering::Relaxed);
            st.vc_prefers = vc_pref;
            // If iPadOS dropped pointer lock while we still want it, schedule a single prompt re-ask.
            if locked && was_sys_locked && !info.is_locked {
                st.auto_retries = 0;
                st.next_retry_at = Some(now + Duration::from_millis(150));
            }
        }

        if !locked {
            st.want_lock = false;
            st.next_retry_at = None;
            st.auto_retries = 0;
            if installed_now
                || st.applied != Some(false)
                || PREFERS_POINTER_LOCKED.load(Ordering::Relaxed)
            {
                PREFERS_POINTER_LOCKED.store(false, Ordering::Relaxed);
                request_pointer_lock_update(&vc);
                st.applied = Some(false);
                st.vc_prefers = false;
            }
            return;
        }

        if !st.want_lock {
            st.want_lock = true;
            st.applied = None;
            st.next_retry_at = None;
            st.auto_retries = 0;
        }

        if st.sys_locked {
            st.next_retry_at = None;
            st.auto_retries = 0;
            st.applied = Some(true);
            if !PREFERS_POINTER_LOCKED.load(Ordering::Relaxed) {
                PREFERS_POINTER_LOCKED.store(true, Ordering::Relaxed);
                request_pointer_lock_update(&vc);
            }
            return;
        }

        // Wait for mouse button release (e.g. clicking "Play" or "Back to Game") before asking
        // SpringBoard for pointer lock; mid-click requests are often declined on iPadOS 26.
        let button_down = is_hardware_mouse_button_down();
        if !button_down && (installed_now || st.applied != Some(true)) {
            PREFERS_POINTER_LOCKED.store(true, Ordering::Relaxed);
            request_pointer_lock_update(&vc);
            st.applied = Some(true);
            st.vc_prefers = true;
            st.next_retry_at = Some(now + Duration::from_millis(500));
            invalidate_pointer_interaction();
            return;
        }

        // At most one automatic follow-up (500ms after initial ask or after a lock drop);
        // further re-asks are driven by grantable events (click-up, focus, resize, resume).
        if !button_down
            && MOUSE_CONNECTED.load(Ordering::Relaxed)
            && st.auto_retries < 1
            && st.next_retry_at.is_some_and(|t| now >= t)
        {
            st.auto_retries = st.auto_retries.saturating_add(1);
            st.next_retry_at = None;
            PREFERS_POINTER_LOCKED.store(true, Ordering::Relaxed);
            request_pointer_lock_update(&vc);
            st.vc_prefers = true;
            invalidate_pointer_interaction();
        }
    }
}
