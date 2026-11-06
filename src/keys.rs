//! Keyboard handling, split into two jobs:
//!
//! 1. [`parse_command`] recognises the `Alt+Shift` master-prefix chords and maps
//!    them to a high-level [`Action`]. The prefix is a *modifier combo* (held
//!    together), chosen to avoid clashing with Hyprland's `$mainMod`.
//! 2. [`encode_key`] turns an ordinary keystroke into the byte sequence a shell
//!    expects, so it can be written straight to the focused PTY.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::protocol::{MouseAction, MouseButton, WireMouse};
use crate::screen::{MouseLevel, MouseTracking};
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
    /// Enter keyboard scrollback navigation on the focused pane.
    EnterScroll,
    /// Show the keybinding popup.
    Help,
}

/// Recognise an `Alt` master-prefix command. Returns `None` for anything that
/// isn't a bound command, so the caller can fall back to PTY forwarding.
///
/// Keymap (prefix = `Alt`):
/// * `Alt + w/a/s/d`  → focus the pane up/left/down/right
/// * `Alt + W/A/S/D`  (i.e. `Alt+Shift`) → create a new pane up/left/down/right
/// * `Alt + q` close pane · `Alt + t` new space · `Alt + r` rename space
/// * `Alt + 1..9` switch space · `Alt + ←/→` cycle spaces
/// * `Alt + h` keybinding help
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
                'h' => Some(Action::Help),
                _ => None,
            }
        }
        // Space switch — plain digits, or their shifted symbols if Shift was
        // consumed into '!', '@', ... (so Alt+Shift+1 works too).
        KeyCode::Char(c) if ('1'..='9').contains(&c) => {
            Some(Action::SwitchSpace(c as usize - '1' as usize))
        }
        // Scrollback ("copy mode"), for when there is no mouse to hand.
        //
        // `Alt+[` is the tmux-shaped binding, but it is only reliable on
        // terminals that speak the Kitty keyboard protocol: the legacy encoding
        // of Alt+[ is `ESC [`, which *is* the CSI introducer, so a terminal
        // without disambiguation cannot tell the chord from the start of an
        // escape sequence. `Alt+PageUp` has no such ambiguity and works
        // everywhere, so both are bound.
        KeyCode::Char('[') | KeyCode::PageUp => Some(Action::EnterScroll),
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

/// How the focused pane wants keys encoded, as reported by its own escape
/// sequences. Both flags live on that pane's [`crate::screen::Screen`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyMode {
    /// The app enabled the Kitty keyboard protocol (`CSI > flags u`).
    pub kitty: bool,
    /// DECCKM (`CSI ? 1 h`) — cursor keys must be sent as SS3, not CSI.
    pub app_cursor: bool,
}

/// xterm's modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4).
fn modifier_param(key: &KeyEvent) -> u8 {
    let m = key.modifiers;
    1 + m.contains(KeyModifiers::SHIFT) as u8
        + 2 * m.contains(KeyModifiers::ALT) as u8
        + 4 * m.contains(KeyModifiers::CONTROL) as u8
}

/// Encode an ordinary keystroke as the bytes a terminal application expects.
/// Returns `None` for keys with no sensible byte representation.
///
/// Two details that legacy encodings get wrong and apps notice:
///
/// * **Modified cursor keys.** `Ctrl+Right` must be `CSI 1;5C`, not a bare
///   `CSI C` — dropping the modifier is why word-wise motion did nothing.
/// * **DECCKM.** While an app has application cursor-key mode on, arrows are
///   `ESC O A` rather than `ESC [ A`; sending the wrong form makes some
///   full-screen apps see garbage instead of a keypress.
///
/// `mode.kitty` additionally lets us express keys with no legacy byte form —
/// above all **Shift+Enter**, which is otherwise indistinguishable from a bare
/// Enter and so just submits instead of inserting a newline (e.g. in Claude Code).
pub fn encode_key(key: KeyEvent, mode: KeyMode) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let modified = ctrl || alt || shift;
    let m = modifier_param(&key);

    // In Kitty mode, encode a *modified* Enter as a CSI-u sequence (`CSI 13 ; m u`).
    // Unmodified Enter keeps its legacy `\r` so plain submits are unaffected.
    if mode.kitty && key.code == KeyCode::Enter && modified {
        return Some(format!("\x1b[13;{m}u").into_bytes());
    }

    // Cursor / editing keys. `final_byte` keys use the `CSI 1 ; m X` form when
    // modified and SS3 when the app asked for it; `~` keys use `CSI n ; m ~`.
    let cursor_key = |last: u8| -> Vec<u8> {
        if modified {
            format!("\x1b[1;{m}{}", last as char).into_bytes()
        } else if mode.app_cursor {
            vec![0x1b, b'O', last]
        } else {
            vec![0x1b, b'[', last]
        }
    };
    let tilde_key = |n: u8| -> Vec<u8> {
        if modified {
            format!("\x1b[{n};{m}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            let mut out = Vec::with_capacity(5);
            if alt {
                out.push(0x1b); // Meta -> ESC prefix
            }
            if ctrl {
                match ctrl_byte(c) {
                    Some(b) => out.push(b),
                    // Ctrl with a key that has no control code (e.g. Ctrl+é):
                    // send the character itself rather than a truncated byte.
                    None => {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => {
            if alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_key(b'A'),
        KeyCode::Down => cursor_key(b'B'),
        KeyCode::Right => cursor_key(b'C'),
        KeyCode::Left => cursor_key(b'D'),
        KeyCode::Home => cursor_key(b'H'),
        KeyCode::End => cursor_key(b'F'),
        KeyCode::Insert => tilde_key(2),
        KeyCode::Delete => tilde_key(3),
        KeyCode::PageUp => tilde_key(5),
        KeyCode::PageDown => tilde_key(6),
        KeyCode::F(n @ 1..=4) => {
            // F1–F4 are SS3 keys (`ESC O P`..`ESC O S`) unless modified.
            let last = b'P' + (n - 1);
            if modified {
                format!("\x1b[1;{m}{}", last as char).into_bytes()
            } else {
                vec![0x1b, b'O', last]
            }
        }
        KeyCode::F(n @ 5..=12) => {
            // xterm's numbering, which is not contiguous.
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            tilde_key(CODES[(n - 5) as usize])
        }
        _ => return None,
    };
    Some(bytes)
}

/// Encode a mouse event for an app that asked for tracking, or `None` if this
/// event isn't one the app wants.
///
/// Two wire formats exist and apps choose with DECSET 1006. SGR (`CSI < b;x;y M`)
/// is unbounded and distinguishes press from release by its final byte; the
/// legacy X10 form packs everything into single bytes offset by 32, which cannot
/// address past column 223 — so anything beyond that is dropped rather than
/// reported at a wrong position.
pub fn encode_mouse(ev: &WireMouse, col: u16, row: u16, track: MouseTracking) -> Option<Vec<u8>> {
    let level = track.level;
    if level == MouseLevel::Off {
        return None;
    }
    // Motion without a button held is only wanted at the highest level; drags
    // need at least `Drag`.
    match ev.kind {
        MouseAction::Moved if level < MouseLevel::Motion => return None,
        MouseAction::Drag if level < MouseLevel::Drag => return None,
        _ => {}
    }

    let mut code: u8 = match ev.kind {
        MouseAction::ScrollUp => 64,
        MouseAction::ScrollDown => 65,
        MouseAction::ScrollLeft => 66,
        MouseAction::ScrollRight => 67,
        MouseAction::Moved => 35, // motion with no button
        _ => match ev.button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::None => 3,
        },
    };
    if matches!(ev.kind, MouseAction::Drag) {
        code += 32; // motion flag
    }
    if ev.mods & 1 != 0 {
        code += 4; // shift
    }
    if ev.mods & 4 != 0 {
        code += 8; // alt (meta)
    }
    if ev.mods & 2 != 0 {
        code += 16; // ctrl
    }

    let release = matches!(ev.kind, MouseAction::Up);
    if track.sgr {
        let final_byte = if release { 'm' } else { 'M' };
        return Some(
            format!("\x1b[<{code};{};{}{final_byte}", col + 1, row + 1).into_bytes(),
        );
    }
    // Legacy X10: a released button is reported as button 3.
    let code = if release { (code & !3) | 3 } else { code };
    if col >= 223 || row >= 223 {
        return None;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        32 + col as u8 + 1,
        32 + row as u8 + 1,
    ])
}

/// The C0 control byte a `Ctrl+<c>` chord produces, following xterm.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
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
        assert_eq!(parse_command(alt('h')), Some(Action::Help));
        // Plain `h` is ordinary input.
        assert!(parse_command(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)).is_none());
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

    const PLAIN: KeyMode = KeyMode { kitty: false, app_cursor: false };
    const KITTY: KeyMode = KeyMode { kitty: true, app_cursor: false };
    const APPCUR: KeyMode = KeyMode { kitty: false, app_cursor: true };

    #[test]
    fn encodes_control_and_arrows() {
        let ctrl_c = encode_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), PLAIN);
        assert_eq!(ctrl_c, Some(vec![0x03]));
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), PLAIN),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), PLAIN),
            Some(vec![b'\r'])
        );
    }

    /// Ctrl+Space is NUL and Ctrl+] is 0x1d — the old `& 0x1f` shortcut got the
    /// letters right and everything else wrong.
    #[test]
    fn encodes_non_letter_control_chords() {
        let ctrl = |c: char| encode_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL), PLAIN);
        assert_eq!(ctrl(' '), Some(vec![0x00]));
        assert_eq!(ctrl('['), Some(vec![0x1b]));
        assert_eq!(ctrl(']'), Some(vec![0x1d]));
        assert_eq!(ctrl('_'), Some(vec![0x1f]));
        assert_eq!(ctrl('?'), Some(vec![0x7f]));
        // A char with no control code is sent as itself, not a truncated byte.
        assert_eq!(ctrl('é'), Some("é".as_bytes().to_vec()));
    }

    /// Modified cursor keys must carry the modifier, or Ctrl+Right (word jump)
    /// arrives as a plain Right and silently does the wrong thing.
    #[test]
    fn modified_cursor_keys_carry_the_modifier() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL), PLAIN),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT), PLAIN),
            Some(b"\x1b[1;3D".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::SHIFT), PLAIN),
            Some(b"\x1b[3;2~".to_vec())
        );
    }

    /// DECCKM swaps arrows to SS3; modified arrows keep the CSI form either way.
    #[test]
    fn application_cursor_mode_uses_ss3() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), APPCUR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), APPCUR),
            Some(b"\x1bOH".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), APPCUR),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn encodes_function_keys() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), PLAIN),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), PLAIN),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE), PLAIN),
            Some(b"\x1b[24~".to_vec())
        );
    }

    // ---- mouse -------------------------------------------------------------

    fn mouse(kind: MouseAction, button: MouseButton) -> WireMouse {
        WireMouse { kind, button, col: 0, row: 0, mods: 0 }
    }

    const SGR: MouseTracking = MouseTracking { level: MouseLevel::Click, sgr: true };
    const X10: MouseTracking = MouseTracking { level: MouseLevel::Click, sgr: false };

    #[test]
    fn an_app_without_tracking_gets_no_mouse_bytes() {
        let off = MouseTracking { level: MouseLevel::Off, sgr: false };
        assert_eq!(
            encode_mouse(&mouse(MouseAction::Down, MouseButton::Left), 3, 4, off),
            None
        );
    }

    /// SGR distinguishes press from release by its final byte, and reports
    /// 1-based coordinates.
    #[test]
    fn sgr_mouse_encoding() {
        let down = mouse(MouseAction::Down, MouseButton::Left);
        assert_eq!(
            encode_mouse(&down, 10, 5, SGR),
            Some(b"\x1b[<0;11;6M".to_vec())
        );
        let up = mouse(MouseAction::Up, MouseButton::Left);
        assert_eq!(encode_mouse(&up, 10, 5, SGR), Some(b"\x1b[<0;11;6m".to_vec()));
        let right = mouse(MouseAction::Down, MouseButton::Right);
        assert_eq!(
            encode_mouse(&right, 0, 0, SGR),
            Some(b"\x1b[<2;1;1M".to_vec())
        );
    }

    #[test]
    fn sgr_wheel_and_modifiers() {
        let mut up = mouse(MouseAction::ScrollUp, MouseButton::None);
        assert_eq!(encode_mouse(&up, 0, 0, SGR), Some(b"\x1b[<64;1;1M".to_vec()));
        let down = mouse(MouseAction::ScrollDown, MouseButton::None);
        assert_eq!(encode_mouse(&down, 0, 0, SGR), Some(b"\x1b[<65;1;1M".to_vec()));
        // Ctrl adds 16, shift 4, alt 8.
        up.mods = 2;
        assert_eq!(encode_mouse(&up, 0, 0, SGR), Some(b"\x1b[<80;1;1M".to_vec()));
    }

    #[test]
    fn legacy_x10_mouse_encoding() {
        let down = mouse(MouseAction::Down, MouseButton::Left);
        assert_eq!(
            encode_mouse(&down, 10, 5, X10),
            Some(vec![0x1b, b'[', b'M', 32, 32 + 11, 32 + 6])
        );
        // Release is reported as button 3 in the legacy form.
        let up = mouse(MouseAction::Up, MouseButton::Left);
        assert_eq!(
            encode_mouse(&up, 10, 5, X10),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 32 + 11, 32 + 6])
        );
        // Past column 223 the legacy form cannot address the cell; report
        // nothing rather than the wrong position.
        assert_eq!(encode_mouse(&down, 300, 5, X10), None);
    }

    /// Motion and drag events only reach apps that asked for those levels.
    #[test]
    fn motion_events_respect_the_tracking_level() {
        let drag = mouse(MouseAction::Drag, MouseButton::Left);
        let moved = mouse(MouseAction::Moved, MouseButton::None);
        assert_eq!(encode_mouse(&drag, 1, 1, SGR), None, "click-only: no drags");
        assert_eq!(encode_mouse(&moved, 1, 1, SGR), None);

        let drag_lvl = MouseTracking { level: MouseLevel::Drag, sgr: true };
        // Drag carries the +32 motion flag on top of button 0.
        assert_eq!(
            encode_mouse(&drag, 1, 1, drag_lvl),
            Some(b"\x1b[<32;2;2M".to_vec())
        );
        assert_eq!(encode_mouse(&moved, 1, 1, drag_lvl), None, "still no bare motion");

        let motion_lvl = MouseTracking { level: MouseLevel::Motion, sgr: true };
        assert_eq!(
            encode_mouse(&moved, 1, 1, motion_lvl),
            Some(b"\x1b[<35;2;2M".to_vec())
        );
    }

    #[test]
    fn scrollback_has_an_unambiguous_binding() {
        // Alt+[ only survives on terminals with escape-code disambiguation,
        // because its legacy form `ESC [` is the CSI introducer.
        assert_eq!(parse_command(alt('[')), Some(Action::EnterScroll));
        assert_eq!(
            parse_command(KeyEvent::new(KeyCode::PageUp, ALT)),
            Some(Action::EnterScroll)
        );
        // Without Alt, PageUp is ordinary input for the shell.
        assert!(parse_command(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn shift_enter_needs_kitty_mode() {
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        // Without the Kitty protocol there is no way to express it — falls back
        // to a bare Enter (the pre-existing, unavoidable behaviour).
        assert_eq!(encode_key(shift_enter, PLAIN), Some(vec![b'\r']));
        // With it, Shift+Enter becomes the CSI-u form apps read as a newline.
        assert_eq!(encode_key(shift_enter, KITTY), Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn kitty_mode_leaves_plain_enter_alone() {
        // A bare Enter must still submit even in Kitty mode.
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), KITTY),
            Some(vec![b'\r'])
        );
    }

    #[test]
    fn kitty_mode_encodes_modifier_combos() {
        // Ctrl+Enter = 1+4 = 5; Alt+Enter = 1+2 = 3.
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL), KITTY),
            Some(b"\x1b[13;5u".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), KITTY),
            Some(b"\x1b[13;3u".to_vec())
        );
    }
}
