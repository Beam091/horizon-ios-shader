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
use objc2::runtime::{AnyClass, Bool, Imp, NSObject, Sel};
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
static SCENE_CONNECTED_COUNT: AtomicU32 = AtomicU32::new(0);
static SCENE_ADOPTED_COUNT: AtomicU32 = AtomicU32::new(0);
static SCENE_LIFECYCLE_INSTALLED: Once = Once::new();
static POINTER_LOCK_OBSERVER_INSTALLED: Once = Once::new();
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

type MakeKeyAndVisibleFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);
static ORIG_MAKE_KEY_AND_VISIBLE: OnceLock<MakeKeyAndVisibleFn> = OnceLock::new();

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
    if let Some(del) = delegate {
        unsafe {
            let _: () = msg_send![del, setWindow: win];
        }
    }
}

unsafe extern "C-unwind" fn swizzled_make_key_and_visible(this: *mut AnyObject, cmd: Sel) {
    let is_winit = !this.is_null()
        && AnyClass::get(c"WinitUIWindow")
            .is_some_and(|cls| unsafe { msg_send![this, isKindOfClass: cls] });
    if is_winit && unsafe { window_needs_scene(this) } {
        let (scene, delegate) = {
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

/// Registers `HorizonSceneDelegate` and installs `UIScene` window adoption for `winit 0.30`
/// before `UIApplicationMain` runs. Must be called on the main thread at process startup.
pub fn install_scene_lifecycle() {
    SCENE_LIFECYCLE_INSTALLED.call_once(|| unsafe {
        let cls = HorizonSceneDelegate::class();
        let cls_ptr = (cls as *const AnyClass).cast_mut().cast();
        for proto_name in [c"UISceneDelegate", c"UIWindowSceneDelegate"] {
            let proto = objc2::ffi::objc_getProtocol(proto_name.as_ptr());
            if !proto.is_null() {
                let _ = objc2::ffi::class_addProtocol(cls_ptr, proto);
            }
        }
        ensure_pointer_lock_observer();

        // Only swizzle `-[UIWindow makeKeyAndVisible]` when Info.plist declares a
        // `UIApplicationSceneManifest`; otherwise UIKit runs without a scene delegate.
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
        let win_cls_ptr = (ui_window_cls as *const AnyClass).cast_mut().cast();
        let method = objc2::ffi::class_getInstanceMethod(win_cls_ptr, sel!(makeKeyAndVisible));
        if method.is_null() {
            return;
        }
        let Some(orig_imp) = objc2::ffi::method_getImplementation(method) else {
            return;
        };
        let orig_fn: MakeKeyAndVisibleFn =
            std::mem::transmute::<Imp, MakeKeyAndVisibleFn>(orig_imp);
        let _ = ORIG_MAKE_KEY_AND_VISIBLE.set(orig_fn);
        let new_imp: Imp =
            std::mem::transmute::<MakeKeyAndVisibleFn, Imp>(swizzled_make_key_and_visible);
        let _ = objc2::ffi::method_setImplementation(method.cast_mut(), new_imp);
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
            _interaction: &AnyObject,
            _request: &AnyObject,
            default_region: &AnyObject,
        ) -> Option<Retained<AnyObject>> {
            if WANT_POINTER_LOCK.load(Ordering::Relaxed)
                && !SYS_POINTER_LOCKED.load(Ordering::Relaxed)
            {
                unsafe { Retained::retain((default_region as *const AnyObject).cast_mut()) }
            } else {
                None
            }
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
    let scale: f64 = unsafe { msg_send![&*view, contentScaleFactor] };
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
                let watch: Retained<HoverWatch> = msg_send![HoverWatch::class(), new];
                let alloc: Allocated<AnyObject> = msg_send![hover_cls, alloc];
                let rec: Option<Obj> = msg_send![
                    alloc,
                    initWithTarget: &*watch,
                    action: sel!(hoverChanged:)
                ];
                if let Some(rec) = rec {
                    // WinitUIView already implements UIGestureRecognizerDelegate returning YES
                    // for simultaneous recognition.
                    let _: () = msg_send![&*rec, setDelegate: &*view];
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
/// fallback cursor hiding continuously even across `false -> true` re-ask pulses.
static WANT_POINTER_LOCK: AtomicBool = AtomicBool::new(false);
/// The exact value returned by `-[WinitUIViewController prefersPointerLocked]`.
static PREFERS_POINTER_LOCKED: AtomicBool = AtomicBool::new(false);
/// Last observed `UIWindowScene.pointerLockState.isLocked` reported by iPadOS.
static SYS_POINTER_LOCKED: AtomicBool = AtomicBool::new(false);
static POINTER_LOCK_INSTALLED_CLS: AtomicUsize = AtomicUsize::new(0);
/// Number of times UIKit itself queried `-[WinitUIViewController prefersPointerLocked]`.
static PREFERS_CB_CALLS: AtomicU32 = AtomicU32::new(0);
static PROBING_PREFERS: AtomicBool = AtomicBool::new(false);

struct PointerLockState {
    want_lock: bool,
    applied: Option<bool>,
    pulse_until: Option<Instant>,
    next_retry_at: Option<Instant>,
    auto_retries: u8,
    last_probe_at: Option<Instant>,
    sys_locked: bool,
    win_scene_kind: u8,
    win_is_key: bool,
    has_pls: bool,
    scene_fullscreen: bool,
    assistive_touch: bool,
    mouse_count: usize,
    scene_window_count: usize,
    activation_state: isize,
    vc_prefers: bool,
    vc_class: String,
    pulse_count: u32,
}

impl PointerLockState {
    const fn new() -> Self {
        Self {
            want_lock: false,
            applied: None,
            pulse_until: None,
            next_retry_at: None,
            auto_retries: 0,
            last_probe_at: None,
            sys_locked: false,
            win_scene_kind: 0,
            win_is_key: false,
            has_pls: false,
            scene_fullscreen: false,
            assistive_touch: false,
            mouse_count: 0,
            scene_window_count: 0,
            activation_state: -99,
            vc_prefers: false,
            vc_class: String::new(),
            pulse_count: 0,
        }
    }
}

static POINTER_LOCK_STATE: Mutex<PointerLockState> = Mutex::new(PointerLockState::new());

/// Returns whether iPadOS currently reports the scene pointer as locked (`UIPointerLockState.isLocked`).
pub fn is_pointer_locked() -> bool {
    SYS_POINTER_LOCKED.load(Ordering::Relaxed)
}

/// Marks the iPadOS pointer-lock state dirty so the next `set_pointer_locked()` call immediately
/// re-asks via a `false -> true` edge (e.g. after returning from background, Control Center,
/// window resize/maximize, click-up while unlocked, or regaining focus).
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
    format!(
        "Pointer: lock={} want={} pref={} cb={} sd={}/{} ws={} key={} act={} pls={} full={} at={} m={} nw={} st={} p={} vc={}",
        if st.sys_locked { "ON" } else { "OFF" },
        if st.want_lock { "ON" } else { "OFF" },
        u8::from(st.vc_prefers),
        PREFERS_CB_CALLS.load(Ordering::Relaxed),
        SCENE_CONNECTED_COUNT.load(Ordering::Relaxed),
        SCENE_ADOPTED_COUNT.load(Ordering::Relaxed),
        st.win_scene_kind,
        u8::from(st.win_is_key),
        st.activation_state,
        u8::from(st.has_pls),
        u8::from(st.scene_fullscreen),
        u8::from(st.assistive_touch),
        st.mouse_count,
        st.scene_window_count,
        POINTER_STYLE_CALLS.load(Ordering::Relaxed),
        st.pulse_count,
        vc,
    )
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

unsafe fn root_view_controller() -> Option<Obj> {
    let app: Option<Obj> = unsafe { msg_send![class!(UIApplication), sharedApplication] };
    let app = app?;
    let winit_vc_cls = AnyClass::get(c"WinitUIViewController");
    let mut winit_vc: Option<Obj> = None;
    let mut key_vc: Option<Obj> = None;
    let mut any_vc: Option<Obj> = None;

    fn inspect_window(
        win: &AnyObject,
        winit_vc_cls: Option<&AnyClass>,
        winit_vc: &mut Option<Obj>,
        key_vc: &mut Option<Obj>,
        any_vc: &mut Option<Obj>,
    ) -> Option<Obj> {
        let vc: Option<Obj> = unsafe { msg_send![win, rootViewController] };
        let vc = vc?;
        let is_key: bool = unsafe { msg_send![win, isKeyWindow] };
        let is_winit =
            winit_vc_cls.is_some_and(|cls| unsafe { msg_send![&*vc, isKindOfClass: cls] });
        if is_winit && is_key {
            return Some(vc);
        }
        if is_winit && winit_vc.is_none() {
            *winit_vc = Some(vc);
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
                        winit_vc_cls,
                        &mut winit_vc,
                        &mut key_vc,
                        &mut any_vc,
                    ) {
                        return Some(found);
                    }
                }
            }
        }
    }
    if let Some(vc) = winit_vc {
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
                winit_vc_cls,
                &mut winit_vc,
                &mut key_vc,
                &mut any_vc,
            ) {
                return Some(found);
            }
        }
    }
    if let Some(vc) = winit_vc.or(key_vc).or(any_vc) {
        return Some(vc);
    }
    let win: Option<Obj> = unsafe { msg_send![&*app, keyWindow] };
    win.and_then(|w| unsafe { msg_send![&*w, rootViewController] })
}

struct SceneLockInfo {
    /// `0` = no `windowScene`, `1` = delegate-less fallback scene, `2` = delegated `UIWindowScene`.
    win_scene_kind: u8,
    win_is_key: bool,
    is_locked: bool,
    has_pls: bool,
    scene_fullscreen: bool,
    assistive_touch: bool,
    mouse_count: usize,
    scene_window_count: usize,
    activation_state: isize,
}

/// Inspects live `UIWindowScene` / `UIPointerLockState` diagnostics, repairing a detached or
/// delegate-less fallback `UIWindow.windowScene` or non-key active window if needed so
/// `setNeedsUpdateOfPrefersPointerLocked` reaches the foreground `UIWindowScene`.
unsafe fn read_scene_lock_state(vc: &AnyObject) -> SceneLockInfo {
    let assistive_touch = unsafe { UIAccessibilityIsAssistiveTouchRunning() }.as_bool();
    let mice: Option<Obj> = unsafe { msg_send![class!(GCMouse), mice] };
    let mouse_count: usize = mice.map_or(0, |list| unsafe { msg_send![&*list, count] });

    let win: Option<Obj> = get!(vc, view).and_then(|v| get!(v, window));
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
            let app: Option<Obj> = unsafe { msg_send![class!(UIApplication), sharedApplication] };
            let app = app?;
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
            win_is_key,
            is_locked: false,
            has_pls: false,
            scene_fullscreen: false,
            assistive_touch,
            mouse_count,
            scene_window_count: 0,
            activation_state: -99,
        };
    };

    let act: isize = unsafe { msg_send![&*scene, activationState] };
    if act == 0 && !win_is_key && let Some(w) = win.as_ref() {
        unsafe {
            let _: () = msg_send![&**w, makeKeyWindow];
        }
        win_is_key = unsafe { msg_send![&**w, isKeyWindow] };
    }

    let scene_window_count: usize =
        get!(scene, windows).map_or(0, |wins| unsafe { msg_send![&*wins, count] });
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

    let lock_sel = sel!(pointerLockState);
    let responds: bool = unsafe { msg_send![&*scene, respondsToSelector: lock_sel] };
    if !responds {
        return SceneLockInfo {
            win_scene_kind,
            win_is_key,
            is_locked: false,
            has_pls: false,
            scene_fullscreen,
            assistive_touch,
            mouse_count,
            scene_window_count,
            activation_state: act,
        };
    }
    let state: Option<Obj> = get!(scene, pointerLockState);
    let has_pls = state.is_some();
    let is_locked = state.is_some_and(|st| unsafe { msg_send![&*st, isLocked] });
    SceneLockInfo {
        win_scene_kind,
        win_is_key,
        is_locked,
        has_pls,
        scene_fullscreen,
        assistive_touch,
        mouse_count,
        scene_window_count,
        activation_state: act,
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
/// unlocks/restores it when a menu or inventory screen is open, toggles `UIHoverGestureRecognizer`
/// and `UIPointerInteraction` (`hiddenPointerStyle` fallback), and retries declined requests on
/// click-up / focus / resize via a genuine `false -> true` edge pulse.
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
        let cls = objc2::ffi::object_getClass((&*vc as *const AnyObject).cast_mut().cast());
        if cls.is_null() {
            st.applied = None;
            return;
        }
        let cls_addr = cls as usize;
        let installed_now =
            POINTER_LOCK_INSTALLED_CLS.swap(cls_addr, Ordering::Relaxed) != cls_addr;
        if installed_now {
            let pref_imp: Imp = std::mem::transmute::<
                unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
                Imp,
            >(prefers_pointer_locked);
            objc2::ffi::class_replaceMethod(
                cls.cast_mut(),
                sel!(prefersPointerLocked),
                pref_imp,
                c"B@:".as_ptr(),
            );
            let child_imp: Imp = std::mem::transmute::<
                unsafe extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject,
                Imp,
            >(child_vc_for_pointer_lock);
            objc2::ffi::class_replaceMethod(
                cls.cast_mut(),
                sel!(childViewControllerForPointerLock),
                child_imp,
                c"@@:".as_ptr(),
            );
            let cname = objc2::ffi::class_getName(cls);
            if !cname.is_null() {
                st.vc_class = CStr::from_ptr(cname).to_string_lossy().into_owned();
            }
        }

        let need_probe = installed_now
            || st.want_lock != locked
            || st.applied != Some(locked)
            || st.pulse_until.is_some()
            || st
                .last_probe_at
                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(120));
        if need_probe {
            let info = read_scene_lock_state(&vc);
            let was_sys_locked = st.sys_locked;
            st.win_scene_kind = info.win_scene_kind;
            st.win_is_key = info.win_is_key;
            st.sys_locked = info.is_locked;
            st.has_pls = info.has_pls;
            st.scene_fullscreen = info.scene_fullscreen;
            st.assistive_touch = info.assistive_touch;
            st.mouse_count = info.mouse_count;
            st.scene_window_count = info.scene_window_count;
            st.activation_state = info.activation_state;
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
            st.pulse_until = None;
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
            st.pulse_until = None;
            st.next_retry_at = None;
            st.auto_retries = 0;
        }

        if st.sys_locked {
            st.pulse_until = None;
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
        if let Some(deadline) = st.pulse_until {
            if now >= deadline && !button_down {
                st.pulse_until = None;
                PREFERS_POINTER_LOCKED.store(true, Ordering::Relaxed);
                request_pointer_lock_update(&vc);
                st.applied = Some(true);
                st.vc_prefers = true;
                st.pulse_count = st.pulse_count.saturating_add(1);
                st.next_retry_at = None;
                invalidate_pointer_interaction();
            }
            return;
        }

        if !button_down && (installed_now || st.applied != Some(true)) {
            if PREFERS_POINTER_LOCKED.load(Ordering::Relaxed) {
                // Force a real `false -> true` edge (60ms hold) so UIKit/SpringBoard does not
                // treat a repeated `prefersPointerLocked == YES` as a cached no-op.
                PREFERS_POINTER_LOCKED.store(false, Ordering::Relaxed);
                request_pointer_lock_update(&vc);
                st.pulse_until = Some(now + Duration::from_millis(60));
                st.vc_prefers = false;
            } else {
                PREFERS_POINTER_LOCKED.store(true, Ordering::Relaxed);
                request_pointer_lock_update(&vc);
                st.applied = Some(true);
                st.vc_prefers = true;
                st.pulse_count = st.pulse_count.saturating_add(1);
                st.next_retry_at = Some(now + Duration::from_millis(500));
                invalidate_pointer_interaction();
            }
            return;
        }

        // At most one automatic follow-up pulse (500ms after initial ask or after a lock drop);
        // further re-asks are driven by grantable events (click-up, focus, resize, resume).
        if !button_down
            && MOUSE_CONNECTED.load(Ordering::Relaxed)
            && st.auto_retries < 1
            && st.next_retry_at.is_some_and(|t| now >= t)
        {
            st.auto_retries = st.auto_retries.saturating_add(1);
            st.next_retry_at = None;
            PREFERS_POINTER_LOCKED.store(false, Ordering::Relaxed);
            request_pointer_lock_update(&vc);
            st.pulse_until = Some(now + Duration::from_millis(60));
            st.vc_prefers = false;
        }
    }
}
