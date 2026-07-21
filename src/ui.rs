//! All rendering. Reads world state and paints it into ratatui's back buffer:
//! the Spaces tab bar, the pane layout tree (blitting each [`crate::screen::Screen`]
//! cell-for-cell), and the status/hint line.

use ratatui::layout::{Constraint, Direction as LDir, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::Mode;
use crate::state::Manager;

pub fn draw(f: &mut Frame, mgr: &mut Manager, mode: &Mode, session: &str) {
    let root = Layout::default()
        .direction(LDir::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Min(1),    // body (panes)
            Constraint::Length(1), // status
        ])
        .split(f.area());

    draw_tabs(f, mgr, session, root[0]);
    draw_panes(f, mgr, root[1]);
    draw_status(f, mgr, mode, root[2]);
}

fn draw_tabs(f: &mut Frame, mgr: &Manager, session: &str, area: Rect) {
    let mut spans = vec![
        Span::styled(
            " mmux ",
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        // Current session name — a persistent "you are here" indicator.
        Span::styled(
            format!(" ◆ {session} "),
            Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    for (i, space) in mgr.spaces.iter().enumerate() {
        let label = format!(" {}:{} ", i + 1, space.name);
        let style = if i == mgr.active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
    }
    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black));
    f.render_widget(bar, area);
}

fn draw_panes(f: &mut Frame, mgr: &mut Manager, area: Rect) {
    // Snapshot the leaf rects, then release the immutable borrow on `mgr`.
    let mut rects: Vec<(u64, Rect)> = Vec::new();
    mgr.active_space().root.compute_rects(area, &mut rects);
    let focused = mgr.focused_id();

    for (id, outer) in rects {
        let is_focused = Some(id) == focused;
        let Some(pane) = mgr.panes.get_mut(&id) else {
            continue;
        };

        let title = pane
            .screen
            .title
            .clone()
            .unwrap_or_else(|| format!("pane {}", pane.id));
        let border_style = if is_focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(format!(" {title} "), border_style));
        let inner = block.inner(outer);
        f.render_widget(block, outer);

        // Record the content rect for focus navigation and PTY resize.
        pane.rect = inner;

        blit_screen(f, &pane.screen, inner, is_focused);
    }
}

/// Copy a screen's cell grid directly into the ratatui buffer.
fn blit_screen(f: &mut Frame, screen: &crate::screen::Screen, inner: Rect, focused: bool) {
    let buf = f.buffer_mut();
    for y in 0..inner.height {
        for x in 0..inner.width {
            let Some(dst) = buf.cell_mut((inner.x + x, inner.y + y)) else {
                continue;
            };
            match screen.cell(x, y) {
                Some(sc) => {
                    // The terminal cursor is drawn as a solid block using a fixed
                    // colour so it stays bright regardless of the cell beneath it
                    // (e.g. it must not go grey over a dim autosuggestion).
                    if focused && x == screen.cx && y == screen.cy {
                        dst.set_char(sc.ch);
                        dst.set_style(Style::default().fg(Color::Black).bg(Color::White));
                        continue;
                    }
                    let mut style = Style::default().fg(sc.fg).bg(sc.bg);
                    if sc.bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if sc.dim {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    if sc.underline {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if sc.reverse {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    dst.set_char(sc.ch);
                    dst.set_style(style);
                }
                None => {
                    dst.set_char(' ');
                }
            }
        }
    }
}

fn draw_status(f: &mut Frame, mgr: &Manager, mode: &Mode, area: Rect) {
    match mode {
        Mode::Rename(buf) => {
            // A clean input bar: dim prompt, white text, and a proper block
            // cursor rendered as an inverse-video cell (like a real terminal
            // cursor) rather than a stark solid glyph.
            let bar = Style::default().fg(Color::White).bg(Color::Rgb(30, 32, 40));
            let line = Line::from(vec![
                Span::styled(" rename space ", Style::default().fg(Color::Black).bg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(buf.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(" ", Style::default().fg(Color::Rgb(30, 32, 40)).bg(Color::White)),
            ]);
            f.render_widget(Paragraph::new(line).style(bar), area);
        }
        Mode::Normal => {
            let space = mgr.active_space().name.clone();
            let text = format!(
                " [{space}]  Alt+wasd focus · Alt+WASD new pane · q close · t new · r rename · 1-9 space · ←/→ cycle "
            );
            let style = Style::default().fg(Color::Black).bg(Color::Cyan);
            f.render_widget(Paragraph::new(text).style(style), area);
        }
    }
}
