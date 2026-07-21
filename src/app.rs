//! The application reducer: all mmux state plus the pure step functions that
//! evolve it. It is UI- and transport-agnostic — the **server** owns the event
//! loop, feeds this input events and PTY output, and renders `mgr`/`mode` with
//! `ui::draw`. Keeping this a reducer is what lets the same logic run headless in
//! a detached daemon.

use crossterm::event::{Event, KeyEvent, KeyEventKind};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::keys::{self, Action};
use crate::pty::PtyEvent;
use crate::state::Manager;

/// Input mode: normal command/passthrough, or capturing a new Space name.
pub enum Mode {
    Normal,
    Rename(String),
}

pub struct App {
    pub(crate) mgr: Manager,
    pub(crate) mode: Mode,
    rx: UnboundedReceiver<PtyEvent>,
}

impl App {
    pub fn new(mgr: Manager, rx: UnboundedReceiver<PtyEvent>) -> Self {
        App {
            mgr,
            mode: Mode::Normal,
            rx,
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

    /// Reconcile each pane's PTY + screen size with its last rendered content rect.
    pub(crate) fn sync_sizes(&mut self) {
        for pane in self.mgr.panes.values_mut() {
            let (w, h) = (pane.rect.width, pane.rect.height);
            if w == 0 || h == 0 {
                continue;
            }
            if w != pane.screen.cols || h != pane.screen.rows {
                pane.screen.resize(w, h);
                pane.process.resize(w, h);
            }
        }
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
                    Mode::Normal => self.handle_normal_key(key),
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
            if let Some(bytes) = keys::encode_key(key) {
                if let Some(pane) = self.mgr.focused_pane_mut() {
                    pane.process.write(&bytes);
                }
            }
            false
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
                    buf.push(c);
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
            Action::SwitchSpace(i) => self.mgr.switch_to(i),
            Action::PrevSpace => self.mgr.prev_space(),
            Action::NextSpace => self.mgr.next_space(),
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
