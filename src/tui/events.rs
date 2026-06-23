use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

/// Low-level UI events produced by the terminal.
///
/// Keys are mapped to intent as late as possible: printable characters are
/// delivered as [`AppEvent::Char`] and interpreted as commands or text by the
/// event handler depending on the active view. Only keys that can never be
/// literal text input (navigation keys, Enter, Esc, Backspace, Delete, Ctrl
/// shortcuts) get a dedicated variant. This is what lets command/navigation
/// letters such as
/// `j`, `n` or `s` be typed into search boxes, titles, and the settings form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// Move selection / cursor up (Up arrow).
    Up,
    /// Move selection / cursor down (Down arrow).
    Down,
    /// Move cursor left (Left arrow).
    Left,
    /// Move cursor right (Right arrow).
    Right,
    /// Move cursor to start of current line.
    Home,
    /// Move cursor to end of current line.
    End,
    /// Move cursor one word left.
    WordLeft,
    /// Move cursor one word right.
    WordRight,
    /// Delete the previous word.
    WordBackspace,
    /// Scroll one page up.
    PageUp,
    /// Scroll one page down.
    PageDown,
    /// Delete the character after the cursor (Delete key).
    Delete,
    /// Confirm / newline (Enter).
    Confirm,
    /// Cancel current mode (Esc).
    Cancel,
    /// Save (Ctrl+S).
    Save,
    /// Undo the last edit (Ctrl+Z).
    Undo,
    /// Redo the last undone edit (Ctrl+Y or Ctrl+Shift+Z).
    Redo,
    /// Tab — toggles modes (e.g. keyword/semantic in search).
    Tab,
    /// Character input — a command key in command views, text elsewhere.
    Char(char),
    /// Backspace (delete previous character).
    Backspace,
    /// Open in-note find (Ctrl+F).
    Find,
    /// Toggle the editor's live markdown preview split (Ctrl+P).
    TogglePreview,
    /// No-op / unmapped key, also used as a periodic tick.
    Tick,
}

/// Start a background task that reads terminal events and sends them
/// through a channel.
pub fn start_event_listener() -> mpsc::Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel::<AppEvent>(32);

    tokio::spawn(async move {
        loop {
            // Poll for events with a short timeout so we can check the channel
            if let Ok(true) = event::poll(std::time::Duration::from_millis(100)) {
                if let Ok(Event::Key(key)) = event::read() {
                    if key.kind == KeyEventKind::Press {
                        let app_event = map_key(key.code, key.modifiers);
                        if tx.send(app_event).await.is_err() {
                            break;
                        }
                    }
                }
            } else {
                // Send a Tick event to keep the loop responsive
                if tx.send(AppEvent::Tick).await.is_err() {
                    break;
                }
            }
        }
    });

    rx
}

/// Map a crossterm KeyCode to a low-level AppEvent.
fn map_key(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    match code {
        // Ctrl+S (and the legacy Ctrl+X alias) save.
        KeyCode::Char('s') | KeyCode::Char('x') if modifiers.contains(KeyModifiers::CONTROL) => {
            AppEvent::Save
        }
        KeyCode::Char('w') | KeyCode::Char('W') if modifiers.contains(KeyModifiers::CONTROL) => {
            AppEvent::WordBackspace
        }
        // Ctrl+Shift+Z redoes; check it before the plain Ctrl+Z undo.
        KeyCode::Char('z') | KeyCode::Char('Z')
            if modifiers.contains(KeyModifiers::CONTROL)
                && modifiers.contains(KeyModifiers::SHIFT) =>
        {
            AppEvent::Redo
        }
        KeyCode::Char('z') | KeyCode::Char('Z') if modifiers.contains(KeyModifiers::CONTROL) => {
            AppEvent::Undo
        }
        KeyCode::Char('y') | KeyCode::Char('Y') if modifiers.contains(KeyModifiers::CONTROL) => {
            AppEvent::Redo
        }
        KeyCode::Char('f') | KeyCode::Char('F') if modifiers.contains(KeyModifiers::CONTROL) => {
            AppEvent::Find
        }
        KeyCode::Char('p') | KeyCode::Char('P') if modifiers.contains(KeyModifiers::CONTROL) => {
            AppEvent::TogglePreview
        }
        KeyCode::Left if modifiers.contains(KeyModifiers::CONTROL) => AppEvent::WordLeft,
        KeyCode::Right if modifiers.contains(KeyModifiers::CONTROL) => AppEvent::WordRight,
        KeyCode::Backspace if modifiers.contains(KeyModifiers::CONTROL) => AppEvent::WordBackspace,
        KeyCode::Home => AppEvent::Home,
        KeyCode::End => AppEvent::End,
        KeyCode::Up => AppEvent::Up,
        KeyCode::Down => AppEvent::Down,
        KeyCode::Left => AppEvent::Left,
        KeyCode::Right => AppEvent::Right,
        KeyCode::PageUp => AppEvent::PageUp,
        KeyCode::PageDown => AppEvent::PageDown,
        KeyCode::Tab => AppEvent::Tab,
        KeyCode::Enter => AppEvent::Confirm,
        KeyCode::Esc => AppEvent::Cancel,
        KeyCode::Backspace => AppEvent::Backspace,
        KeyCode::Delete => AppEvent::Delete,
        // All printable characters flow through as Char; the handler decides
        // whether they are commands or literal text based on the active view.
        KeyCode::Char(c) => AppEvent::Char(c),
        _ => AppEvent::Tick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_editor_navigation_keys() {
        assert_eq!(map_key(KeyCode::Home, KeyModifiers::NONE), AppEvent::Home);
        assert_eq!(map_key(KeyCode::End, KeyModifiers::NONE), AppEvent::End);
        assert_eq!(
            map_key(KeyCode::Left, KeyModifiers::CONTROL),
            AppEvent::WordLeft
        );
        assert_eq!(
            map_key(KeyCode::Right, KeyModifiers::CONTROL),
            AppEvent::WordRight
        );
        assert_eq!(
            map_key(KeyCode::Backspace, KeyModifiers::CONTROL),
            AppEvent::WordBackspace
        );
        assert_eq!(
            map_key(KeyCode::Char('w'), KeyModifiers::CONTROL),
            AppEvent::WordBackspace
        );
    }

    #[test]
    fn maps_undo_and_redo_keys() {
        assert_eq!(
            map_key(KeyCode::Char('z'), KeyModifiers::CONTROL),
            AppEvent::Undo
        );
        assert_eq!(
            map_key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            AppEvent::Redo
        );
        assert_eq!(
            map_key(
                KeyCode::Char('z'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            AppEvent::Redo
        );
        // A plain 'z' is still text, not undo.
        assert_eq!(
            map_key(KeyCode::Char('z'), KeyModifiers::NONE),
            AppEvent::Char('z')
        );
    }

    #[test]
    fn maps_ctrl_p_to_toggle_preview() {
        assert_eq!(
            map_key(KeyCode::Char('p'), KeyModifiers::CONTROL),
            AppEvent::TogglePreview
        );
        // A plain 'p' is still text, not a toggle.
        assert_eq!(
            map_key(KeyCode::Char('p'), KeyModifiers::NONE),
            AppEvent::Char('p')
        );
    }
}
