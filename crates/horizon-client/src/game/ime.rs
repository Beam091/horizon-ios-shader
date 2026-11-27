//! Input method (IME) text: committed composition goes to the focused text
//! field through the same sinks the keyboard and `horizon-ctl type` use.
//! The window only asks the OS for an input method while such a field has
//! focus, so movement keys keep arriving as plain key events.
use super::{Game, Screen, books::BookScreen};
use horizon_client::{
    hud::HudBatch,
    preedit::{self, PreeditEvent},
};

impl Game<'_> {
    /// Whether a text box has focus right now (the fields `ime_commit` can fill).
    pub(super) fn text_field_focused(&mut self) -> bool {
        if self.account_editing_offline() {
            return true;
        }
        if self.dialog_open() {
            return self.dialog_has_text_field();
        }
        if let Some(title) = self.title.as_mut() {
            // A caret field, an append-only one (`focused_field`), or a game
            // rule's box (`has_caret_field` asks the rules screen itself, and
            // `title_edit` types into it).
            return title.has_caret_field() || title.focused_field().is_some();
        }
        if self.sign_edit.is_some() || matches!(self.book, Some(BookScreen::Edit { .. })) {
            return true;
        }
        match self.screen {
            Screen::Chat | Screen::CommandBlock | Screen::Social => true,
            Screen::DebugOptions => self
                .debug_options
                .as_ref()
                .is_some_and(|s| s.search_focused()),
            // The language list's search, the settings search and the pack
            // screen's search (OptionsUi::typing).
            Screen::Options => self.ui.options.typing(),
            Screen::WorldOptions => self.world_options_typing(),
            Screen::Creative => self.creative_screen.tab().search,
            Screen::BlockEditor => self.block_editor.as_ref().is_some_and(|s| s.text_focused()),
            Screen::Inventory => {
                self.mod_browser_typing() || self.recipe_search_focused() || self.station_typing()
            }
            _ => false,
        }
    }

    /// Folds down the soft keyboard (e.g. when pressing Return or tapping outside a text box)
    /// until the player taps a text field again.
    pub(super) fn dismiss_ime(&mut self) {
        self.ime_dismissed = true;
        self.sync_ime();
    }

    /// Routes an Enter/Return from IME or soft keyboard to the active field (inserting newlines
    /// in Signs and Books without folding the keyboard, submitting Chat or Offline Account, and
    /// folding the keyboard for single-line search/rename fields).
    pub(super) fn handle_ime_enter(&mut self) {
        if self.account_editing_offline() {
            let _ = self.account_key(winit::keyboard::KeyCode::Enter, None);
            return;
        }
        if self.sign_edit.is_some() {
            self.sign_input(Some(winit::keyboard::KeyCode::Enter), None);
            return;
        }
        if matches!(self.book, Some(BookScreen::Edit { .. })) {
            self.book_input(Some(winit::keyboard::KeyCode::Enter), None);
            return;
        }
        if self.screen == Screen::Chat && self.title.is_none() {
            self.chat_key_code(winit::keyboard::KeyCode::Enter, None);
        }
        self.dismiss_ime();
    }

    /// Allow the input method only while a text box has focus and has not been dismissed.
    pub(super) fn sync_ime(&mut self) {
        // iOS: follow the iPhone keyboard's height, for the chat to sit above it.
        horizon_client::screen_keyboard::watch();
        let raw = self.text_field_focused();
        if !raw {
            self.ime_dismissed = false;
        }
        let want = raw && !self.ime_dismissed;
        if want != self.ime_allowed
            && let Some(window) = &self.window
        {
            window.set_ime_allowed(want);
            self.ime_allowed = want;
            // The field lost the focus (or a new one took it): the composition
            // and the candidate window's place belong to the old one.
            self.ime_area = None;
            if !want {
                preedit::set_preedit(None);
            }
        }
    }

    /// `WindowEvent::Ime(Preedit)`: `KeyboardHandler.textEditing`. The text
    /// still being composed is shown under the focused field's caret.
    pub(super) fn ime_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        preedit::set_preedit(
            self.text_field_focused()
                .then(|| PreeditEvent::from_winit(text, cursor))
                .flatten(),
        );
    }

    /// With the frame's GUI built: draw the composition over it and move the
    /// input method's candidate window to the caret
    /// (`TextInputManager.setTextInputArea`, in window pixels).
    pub(super) fn flush_ime(&mut self, hud: &mut HudBatch) {
        preedit::draw_overlay(hud, &self.gui.atlas);
        let Some([x0, y0, x1, y1]) = preedit::take_text_input_area() else {
            return;
        };
        let scale = hud.scale;
        let x = (x0 * scale).round() as i32;
        let y = (y0 * scale).round() as i32;
        let width = (((x1 - x0) * scale).round() as i32).max(1);
        let height = (((y1 - y0) * scale).round() as i32).max(1);
        let area = [x, y, width, height];
        if self.ime_area == Some(area) {
            return;
        }
        if let Some(window) = &self.window
            && self.ime_allowed
        {
            window.set_ime_cursor_area(
                winit::dpi::PhysicalPosition::new(x, y),
                winit::dpi::PhysicalSize::new(width as u32, height as u32),
            );
            self.ime_area = Some(area);
        }
    }

    /// Text the input method committed (`WindowEvent::Ime(Commit)`).
    pub(super) fn ime_commit(&mut self, text: &str) {
        preedit::set_preedit(None);
        let has_enter = text.contains('\n') || text.contains('\r');
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if self.account_editing_offline() {
            if !text.is_empty() {
                self.account_type_text(&text);
            }
            if has_enter {
                self.handle_ime_enter();
            }
            return;
        }
        // ScreenEvent.CharacterTyped.Pre: the input method's committed text
        // reaches a text box by characters, as a key's own text does, so the
        // open screen hears of each of them first and a cancelled one never
        // reaches the box.
        let Some(text) = self.screen_text_typed(&text) else {
            if has_enter {
                self.handle_ime_enter();
            }
            return;
        };
        if !self.text_field_focused() {
            if has_enter {
                self.dismiss_ime();
            }
            return;
        }
        if !text.is_empty() {
            if self.dialog_type(&text) {
                if has_enter {
                    self.handle_ime_enter();
                }
                return;
            }
            if self.title.is_some() {
                self.title_edit(Some(&text), false);
            } else if self.book.is_some() {
                self.book_input(None, Some(&text));
            } else if self.sign_edit.is_some() {
                self.sign_input(None, Some(&text));
            } else {
                match self.screen {
                    Screen::Chat => self.chat_type(&text),
                    Screen::CommandBlock => self.command_block_type(&text),
                    Screen::Social => self.social_type(&text),
                    Screen::DebugOptions => self.debug_options_type(&text),
                    Screen::Options => self.ui.options.type_text(&self.gui.atlas, &text),
                    Screen::WorldOptions => self.world_options_text(Some(&text)),
                    Screen::Creative => self.creative_search_edit(Some(&text)),
                    Screen::BlockEditor => self.block_editor_type(&text),
                    Screen::Inventory if self.mod_browser_typing() => {
                        self.mod_browser_text(Some(&text), false)
                    }
                    Screen::Inventory if self.recipe_search_focused() => {
                        self.recipe_text(Some(&text), false)
                    }
                    Screen::Inventory => self.station_text(Some(&text), false),
                    _ => {}
                }
            }
        }
        if has_enter {
            self.handle_ime_enter();
        }
    }

    /// Pastes text from the system clipboard (`UIPasteboard` on iOS) directly into whichever text
    /// field currently has focus, keeping the soft keyboard open.
    pub(super) fn paste_into_focused_field(&mut self) {
        if !self.text_field_focused() {
            return;
        }
        let raw = {
            use horizon_client::clipboard::Clipboard;
            self.clipboard.get()
        };
        if raw.is_empty() {
            return;
        }
        if self.account_editing_offline() {
            self.account_paste(&raw);
            return;
        }
        // Normalize line endings into spaces (preserving single spaces) and strip control chars
        // so single-line fields (Chat, Server IP, Command Block, Anvil, Search) paste cleanly
        // without triggering Return / dismissing the iOS soft keyboard.
        let mut cleaned = String::with_capacity(raw.len());
        let mut prev_newline = false;
        for ch in raw.chars() {
            if ch == '\n' || ch == '\r' {
                if !prev_newline {
                    cleaned.push(' ');
                    prev_newline = true;
                }
            } else if !ch.is_control() {
                cleaned.push(ch);
                prev_newline = false;
            }
        }
        let cleaned = cleaned.trim_end_matches(' ');
        if !cleaned.is_empty() {
            self.ime_commit(cleaned);
        }
    }
}
