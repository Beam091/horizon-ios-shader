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
