//! Touch screens. Over menus, [`TouchMouse`] makes the first finger the mouse.
//! While playing, [`TouchControls`] are the on-screen controls, whose layout
//! the player edits (controls.rs), with their icons (icons.rs). iOS sends
//! only touches.
mod controls;
pub mod icons;

pub use controls::*;
use std::time::{Duration, Instant};
use winit::dpi::PhysicalPosition;
use winit::event::{
    DeviceId, ElementState, MouseButton, MouseScrollDelta, Touch, TouchPhase, WindowEvent,
};

/// Max finger movement (physical pixels) for a touch to count as stationary (tap or long-press).
const TAP_SLOP: f64 = 18.0;
/// Max duration of a single tap in trackpad mode.
const TAP_MAX: Duration = Duration::from_millis(260);
/// Max time between a tap and the next touch-down to start a double-tap-and-hold left drag.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(320);
/// Hold duration in place to trigger a right-click in trackpad mode.
const HOLD_RIGHT_CLICK: Duration = Duration::from_millis(380);
/// Relative cursor speed multiplier in trackpad mode.
const TRACKPAD_SPEED: f64 = 1.35;

/// A touch screen as a mouse: in direct mode, the first finger down moves the
/// cursor and holds the left button until it lifts, while two fingers scroll
/// the mouse wheel. In trackpad mode (`trackpad: true`), dragging one finger
/// moves a virtual mouse cursor, a single tap left-clicks, a double-tap-and-hold
/// left-drags, a long press right-clicks, and two fingers scroll.
#[derive(Default, Debug)]
pub struct TouchMouse {
    finger: Option<u64>,
    primary_start: PhysicalPosition<f64>,
    primary_last: PhysicalPosition<f64>,
    primary_since: Option<Instant>,
    primary_moved: bool,
    second_finger: Option<(u64, PhysicalPosition<f64>)>,
    scrolling: bool,
    direct_left_down: bool,
    /// Whether virtual trackpad mouse mode is active.
    pub trackpad: bool,
    /// Virtual cursor position in physical window pixels (used in trackpad mode).
    pub cursor: Option<PhysicalPosition<f64>>,
    last_tap: Option<Instant>,
    trackpad_left_drag: bool,
    trackpad_right_down: bool,
    device_id: Option<DeviceId>,
}

impl TouchMouse {
    /// The finger acting as the mouse.
    pub fn finger(&self) -> Option<u64> {
        self.finger
    }

    /// Whether any touch gesture (primary or two-finger scroll) is currently active.
    pub fn has_finger(&self, id: u64) -> bool {
        self.finger == Some(id) || self.second_finger.is_some_and(|(fid, _)| fid == id)
    }

    /// Whether a two-finger scroll gesture is currently active.
    pub fn scrolling(&self) -> bool {
        self.scrolling
    }

    /// Resets active touches when toggling trackpad mode or leaving a menu.
    pub fn reset_touches(&mut self) {
        self.finger = None;
        self.second_finger = None;
        self.scrolling = false;
        self.direct_left_down = false;
        self.primary_since = None;
        self.primary_moved = false;
        self.last_tap = None;
        self.trackpad_left_drag = false;
        self.trackpad_right_down = false;
    }

    /// Polls time-based gestures (long-press right-click in trackpad mode).
    pub fn poll(&mut self, now: Instant) -> Vec<WindowEvent> {
        if self.trackpad
            && self.finger.is_some()
            && !self.scrolling
            && !self.primary_moved
            && !self.trackpad_left_drag
            && !self.trackpad_right_down
            && self
                .primary_since
                .is_some_and(|s| now.duration_since(s) >= HOLD_RIGHT_CLICK)
        {
            self.trackpad_right_down = true;
            let device_id = self.device_id.unwrap_or_else(DeviceId::dummy);
            let position = self.cursor.unwrap_or(PhysicalPosition::new(0.0, 0.0));
            return vec![
                WindowEvent::CursorMoved {
                    device_id,
                    position,
                },
                WindowEvent::MouseInput {
                    device_id,
                    state: ElementState::Pressed,
                    button: MouseButton::Right,
                },
            ];
        }
        Vec::new()
    }

    /// Menu touch translation supporting 2-finger trackpad scroll in both modes
    /// and relative cursor + tap/double-tap-drag/long-press in trackpad mode.
    pub fn translate_menu(
        &mut self,
        touch: Touch,
        now: Instant,
        bounds: (f64, f64),
    ) -> Vec<WindowEvent> {
        let Touch {
            device_id,
            phase,
            location,
            id,
            ..
        } = touch;
        let btn = |button, state| WindowEvent::MouseInput {
            device_id,
            state,
            button,
        };
        let wheel = |dx: f64, dy: f64| WindowEvent::MouseWheel {
            device_id,
            delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(dx, dy)),
            phase: TouchPhase::Moved,
        };

        if !self.trackpad {
            let moved = WindowEvent::CursorMoved {
                device_id,
                position: location,
            };
            return match phase {
                TouchPhase::Started if self.finger.is_none() => {
                    self.finger = Some(id);
                    self.device_id = Some(device_id);
                    self.primary_start = location;
                    self.primary_last = location;
                    self.primary_since = Some(now);
                    self.primary_moved = false;
                    self.scrolling = false;
                    self.direct_left_down = true;
                    self.cursor = Some(location);
                    vec![moved, btn(MouseButton::Left, ElementState::Pressed)]
                }
                TouchPhase::Started
                    if self.second_finger.is_none() && self.finger != Some(id) =>
                {
                    self.second_finger = Some((id, location));
                    self.scrolling = true;
                    if std::mem::replace(&mut self.direct_left_down, false) {
                        vec![btn(MouseButton::Left, ElementState::Released)]
                    } else {
                        Vec::new()
                    }
                }
                TouchPhase::Moved if self.scrolling => {
                    if self.finger == Some(id) {
                        let dx = location.x - self.primary_last.x;
                        let dy = location.y - self.primary_last.y;
                        self.primary_last = location;
                        if dx != 0.0 || dy != 0.0 {
                            return vec![wheel(dx * 0.6, dy * 0.6)];
                        }
                    } else if let Some((fid, last)) = &mut self.second_finger
                        && *fid == id
                    {
                        let dx = location.x - last.x;
                        let dy = location.y - last.y;
                        *last = location;
                        if dx != 0.0 || dy != 0.0 {
                            return vec![wheel(dx * 0.6, dy * 0.6)];
                        }
                    }
                    Vec::new()
                }
                TouchPhase::Moved if self.finger == Some(id) => {
                    self.primary_last = location;
                    self.cursor = Some(location);
                    vec![moved]
                }
                TouchPhase::Ended | TouchPhase::Cancelled
                    if self.second_finger.is_some_and(|(fid, _)| fid == id) =>
                {
                    self.second_finger = None;
                    Vec::new()
                }
                TouchPhase::Ended | TouchPhase::Cancelled if self.finger == Some(id) => {
                    self.finger = None;
                    self.second_finger = None;
                    self.scrolling = false;
                    if std::mem::replace(&mut self.direct_left_down, false) {
                        self.cursor = Some(location);
                        vec![moved, btn(MouseButton::Left, ElementState::Released)]
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            };
        }

        // Trackpad Virtual Mouse Mode
        let cur = self
            .cursor
            .get_or_insert_with(|| PhysicalPosition::new(bounds.0 * 0.5, bounds.1 * 0.5));
        match phase {
            TouchPhase::Started if self.finger.is_none() => {
                self.finger = Some(id);
                self.device_id = Some(device_id);
                self.primary_start = location;
                self.primary_last = location;
                self.primary_since = Some(now);
                self.primary_moved = false;
                self.scrolling = false;
                self.trackpad_right_down = false;
                if self
                    .last_tap
                    .is_some_and(|t| now.duration_since(t) <= DOUBLE_TAP_WINDOW)
                {
                    self.last_tap = None;
                    self.trackpad_left_drag = true;
                    vec![
                        WindowEvent::CursorMoved {
                            device_id,
                            position: *cur,
                        },
                        btn(MouseButton::Left, ElementState::Pressed),
                    ]
                } else {
                    self.trackpad_left_drag = false;
                    Vec::new()
                }
            }
            TouchPhase::Started if self.second_finger.is_none() && self.finger != Some(id) => {
                self.second_finger = Some((id, location));
                self.scrolling = true;
                self.last_tap = None;
                let mut out = Vec::new();
                if std::mem::replace(&mut self.trackpad_left_drag, false) {
                    out.push(btn(MouseButton::Left, ElementState::Released));
                }
                if std::mem::replace(&mut self.trackpad_right_down, false) {
                    out.push(btn(MouseButton::Right, ElementState::Released));
                }
                out
            }
            TouchPhase::Moved if self.scrolling => {
                if self.finger == Some(id) {
                    let dx = location.x - self.primary_last.x;
                    let dy = location.y - self.primary_last.y;
                    self.primary_last = location;
                    if dx != 0.0 || dy != 0.0 {
                        return vec![wheel(dx * 0.6, dy * 0.6)];
                    }
                } else if let Some((fid, last)) = &mut self.second_finger
                    && *fid == id
                {
                    let dx = location.x - last.x;
                    let dy = location.y - last.y;
                    *last = location;
                    if dx != 0.0 || dy != 0.0 {
                        return vec![wheel(dx * 0.6, dy * 0.6)];
                    }
                }
                Vec::new()
            }
            TouchPhase::Moved if self.finger == Some(id) => {
                let mut out = Vec::new();
                if !self.primary_moved
                    && !self.trackpad_left_drag
                    && !self.trackpad_right_down
                    && self
                        .primary_since
                        .is_some_and(|s| now.duration_since(s) >= HOLD_RIGHT_CLICK)
                {
                    self.trackpad_right_down = true;
                    out.push(WindowEvent::CursorMoved {
                        device_id,
                        position: *cur,
                    });
                    out.push(btn(MouseButton::Right, ElementState::Pressed));
                }
                let dx = (location.x - self.primary_last.x) * TRACKPAD_SPEED;
                let dy = (location.y - self.primary_last.y) * TRACKPAD_SPEED;
                self.primary_last = location;
                let dist = ((location.x - self.primary_start.x).powi(2)
                    + (location.y - self.primary_start.y).powi(2))
                .sqrt();
                if dist > TAP_SLOP {
                    self.primary_moved = true;
                }
                cur.x = (cur.x + dx).clamp(0.0, bounds.0.max(1.0));
                cur.y = (cur.y + dy).clamp(0.0, bounds.1.max(1.0));
                out.push(WindowEvent::CursorMoved {
                    device_id,
                    position: *cur,
                });
                out
            }
            TouchPhase::Ended | TouchPhase::Cancelled
                if self.second_finger.is_some_and(|(fid, _)| fid == id) =>
            {
                self.second_finger = None;
                Vec::new()
            }
            TouchPhase::Ended | TouchPhase::Cancelled if self.finger == Some(id) => {
                self.finger = None;
                self.second_finger = None;
                let was_scrolling = std::mem::replace(&mut self.scrolling, false);
                let was_left_drag = std::mem::replace(&mut self.trackpad_left_drag, false);
                let was_right_down = std::mem::replace(&mut self.trackpad_right_down, false);
                let since = self.primary_since.take();
                let position = *cur;
                if was_left_drag {
                    vec![
                        WindowEvent::CursorMoved {
                            device_id,
                            position,
                        },
                        btn(MouseButton::Left, ElementState::Released),
                    ]
                } else if was_right_down {
                    vec![
                        WindowEvent::CursorMoved {
                            device_id,
                            position,
                        },
                        btn(MouseButton::Right, ElementState::Released),
                    ]
                } else if !was_scrolling && phase == TouchPhase::Ended && !self.primary_moved {
                    if since.is_some_and(|s| now.duration_since(s) >= HOLD_RIGHT_CLICK) {
                        vec![
                            WindowEvent::CursorMoved {
                                device_id,
                                position,
                            },
                            btn(MouseButton::Right, ElementState::Pressed),
                            btn(MouseButton::Right, ElementState::Released),
                        ]
                    } else if since.is_some_and(|s| now.duration_since(s) <= TAP_MAX) {
                        self.last_tap = Some(now);
                        vec![
                            WindowEvent::CursorMoved {
                                device_id,
                                position,
                            },
                            btn(MouseButton::Left, ElementState::Pressed),
                            btn(MouseButton::Left, ElementState::Released),
                        ]
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// The mouse events `event` stands for. A touch by another finger gives
    /// none; any other event passes through unchanged.
    pub fn translate(&mut self, event: WindowEvent) -> Vec<WindowEvent> {
        let WindowEvent::Touch(Touch {
            device_id,
            phase,
            location,
            id,
            ..
        }) = event
        else {
            return vec![event];
        };
        let moved = WindowEvent::CursorMoved {
            device_id,
            position: location,
        };
        let button = |state| WindowEvent::MouseInput {
            device_id,
            state,
            button: MouseButton::Left,
        };
        match phase {
            TouchPhase::Started if self.finger.is_none() => {
                self.finger = Some(id);
                vec![moved, button(ElementState::Pressed)]
            }
            TouchPhase::Moved if self.finger == Some(id) => vec![moved],
            TouchPhase::Ended | TouchPhase::Cancelled if self.finger == Some(id) => {
                self.finger = None;
                vec![moved, button(ElementState::Released)]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;
    use winit::event::DeviceId;

    fn touch(id: u64, phase: TouchPhase, x: f64, y: f64) -> WindowEvent {
        WindowEvent::Touch(raw_touch(id, phase, x, y))
    }

    fn raw_touch(id: u64, phase: TouchPhase, x: f64, y: f64) -> Touch {
        Touch {
            device_id: DeviceId::dummy(),
            phase,
            location: PhysicalPosition::new(x, y),
            force: None,
            id,
        }
    }

    /// (x, y) of a cursor move, or the button's state.
    #[derive(Debug, PartialEq)]
    enum Out {
        Move(f64, f64),
        Button(ElementState),
        RightButton(ElementState),
        Wheel(f64, f64),
    }

    fn outs(events: Vec<WindowEvent>) -> Vec<Out> {
        events
            .into_iter()
            .map(|e| match e {
                WindowEvent::CursorMoved { position, .. } => Out::Move(position.x, position.y),
                WindowEvent::MouseInput {
                    state,
                    button: MouseButton::Left,
                    ..
                } => Out::Button(state),
                WindowEvent::MouseInput {
                    state,
                    button: MouseButton::Right,
                    ..
                } => Out::RightButton(state),
                WindowEvent::MouseWheel {
                    delta: MouseScrollDelta::PixelDelta(p),
                    ..
                } => Out::Wheel(p.x, p.y),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_tap_is_a_left_click_where_the_finger_is() {
        let mut t = TouchMouse::default();
        assert_eq!(
            outs(t.translate(touch(1, TouchPhase::Started, 10.0, 20.0))),
            [Out::Move(10.0, 20.0), Out::Button(ElementState::Pressed)]
        );
        assert_eq!(
            outs(t.translate(touch(1, TouchPhase::Ended, 10.0, 20.0))),
            [Out::Move(10.0, 20.0), Out::Button(ElementState::Released)]
        );
    }

    #[test]
    fn a_drag_moves_the_cursor_with_the_button_held() {
        let mut t = TouchMouse::default();
        t.translate(touch(1, TouchPhase::Started, 0.0, 0.0));
        assert_eq!(
            outs(t.translate(touch(1, TouchPhase::Moved, 5.0, 7.0))),
            [Out::Move(5.0, 7.0)]
        );
    }

    #[test]
    fn a_second_finger_is_ignored_until_the_first_lifts() {
        let mut t = TouchMouse::default();
        t.translate(touch(1, TouchPhase::Started, 0.0, 0.0));
        assert!(
            t.translate(touch(2, TouchPhase::Started, 9.0, 9.0))
                .is_empty()
        );
        assert!(
            t.translate(touch(2, TouchPhase::Moved, 8.0, 8.0))
                .is_empty()
        );
        assert!(
            t.translate(touch(2, TouchPhase::Ended, 8.0, 8.0))
                .is_empty()
        );
        // A cancelled touch (a system gesture took over) still lets go of the button.
        assert_eq!(
            outs(t.translate(touch(1, TouchPhase::Cancelled, 1.0, 1.0))),
            [Out::Move(1.0, 1.0), Out::Button(ElementState::Released)]
        );
        // The next finger down becomes the mouse.
        assert_eq!(
            outs(t.translate(touch(2, TouchPhase::Started, 3.0, 4.0))),
            [Out::Move(3.0, 4.0), Out::Button(ElementState::Pressed)]
        );
    }

    #[test]
    fn other_events_pass_through() {
        let mut t = TouchMouse::default();
        let out = t.translate(WindowEvent::Focused(true));
        assert!(matches!(out[..], [WindowEvent::Focused(true)]));
    }

    #[test]
    fn two_fingers_scroll_in_direct_and_trackpad_modes() {
        let mut t = TouchMouse::default();
        let now = Instant::now();
        let bounds = (1000.0, 500.0);
        assert_eq!(
            outs(t.translate_menu(raw_touch(1, TouchPhase::Started, 100.0, 100.0), now, bounds)),
            [Out::Move(100.0, 100.0), Out::Button(ElementState::Pressed)]
        );
        // Second finger cancels the held left button and starts scrolling.
        assert_eq!(
            outs(t.translate_menu(raw_touch(2, TouchPhase::Started, 200.0, 100.0), now, bounds)),
            [Out::Button(ElementState::Released)]
        );
        assert!(t.scrolling());
        assert_eq!(
            outs(t.translate_menu(raw_touch(1, TouchPhase::Moved, 100.0, 150.0), now, bounds)),
            [Out::Wheel(0.0, 30.0)]
        );
    }

    #[test]
    fn trackpad_mode_supports_tap_double_tap_drag_and_long_press_right_click() {
        let mut t = TouchMouse {
            trackpad: true,
            cursor: Some(PhysicalPosition::new(500.0, 250.0)),
            ..TouchMouse::default()
        };
        let now = Instant::now();
        let bounds = (1000.0, 500.0);

        // 1. Single tap -> Left click at virtual cursor
        assert!(
            t.translate_menu(raw_touch(1, TouchPhase::Started, 100.0, 100.0), now, bounds)
                .is_empty()
        );
        assert_eq!(
            outs(t.translate_menu(
                raw_touch(1, TouchPhase::Ended, 100.0, 100.0),
                now + Duration::from_millis(80),
                bounds,
            )),
            [
                Out::Move(500.0, 250.0),
                Out::Button(ElementState::Pressed),
                Out::Button(ElementState::Released),
            ]
        );

        // 2. Second tap within DOUBLE_TAP_WINDOW -> holds Left button and drags
        let t2 = now + Duration::from_millis(180);
        assert_eq!(
            outs(t.translate_menu(raw_touch(2, TouchPhase::Started, 100.0, 100.0), t2, bounds)),
            [Out::Move(500.0, 250.0), Out::Button(ElementState::Pressed)]
        );
        assert_eq!(
            outs(t.translate_menu(raw_touch(2, TouchPhase::Moved, 120.0, 100.0), t2, bounds)),
            [Out::Move(527.0, 250.0)]
        );
        assert_eq!(
            outs(t.translate_menu(raw_touch(2, TouchPhase::Ended, 120.0, 100.0), t2, bounds)),
            [Out::Move(527.0, 250.0), Out::Button(ElementState::Released)]
        );

        // 3. Long press in place -> Right click
        let t3 = now + Duration::from_millis(1000);
        assert!(
            t.translate_menu(raw_touch(3, TouchPhase::Started, 100.0, 100.0), t3, bounds)
                .is_empty()
        );
        assert_eq!(
            outs(t.poll(t3 + Duration::from_millis(400))),
            [
                Out::Move(527.0, 250.0),
                Out::RightButton(ElementState::Pressed),
            ]
        );
        assert_eq!(
            outs(t.translate_menu(
                raw_touch(3, TouchPhase::Ended, 100.0, 100.0),
                t3 + Duration::from_millis(450),
                bounds,
            )),
            [
                Out::Move(527.0, 250.0),
                Out::RightButton(ElementState::Released),
            ]
        );
    }
}
