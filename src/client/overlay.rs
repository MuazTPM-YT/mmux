//! Client-side session switcher overlays (rendered on the client's own terminal,
//! composited over the attached session which is repainted on close).
//!
//! Rename/new are edited **inline** in the list: `R` turns the selected row into
//! an input field pre-filled with its name; `N` adds a fresh input row. No popup.
//!
//! Delete/Rename act on live servers via control messages (`Shutdown`/`Rename`),
//! so they work on active sessions — not just passive ones.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::protocol::{self, ClientMsg};
use crate::session::{self, SessionInfo};

/// What the switcher wants the client to do after a keystroke.
pub enum Nav {
    Stay,
    Close,
    /// Attach to this session (creating it if it doesn't exist).
    Switch(String),
    /// The current session was renamed — keep the connection, update the name.
    Renamed(String),
}

/// Session names are capped at this length by `session::sanitize`, so there is
/// no point letting the input field grow past it.
const MAX_NAME: usize = 64;

/// An in-progress inline edit. `target = Some(old)` renames; `None` creates.
pub struct Edit {
    input: String,
    target: Option<String>,
}

pub enum Overlay {
    List {
        entries: Vec<SessionInfo>,
        sel: usize,
        editing: Option<Edit>,
    },
    Search {
        query: String,
        results: Vec<SessionInfo>,
        sel: usize,
    },
}

impl Overlay {
    /// Session list, with the selection defaulting to the current session.
    pub fn list(current: &str) -> Self {
        let entries = session::list();
        let sel = entries.iter().position(|e| e.name == current).unwrap_or(0);
        Overlay::List { entries, sel, editing: None }
    }

    pub fn search(current: &str) -> Self {
        let results = session::list();
        let sel = results.iter().position(|e| e.name == current).unwrap_or(0);
        Overlay::Search { query: String::new(), results, sel }
    }
}

fn clamp(sel: &mut usize, len: usize) {
    if len == 0 {
        *sel = 0;
    } else if *sel >= len {
        *sel = len - 1;
    }
}

fn wait_until(mut cond: impl FnMut() -> bool, max_ms: u64) {
    let mut waited = 0;
    while waited < max_ms && !cond() {
        std::thread::sleep(Duration::from_millis(20));
        waited += 20;
    }
}

/// Perform a rename (active → control message + rebind; passive → record move).
fn do_rename(old: &str, new: &str) {
    if session::is_active(old) {
        let _ = protocol::send_control(old, &ClientMsg::Rename(new.to_string()));
        wait_until(|| session::is_active(new), 800);
    } else {
        session::rename(old, new);
    }
}

pub fn handle_key(ov: &mut Overlay, key: KeyEvent, current: &str) -> Nav {
    match ov {
        Overlay::List { entries, sel, editing } => {
            // ---- inline editing mode -------------------------------------
            if editing.is_some() {
                return match key.code {
                    KeyCode::Esc => {
                        *editing = None;
                        Nav::Stay
                    }
                    KeyCode::Enter => {
                        let Some(edit) = editing.take() else {
                            return Nav::Stay;
                        };
                        let new = session::sanitize(&edit.input);
                        if edit.input.trim().is_empty() {
                            *editing = Some(edit); // don't commit empty
                            return Nav::Stay;
                        }
                        match edit.target {
                            None => Nav::Switch(new), // create + switch
                            Some(old) => {
                                if new != old {
                                    do_rename(&old, &new);
                                }
                                if old == current {
                                    Nav::Renamed(new)
                                } else {
                                    *entries = session::list();
                                    clamp(sel, entries.len());
                                    Nav::Stay
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(e) = editing {
                            e.input.pop();
                        }
                        Nav::Stay
                    }
                    KeyCode::Char(c) => {
                        if let Some(e) = editing {
                            if e.input.chars().count() < MAX_NAME {
                                e.input.push(c);
                            }
                        }
                        Nav::Stay
                    }
                    _ => Nav::Stay,
                };
            }

            // ---- normal navigation ---------------------------------------
            match key.code {
                KeyCode::Esc => Nav::Close,
                KeyCode::Up | KeyCode::Char('w') => {
                    *sel = sel.saturating_sub(1);
                    Nav::Stay
                }
                KeyCode::Down | KeyCode::Char('s') => {
                    *sel += 1;
                    clamp(sel, entries.len());
                    Nav::Stay
                }
                KeyCode::Enter => match entries.get(*sel) {
                    Some(e) => Nav::Switch(e.name.clone()),
                    None => Nav::Close,
                },
                KeyCode::Char('D') => {
                    if let Some(e) = entries.get(*sel) {
                        if e.name != current {
                            let target = e.name.clone();
                            if e.active {
                                let _ = protocol::send_control(&target, &ClientMsg::Shutdown);
                                wait_until(|| !session::is_active(&target), 800);
                            }
                            session::delete(&target);
                        }
                    }
                    *entries = session::list();
                    clamp(sel, entries.len());
                    Nav::Stay
                }
                // Inline rename: the selected row becomes an input, pre-filled.
                KeyCode::Char('R') => {
                    if let Some(e) = entries.get(*sel) {
                        *editing = Some(Edit {
                            input: e.name.clone(),
                            target: Some(e.name.clone()),
                        });
                    }
                    Nav::Stay
                }
                // Inline new: a fresh input row.
                KeyCode::Char('N') => {
                    *editing = Some(Edit { input: String::new(), target: None });
                    Nav::Stay
                }
                _ => Nav::Stay,
            }
        }
        Overlay::Search { query, results, sel } => match key.code {
            KeyCode::Esc => Nav::Close,
            KeyCode::Enter => match results.get(*sel) {
                Some(e) => Nav::Switch(e.name.clone()),
                None if !query.trim().is_empty() => Nav::Switch(session::sanitize(query)),
                None => Nav::Close,
            },
            KeyCode::Up => {
                *sel = sel.saturating_sub(1);
                Nav::Stay
            }
            KeyCode::Down => {
                *sel += 1;
                clamp(sel, results.len());
                Nav::Stay
            }
            KeyCode::Backspace => {
                query.pop();
                *results = filter(query);
                *sel = 0;
                Nav::Stay
            }
            KeyCode::Char(c) => {
                if query.chars().count() < MAX_NAME {
                    query.push(c);
                    *results = filter(query);
                    *sel = 0;
                }
                Nav::Stay
            }
            _ => Nav::Stay,
        },
    }
}

fn filter(query: &str) -> Vec<SessionInfo> {
    let q = query.to_lowercase();
    session::list()
        .into_iter()
        .filter(|s| s.name.to_lowercase().contains(&q))
        .collect()
}

// ---- rendering ------------------------------------------------------------

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

const ACCENT: Color = Color::Cyan;
const FIELD_BG: Color = Color::Rgb(38, 40, 52);

pub fn render(f: &mut Frame, ov: &Overlay, current: &str) {
    match ov {
        Overlay::List { entries, sel, editing } => render_list(f, entries, *sel, current, editing),
        Overlay::Search { query, results, sel } => render_search(f, query, results, *sel, current),
    }
}

fn glyph_of(active: bool) -> (&'static str, Color) {
    if active {
        ("●", Color::Green)
    } else {
        ("○", Color::DarkGray)
    }
}

fn session_line(e: &SessionInfo, selected: bool, is_current: bool) -> Line<'static> {
    let (glyph, gcol) = glyph_of(e.active);
    let mut label = e.name.clone();
    if e.active {
        label.push_str(" (active)");
    }
    if is_current {
        label.push_str("  — current");
    }
    let name_style = if selected {
        Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
    } else if is_current {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    Line::from(vec![
        Span::styled(if selected { " ▸ " } else { "   " }, Style::default().fg(ACCENT)),
        Span::styled(format!("{glyph} "), Style::default().fg(gcol)),
        Span::styled(label, name_style),
    ])
}

/// An inline input row (used for both rename and new).
fn edit_line(glyph: &str, gcol: Color, input: &str) -> Line<'static> {
    let field = Style::default().fg(Color::White).bg(FIELD_BG);
    Line::from(vec![
        Span::styled(" ▸ ", Style::default().fg(ACCENT)),
        Span::styled(format!("{glyph} "), Style::default().fg(gcol)),
        Span::styled(format!(" {input}"), field.add_modifier(Modifier::BOLD)),
        Span::styled(" ", Style::default().fg(FIELD_BG).bg(Color::White)), // block cursor
        Span::styled("   ", Style::default().bg(FIELD_BG)),                // trailing pad
    ])
}

fn render_list(f: &mut Frame, entries: &[SessionInfo], sel: usize, current: &str, editing: &Option<Edit>) {
    let full = f.area();
    let w = (full.width * 2 / 3).clamp(56, full.width.saturating_sub(4).max(56));
    let extra = if matches!(editing, Some(e) if e.target.is_none()) { 1 } else { 0 };
    let h = ((entries.len() as u16) + extra + 6)
        .max(16)
        .min((full.height * 4 / 5).max(16));
    let area = centered(full, w, h);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Sessions ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    // Which row (if any) is being renamed in place.
    let renaming = matches!(editing, Some(e) if e.target.is_some());

    let mut lines: Vec<Line> = vec![Line::raw("")];
    if entries.is_empty() && extra == 0 {
        lines.push(Line::styled(
            "   no sessions yet — press N",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for (i, e) in entries.iter().enumerate() {
        if renaming && i == sel {
            let (glyph, gcol) = glyph_of(e.active);
            let input = editing.as_ref().map(|x| x.input.as_str()).unwrap_or("");
            lines.push(edit_line(glyph, gcol, input));
        } else {
            lines.push(session_line(e, editing.is_none() && i == sel, e.name == current));
        }
    }
    // The "new session" input appears as its own row.
    if let Some(e) = editing {
        if e.target.is_none() {
            lines.push(edit_line("+", ACCENT, &e.input));
        }
    }
    f.render_widget(Paragraph::new(lines), rows[0]);

    let hint = if editing.is_some() {
        " type a name · Enter save · Esc cancel"
    } else {
        " w/s move · Enter open · D delete · R rename · N new · Esc close"
    };
    f.render_widget(
        Paragraph::new(Line::styled(hint, Style::default().fg(Color::DarkGray))),
        rows[1],
    );
}

fn render_search(f: &mut Frame, query: &str, results: &[SessionInfo], sel: usize, current: &str) {
    let area = f.area();
    f.render_widget(Clear, area);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, e) in results.iter().enumerate() {
        lines.push(session_line(e, i == sel, e.name == current));
    }
    if results.is_empty() {
        lines.push(Line::styled(
            "   (no match — Enter creates this session)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(" Search sessions (↑/↓, Enter, Esc) ");
    let inner = block.inner(rows[0]);
    f.render_widget(block, rows[0]);
    f.render_widget(Paragraph::new(lines), inner);

    let bar = Line::from(vec![
        Span::styled(" search ", Style::default().fg(Color::Black).bg(ACCENT)),
        Span::raw(" "),
        Span::styled(query.to_string(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(" ", Style::default().fg(FIELD_BG).bg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(bar).style(Style::default().bg(FIELD_BG)), rows[1]);
}
