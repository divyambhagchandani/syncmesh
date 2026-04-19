//! Keyboard → `UiEvent` translation.
//!
//! Modal: the UI is in either "normal" mode (most keys trigger actions) or
//! "chat input" mode (keys feed into the input buffer). A third transient
//! "help overlay" mode dismisses on any key. Mode transitions are handled
//! by `run_ui`; this module is pure translation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::UiEvent;

/// Current UI input mode — determines how key events are interpreted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Chat,
    Help,
}

/// What the UI should do in response to a key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Emit this user-intent event to the event loop.
    Emit(UiEvent),
    /// Append a character to the chat input buffer.
    ChatAppend(char),
    /// Delete the last character from the chat input buffer.
    ChatBackspace,
    /// Delete the last word from the chat input buffer (Ctrl-W).
    ChatWordDelete,
    /// Submit the chat input buffer as a message (Enter in chat mode).
    ChatSubmit,
    /// Scroll the chat pane toward older messages. Disables follow-mode.
    ChatScrollUp,
    /// Scroll the chat pane toward newer messages. Re-enables follow-mode
    /// once we reach the bottom.
    ChatScrollDown,
    /// Jump to the newest chat message and re-enable follow-mode.
    ChatScrollBottom,
    /// Enter a different mode.
    SetMode(Mode),
    /// Ignore this key.
    Ignore,
}

/// Translate a `KeyEvent` to a `KeyAction` given the current mode. Pure —
/// unit-testable without a terminal.
#[must_use]
pub fn translate(mode: Mode, ev: KeyEvent) -> KeyAction {
    if mode == Mode::Help {
        // Any key dismisses the help overlay.
        return KeyAction::SetMode(Mode::Normal);
    }

    // Ctrl-C always quits, in any mode.
    if ev.modifiers.contains(KeyModifiers::CONTROL) && matches!(ev.code, KeyCode::Char('c')) {
        return KeyAction::Emit(UiEvent::Quit);
    }

    match mode {
        Mode::Help => unreachable!(),
        Mode::Normal => translate_normal(ev),
        Mode::Chat => translate_chat(ev),
    }
}

fn translate_normal(ev: KeyEvent) -> KeyAction {
    match ev.code {
        KeyCode::Char('q') => KeyAction::Emit(UiEvent::Quit),
        KeyCode::Char('r') => KeyAction::Emit(UiEvent::ToggleReady),
        KeyCode::Char('c') => KeyAction::Emit(UiEvent::CopyTicket),
        KeyCode::Char(' ') => KeyAction::Emit(UiEvent::TogglePauseRelay),
        KeyCode::Char('/') => KeyAction::SetMode(Mode::Chat),
        KeyCode::Tab => KeyAction::Emit(UiEvent::ToggleOverride),
        KeyCode::Char('?') => KeyAction::SetMode(Mode::Help),
        KeyCode::PageUp => KeyAction::ChatScrollUp,
        KeyCode::PageDown => KeyAction::ChatScrollDown,
        KeyCode::End => KeyAction::ChatScrollBottom,
        _ => KeyAction::Ignore,
    }
}

fn translate_chat(ev: KeyEvent) -> KeyAction {
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char('w') = ev.code {
            return KeyAction::ChatWordDelete;
        }
    }
    match ev.code {
        KeyCode::Enter => KeyAction::ChatSubmit,
        KeyCode::Esc => KeyAction::SetMode(Mode::Normal),
        KeyCode::Backspace => KeyAction::ChatBackspace,
        // PgUp/PgDn work in chat mode too so you can peek at scrollback while
        // composing. End jumps back to the bottom.
        KeyCode::PageUp => KeyAction::ChatScrollUp,
        KeyCode::PageDown => KeyAction::ChatScrollDown,
        KeyCode::End => KeyAction::ChatScrollBottom,
        KeyCode::Char(c) => KeyAction::ChatAppend(c),
        _ => KeyAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn normal_r_toggles_ready() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::Char('r'))),
            KeyAction::Emit(UiEvent::ToggleReady)
        );
    }

    #[test]
    fn normal_space_toggles_pause() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::Char(' '))),
            KeyAction::Emit(UiEvent::TogglePauseRelay)
        );
    }

    #[test]
    fn normal_slash_enters_chat_mode() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::Char('/'))),
            KeyAction::SetMode(Mode::Chat)
        );
    }

    #[test]
    fn normal_q_quits() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::Char('q'))),
            KeyAction::Emit(UiEvent::Quit)
        );
    }

    #[test]
    fn normal_tab_toggles_override() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::Tab)),
            KeyAction::Emit(UiEvent::ToggleOverride)
        );
    }

    #[test]
    fn normal_question_opens_help() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::Char('?'))),
            KeyAction::SetMode(Mode::Help)
        );
    }

    #[test]
    fn ctrl_c_always_quits_even_in_chat() {
        assert_eq!(
            translate(Mode::Chat, ctrl(KeyCode::Char('c'))),
            KeyAction::Emit(UiEvent::Quit)
        );
    }

    #[test]
    fn chat_enter_submits() {
        assert_eq!(
            translate(Mode::Chat, key(KeyCode::Enter)),
            KeyAction::ChatSubmit
        );
    }

    #[test]
    fn chat_esc_returns_to_normal() {
        assert_eq!(
            translate(Mode::Chat, key(KeyCode::Esc)),
            KeyAction::SetMode(Mode::Normal)
        );
    }

    #[test]
    fn chat_char_appends() {
        assert_eq!(
            translate(Mode::Chat, key(KeyCode::Char('x'))),
            KeyAction::ChatAppend('x')
        );
    }

    #[test]
    fn chat_ctrl_w_word_deletes() {
        assert_eq!(
            translate(Mode::Chat, ctrl(KeyCode::Char('w'))),
            KeyAction::ChatWordDelete
        );
    }

    #[test]
    fn chat_backspace_deletes_one() {
        assert_eq!(
            translate(Mode::Chat, key(KeyCode::Backspace)),
            KeyAction::ChatBackspace
        );
    }

    #[test]
    fn help_any_key_dismisses() {
        assert_eq!(
            translate(Mode::Help, key(KeyCode::Char('r'))),
            KeyAction::SetMode(Mode::Normal)
        );
        assert_eq!(
            translate(Mode::Help, key(KeyCode::Esc)),
            KeyAction::SetMode(Mode::Normal)
        );
    }

    #[test]
    fn normal_pageup_scrolls_chat_up() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::PageUp)),
            KeyAction::ChatScrollUp
        );
    }

    #[test]
    fn normal_pagedown_scrolls_chat_down() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::PageDown)),
            KeyAction::ChatScrollDown
        );
    }

    #[test]
    fn normal_end_jumps_chat_to_bottom() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::End)),
            KeyAction::ChatScrollBottom
        );
    }

    #[test]
    fn chat_mode_pageup_also_scrolls() {
        assert_eq!(
            translate(Mode::Chat, key(KeyCode::PageUp)),
            KeyAction::ChatScrollUp
        );
    }

    #[test]
    fn unmapped_normal_key_is_ignored() {
        assert_eq!(
            translate(Mode::Normal, key(KeyCode::Char('x'))),
            KeyAction::Ignore
        );
    }
}
