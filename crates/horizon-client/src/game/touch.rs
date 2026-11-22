//! The touch screen (iOS, which sends touches and no mouse). Over menus, the
//! first finger is the mouse; while playing, the on-screen controls
//! (`horizon_client_input::touch`) act as a virtual controller: its buttons
//! are the ones bound in the controller settings, its stick the left stick.
//! Dragging the view turns it like the mouse does. The Edit button opens the
//! layout editor; its layout is kept in `touch-controls.txt` beside
//! `options.txt`.
use super::*;
use horizon_client::gamepad::{Button, PadState};
use horizon_client_input::touch::{
    Area, Control, EditorButton, Layout, TouchButton, TouchSettings, icons,
};
use winit::event::{Touch, TouchPhase};

/// How far the view turns per GUI unit dragged, in mouse-movement units
/// (`look_turn`): a drag across a phone's screen turns about half a circle.
const LOOK_SPEED: f64 = 2.6;

/// Colours, as the HUD's `0xAARRGGBB`, at full opacity.
const FACE: u32 = 0x6000_0000;
const FACE_PRESSED: u32 = 0xa0ff_ffff;
const EDGE: u32 = 0xa0ff_ffff;
const INK: u32 = 0xffff_ffff;
const INK_PRESSED: u32 = 0xff20_2020;
const SELECTED: u32 = 0xffff_e040;
const HIDDEN_EDGE: u32 = 0xc0ff_5050;
const SHADE: u32 = 0x5000_0000;

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
impl Game<'_> {
    /// The part of the window clear of the notch and the home bar, in GUI
    /// units at `scale`: iOS reports it as the window's inner rectangle.
    pub(super) fn touch_safe(&self, width: f32, height: f32, scale: f32) -> Area {
        let whole = Area {
            x: 0.0,
            y: 0.0,
            w: width,
            h: height,
        };
        let Some(window) = self.window.as_ref() else {
            return whole;
        };
        let (Ok(inner), Ok(outer)) = (window.inner_position(), window.outer_position()) else {
            return whole;
        };
        let size = window.inner_size();
        let safe = Area {
            x: (inner.x - outer.x) as f32 / scale,
            y: (inner.y - outer.y) as f32 / scale,
            w: size.width as f32 / scale,
            h: size.height as f32 / scale,
        };
        // A safe area that doesn't fit the window is no safe area.
        if safe.w <= 0.0
            || safe.h <= 0.0
            || safe.x + safe.w > width + 1.0
            || safe.y + safe.h > height + 1.0
        {
            return whole;
        }
        safe
    }

    /// The controls' layout on this window, in GUI units, and the GUI scale.
    fn touch_layout(&self) -> Option<(Layout, f32)> {
        let batch = self.gui_scale()?;
        let safe = self.touch_safe(batch.width, batch.height, batch.scale);
        Some((
            self.pad.touch.layout(batch.width, batch.height, safe),
            batch.scale,
        ))
    }

    /// The saved layout, read once.
    fn touch_load(&mut self) {
        if self.pad.touch_loaded {
            return;
        }
        self.pad.touch_loaded = true;
        let path = touch_settings_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => self.pad.touch.settings = TouchSettings::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!(
                "Horizon: touch controls: can't read {}: {e}",
                path.display()
            ),
        }
    }

    /// Whether the on-screen touch buttons should be drawn: hidden automatically
    /// when a hardware keyboard/mouse (`GCKeyboard`/`GCMouse`) or game controller is in use.
    pub(super) fn touch_controls_visible(&self) -> bool {
        TOUCH_CONTROLS && !self.pad.ios_hw_active && !self.pad.hw_pad_active
    }

    /// Whether the top-left menu Shift button is currently active (toggled or held).
    pub(super) fn touch_shift_active(&self) -> bool {
        self.pad.touch_menu_shift || self.pad.touch_menu_shift_finger.is_some()
    }

    /// Syncs the top-left menu Shift button into `self.modifiers`, `self.physical_keys`, and `self.keys`.
    fn sync_touch_menu_shift(&mut self) {
        let in_container =
            self.title.is_none() && matches!(self.screen, Screen::Inventory | Screen::Creative);
        if !in_container {
            self.pad.touch_menu_shift = false;
            self.pad.touch_menu_shift_finger = None;
        }
        let active = in_container && self.touch_shift_active();
        if active != self.pad.touch_menu_shift_applied {
            self.pad.touch_menu_shift_applied = active;
            // Don't release ShiftLeft if a physical hardware keyboard is currently holding it.
            let hw_shift = self.pad.ios_hw_keys.contains(&KeyCode::ShiftLeft);
            if active || !hw_shift {
                horizon_client_input::modifiers::set_key(
                    &mut self.modifiers,
                    &mut self.physical_keys,
                    KeyCode::ShiftLeft,
                    active,
                );
                if active {
                    self.keys.insert(KeyCode::ShiftLeft);
                } else {
                    self.keys.remove(&KeyCode::ShiftLeft);
                }
            }
        }
    }

    /// The Esc button: always top left in the safe area, on every screen, as
    /// the keyboard's Escape (a touch screen has no key for it). Playing, it
    /// opens the pause menu (where Save and Quit leaves the world); over a
    /// menu (an inventory, a crafting table, chat, the options, a world list)
    /// it closes it or goes back. Not while the layout editor or the mods
    /// screen is open (their Done and Close close them).
    fn touch_close_button(&self, batch: &HudBatch) -> Option<Area> {
        if !self.touch_controls_visible()
            || self.pad.mod_browser.is_some()
            || self.pad.account_panel.is_some()
            || (self.pad.touch.editing() && !self.pad_in_menu())
        {
            return None;
        }
        let safe = self.touch_safe(batch.width, batch.height, batch.scale);
        Some(Area {
            x: safe.x + 4.0,
            y: safe.y + 4.0,
            w: 28.0,
            h: 20.0,
        })
    }

    /// The Mouse toggle button next to Esc when a menu is open: switches between
    /// direct touch and virtual trackpad mouse mode. Hidden when a real hardware
    /// mouse is connected so the virtual cursor never fights the native iPadOS pointer.
    fn touch_mouse_button(&self, batch: &HudBatch) -> Option<Area> {
        if !self.pad_in_menu() || self.pad.ios_hw_mouse_connected {
            return None;
        }
        let esc = self.touch_close_button(batch)?;
        Some(Area {
            x: esc.x + esc.w + 4.0,
            y: esc.y,
            w: 38.0,
            h: esc.h,
        })
    }

    /// The Shift button next to Mouse when an inventory or container screen is open:
    /// lets players quick-move items (`Shift-Click`) by tapping to toggle or holding.
    fn touch_shift_button(&self, batch: &HudBatch) -> Option<Area> {
        if self.title.is_some() || !matches!(self.screen, Screen::Inventory | Screen::Creative) {
            return None;
        }
        let anchor = self
            .touch_mouse_button(batch)
            .or_else(|| self.touch_close_button(batch))?;
        Some(Area {
            x: anchor.x + anchor.w + 4.0,
            y: anchor.y,
            w: 34.0,
            h: anchor.h,
        })
    }

    /// The Esc, Mouse, and Shift buttons at the top-left, plus the virtual mouse cursor
    /// (ONLY in touch-screen trackpad mode — never over a physical iPadOS mouse pointer).
    pub(super) fn draw_touch_close(&self, batch: &mut HudBatch) {
        let atlas = &self.gui.atlas;
        if let Some(a) = self.touch_close_button(batch) {
            draw_top_pill(
                batch,
                atlas,
                a,
                &horizon_client::lang::get("horizon.touch.escape"),
                false,
            );
        }
        if let Some(a) = self.touch_mouse_button(batch) {
            draw_top_pill(
                batch,
                atlas,
                a,
                &horizon_client::lang::get("horizon.touch.mouse"),
                self.pad.touch_mouse.trackpad,
            );
        }
        if let Some(a) = self.touch_shift_button(batch) {
            draw_top_pill(
                batch,
                atlas,
                a,
                &horizon_client::lang::get("horizon.touch.shift"),
                self.touch_shift_active(),
            );
        }
        if TOUCH_CONTROLS
            && self.pad_in_menu()
            && self.pad.touch_mouse.trackpad
            && !self.pad.ios_hw_mouse_connected
        {
            draw_virtual_cursor(batch, atlas, self.cursor);
        }
    }

    /// The world is being played, with no screen over it: the controls are up.
    fn touch_playing(&self) -> bool {
        !self.pad_in_menu() && self.player.is_some() && !self.ui.hide_gui
    }

    /// The mouse events `touch` stands for: a finger over a menu, or one that
    /// came down while a menu was open. A finger on the controls gives none.
    pub(super) fn touch_event(&mut self, touch: Touch) -> Vec<WindowEvent> {
        self.touch_load();
        // Identify indirect pointer clicks from a connected iPadOS hardware mouse so they never
        // re-enable the on-screen touch HUD or trigger virtual touch buttons during gameplay.
        #[cfg(target_os = "ios")]
        let is_hw_mouse_touch = self.pad.ios_hw_mouse_connected
            && (horizon_client_input::ios_controllers::is_hardware_mouse_button_down()
                || self.pad.ios_hw_hover_pos.is_some_and(|(hx, hy)| {
                    let dx = touch.location.x - f64::from(hx);
                    let dy = touch.location.y - f64::from(hy);
                    dx * dx + dy * dy <= 32.0 * 32.0
                }));
        #[cfg(not(target_os = "ios"))]
        let is_hw_mouse_touch = false;

        if is_hw_mouse_touch && self.touch_playing() {
            // While in-world, GCMouse handles all hardware mouse buttons directly.
            return Vec::new();
        }
        // A direct finger touch on the screen brings touch controls back if hardware input hid them.
        if touch.phase == TouchPhase::Started && !is_hw_mouse_touch {
            self.pad.ios_hw_active = false;
            self.pad.hw_pad_active = false;
        }
        // The iOS Microsoft sign-in overlay and the mods screen.
        if self.account_touch(&touch) || self.mods_touch(&touch) {
            return Vec::new();
        }
        // Release or toggle the top-left Shift button if the finger holding it lifted.
        if let Some((shift_id, since)) = self.pad.touch_menu_shift_finger
            && shift_id == touch.id
        {
            if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.pad.touch_menu_shift_finger = None;
                if touch.phase == TouchPhase::Ended
                    && Instant::now().duration_since(since) <= std::time::Duration::from_millis(260)
                {
                    self.pad.touch_menu_shift = !self.pad.touch_menu_shift;
                }
                self.sync_touch_menu_shift();
            }
            return Vec::new();
        }
        let mouse = self.pad.touch_mouse.has_finger(touch.id);
        if !mouse
            && touch.phase == TouchPhase::Started
            && let Some(batch) = self.gui_scale()
        {
            let at = (
                touch.location.x as f32 / batch.scale,
                touch.location.y as f32 / batch.scale,
            );
            // The Esc button, top left: Escape's job.
            if let Some(close) = self.touch_close_button(&batch)
                && close.contains(at)
            {
                log::info!("Horizon: touch Esc");
                if let Err(e) = self.pad_back() {
                    log::warn!("Horizon: closing the menu: {e}");
                }
                return Vec::new();
            }
            // The Mouse toggle button, next to Esc in menus.
            if let Some(mouse_btn) = self.touch_mouse_button(&batch)
                && mouse_btn.contains(at)
            {
                self.pad.touch_mouse.trackpad = !self.pad.touch_mouse.trackpad;
                self.pad.touch_mouse.reset_touches();
                if self.pad.touch_mouse.trackpad {
                    let cx = f64::from(batch.width * 0.5 * batch.scale);
                    let cy = f64::from(batch.height * 0.5 * batch.scale);
                    let pos = winit::dpi::PhysicalPosition::new(cx, cy);
                    self.pad.touch_mouse.cursor = Some(pos);
                    self.cursor = (batch.width * 0.5, batch.height * 0.5);
                }
                self.audio.play_button_click();
                return Vec::new();
            }
            // The Shift toggle/hold button, next to Mouse in inventory/creative screens.
            if let Some(shift_btn) = self.touch_shift_button(&batch)
                && shift_btn.contains(at)
            {
                self.pad.touch_menu_shift_finger = Some((touch.id, Instant::now()));
                self.sync_touch_menu_shift();
                self.audio.play_button_click();
                return Vec::new();
            }
        }
        // The on-screen keyboard takes every finger while it is open.
        if !mouse && self.pad.menu.osk.is_some() {
            if touch.phase == TouchPhase::Ended
                && let Some((_, scale)) = self.touch_layout()
            {
                let at = (
                    touch.location.x as f32 / scale,
                    touch.location.y as f32 / scale,
                );
                self.touch_keyboard(at);
            }
            return Vec::new();
        }
        if !mouse
            && self.touch_playing()
            && let Some((layout, scale)) = self.touch_layout()
        {
            let at = (
                touch.location.x as f32 / scale,
                touch.location.y as f32 / scale,
            );
            if touch.phase == TouchPhase::Started
                && !self.pad.touch.editing()
                && let Some(button) = layout.button_at(at)
            {
                log::info!(
                    "Horizon: touch button {button:?} at ({:.0}, {:.0})",
                    at.0,
                    at.1
                );
            }
            self.pad
                .touch
                .touch(&layout, touch.id, touch.phase, at, Instant::now());
            self.last_input = Instant::now();
            return Vec::new();
        }
        // On iOS, winit's `inner_size()` reports the Safe Area (excluding notch/Dynamic Island
        // insets), which stops the virtual cursor short of the right edge of the display.
        // Use the full framebuffer dimensions instead so the cursor reaches every edge.
        let bounds = self
            .gui_scale()
            .map(|b| {
                (
                    f64::from(b.width * b.scale).max(1.0),
                    f64::from(b.height * b.scale).max(1.0),
                )
            })
            .or_else(|| {
                self.window.as_ref().map(|w| {
                    let s = w.outer_size();
                    (f64::from(s.width.max(1)), f64::from(s.height.max(1)))
                })
            })
            .unwrap_or((1920.0, 1080.0));
        self.pad
            .touch_mouse
            .translate_menu(touch, Instant::now(), bounds)
    }

    /// Once per loop iteration, before the pads are read: the controls'
    /// buttons and stick as a virtual controller, the view's drag as a turn
    /// and a tapped slot as its number key, plus trackpad long-press and
    /// iOS/iPadOS hardware keyboard & mouse (`GCKeyboard`/`GCMouse`).
    pub(super) fn touch_frame(&mut self, event_loop: &dyn super::event_loop::EventLoopCtl) {
        self.account_poll();
        self.mods_poll();
        self.sync_touch_menu_shift();
        if let Some(id) = self.window.as_ref().map(|w| w.id()) {
            let events = self.pad.touch_mouse.poll(Instant::now());
            for event in events {
                self.handle_window_event(
                    event_loop,
                    id,
                    event,
                    super::input_trace::InputOrigin::User,
                );
            }
        }
        #[cfg(target_os = "ios")]
        self.poll_ios_hardware_keyboard_mouse(event_loop);
        if let Some(settings) = self.pad.touch.take_saved() {
            let path = touch_settings_path();
            match std::fs::write(&path, settings.to_text()) {
                Ok(()) => log::info!("Horizon: touch controls saved to {}", path.display()),
                Err(e) => log::warn!(
                    "Horizon: touch controls: can't save {}: {e}",
                    path.display()
                ),
            }
        }
        let layout = self
            .touch_layout()
            .filter(|_| self.touch_playing())
            .map(|(layout, _)| layout);
        let Some(layout) = layout else {
            if self.pad.touch.active() {
                self.pad.touch.release_all();
            }
            self.pad.touch_zoom = false;
            self.pad.touch_state = PadState::default();
            return;
        };
        // The controller's toggle sneak flips sneaking itself on each press.
        self.pad.touch.sneak_taps =
            self.settings.controller.toggle_sneak && !self.settings.toggle_crouch;
        let frame = self.pad.touch.frame(&layout, Instant::now());
        let mut state = PadState {
            left: frame.stick,
            ..PadState::default()
        };
        self.pad.touch_zoom = frame.held.contains(&TouchButton::Zoom);
        // F3 is the keyboard's: pressed and released as the key.
        let debug = frame.held.contains(&TouchButton::Debug);
        if debug != self.pad.touch_debug {
            self.pad.touch_debug = debug;
            // KeyboardHandler.keyPress: F3 is a debug key first (its release toggles the
            // overlay), the way the keyboard's F3 goes in window_event.
            use horizon_client::debug_keys::KeyAction;
            let action = if debug {
                KeyAction::Press
            } else {
                KeyAction::Release
            };
            if debug {
                self.physical_keys.insert(KeyCode::F3);
            } else {
                self.physical_keys.remove(&KeyCode::F3);
            }
            if !self.debug_key(KeyCode::F3, action)
                && let Err(e) = self.mapped_key_event(KeyCode::F3, debug, false)
            {
                log::warn!("Horizon: touch F3: {e}");
            }
        }
        for held in frame.held {
            let button = match held.action() {
                Some(action) => self.settings.controller.binding(action),
                None if held == TouchButton::Pause => Some(Button::Start),
                None => None,
            };
            match button {
                Some(Button::LeftTrigger) => state.triggers[0] = 1.0,
                Some(Button::RightTrigger) => state.triggers[1] = 1.0,
                Some(button) => state.set(button, true),
                None => {}
            }
        }
        self.pad.touch_state = state;
        if frame.look != (0.0, 0.0) {
            let sens = self.ios_touch_sensitivity();
            self.look_turn(
                f64::from(frame.look.0) * LOOK_SPEED * sens,
                f64::from(frame.look.1) * LOOK_SPEED * sens,
            );
        }
        if let Some(slot) = frame.slot
            && !self.spectator()
        {
            self.hotbar_presses.push(slot);
        }
    }

    /// Vanilla `MouseHandler.turnPlayer` sensitivity curve: 1.0 at 100% (0.5), up to ~4.1x at 200% (1.0).
    fn sensitivity_curve(raw: f32) -> f64 {
        let ss = f64::from(raw.clamp(0.0, 1.0)) * 0.6 + 0.2;
        ss * ss * ss * 8.0
    }

    /// Separate touch-screen camera drag sensitivity (`Options -> Mouse / Controller -> Touch Sensitivity`).
    fn ios_touch_sensitivity(&self) -> f64 {
        Self::sensitivity_curve(self.settings.controller.touch_sensitivity)
    }

    /// Hardware mouse camera sensitivity (`Options -> Controls -> Mouse Settings -> Sensitivity`).
    fn ios_mouse_sensitivity(&self) -> f64 {
        Self::sensitivity_curve(self.settings.sensitivity)
    }

    /// Moves what was drawn into `batch` since `from` (a vertex count) by the
    /// hotbar's offset in the touch layout: the hotbar, hearts, food and
    /// experience bar follow the hotbar wherever the player put it.
    pub(super) fn touch_follow_hotbar(&self, batch: &mut HudBatch, from: usize) {
        if !self.touch_controls_visible() {
            return;
        }
        let safe = self.touch_safe(batch.width, batch.height, batch.scale);
        let layout = self.pad.touch.layout(batch.width, batch.height, safe);
        let (dx, dy) = layout.hotbar_offset();
        if (dx, dy) == (0.0, 0.0) {
            return;
        }
        for v in batch.vertices.iter_mut().skip(from) {
            v.pos[0] += dx;
            v.pos[1] += dy;
        }
    }

    /// The controls over the HUD while playing, or the layout editor.
    pub(super) fn draw_touch_controls(&self, batch: &mut HudBatch) {
        if !self.touch_controls_visible() || !self.touch_playing() {
            return;
        }
        let atlas = &self.gui.atlas;
        let safe = self.touch_safe(batch.width, batch.height, batch.scale);
        let layout = self.pad.touch.layout(batch.width, batch.height, safe);
        if let Some(view) = self.pad.touch.editor() {
            self.draw_touch_editor(batch, &layout, view);
            return;
        }
        for item in layout.items.iter().filter(|i| !i.hidden) {
            match item.control {
                Control::Button(button) => {
                    let pressed = if button == TouchButton::Sneak && self.pad.touch.sneak_taps {
                        self.keys.contains(&KeyCode::ShiftLeft)
                    } else {
                        self.pad.touch.pressed(button)
                    };
                    draw_button(batch, atlas, button, item.area, item.opacity, pressed);
                }
                Control::Stick => {
                    let r = layout.stick_radius;
                    let (origin, knob, k) = match self.pad.touch.stick(&layout) {
                        Some((origin, knob)) => (origin, knob, item.opacity),
                        // At rest it shows faintly where it sits.
                        None => (layout.stick_home, layout.stick_home, item.opacity * 0.6),
                    };
                    disc(batch, atlas, origin, r, faded(FACE, k));
                    ring(batch, atlas, origin, r, faded(EDGE, k));
                    disc(batch, atlas, knob, r * 0.4, faded(FACE_PRESSED, k));
                }
                Control::Hotbar => {}
            }
        }
    }

    /// The editor: every control (hidden ones dimmed with a red edge), the
    /// picked one outlined, the toolbar, and what is picked.
    fn draw_touch_editor(
        &self,
        batch: &mut HudBatch,
        layout: &Layout,
        view: horizon_client_input::touch::EditorView,
    ) {
        let atlas = &self.gui.atlas;
        batch.fill(atlas, 0.0, 0.0, batch.width, batch.height, SHADE);
        for item in &layout.items {
            let k = if item.hidden { 0.35 } else { item.opacity };
            let a = item.area;
            match item.control {
                Control::Button(button) => draw_button(batch, atlas, button, a, k, false),
                Control::Stick => {
                    outline(batch, atlas, a, faded(EDGE, 0.5));
                    let home = layout.stick_home;
                    disc(batch, atlas, home, layout.stick_radius, faded(FACE, k));
                    ring(batch, atlas, home, layout.stick_radius, faded(EDGE, k));
                }
                Control::Hotbar => outline(batch, atlas, a, faded(EDGE, 0.8)),
            }
            if item.hidden {
                outline(batch, atlas, a, HIDDEN_EDGE);
            }
            if view.selected == Some(item.control) {
                let grown = Area {
                    x: a.x - 2.0,
                    y: a.y - 2.0,
                    w: a.w + 4.0,
                    h: a.h + 4.0,
                };
                outline(batch, atlas, grown, SELECTED);
                outline(
                    batch,
                    atlas,
                    Area {
                        x: grown.x + 1.0,
                        y: grown.y + 1.0,
                        w: grown.w - 2.0,
                        h: grown.h - 2.0,
                    },
                    SELECTED,
                );
            }
        }
        let mut bottom = layout.safe.y;
        for (button, a) in EditorButton::toolbar(layout) {
            let face = if button == EditorButton::Done {
                0xd030_8040
            } else {
                0xc020_2020
            };
            batch.fill(atlas, a.x, a.y, a.w, a.h, face);
            outline(batch, atlas, a, EDGE);
            let label = horizon_client::lang::get(button.label(view.placement.hidden));
            let label = fit(atlas, &label, a.w - 4.0);
            let width = HudBatch::text_width(atlas, &label);
            batch.text(
                atlas,
                &label,
                a.x + (a.w - width) / 2.0,
                a.y + (a.h - 8.0) / 2.0,
                INK,
                true,
            );
            bottom = bottom.max(a.y + a.h);
        }
        let caption = match view.selected {
            Some(control) => {
                let name = horizon_client::lang::get(control.label());
                let p = view.placement;
                if p.hidden {
                    horizon_client::lang::get("horizon.touch.edit.hidden").replace("{name}", &name)
                } else {
                    horizon_client::lang::get("horizon.touch.edit.caption")
                        .replace("{name}", &name)
                        .replace("{size}", &format!("{:.0}", p.size * 100.0))
                        .replace("{opacity}", &format!("{:.0}", p.opacity * 100.0))
                }
            }
            None => horizon_client::lang::get("horizon.touch.edit.hint"),
        };
        let caption = fit(atlas, &caption, layout.safe.w - 8.0);
        let width = HudBatch::text_width(atlas, &caption);
        batch.text(
            atlas,
            &caption,
            layout.safe.x + (layout.safe.w - width) / 2.0,
            bottom + 4.0,
            INK,
            true,
        );
    }
}

/// On iOS the touch controls are drawn and played.
pub(super) const TOUCH_CONTROLS: bool = cfg!(target_os = "ios");

/// Where the layout is kept: beside `options.txt`.
fn touch_settings_path() -> std::path::PathBuf {
    horizon_client::logging::player_dir().join("touch-controls.txt")
}

/// `colour` with its alpha scaled by `k`.
fn faded(colour: u32, k: f32) -> u32 {
    let alpha = ((colour >> 24) as f32 * k.clamp(0.0, 1.0)).round() as u32;
    (alpha << 24) | (colour & 0x00ff_ffff)
}

/// A button: its chamfered/rounded face, soft edge and icon, at opacity `k`.
fn draw_button(
    batch: &mut HudBatch,
    atlas: &HudAtlas,
    button: TouchButton,
    a: Area,
    k: f32,
    pressed: bool,
) {
    let (face, edge, ink) = if pressed {
        (FACE_PRESSED, SELECTED, INK_PRESSED)
    } else {
        (FACE, EDGE, INK)
    };
    let f = faded(face, k);
    let e = faded(edge, k);
    // Chamfered 1-unit corners give the button a clean rounded pill look on Retina displays.
    if a.w >= 6.0 && a.h >= 6.0 {
        batch.fill(atlas, a.x + 1.0, a.y + 1.0, a.w - 2.0, a.h - 2.0, f);
        batch.fill(atlas, a.x + 1.0, a.y, a.w - 2.0, 1.0, e);
        batch.fill(atlas, a.x + 1.0, a.y + a.h - 1.0, a.w - 2.0, 1.0, e);
        batch.fill(atlas, a.x, a.y + 1.0, 1.0, a.h - 2.0, e);
        batch.fill(atlas, a.x + a.w - 1.0, a.y + 1.0, 1.0, a.h - 2.0, e);
    } else {
        batch.fill(atlas, a.x, a.y, a.w, a.h, f);
        outline(batch, atlas, a, e);
    }
    draw_icon(batch, atlas, icons::icon(button), a, faded(ink, k));
}

/// `icon` centred in `a`, in `ink`.
fn draw_icon(batch: &mut HudBatch, atlas: &HudAtlas, icon: &icons::Icon, a: Area, ink: u32) {
    let px = (a.w.min(a.h) * 0.62 / icons::SIZE as f32).max(0.5);
    let side = px * icons::SIZE as f32;
    let (x0, y0) = (a.x + (a.w - side) / 2.0, a.y + (a.h - side) / 2.0);
    for (row, col, len) in icons::runs(icon) {
        batch.fill(
            atlas,
            x0 + col as f32 * px,
            y0 + row as f32 * px,
            len as f32 * px,
            px,
            ink,
        );
    }
}

/// A one-unit frame round `a`.
fn outline(batch: &mut HudBatch, atlas: &HudAtlas, a: Area, c: u32) {
    batch.fill(atlas, a.x, a.y, a.w, 1.0, c);
    batch.fill(atlas, a.x, a.y + a.h - 1.0, a.w, 1.0, c);
    batch.fill(atlas, a.x, a.y + 1.0, 1.0, a.h - 2.0, c);
    batch.fill(atlas, a.x + a.w - 1.0, a.y + 1.0, 1.0, a.h - 2.0, c);
}

/// A filled circle, as one span per unit-high row.
fn disc(batch: &mut HudBatch, atlas: &HudAtlas, (cx, cy): (f32, f32), r: f32, c: u32) {
    let rows = r.ceil() as i32;
    for i in -rows..rows {
        let y = i as f32 + 0.5;
        let half = (r * r - y * y).max(0.0).sqrt();
        if half > 0.0 {
            batch.fill(atlas, cx - half, cy + i as f32, half * 2.0, 1.0, c);
        }
    }
}

/// A circle's one-unit edge.
fn ring(batch: &mut HudBatch, atlas: &HudAtlas, (cx, cy): (f32, f32), r: f32, c: u32) {
    let rows = r.ceil() as i32;
    let inner = (r - 1.0).max(0.0);
    for i in -rows..rows {
        let y = i as f32 + 0.5;
        let outer = (r * r - y * y).max(0.0).sqrt();
        let within = (inner * inner - y * y).max(0.0).sqrt();
        if outer <= 0.0 {
            continue;
        }
        let w = (outer - within).max(1.0);
        batch.fill(atlas, cx - outer, cy + i as f32, w, 1.0, c);
        batch.fill(atlas, cx + outer - w, cy + i as f32, w, 1.0, c);
    }
}

/// `text` cut to `width`, keeping whole characters.
fn fit(atlas: &HudAtlas, text: &str, width: f32) -> String {
    let mut out = String::new();
    for c in text.chars() {
        out.push(c);
        if HudBatch::text_width(atlas, &out) > width {
            out.pop();
            break;
        }
    }
    out
}

/// Draws a top-bar pill button (`Esc`, `Mouse`, `Shift`) with an active highlight state.
fn draw_top_pill(batch: &mut HudBatch, atlas: &HudAtlas, a: Area, label: &str, active: bool) {
    let (face, edge) = if active {
        (0xd030_8040, SELECTED)
    } else {
        (0xc020_2020, EDGE)
    };
    if a.w >= 6.0 && a.h >= 6.0 {
        batch.fill(atlas, a.x + 1.0, a.y + 1.0, a.w - 2.0, a.h - 2.0, face);
        batch.fill(atlas, a.x + 1.0, a.y, a.w - 2.0, 1.0, edge);
        batch.fill(atlas, a.x + 1.0, a.y + a.h - 1.0, a.w - 2.0, 1.0, edge);
        batch.fill(atlas, a.x, a.y + 1.0, 1.0, a.h - 2.0, edge);
        batch.fill(atlas, a.x + a.w - 1.0, a.y + 1.0, 1.0, a.h - 2.0, edge);
    } else {
        batch.fill(atlas, a.x, a.y, a.w, a.h, face);
        outline(batch, atlas, a, edge);
    }
    let label = fit(atlas, label, a.w - 4.0);
    let width = HudBatch::text_width(atlas, &label);
    batch.text(
        atlas,
        &label,
        a.x + (a.w - width) / 2.0,
        a.y + (a.h - 8.0) / 2.0,
        INK,
        true,
    );
}

/// Draws a high-contrast virtual mouse arrow pointer at `(cx, cy)` in GUI units.
fn draw_virtual_cursor(batch: &mut HudBatch, atlas: &HudAtlas, (cx, cy): (f32, f32)) {
    const POINTER: [&str; 11] = [
        "#........",
        "##.......",
        "#@#......",
        "#@@#.....",
        "#@@@#....",
        "#@@@@#...",
        "#@@@@@#..",
        "#@@@@@@#.",
        "#@@@#####",
        "#@#@@#...",
        "##..##...",
    ];
    for (row, line) in POINTER.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let color = match ch {
                '#' => 0xff10_1010,
                '@' => 0xffff_ffff,
                _ => continue,
            };
            batch.fill(atlas, cx + col as f32, cy + row as f32, 1.0, 1.0, color);
        }
    }
}

#[cfg(target_os = "ios")]
impl Game<'_> {
    /// Polls `GCKeyboard`, `GCMouse`, and `UIHoverGestureRecognizer` on iOS/iPadOS:
    /// - In-world: locks & hides the iPadOS pointer (`prefersPointerLocked`), drives 360° camera
    ///   look with Hardware Mouse Sensitivity, buttons, and crisp 1-notch hotbar scrolling.
    /// - In menus: unlocks the real iPadOS cursor, tracks it 1:1 via `UIHoverGestureRecognizer`
    ///   (never drawing a competing virtual cursor), lets UIKit handle Left-click/drag cleanly
    ///   without double-firing, and drives Right/Middle clicks & smooth menu scrolling via `GCMouse`.
    pub(super) fn poll_ios_hardware_keyboard_mouse(
        &mut self,
        event_loop: &dyn super::event_loop::EventLoopCtl,
    ) {
        let playing = self.touch_playing();
        horizon_client_input::ios_controllers::set_pointer_locked(playing);

        let id = self.window.as_ref().map(|w| w.id());
        let buttons = [
            winit::event::MouseButton::Left,
            winit::event::MouseButton::Right,
            winit::event::MouseButton::Middle,
        ];

        // Flush stale deltas and release any held in-world mouse buttons when crossing between
        // gameplay and UI menus so the camera never jumps and buttons never get stuck.
        if self.pad.ios_hw_was_playing != playing {
            let was_playing = self.pad.ios_hw_was_playing;
            self.pad.ios_hw_was_playing = playing;
            horizon_client_input::ios_controllers::flush_hardware_mouse_deltas();
            self.pad.ios_hw_scroll_accum = 0.0;
            self.pad.ios_hw_last_scroll_step = None;
            if was_playing && !playing {
                if let Some(id) = id {
                    for (i, &button) in buttons.iter().enumerate() {
                        if std::mem::replace(&mut self.pad.ios_hw_mouse_buttons[i], false) {
                            self.handle_window_event(
                                event_loop,
                                id,
                                WindowEvent::MouseInput {
                                    device_id: winit::event::DeviceId::dummy(),
                                    state: winit::event::ElementState::Released,
                                    button,
                                },
                                super::input_trace::InputOrigin::User,
                            );
                        }
                    }
                } else {
                    self.pad.ios_hw_mouse_buttons = [false; 3];
                }
            }
        }

        let hw = horizon_client_input::ios_controllers::poll_hardware_input();
        self.pad.ios_hw_mouse_connected = hw.mouse_connected;
        if hw.mouse_connected {
            // Never allow virtual trackpad mode to fight a connected physical mouse.
            self.pad.touch_mouse.trackpad = false;
        } else {
            self.pad.ios_hw_hover_pos = None;
        }
        if let Some(pos) = hw.hover_pos {
            self.pad.ios_hw_hover_pos = Some(pos);
        }

        let any_activity = !hw.keys.is_empty()
            || hw.mouse_buttons.iter().any(|&b| b)
            || hw.mouse_delta != (0.0, 0.0)
            || hw.mouse_scroll != (0.0, 0.0)
            || hw.hover_pos.is_some();
        if any_activity {
            self.pad.ios_hw_active = true;
        } else if !hw.keyboard_connected && !hw.mouse_connected {
            self.pad.ios_hw_active = false;
        }
        let Some(id) = id else {
            return;
        };

        // 1. Hardware keyboard key edges (when not typing into an active IME field).
        let old_keys = std::mem::replace(&mut self.pad.ios_hw_keys, hw.keys.clone());
        let ime_typing = self.ime_allowed;
        for &key in &old_keys {
            if !hw.keys.contains(&key) {
                self.press_modifier_key(key, false);
                self.physical_keys.remove(&key);
                let _ = self.debug_key(key, horizon_client::debug_keys::KeyAction::Release);
                let _ = self.mapped_key_event(key, false, false);
            }
        }
        for &key in &hw.keys {
            if !old_keys.contains(&key) {
                self.last_input = Instant::now();
                self.press_modifier_key(key, true);
                self.physical_keys.insert(key);
                if self.screen == Screen::Options
                    && (self.options_waiting_for_key()
                        || self.ui.options.selected_pad_action.is_some())
                {
                    let _ = self.options_key(key, None);
                    self.pad.options_bind_started_at = None;
                    continue;
                }
                if key == KeyCode::Escape && self.pad_in_menu() {
                    let _ = self.pad_back();
                    continue;
                }
                if !ime_typing {
                    if self.screen == Screen::Options {
                        let _ = self.options_key(key, None);
                        continue;
                    }
                    if !self.debug_key(key, horizon_client::debug_keys::KeyAction::Press) {
                        let _ = self.mapped_key_event(key, true, false);
                    }
                }
            }
        }

        // 2. Hardware mouse movement:
        // - In-world: locked relative deltas from `GCMouseInput` drive camera look.
        // - In menus: exact native coordinates from `UIHoverGestureRecognizer` track the real
        //   iPadOS pointer 1:1 across X and Y without fighting UIKit or drifting.
        let (dx, dy) = hw.mouse_delta;
        if playing {
            if dx != 0.0 || dy != 0.0 {
                self.last_input = Instant::now();
                let sens = self.ios_mouse_sensitivity();
                let x = if self.settings.invert_x { -dx } else { dx };
                let y = if self.settings.invert_y { -dy } else { dy };
                self.look_turn(f64::from(x) * 2.5 * sens, f64::from(y) * 2.5 * sens);
            }
        } else if let Some((hx, hy)) = hw.hover_pos
            && let Some(batch) = self.gui_scale()
        {
            self.last_input = Instant::now();
            let max_x = f64::from(batch.width * batch.scale).max(1.0);
            let max_y = f64::from(batch.height * batch.scale).max(1.0);
            let position = winit::dpi::PhysicalPosition::new(
                f64::from(hx).clamp(0.0, max_x),
                f64::from(hy).clamp(0.0, max_y),
            );
            self.pad.touch_mouse.cursor = Some(position);
            self.handle_window_event(
                event_loop,
                id,
                WindowEvent::CursorMoved {
                    device_id: winit::event::DeviceId::dummy(),
                    position,
                },
                super::input_trace::InputOrigin::User,
            );
        } else if (hw.mouse_buttons[1] || hw.mouse_buttons[2])
            && (dx != 0.0 || dy != 0.0)
            && let Some(batch) = self.gui_scale()
        {
            // Fallback during right/middle-button drag (e.g. inventory stack split) if iPadOS pauses
            // UIHoverGestureRecognizer while a secondary button is held.
            self.last_input = Instant::now();
            let scale = batch.scale;
            let nx = (self.cursor.0 * scale + dx).clamp(0.0, batch.width * scale);
            let ny = (self.cursor.1 * scale + dy).clamp(0.0, batch.height * scale);
            let position = winit::dpi::PhysicalPosition::new(f64::from(nx), f64::from(ny));
            self.pad.touch_mouse.cursor = Some(position);
            self.handle_window_event(
                event_loop,
                id,
                WindowEvent::CursorMoved {
                    device_id: winit::event::DeviceId::dummy(),
                    position,
                },
                super::input_trace::InputOrigin::User,
            );
        }

        // 3. Hardware mouse buttons (`leftButton`, `rightButton`, `middleButton`).
        // In menus, UIKit (`UIApplicationSupportsIndirectInputEvents`) already delivers Left-click
        // and Left-drag at the exact native pointer location via `WindowEvent::Touch`, so only emit
        // `GCMouse` Left button while in-world (avoiding double-click / drag conflicts in UI).
        // Right and Middle buttons are not delivered as touches by winit, so always emit them here.
        let mut effective_buttons = hw.mouse_buttons;
        if !playing {
            effective_buttons[0] = false;
        }
        let old_btns = std::mem::replace(&mut self.pad.ios_hw_mouse_buttons, effective_buttons);
        for (i, &button) in buttons.iter().enumerate() {
            if effective_buttons[i] != old_btns[i] {
                let pressed = effective_buttons[i];
                self.last_input = Instant::now();
                if playing {
                    self.captured = true;
                }
                let state = if pressed {
                    winit::event::ElementState::Pressed
                } else {
                    winit::event::ElementState::Released
                };
                self.handle_window_event(
                    event_loop,
                    id,
                    WindowEvent::MouseInput {
                        device_id: winit::event::DeviceId::dummy(),
                        state,
                        button,
                    },
                    super::input_trace::InputOrigin::User,
                );
            }
        }

        // 4. Hardware mouse scroll wheel (`GCMouseInput.scroll.valueChangedHandler`).
        let (sx, sy) = hw.mouse_scroll;
        if sx != 0.0 || sy != 0.0 {
            let now = Instant::now();
            self.last_input = now;
            if playing {
                if sy != 0.0 {
                    // Scale the anti-bounce interval with Mouse Settings -> Scroll Sensitivity
                    // (1.00 -> 28ms for crisp 1-notch stepping; higher = faster, lower = slower).
                    let wheel_sens = self.settings.mouse_wheel_sensitivity.clamp(0.4, 2.5);
                    let min_interval_ms = (28.0 / wheel_sens).round() as u64;
                    let step_ready = self.pad.ios_hw_last_scroll_step.is_none_or(|t| {
                        now.duration_since(t) >= std::time::Duration::from_millis(min_interval_ms)
                    });
                    if step_ready {
                        // Reset accumulator after a brief pause or on scroll direction reversal so
                        // each physical wheel notch reliably advances the hotbar cleanly.
                        if self
                            .pad
                            .ios_hw_last_scroll_event
                            .is_some_and(|t| {
                                now.duration_since(t) > std::time::Duration::from_millis(160)
                            })
                            || (self.pad.ios_hw_scroll_accum != 0.0
                                && self.pad.ios_hw_scroll_accum.signum() != sy.signum())
                        {
                            self.pad.ios_hw_scroll_accum = 0.0;
                        }
                        self.pad.ios_hw_last_scroll_event = Some(now);
                        // Clamp each frame's contribution so a burst of callbacks doesn't leap across slots.
                        self.pad.ios_hw_scroll_accum += sy.clamp(-1.0, 1.0);
                        if self.pad.ios_hw_scroll_accum.abs() >= 0.20 {
                            let dir = self.pad.ios_hw_scroll_accum.signum();
                            self.pad.ios_hw_scroll_accum = 0.0;
                            self.pad.ios_hw_last_scroll_step = Some(now);
                            self.handle_window_event(
                                event_loop,
                                id,
                                WindowEvent::MouseWheel {
                                    device_id: winit::event::DeviceId::dummy(),
                                    delta: winit::event::MouseScrollDelta::LineDelta(0.0, dir),
                                    phase: TouchPhase::Moved,
                                },
                                super::input_trace::InputOrigin::User,
                            );
                        }
                    }
                }
            } else {
                // Smooth, controlled vertical & horizontal UI list / creative inventory scrolling.
                let mx = sx.clamp(-3.0, 3.0) * 14.0;
                let my = sy.clamp(-3.0, 3.0) * 14.0;
                self.handle_window_event(
                    event_loop,
                    id,
                    WindowEvent::MouseWheel {
                        device_id: winit::event::DeviceId::dummy(),
                        delta: winit::event::MouseScrollDelta::PixelDelta(
                            winit::dpi::PhysicalPosition::new(f64::from(mx), f64::from(my)),
                        ),
                        phase: TouchPhase::Moved,
                    },
                    super::input_trace::InputOrigin::User,
                );
            }
        }
    }
}
