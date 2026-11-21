//! The Microsoft sign-in overlay of the iOS build (which has no PC launcher): a "Sign in"
//! button (or the signed-in player's name) sits beside "Get mods" on the title screen. Tapping
//! it starts the OAuth2 device-code flow (`horizon_client::ms_login`), copies the sign-in code
//! to the clipboard and opens `microsoft.com/link` in Safari so the player can join online-mode
//! servers directly from iPhone or iPad.
use super::*;
use horizon_client::clipboard::Clipboard;
use horizon_client::ms_login::{self, Login, State};
use horizon_client_input::touch::Area;
use winit::event::{Touch, TouchPhase};

const BAR: f32 = 20.0;
const GAP: f32 = 4.0;
const PANEL: u32 = 0xf010_1216;
const BUTTON: u32 = 0xff28_68a8;
const BUTTON_GREEN: u32 = 0xff30_8040;
const BUTTON_OFF: u32 = 0xff40_4448;
const BUTTON_RED: u32 = 0xff98_3838;
const INK: u32 = 0xffff_ffff;
const DIM: u32 = 0xffb0_b6c0;
const ACCENT: u32 = 0xff70_d8ff;
const BAD: u32 = 0xffff_7070;

/// The open Microsoft account overlay on iOS.
pub(super) struct AccountPanel {
    login: Option<Login>,
    state: Option<State>,
    /// True once the current device code has been copied to the clipboard.
    copied: bool,
}

impl AccountPanel {
    fn open() -> Self {
        let mut panel = Self {
            login: None,
            state: None,
            copied: false,
        };
        if horizon_client::auth::current().is_none() {
            panel.start_login();
        }
        panel
    }

    fn start_login(&mut self) {
        if let Some(old) = self.login.take() {
            old.cancel();
        }
        self.login = Some(Login::start());
        self.state = Some(State::Starting);
        self.copied = false;
    }

    fn close(&mut self) {
        if let Some(login) = self.login.take() {
            login.cancel();
        }
    }
}

struct PanelLayout {
    panel: Area,
    close: Area,
    primary: Area,
    secondary: Option<Area>,
}

fn layout(safe: Area) -> PanelLayout {
    let w = (safe.w - GAP * 4.0).clamp(180.0, 290.0);
    let h = (safe.h - GAP * 4.0).clamp(112.0, 146.0);
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

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
impl Game<'_> {
    fn account_safe(&self, batch: &HudBatch) -> Area {
        self.touch_safe(batch.width, batch.height, batch.scale)
    }

    /// The title screen's "Sign in" (or signed-in player's name) button, to the left of
    /// "Get mods" (iOS).
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
        let w = 78.0;
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
        panel.state = Some(state);
    }

    /// A touch for the Microsoft account button or its overlay: `true` when it took it.
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
                log::info!("Horizon: opening the Microsoft sign-in panel");
                self.pad.account_panel = Some(AccountPanel::open());
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
            return true;
        }
        let signed_in = horizon_client::auth::current();
        if let Some(session) = signed_in {
            if l.primary.contains(at) {
                let _ = session;
                ms_login::sign_out();
                if let Some(panel) = self.pad.account_panel.as_mut() {
                    panel.state = None;
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
            Some(State::Failed(_)) | None => {
                if l.primary.contains(at)
                    && let Some(panel) = self.pad.account_panel.as_mut()
                {
                    panel.start_login();
                    return true;
                }
            }
            Some(State::Starting | State::Working | State::Done(_)) => {}
        }
        true
    }

    /// The title screen's account button, or the sign-in overlay over everything.
    pub(super) fn draw_account(&self, batch: &mut HudBatch) {
        let atlas = &self.gui.atlas;
        if let Some(b) = self.account_button(batch) {
            let signed_in = horizon_client::auth::current();
            let (label, bg) = match signed_in {
                Some(session) => (session.name.clone(), BUTTON_GREEN),
                None => (
                    ms_login::saved_name()
                        .unwrap_or_else(|| horizon_client::lang::get("horizon.account.sign_in")),
                    BUTTON,
                ),
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
        if let Some(session) = horizon_client::auth::current() {
            let line = horizon_client::lang::get("horizon.account.signed_in")
                .replace("{name}", &session.name);
            batch.text(atlas, &fit(atlas, &line, max_w), x, y, INK, true);
            y += 14.0;
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
            return;
        }
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
            Some(State::Done(name)) => {
                let line =
                    horizon_client::lang::get("horizon.account.signed_in").replace("{name}", name);
                batch.text(atlas, &fit(atlas, &line, max_w), x, y, INK, true);
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
            }
            None => {
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
                batch.fill(
                    atlas, l.primary.x, l.primary.y, l.primary.w, l.primary.h, BUTTON,
                );
                centred(
                    batch,
                    atlas,
                    &horizon_client::lang::get("horizon.account.sign_in"),
                    l.primary,
                    INK,
                );
            }
        }
    }
}
