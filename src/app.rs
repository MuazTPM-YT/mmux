//! The application reducer: all mmux state plus the pure step functions that
//! evolve it. It is UI- and transport-agnostic — the **server** owns the event
//! loop, feeds this input events and PTY output, and renders `mgr`/`mode` with
//! `ui::draw`. Keeping this a reducer is what lets the same logic run headless in
//! a detached daemon.

use crossterm::event::{Event, KeyEvent, KeyEventKind};
use tokio::sync::mpsc::Receiver;

use crate::keys::{self, Action, KeyMode};
use crate::protocol::{MouseAction, WireMouse};
use crate::pty::PtyEvent;
use crate::state::Manager;

/// Longest Space name the rename field accepts, so a stuck key can't grow the
/// buffer without bound.
const MAX_SPACE_NAME: usize = 64;
/// Lines the wheel moves per notch, matching what terminals normally do.
const SCROLL_LINES: isize = 3;

/// Input mode: normal command/passthrough, capturing a new Space name,
/// navigating the focused pane's scrollback from the keyboard, or showing the
/// keybinding popup.
pub enum Mode {
    Normal,
    Rename(String),
    Scroll,
    Help,
}

pub struct App {
    pub(crate) mgr: Manager,
    pub(crate) mode: Mode,
    rx: Receiver<PtyEvent>,
    /// The split seam a mouse drag grabbed, held until the button is released.
    dragging: Option<crate::state::Divider>,
}

impl App {
    pub fn new(mgr: Manager, rx: Receiver<PtyEvent>) -> Self {
        App {
            mgr,
            mode: Mode::Normal,
            rx,
            dragging: None,
        }
    }

    /// True once every space/pane is gone — the server should shut down.
    pub(crate) fn is_empty(&self) -> bool {
        self.mgr.is_empty()
    }

    /// Await the next PTY event (used as a `select!` branch by the server).
    pub(crate) async fn recv_pty(&mut self) -> Option<PtyEvent> {
        self.rx.recv().await
    }

    /// Apply one PTY event and drain any others already queued.
    pub(crate) fn drain_pty(&mut self, first: PtyEvent) {
        self.handle_pty_event(first);
        while let Ok(ev) = self.rx.try_recv() {
            self.handle_pty_event(ev);
        }
    }

    /// Reconcile each pane's PTY + screen size with its last rendered content
    /// rect. Returns `true` if any pane was actually resized, which means the
    /// frame that was just drawn used the *old* grid shape and has to be redrawn
    /// — otherwise a resized pane keeps showing mis-shaped content until its
    /// shell happens to print something.
    pub(crate) fn sync_sizes(&mut self) -> bool {
        let mut changed = false;
        for pane in self.mgr.panes.values_mut() {
            let (w, h) = (pane.rect.width, pane.rect.height);
            if w == 0 || h == 0 {
                continue;
            }
            if w != pane.screen.cols || h != pane.screen.rows {
                pane.screen.resize(w, h);
                pane.process.resize(w, h);
                changed = true;
            }
        }
        changed
    }

    /// Handle one terminal event. Returns `true` if the UI needs to be redrawn.
    pub(crate) fn handle_terminal_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) => {
                // Ignore key-release events (kitty/enhanced protocols emit them).
                if key.kind == KeyEventKind::Release {
                    return false;
                }
                match &mut self.mode {
                    Mode::Rename(_) => self.handle_rename_key(key),
                    Mode::Scroll => self.handle_scroll_key(key),
                    Mode::Normal => self.handle_normal_key(key),
                    // Any key closes the popup and is swallowed, so a stray
                    // keypress can't reach the shell hidden behind it.
                    Mode::Help => {
                        self.mode = Mode::Normal;
                        true
                    }
                }
            }
            // A resize must reflow the layout on the next draw.
            Event::Resize(_, _) => true,
            _ => false,
        }
    }

    /// Returns `true` if the key changed mmux state (a command); `false` if it
    /// was merely forwarded to the shell (whose echo will trigger the redraw).
    fn handle_normal_key(&mut self, key: KeyEvent) -> bool {
        if let Some(action) = keys::parse_command(key) {
            self.apply_action(action);
            true
        } else {
            if let Some(pane) = self.mgr.focused_pane_mut() {
                let mode = KeyMode {
                    kitty: pane.screen.kitty_keyboard != 0,
                    app_cursor: pane.screen.app_cursor_keys,
                };
                // Typing means you want to be back at the prompt, not reading
                // history — same reflex every terminal has.
                let snapped = pane.screen.scroll_to_live();
                if let Some(bytes) = keys::encode_key(key, mode) {
                    pane.process.write(&bytes);
                }
                return snapped;
            }
            false
        }
    }

    /// Keyboard scrollback navigation (`Alt+[`). Any key that isn't a movement
    /// leaves the mode, so there is no way to get stuck in it.
    fn handle_scroll_key(&mut self, key: KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        let page = self
            .mgr
            .focused_pane_mut()
            .map(|p| p.screen.rows.saturating_sub(1).max(1) as isize)
            .unwrap_or(1);
        let Some(pane) = self.mgr.focused_pane_mut() else {
            self.mode = Mode::Normal;
            return true;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('w') | KeyCode::Char('k') => {
                pane.screen.scroll_view(-1);
            }
            KeyCode::Down | KeyCode::Char('s') | KeyCode::Char('j') => {
                pane.screen.scroll_view(1);
            }
            KeyCode::PageUp | KeyCode::Char('b') => {
                pane.screen.scroll_view(-page);
            }
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
                pane.screen.scroll_view(page);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                let top = pane.screen.scrollback_len();
                pane.screen.scroll_view_to(top);
            }
            KeyCode::End | KeyCode::Char('G') => {
                pane.screen.scroll_to_live();
            }
            // Anything else exits, snapping back to live output.
            _ => {
                pane.screen.scroll_to_live();
                self.mode = Mode::Normal;
            }
        }
        true
    }

    /// Handle one mouse event. Returns `true` if the UI needs redrawing.
    ///
    /// Routing rule: an event belongs to the pane's application whenever that
    /// application asked for mouse tracking, **except** wheel events while the
    /// view is scrolled back — otherwise you could never scroll out of history.
    /// Everything an app doesn't want becomes an mmux gesture: click to focus,
    /// drag a seam to resize, wheel to scroll history.
    pub(crate) fn handle_mouse(&mut self, ev: &WireMouse) -> bool {
        if !matches!(self.mode, Mode::Normal | Mode::Scroll) {
            return false;
        }

        // A drag in progress owns every event until the button comes up.
        if let Some(d) = self.dragging {
            return match ev.kind {
                MouseAction::Drag => self.mgr.drag_divider(&d, ev.col, ev.row),
                MouseAction::Up => {
                    self.dragging = None;
                    false
                }
                _ => false,
            };
        }

        // Grabbing a seam takes precedence over the panes either side of it —
        // unless the app there tracks the mouse. Panes have no frames, so the
        // seam is made of the panes' own edge cells, and an app (vim, htop)
        // must keep getting its clicks on them.
        if matches!(ev.kind, MouseAction::Down) {
            if let Some(d) = self.mgr.divider_at(ev.col, ev.row) {
                let pane = self.mgr.pane_at(ev.col, ev.row);
                let app_wants = pane
                    .and_then(|id| self.mgr.panes.get(&id))
                    .is_some_and(|p| p.screen.mouse.enabled());
                if !app_wants {
                    self.dragging = Some(d);
                    return pane.is_some_and(|id| self.mgr.focus_pane(id));
                }
            }
        }

        let Some(id) = self.mgr.pane_at(ev.col, ev.row) else {
            return false;
        };

        // Clicking a pane focuses it, whether or not the app also wants the
        // click — otherwise a click inside vim would leave focus elsewhere.
        let mut redraw = false;
        if matches!(ev.kind, MouseAction::Down) {
            redraw |= self.mgr.focus_pane(id);
        }

        let Some(pane) = self.mgr.panes.get_mut(&id) else {
            return redraw;
        };
        let scrolled_back = pane.screen.view_offset > 0;

        if pane.screen.mouse.enabled() && !(ev.is_scroll() && scrolled_back) {
            // Translate to pane-local coordinates before handing it over.
            let (Some(col), Some(row)) = (
                ev.col.checked_sub(pane.rect.x),
                ev.row.checked_sub(pane.rect.y),
            ) else {
                return redraw;
            };
            if col < pane.rect.width && row < pane.rect.height {
                if let Some(bytes) = keys::encode_mouse(ev, col, row, pane.screen.mouse) {
                    pane.process.write(&bytes);
                }
            }
            return redraw;
        }

        // mmux's own gesture: the wheel walks scrollback.
        match ev.kind {
            MouseAction::ScrollUp => redraw |= pane.screen.scroll_view(-SCROLL_LINES),
            MouseAction::ScrollDown => redraw |= pane.screen.scroll_view(SCROLL_LINES),
            _ => {}
        }
        redraw
    }

    /// Forward pasted text to the focused pane's shell. Like a forwarded
    /// keystroke this never changes mmux state, so it needs no redraw — the
    /// shell's echo triggers one.
    pub(crate) fn paste(&mut self, text: &str) {
        if let Mode::Normal = self.mode {
            if let Some(pane) = self.mgr.focused_pane_mut() {
                pane.paste(text);
            }
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        // Commit / cancel first, then fall through to text editing.
        match key.code {
            KeyCode::Enter => {
                if let Mode::Rename(buf) = std::mem::replace(&mut self.mode, Mode::Normal) {
                    if !buf.is_empty() {
                        self.mgr.rename_active(buf);
                    }
                }
            }
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                if let Mode::Rename(buf) = &mut self.mode {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Mode::Rename(buf) = &mut self.mode {
                    if buf.chars().count() < MAX_SPACE_NAME {
                        buf.push(c);
                    }
                }
            }
            _ => {}
        }
        // Any key in rename mode updates the status line.
        true
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::Focus(dir) => self.mgr.focus_dir(dir),
            Action::NewPane(dir) => {
                let _ = self.mgr.new_pane(dir);
            }
            Action::ClosePane => self.mgr.close_focused(),
            Action::NewSpace => {
                let _ = self.mgr.new_space();
            }
            Action::RenameSpace => {
                // Start from a blank field so the user types the new name
                // directly instead of backspacing the old one.
                self.mode = Mode::Rename(String::new());
            }
            Action::Help => self.mode = Mode::Help,
            Action::SwitchSpace(i) => self.mgr.switch_to(i),
            Action::PrevSpace => self.mgr.prev_space(),
            Action::NextSpace => self.mgr.next_space(),
            Action::EnterScroll => {
                // Pointless on a pane with no history, and on the alternate
                // screen the app owns scrolling anyway.
                let Some(pane) = self.mgr.focused_pane_mut() else {
                    return;
                };
                if pane.screen.scrollback_len() > 0 {
                    // Page back on entry: the mode is only useful once you can
                    // see history, and it makes the keypress visibly do something.
                    let page = pane.screen.rows.saturating_sub(1).max(1) as isize;
                    pane.screen.scroll_view(-page);
                    self.mode = Mode::Scroll;
                }
            }
        }
    }

    fn handle_pty_event(&mut self, ev: PtyEvent) {
        match ev {
            PtyEvent::Data { id, bytes } => {
                if let Some(pane) = self.mgr.panes.get_mut(&id) {
                    pane.feed(&bytes);
                }
            }
            PtyEvent::Exited { id } => {
                // Only act if the pane still exists (it may already be closed).
                if self.mgr.panes.contains_key(&id) {
                    self.mgr.close_pane(id);
                }
            }
        }
    }
}
