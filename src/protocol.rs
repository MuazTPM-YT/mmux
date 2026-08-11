//! Wire protocol between the client (`mmux <name>`) and the per-session server.
//!
//! Messages are length-prefixed postcard frames. The server renders the whole UI
//! to terminal escape bytes and ships them as `ServerMsg::Frame`; the client just
//! writes those to its real terminal and forwards input as `ClientMsg`.

use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Reject absurd frame sizes so a corrupt length can't trigger a huge alloc.
const MAX_FRAME: u32 = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMsg {
    /// First message: the client's terminal size.
    Attach { cols: u16, rows: u16 },
    /// A keystroke to feed to the session.
    Key(WireKey),
    /// A mouse event at client-terminal coordinates.
    Mouse(WireMouse),
    /// A bracketed paste from the client's terminal, forwarded to the focused pane.
    Paste(String),
    /// The client's terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Ask the server to emit a full frame (after the client painted an overlay).
    Redraw,
    /// The client is detaching; the server should keep running.
    Detach,
    /// Control: destroy this session (server purges its record + exits).
    Shutdown,
    /// Control: rename this session — rebind the socket + move the record.
    Rename(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ServerMsg {
    /// Terminal escape bytes to write verbatim to the client's terminal.
    Frame(Vec<u8>),
    /// The server is shutting down (its last shell exited).
    Bye,
}

/// A serialisable `crossterm::event::MouseEvent`, in the client's terminal
/// coordinates (0-based). The server maps them onto panes itself, since only it
/// knows the layout.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireMouse {
    pub kind: MouseAction,
    pub button: MouseButton,
    pub col: u16,
    pub row: u16,
    /// SHIFT=1, CONTROL=2, ALT=4 (bitwise-or), same encoding as `WireKey`.
    pub mods: u8,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Down,
    Up,
    Drag,
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Motion or scroll events carry no button.
    None,
}

impl WireMouse {
    pub fn from_event(e: MouseEvent) -> Self {
        use crossterm::event::{MouseButton as CB, MouseEventKind as CK};
        let btn = |b: CB| match b {
            CB::Left => MouseButton::Left,
            CB::Middle => MouseButton::Middle,
            CB::Right => MouseButton::Right,
        };
        let (kind, button) = match e.kind {
            CK::Down(b) => (MouseAction::Down, btn(b)),
            CK::Up(b) => (MouseAction::Up, btn(b)),
            CK::Drag(b) => (MouseAction::Drag, btn(b)),
            CK::Moved => (MouseAction::Moved, MouseButton::None),
            CK::ScrollUp => (MouseAction::ScrollUp, MouseButton::None),
            CK::ScrollDown => (MouseAction::ScrollDown, MouseButton::None),
            CK::ScrollLeft => (MouseAction::ScrollLeft, MouseButton::None),
            CK::ScrollRight => (MouseAction::ScrollRight, MouseButton::None),
        };
        let mut mods = 0u8;
        if e.modifiers.contains(KeyModifiers::SHIFT) {
            mods |= 1;
        }
        if e.modifiers.contains(KeyModifiers::CONTROL) {
            mods |= 2;
        }
        if e.modifiers.contains(KeyModifiers::ALT) {
            mods |= 4;
        }
        WireMouse {
            kind,
            button,
            col: e.column,
            row: e.row,
            mods,
        }
    }

    /// True for wheel events, which mmux may consume for scrollback.
    pub fn is_scroll(&self) -> bool {
        matches!(
            self.kind,
            MouseAction::ScrollUp
                | MouseAction::ScrollDown
                | MouseAction::ScrollLeft
                | MouseAction::ScrollRight
        )
    }
}

/// A serialisable subset of `crossterm::event::KeyEvent`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WireKey {
    pub code: WireCode,
    /// SHIFT=1, CONTROL=2, ALT=4 (bitwise-or).
    pub mods: u8,
    /// Press=0, Repeat=1, Release=2.
    pub kind: u8,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum WireCode {
    Char(char),
    Enter,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Esc,
    F(u8),
    Other,
}

impl WireKey {
    pub fn from_event(e: KeyEvent) -> Self {
        let mut mods = 0u8;
        if e.modifiers.contains(KeyModifiers::SHIFT) {
            mods |= 1;
        }
        if e.modifiers.contains(KeyModifiers::CONTROL) {
            mods |= 2;
        }
        if e.modifiers.contains(KeyModifiers::ALT) {
            mods |= 4;
        }
        let kind = match e.kind {
            KeyEventKind::Press => 0,
            KeyEventKind::Repeat => 1,
            KeyEventKind::Release => 2,
        };
        let code = match e.code {
            KeyCode::Char(c) => WireCode::Char(c),
            KeyCode::Enter => WireCode::Enter,
            KeyCode::Backspace => WireCode::Backspace,
            KeyCode::Left => WireCode::Left,
            KeyCode::Right => WireCode::Right,
            KeyCode::Up => WireCode::Up,
            KeyCode::Down => WireCode::Down,
            KeyCode::Home => WireCode::Home,
            KeyCode::End => WireCode::End,
            KeyCode::PageUp => WireCode::PageUp,
            KeyCode::PageDown => WireCode::PageDown,
            KeyCode::Tab => WireCode::Tab,
            KeyCode::BackTab => WireCode::BackTab,
            KeyCode::Delete => WireCode::Delete,
            KeyCode::Insert => WireCode::Insert,
            KeyCode::Esc => WireCode::Esc,
            KeyCode::F(n) => WireCode::F(n),
            _ => WireCode::Other,
        };
        WireKey { code, mods, kind }
    }

    pub fn to_event(&self) -> Option<KeyEvent> {
        let mut m = KeyModifiers::empty();
        if self.mods & 1 != 0 {
            m |= KeyModifiers::SHIFT;
        }
        if self.mods & 2 != 0 {
            m |= KeyModifiers::CONTROL;
        }
        if self.mods & 4 != 0 {
            m |= KeyModifiers::ALT;
        }
        let kind = match self.kind {
            1 => KeyEventKind::Repeat,
            2 => KeyEventKind::Release,
            _ => KeyEventKind::Press,
        };
        let code = match self.code {
            WireCode::Char(c) => KeyCode::Char(c),
            WireCode::Enter => KeyCode::Enter,
            WireCode::Backspace => KeyCode::Backspace,
            WireCode::Left => KeyCode::Left,
            WireCode::Right => KeyCode::Right,
            WireCode::Up => KeyCode::Up,
            WireCode::Down => KeyCode::Down,
            WireCode::Home => KeyCode::Home,
            WireCode::End => KeyCode::End,
            WireCode::PageUp => KeyCode::PageUp,
            WireCode::PageDown => KeyCode::PageDown,
            WireCode::Tab => KeyCode::Tab,
            WireCode::BackTab => KeyCode::BackTab,
            WireCode::Delete => KeyCode::Delete,
            WireCode::Insert => KeyCode::Insert,
            WireCode::Esc => KeyCode::Esc,
            WireCode::F(n) => KeyCode::F(n),
            WireCode::Other => return None,
        };
        let mut ev = KeyEvent::new(code, m);
        ev.kind = kind;
        Some(ev)
    }
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = postcard::to_stdvec(msg).map_err(|e| io::Error::other(e.to_string()))?;
    w.write_u32(bytes.len() as u32).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Send a single control message synchronously (used by the switcher for
/// Shutdown/Rename against another session's server). Blocking, but these are
/// rare, deliberate user actions.
pub fn send_control(name: &str, msg: &ClientMsg) -> io::Result<()> {
    use std::io::{Read as _, Write as _};
    let path = crate::session::socket_path(name);
    let bytes = postcard::to_stdvec(msg).map_err(|e| io::Error::other(e.to_string()))?;
    let mut s = std::os::unix::net::UnixStream::connect(&path)?;
    s.write_all(&(bytes.len() as u32).to_be_bytes())?;
    s.write_all(&bytes)?;
    s.flush()?;
    // Half-close so the server sees our data then a clean EOF, and block until it
    // finishes handling (it drops the connection), guaranteeing delivery before
    // we return. Bounded so a stuck server can't hang us.
    let _ = s.shutdown(std::net::Shutdown::Write);
    s.set_read_timeout(Some(std::time::Duration::from_millis(500))).ok();
    let _ = s.read(&mut [0u8; 1]);
    Ok(())
}

pub async fn read_msg<R, T>(r: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = r.read_u32().await?;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    postcard::from_bytes(&buf).map_err(|e| io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wirekey_roundtrips_common_events() {
        let cases = [
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('W'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE),
        ];
        for ev in cases {
            let wire = WireKey::from_event(ev);
            assert_eq!(wire.to_event(), Some(ev));
        }
    }
}
