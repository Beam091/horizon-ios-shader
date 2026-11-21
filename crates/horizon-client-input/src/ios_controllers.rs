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

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use winit::keyboard::KeyCode;

/// State of a hardware keyboard (`GCKeyboard`) and mouse/trackpad (`GCMouse`) connected to iOS/iPadOS.
#[derive(Clone, Debug, Default)]
pub struct HardwareInput {
    pub keyboard_connected: bool,
    pub mouse_connected: bool,
    pub keys: Vec<KeyCode>,
    pub mouse_buttons: [bool; 3],
    pub mouse_delta: (f32, f32),
    pub mouse_scroll: (f32, f32),
}

static MOUSE_DX: AtomicU32 = AtomicU32::new(0);
static MOUSE_DY: AtomicU32 = AtomicU32::new(0);
static INSTALLED_MOUSE_INPUT: AtomicUsize = AtomicUsize::new(0);

fn atomic_add_f32(atom: &AtomicU32, delta: f32) {
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
    (79, KeyCode::ArrowRight),
    (80, KeyCode::ArrowLeft),
    (81, KeyCode::ArrowDown),
    (82, KeyCode::ArrowUp),
    (224, KeyCode::ControlLeft),
    (225, KeyCode::ShiftLeft),
    (226, KeyCode::AltLeft),
    (227, KeyCode::SuperLeft),
    (228, KeyCode::ControlRight),
    (229, KeyCode::ShiftRight),
    (230, KeyCode::AltRight),
    (231, KeyCode::SuperRight),
];

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
        let ptr = &*input as *const AnyObject as usize;
        if INSTALLED_MOUSE_INPUT.swap(ptr, Ordering::Relaxed) != ptr {
            let block = (&MOUSE_MOVED_BLOCK as *const StaticBlock).cast::<AnyObject>();
            let _: () = unsafe { msg_send![&*input, setMouseMovedHandler: block] };
        }
        out.mouse_buttons = [
            pressed(get!(input, leftButton)),
            pressed(get!(input, rightButton)),
            pressed(get!(input, middleButton)),
        ];
        let [sx, sy] = stick(get!(input, scroll));
        out.mouse_scroll = (sx, sy);
    } else {
        INSTALLED_MOUSE_INPUT.store(0, Ordering::Relaxed);
    }
    out.mouse_delta = (atomic_take_f32(&MOUSE_DX), atomic_take_f32(&MOUSE_DY));
    out
}

use objc2::runtime::{Bool, Imp, Sel};
use objc2::sel;
use std::sync::atomic::AtomicBool;

static POINTER_LOCKED: AtomicBool = AtomicBool::new(false);
static POINTER_LOCK_INSTALLED_CLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn prefers_pointer_locked(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    Bool::new(POINTER_LOCKED.load(Ordering::Relaxed))
}

unsafe fn root_view_controller() -> Option<Obj> {
    let app: Option<Obj> = unsafe { msg_send![class!(UIApplication), sharedApplication] };
    let app = app?;
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
                let mut fallback: Option<Obj> = None;
                for j in 0..wcount {
                    let win: Option<Obj> = unsafe { msg_send![&*windows, objectAtIndex: j] };
                    let Some(win) = win else { continue };
                    let vc: Option<Obj> = unsafe { msg_send![&*win, rootViewController] };
                    let Some(vc) = vc else { continue };
                    let is_key: bool = unsafe { msg_send![&*win, isKeyWindow] };
                    if is_key {
                        return Some(vc);
                    }
                    if fallback.is_none() {
                        fallback = Some(vc);
                    }
                }
                if fallback.is_some() {
                    return fallback;
                }
            }
        }
    }
    let win: Option<Obj> = unsafe { msg_send![&*app, keyWindow] };
    win.and_then(|w| unsafe { msg_send![&*w, rootViewController] })
}

/// Locks and hides the iPadOS/iOS hardware mouse pointer while in-world (`prefersPointerLocked`),
/// and unlocks/restores it when a menu or inventory screen is open.
pub fn set_pointer_locked(locked: bool) {
    let prev = POINTER_LOCKED.swap(locked, Ordering::Relaxed);
    unsafe {
        let Some(vc) = root_view_controller() else {
            return;
        };
        let cls = objc2::ffi::object_getClass((&*vc as *const AnyObject).cast_mut().cast());
        if cls.is_null() {
            return;
        }
        let cls_addr = cls as usize;
        let installed_now =
            POINTER_LOCK_INSTALLED_CLS.swap(cls_addr, Ordering::Relaxed) != cls_addr;
        if installed_now {
            let imp: Imp = std::mem::transmute::<
                unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
                Imp,
            >(prefers_pointer_locked);
            objc2::ffi::class_replaceMethod(
                cls.cast_mut(),
                sel!(prefersPointerLocked),
                imp,
                c"B@:".as_ptr(),
            );
        }
        if prev != locked || installed_now {
            let update_sel = sel!(setNeedsUpdateOfPrefersPointerLocked);
            let responds: bool = msg_send![&*vc, respondsToSelector: update_sel];
            if responds {
                let _: () = msg_send![&*vc, setNeedsUpdateOfPrefersPointerLocked];
            }
        }
    }
}
