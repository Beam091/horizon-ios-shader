//! The Account overlay of the iOS build (which has no PC launcher): a button on the title screen
//! beside "Get mods" opens a panel where the player can either set/use an **Offline Account**
//! username (persisted in `horizon-player.txt` for Singleplayer, LAN, and offline-mode servers)
//! or sign in with a **Microsoft Account** via the OAuth2 device-code flow (`horizon_client::ms_login`)
//! for online-mode servers and Realms.
use super::*;
use horizon_client::clipboard::Clipboard;
use horizon_client::ms_login::{self, Login, State};
use horizon_client_input::touch::Area;
use winit::event::{Touch, TouchPhase};
use winit::keyboard::KeyCode;

const BAR: f32 = 20.0;
const GAP: f32 = 4.0;
const PANEL: u32 = 0xf010_1216;
const FIELD_BG: u32 = 0xff1c_2028;
const FIELD_BORDER: u32 = 0xff50_5868;
const FIELD_ACTIVE: u32 = 0xff70_d8ff;
const BUTTON: u32 = 0xff28_68a8;
const BUTTON_GREEN: u32 = 0xff30_8040;
const BUTTON_OFF: u32 = 0xff40_4448;
const BUTTON_RED: u32 = 0xff98_3838;
const INK: u32 = 0xffff_ffff;
const DIM: u32 = 0xffb0_b6c0;
const ACCENT: u32 = 0xff70_d8ff;
const GOOD: u32 = 0xff70_e890;
const BAD: u32 = 0xffff_7070;

/// The open iOS Account overlay (supports both Offline Account and Microsoft Account).
pub(super) struct AccountPanel {
    login: Option<Login>,
    state: Option<State>,
    /// True once the current device code has been copied to the clipboard.
    copied: bool,
    /// Editable offline username buffer (1–16 ASCII alphanumeric / `_`).
    offline_input: String,
    /// True when the offline username text field is focused for IME / keyboard input.
    pub(super) editing_offline: bool,
    /// Optional status feedback `(message, is_error)` after saving or invalid input.
    notice: Option<(String, bool)>,
}

impl AccountPanel {
    fn open(current_offline: String) -> Self {
        Self {
            login: None,
            state: None,
            copied: false,
            offline_input: current_offline,
            editing_offline: false,
            notice: None,
        }
    }

    fn start_login(&mut self) {
        if let Some(old) = self.login.take() {
            old.cancel();
        }
        self.editing_offline = false;
        self.notice = None;
        self.login = Some(Login::start());
        self.state = Some(State::Starting);
        self.copied = false;
    }

    fn cancel_login(&mut self) {
        if let Some(login) = self.login.take() {
            login.cancel();
        }
        self.state = None;
        self.copied = false;
    }

    fn close(&mut self) {
        self.cancel_login();
        self.editing_offline = false;
    }

    fn in_ms_flow(&self) -> bool {
        matches!(
            self.state,
            Some(State::Starting | State::Code { .. } | State::Working | State::Failed(_))
        )
    }
}

struct PanelLayout {
    panel: Area,
    close: Area,
    offline_field: Area,
    offline_apply: Area,
    primary: Area,
    secondary: Option<Area>,
}

fn layout(safe: Area) -> PanelLayout {
    let w = (safe.w - GAP * 4.0).clamp(210.0, 310.0);
    let h = (safe.h - GAP * 4.0).clamp(138.0, 168.0);
    let panel = Area {
        x: safe.x + (safe.w - w) / 2.0,
        y: safe.y + (safe.h - h) / 2.0,
        w,
        h,
    };
    let close = Area {
        x: panel.x + panel.w - 46.0 - GAP,
        y: panel.y + GAP,
        w: 46.0,
        h: BAR,
    };
    let apply_w = 78.0;
    let field_y = panel.y + BAR + GAP * 3.0 + 36.0;
    let offline_field = Area {
        x: panel.x + GAP * 2.0,
        y: field_y,
        w: (panel.w - GAP * 5.0 - apply_w).max(80.0),
        h: BAR,
    };
    let offline_apply = Area {
        x: offline_field.x + offline_field.w + GAP,
        y: field_y,
        w: apply_w,
        h: BAR,
    };
    let bw = ((panel.w - GAP * 3.0) / 2.0).max(76.0);
    let by = panel.y + panel.h - BAR - GAP * 2.0;
    let primary = Area {
        x: panel.x + GAP,
        y: by,
        w: bw,
        h: BAR,
    };
    let secondary = Some(Area {
        x: panel.x + panel.w - bw - GAP,
        y: by,
        w: bw,
        h: BAR,
    });
    PanelLayout {
        panel,
        close,
        offline_field,
        offline_apply,
        primary,
        secondary,
    }
}

fn centred(batch: &mut HudBatch, atlas: &HudAtlas, text: &str, area: Area, colour: u32) {
    let shown = fit(atlas, text, (area.w - 6.0).max(12.0));
    let w = HudBatch::text_width(atlas, &shown);
    batch.text(
        atlas,
        &shown,
        area.x + (area.w - w) / 2.0,
        area.y + (area.h - 8.0) / 2.0,
        colour,
        true,
    );
}

fn fit(atlas: &HudAtlas, text: &str, max_w: f32) -> String {
    if HudBatch::text_width(atlas, text) <= max_w {
        return text.to_owned();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut next = out.clone();
        next.push(ch);
        next.push_str("...");
        if HudBatch::text_width(atlas, &next) > max_w {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn filter_mc_name_chars(input: &str, dest: &mut String) {
    for ch in input.chars() {
        if dest.len() >= 16 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            dest.push(ch);
        }
    }
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
impl Game<'_> {
    fn account_safe(&self, batch: &HudBatch) -> Area {
        self.touch_safe(batch.width, batch.height, batch.scale)
    }

    /// True while the iOS Account panel's Offline Username box is focused.
    pub(super) fn account_editing_offline(&self) -> bool {
        self.pad
            .account_panel
            .as_ref()
            .is_some_and(|p| p.editing_offline && !p.in_ms_flow())
    }

    /// Appends valid Minecraft username characters into the Offline Username field.
    pub(super) fn account_type_text(&mut self, text: &str) {
        if let Some(panel) = self.pad.account_panel.as_mut()
            && panel.editing_offline
            && !panel.in_ms_flow()
        {
            filter_mc_name_chars(text, &mut panel.offline_input);
            panel.notice = None;
        }
    }

    /// Pastes clipboard text into the Offline Username field.
    pub(super) fn account_paste(&mut self, raw: &str) {
        self.account_type_text(raw);
    }

    /// Saves the typed offline username to `horizon-player.txt` and activates Offline mode.
    fn account_apply_offline(&mut self) {
        let name = match self.pad.account_panel.as_ref() {
            Some(p) => p.offline_input.trim().to_owned(),
            None => return,
        };
        let dir = horizon_client::logging::player_dir();
        match horizon_client::offline_name::save(&dir, &name) {
            Ok(()) => {
                // Switch out of any active Microsoft session so offline play persists cleanly.
                if horizon_client::auth::current().is_some() || ms_login::saved_name().is_some() {
                    ms_login::sign_out();
                }
                self.options.player_name = Some(name.clone());
                if let Some(panel) = self.pad.account_panel.as_mut() {
                    panel.offline_input = name.clone();
                    panel.editing_offline = false;
                    panel.state = None;
                    panel.notice = Some((
                        horizon_client::lang::get("horizon.account.offline_saved")
                            .replace("{name}", &name),
                        false,
                    ));
                }
                self.dismiss_ime();
                self.audio.play_button_click();
            }
            Err(_) => {
                if let Some(panel) = self.pad.account_panel.as_mut() {
                    panel.notice = Some((
                        horizon_client::lang::get("horizon.account.offline_invalid"),
                        true,
                    ));
                }
            }
        }
    }

    /// Handles physical/virtual key events while the iOS Account panel is open.
    /// Returns `true` when the panel consumed the key.
    pub(super) fn account_key(&mut self, key: KeyCode, text: Option<&str>) -> bool {
        let Some(panel) = self.pad.account_panel.as_mut() else {
            return false;
        };
        if key == KeyCode::Escape {
            if panel.editing_offline {
                panel.editing_offline = false;
                self.dismiss_ime();
            } else if panel.in_ms_flow() {
                panel.cancel_login();
            } else if let Some(mut closed) = self.pad.account_panel.take() {
                closed.close();
                self.sync_ime();
            }
            return true;
        }
        if !panel.editing_offline || panel.in_ms_flow() {
            return false;
        }
        match key {
            KeyCode::Backspace | KeyCode::Delete => {
                panel.offline_input.pop();
                panel.notice = None;
                true
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                self.account_apply_offline();
                true
            }
            _ => {
                if let Some(t) = text {
                    filter_mc_name_chars(t, &mut panel.offline_input);
                    panel.notice = None;
                }
                true
            }
        }
    }

    /// The title screen's Account button (shows signed-in Microsoft name or active offline name),
    /// to the left of "Get mods" (iOS).
    fn account_button(&self, batch: &HudBatch) -> Option<Area> {
        if !super::touch::TOUCH_CONTROLS
            || self.title.is_none()
            || self.screen == Screen::Options
            || self.pad.mod_browser.is_some()
            || self.pad.account_panel.is_some()
        {
            return None;
        }
        let safe = self.account_safe(batch);
        let w = 84.0;
        // Sits immediately to the left of the 70 dp "Get mods" button.
        let x = safe.x + safe.w - 70.0 - GAP - w - GAP;
        Some(Area {
            x,
            y: safe.y + GAP,
            w,
            h: BAR,
        })
    }

    /// Poll a running Microsoft sign-in thread and copy the device code when it arrives.
    pub(super) fn account_poll(&mut self) {
        let Some(panel) = self.pad.account_panel.as_mut() else {
            return;
        };
        let Some(login) = panel.login.as_ref() else {
            return;
        };
        let state = login.state();
        if let State::Code { user_code, .. } = &state
            && !panel.copied
        {
            self.clipboard.set(user_code);
            panel.copied = true;
        }
        if matches!(state, State::Done(_) | State::Failed(_)) {
            panel.login = None;
        }
        if let State::Done(name) = &state {
            self.options.player_name = Some(name.clone());
        }
        panel.state = Some(state);
    }

    /// A touch for the iOS Account button or its overlay: `true` when it took it.
    pub(super) fn account_touch(&mut self, touch: &Touch) -> bool {
        let Some(batch) = self.gui_scale() else {
            return false;
        };
        let at = (
            touch.location.x as f32 / batch.scale,
            touch.location.y as f32 / batch.scale,
        );
        if self.pad.account_panel.is_none() {
            if touch.phase == TouchPhase::Started
                && self.account_button(&batch).is_some_and(|b| b.contains(at))
            {
                let current_offline = horizon_client::offline_name::load_or_create(
                    &horizon_client::logging::player_dir(),
                );
                log::info!("Horizon: opening the iOS account panel");
                self.pad.account_panel = Some(AccountPanel::open(current_offline));
                return true;
            }
            return false;
        }
        if touch.phase != TouchPhase::Started {
            return true;
        }
        let l = layout(self.account_safe(&batch));
        if l.close.contains(at) {
            if let Some(mut panel) = self.pad.account_panel.take() {
                panel.close();
            }
            self.dismiss_ime();
            return true;
        }

        let in_ms_flow = self
            .pad
            .account_panel
            .as_ref()
            .is_some_and(AccountPanel::in_ms_flow);
        if in_ms_flow {
            let state = self
                .pad
                .account_panel
                .as_ref()
                .and_then(|p| p.state.clone());
            match state {
                Some(State::Code { user_code, uri }) => {
                    if l.primary.contains(at) {
                        self.clipboard.set(&user_code);
                        packs::open_uri(&uri);
                        return true;
                    }
                    if l.secondary.is_some_and(|s| s.contains(at)) {
                        self.clipboard.set(&user_code);
                        return true;
                    }
                }
                Some(State::Failed(_)) => {
                    if l.primary.contains(at)
                        && let Some(panel) = self.pad.account_panel.as_mut()
                    {
                        panel.start_login();
                        return true;
                    }
                    if l.secondary.is_some_and(|s| s.contains(at))
                        && let Some(panel) = self.pad.account_panel.as_mut()
                    {
                        panel.cancel_login();
                        return true;
                    }
                }
                Some(State::Starting | State::Working) => {
                    if l.primary.contains(at)
                        && let Some(panel) = self.pad.account_panel.as_mut()
                    {
                        panel.cancel_login();
                        return true;
                    }
                }
                _ => {}
            }
            return true;
        }

        // Offline username field tap: focus & open keyboard.
        if l.offline_field.contains(at) {
            if let Some(panel) = self.pad.account_panel.as_mut() {
                panel.editing_offline = true;
            }
            self.ime_dismissed = false;
            self.sync_ime();
            return true;
        }
        // "Use Offline" button tap: validate, persist, and switch to Offline mode.
        if l.offline_apply.contains(at) {
            self.account_apply_offline();
            return true;
        }

        // Tapping elsewhere inside the panel dismisses offline field focus cleanly.
        let was_editing = self.account_editing_offline();
        if was_editing {
            if let Some(panel) = self.pad.account_panel.as_mut() {
                panel.editing_offline = false;
            }
            self.dismiss_ime();
        }

        let signed_in = horizon_client::auth::current();
        if signed_in.is_some() {
            if l.primary.contains(at) {
                ms_login::sign_out();
                self.options.player_name = None;
                if let Some(panel) = self.pad.account_panel.as_mut() {
                    panel.state = None;
                    panel.notice = None;
                }
                return true;
            }
            if l.secondary.is_some_and(|s| s.contains(at)) {
                if let Some(panel) = self.pad.account_panel.as_mut() {
                    panel.start_login();
                }
                return true;
            }
            return true;
        }

        // Not signed in with Microsoft: bottom button starts Microsoft sign-in.
        let ms_btn = Area {
            x: l.panel.x + GAP * 2.0,
            y: l.primary.y,
            w: l.panel.w - GAP * 4.0,
            h: BAR,
        };
        if ms_btn.contains(at)
            && let Some(panel) = self.pad.account_panel.as_mut()
        {
            panel.start_login();
            return true;
        }
        true
    }

    /// The title screen's account button, or the account overlay over everything.
    pub(super) fn draw_account(&self, batch: &mut HudBatch) {
        let atlas = &self.gui.atlas;
        if let Some(b) = self.account_button(batch) {
            let signed_in = horizon_client::auth::current();
            let (label, bg) = match signed_in {
                Some(session) => (session.name.clone(), BUTTON_GREEN),
                None => (self.options.player_name(), BUTTON),
            };
            batch.fill(atlas, b.x, b.y, b.w, b.h, bg);
            centred(batch, atlas, &label, b, INK);
        }
        let Some(panel) = self.pad.account_panel.as_ref() else {
            return;
        };
        let l = layout(self.account_safe(batch));
        let p = l.panel;
        batch.fill(atlas, 0.0, 0.0, batch.width, batch.height, 0x8000_0000);
        batch.fill(atlas, p.x, p.y, p.w, p.h, PANEL);
        batch.text(
            atlas,
            &horizon_client::lang::get("horizon.account.title"),
            p.x + GAP * 2.0,
            p.y + GAP + 6.0,
            INK,
            true,
        );
        batch.fill(
            atlas, l.close.x, l.close.y, l.close.w, l.close.h, BUTTON_OFF,
        );
        centred(
            batch,
            atlas,
            &horizon_client::lang::get("horizon.mods.close"),
            l.close,
            INK,
        );
        let x = p.x + GAP * 2.0;
        let max_w = p.w - GAP * 4.0;
        let mut y = p.y + BAR + GAP * 3.0;

        if panel.in_ms_flow() {
            match panel.state.as_ref() {
                Some(State::Starting) => {
                    batch.text(
                        atlas,
                        &fit(
                            atlas,
                            &horizon_client::lang::get("horizon.account.starting"),
                            max_w,
                        ),
                        x,
                        y,
                        DIM,
                        false,
                    );
                    batch.fill(
                        atlas, l.primary.x, l.primary.y, l.primary.w, l.primary.h, BUTTON_OFF,
                    );
                    centred(
                        batch,
                        atlas,
                        &horizon_client::lang::get("gui.cancel"),
                        l.primary,
                        INK,
                    );
                }
                Some(State::Code { user_code, uri }) => {
                    let step = horizon_client::lang::get("horizon.account.enter_code")
                        .replace("{uri}", uri);
                    batch.text(atlas, &fit(atlas, &step, max_w), x, y, DIM, false);
                    y += 15.0;
                    batch.text(atlas, user_code, x, y, ACCENT, true);
                    y += 15.0;
                    batch.text(
                        atlas,
                        &fit(
                            atlas,
                            &horizon_client::lang::get("horizon.account.copied"),
                            max_w,
                        ),
                        x,
                        y,
                        DIM,
                        false,
                    );
                    batch.fill(
                        atlas, l.primary.x, l.primary.y, l.primary.w, l.primary.h, BUTTON_GREEN,
                    );
                    centred(
                        batch,
                        atlas,
                        &horizon_client::lang::get("horizon.account.open_safari"),
                        l.primary,
                        INK,
                    );
                    if let Some(sec) = l.secondary {
                        batch.fill(atlas, sec.x, sec.y, sec.w, sec.h, BUTTON);
                        centred(
                            batch,
                            atlas,
                            &horizon_client::lang::get("horizon.account.copy_code"),
                            sec,
                            INK,
                        );
                    }
                }
                Some(State::Working) => {
                    batch.text(
                        atlas,
                        &fit(
                            atlas,
                            &horizon_client::lang::get("horizon.account.working"),
                            max_w,
                        ),
                        x,
                        y,
                        DIM,
                        false,
                    );
                }
                Some(State::Failed(err)) => {
                    batch.text(atlas, &fit(atlas, err, max_w), x, y, BAD, false);
                    batch.fill(
                        atlas, l.primary.x, l.primary.y, l.primary.w, l.primary.h, BUTTON,
                    );
                    centred(
                        batch,
                        atlas,
                        &horizon_client::lang::get("horizon.account.try_again"),
                        l.primary,
                        INK,
                    );
                    if let Some(sec) = l.secondary {
                        batch.fill(atlas, sec.x, sec.y, sec.w, sec.h, BUTTON_OFF);
                        centred(
                            batch,
                            atlas,
                            &horizon_client::lang::get("gui.cancel"),
                            sec,
                            INK,
                        );
                    }
                }
                _ => {}
            }
            return;
        }

        // Status header: Online (Microsoft) vs Offline mode.
        let signed_in = horizon_client::auth::current();
        if let Some(session) = signed_in.as_ref() {
            let line = horizon_client::lang::get("horizon.account.signed_in")
                .replace("{name}", &session.name);
            batch.text(atlas, &fit(atlas, &line, max_w), x, y, GOOD, true);
            y += 12.0;
            batch.text(
                atlas,
                &fit(
                    atlas,
                    &horizon_client::lang::get("horizon.account.online_ready"),
                    max_w,
                ),
                x,
                y,
                DIM,
                false,
            );
        } else {
            let active_name = self.options.player_name();
            let line = horizon_client::lang::get("horizon.account.offline_active")
                .replace("{name}", &active_name);
            batch.text(atlas, &fit(atlas, &line, max_w), x, y, ACCENT, true);
            y += 12.0;
            batch.text(
                atlas,
                &fit(
                    atlas,
                    &horizon_client::lang::get("horizon.account.offline"),
                    max_w,
                ),
                x,
                y,
                DIM,
                false,
            );
        }

        // Offline Account section: label + editable username field + "Use Offline" button.
        let label_y = l.offline_field.y - 11.0;
        batch.text(
            atlas,
            &fit(
                atlas,
                &horizon_client::lang::get("horizon.account.offline_label"),
                max_w,
            ),
            x,
            label_y,
            DIM,
            false,
        );
        let f = l.offline_field;
        let border = if panel.editing_offline {
            FIELD_ACTIVE
        } else {
            FIELD_BORDER
        };
        batch.fill(atlas, f.x - 1.0, f.y - 1.0, f.w + 2.0, f.h + 2.0, border);
        batch.fill(atlas, f.x, f.y, f.w, f.h, FIELD_BG);
        let display_text = if panel.editing_offline {
            format!("{}_", panel.offline_input)
        } else if panel.offline_input.is_empty() {
            horizon_client::lang::get("horizon.account.offline_placeholder")
        } else {
            panel.offline_input.clone()
        };
        let text_col = if panel.offline_input.is_empty() && !panel.editing_offline {
            DIM
        } else {
            INK
        };
        batch.text(
            atlas,
            &fit(atlas, &display_text, (f.w - 8.0).max(16.0)),
            f.x + 4.0,
            f.y + (f.h - 8.0) / 2.0,
            text_col,
            true,
        );
        batch.fill(
            atlas,
            l.offline_apply.x,
            l.offline_apply.y,
            l.offline_apply.w,
            l.offline_apply.h,
            BUTTON_GREEN,
        );
        centred(
            batch,
            atlas,
            &horizon_client::lang::get("horizon.account.use_offline"),
            l.offline_apply,
            INK,
        );

        // Feedback notice (saved / invalid username).
        if let Some((msg, is_err)) = panel.notice.as_ref() {
            let notice_y = f.y + f.h + 5.0;
            batch.text(
                atlas,
                &fit(atlas, msg, max_w),
                x,
                notice_y,
                if *is_err { BAD } else { GOOD },
                false,
            );
        }

        // Bottom Microsoft Account action row.
        if signed_in.is_some() {
            batch.fill(
                atlas, l.primary.x, l.primary.y, l.primary.w, l.primary.h, BUTTON_RED,
            );
            centred(
                batch,
                atlas,
                &horizon_client::lang::get("horizon.account.sign_out"),
                l.primary,
                INK,
            );
            if let Some(sec) = l.secondary {
                batch.fill(atlas, sec.x, sec.y, sec.w, sec.h, BUTTON);
                centred(
                    batch,
                    atlas,
                    &horizon_client::lang::get("horizon.account.switch"),
                    sec,
                    INK,
                );
            }
        } else {
            let ms_btn = Area {
                x: p.x + GAP * 2.0,
                y: l.primary.y,
                w: p.w - GAP * 4.0,
                h: BAR,
            };
            batch.fill(atlas, ms_btn.x, ms_btn.y, ms_btn.w, ms_btn.h, BUTTON);
            centred(
                batch,
                atlas,
                &horizon_client::lang::get("horizon.account.sign_in_ms"),
                ms_btn,
                INK,
            );
        }
    }
}
