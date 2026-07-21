//! Keyboard handling, split into two jobs:
//!
//! 1. [`parse_command`] recognises the `Alt+Shift` master-prefix chords and maps
//!    them to a high-level [`Action`]. The prefix is a *modifier combo* (held
//!    together), chosen to avoid clashing with Hyprland's `$mainMod`.
//! 2. [`encode_key`] turns an ordinary keystroke into the byte sequence a shell
//!    expects, so it can be written straight to the focused PTY.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::Direction;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Move focus to the neighbouring pane in this direction.
    Focus(Direction),
    /// Create a new pane on this side of the focused pane and focus it.
    NewPane(Direction),
    ClosePane,
    NewSpace,
    RenameSpace,
    SwitchSpace(usize),
    PrevSpace,
    NextSpace,
}

/// Recognise an `Alt` master-prefix command. Returns `None` for anything that
/// isn't a bound command, so the caller can fall back to PTY forwarding.
///
/// Keymap (prefix = `Alt`):
/// * `Alt + w/a/s/d`  → focus the pane up/left/down/right
/// * `Alt + W/A/S/D`  (i.e. `Alt+Shift`) → create a new pane up/left/down/right
/// * `Alt + q` close pane · `Alt + t` new space · `Alt + r` rename space
/// * `Alt + 1..9` switch space · `Alt + ←/→` cycle spaces
///
/// "Shift" is detected robustly: an uppercase char OR the SHIFT flag, because
/// terminals consume the Shift modifier when it produces a shifted glyph.
pub fn parse_command(key: KeyEvent) -> Option<Action> {
    if !key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    let shift_flag = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char(c) if c.is_ascii_alphabetic() => {
            let shifted = c.is_ascii_uppercase() || shift_flag;
            match c.to_ascii_lowercase() {
                // WASD: lowercase focuses, uppercase (Shift) creates a pane.
                'w' => Some(dir_action(Direction::Up, shifted)),
                'a' => Some(dir_action(Direction::Left, shifted)),
                's' => Some(dir_action(Direction::Down, shifted)),
                'd' => Some(dir_action(Direction::Right, shifted)),
                'q' => Some(Action::ClosePane),
                't' => Some(Action::NewSpace),
                'r' => Some(Action::RenameSpace),
                _ => None,
            }
        }
        // Space switch — plain digits, or their shifted symbols if Shift was
        // consumed into '!', '@', ... (so Alt+Shift+1 works too).
        KeyCode::Char(c) if ('1'..='9').contains(&c) => {
            Some(Action::SwitchSpace(c as usize - '1' as usize))
        }
        KeyCode::Char(c) => shifted_digit(c).map(Action::SwitchSpace),
        KeyCode::Left => Some(Action::PrevSpace),
        KeyCode::Right => Some(Action::NextSpace),
        _ => None,
    }
}

fn dir_action(dir: Direction, new_pane: bool) -> Action {
    if new_pane {
        Action::NewPane(dir)
    } else {
        Action::Focus(dir)
    }
}

/// Map a shifted number-row symbol to a zero-based space index (`!`->0 .. `(`->8).
fn shifted_digit(c: char) -> Option<usize> {
    "!@#$%^&*(".chars().position(|s| s == c)
}

/// Encode an ordinary keystroke as the bytes a terminal application expects.
/// Returns `None` for keys with no sensible byte representation.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Control codes: map to the C0 range (e.g. Ctrl-C -> 0x03).
                let b = (c.to_ascii_uppercase() as u8) & 0x1f;
                vec![b]
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                let mut out = Vec::new();
                if alt {
                    out.push(0x1b); // Meta -> ESC prefix
                }
                out.extend_from_slice(s.as_bytes());
                out
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    const ALT: KeyModifiers = KeyModifiers::ALT;
    const ALT_SHIFT: KeyModifiers =
        KeyModifiers::from_bits_truncate(KeyModifiers::ALT.bits() | KeyModifiers::SHIFT.bits());

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), ALT)
    }

    #[test]
    fn lowercase_wasd_focuses_panes() {
        assert_eq!(parse_command(alt('w')), Some(Action::Focus(Direction::Up)));
        assert_eq!(parse_command(alt('a')), Some(Action::Focus(Direction::Left)));
        assert_eq!(parse_command(alt('s')), Some(Action::Focus(Direction::Down)));
        assert_eq!(parse_command(alt('d')), Some(Action::Focus(Direction::Right)));
    }

    #[test]
    fn uppercase_wasd_creates_panes() {
        // Uppercase letter (Shift consumed into the glyph), ALT only.
        assert_eq!(parse_command(alt('W')), Some(Action::NewPane(Direction::Up)));
        assert_eq!(parse_command(alt('A')), Some(Action::NewPane(Direction::Left)));
        assert_eq!(parse_command(alt('S')), Some(Action::NewPane(Direction::Down)));
        assert_eq!(parse_command(alt('D')), Some(Action::NewPane(Direction::Right)));
        // Same intent delivered as lowercase + explicit SHIFT flag.
        assert_eq!(
            parse_command(KeyEvent::new(KeyCode::Char('w'), ALT_SHIFT)),
            Some(Action::NewPane(Direction::Up))
        );
    }

    #[test]
    fn management_bindings() {
        assert_eq!(parse_command(alt('q')), Some(Action::ClosePane));
        assert_eq!(parse_command(alt('t')), Some(Action::NewSpace));
        assert_eq!(parse_command(alt('r')), Some(Action::RenameSpace));
    }

    #[test]
    fn digit_and_shifted_digit_switch_space() {
        assert_eq!(parse_command(alt('1')), Some(Action::SwitchSpace(0)));
        assert_eq!(parse_command(alt('9')), Some(Action::SwitchSpace(8)));
        // Alt+Shift+1 arrives as '!' (Shift consumed) -> space index 0.
        assert_eq!(parse_command(alt('!')), Some(Action::SwitchSpace(0)));
        assert_eq!(parse_command(alt('#')), Some(Action::SwitchSpace(2)));
    }

    #[test]
    fn arrows_cycle_spaces() {
        assert_eq!(
            parse_command(KeyEvent::new(KeyCode::Left, ALT)),
            Some(Action::PrevSpace)
        );
        assert_eq!(
            parse_command(KeyEvent::new(KeyCode::Right, ALT)),
            Some(Action::NextSpace)
        );
    }

    #[test]
    fn plain_key_without_alt_is_not_a_command() {
        // Without the Alt prefix, 'w' is just input for the shell.
        assert!(parse_command(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn encodes_control_and_arrows() {
        let ctrl_c = encode_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(ctrl_c, Some(vec![0x03]));
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
    }
}
