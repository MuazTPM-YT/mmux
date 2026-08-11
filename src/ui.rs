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
    // Snapshot the leaf rects and split seams, then release the borrow on `mgr`.
    let mut rects: Vec<(u64, Rect)> = Vec::new();
    let mut dividers = Vec::new();
    let Some(space) = mgr.active_space() else {
        return;
    };
    space.root.compute_layout(area, &mut rects, &mut dividers);
    mgr.dividers = dividers;
    let focused = mgr.focused_id();

    for (id, outer) in rects {
        let is_focused = Some(id) == focused;
        let Some(pane) = mgr.panes.get_mut(&id) else {
            continue;
        };

        // A scrolled-back pane gets a yellow frame: the strongest possible hint
        // that what you are looking at is history, not live output.
        let border_style = if pane.screen.view_offset > 0 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else if is_focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        // No title on the border — a bare, sleek frame whose only job is to show
        // which pane is focused (cyan) and separate splits.
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style);
        let inner = block.inner(outer);
        f.render_widget(block, outer);

        // Record the rects for focus navigation, PTY resize and mouse hit-testing.
        pane.rect = inner;
        pane.outer = outer;

        blit_screen(f, &pane.screen, inner, is_focused);
    }
}

/// Copy a screen's cell grid directly into the ratatui buffer.
///
/// Double-width glyphs need care in both directions: the continuation cell must
/// be emitted as an empty symbol (ratatui's diff then skips the column the glyph
/// already covers), and a glyph whose second half would land outside `inner` is
/// replaced by a space so it cannot bleed over the pane border.
fn blit_screen(f: &mut Frame, screen: &crate::screen::Screen, inner: Rect, focused: bool) {
    // Park the cursor on the left half of a wide glyph, never on its
    // continuation cell — otherwise the block lands on a column the terminal
    // never draws separately. A scrolled-back view shows no cursor: it belongs
    // to the live screen, which isn't what's on display.
    let cursor = (focused && screen.cursor_visible && screen.view_offset == 0).then(|| {
        let cx = match screen.cell(screen.cx, screen.cy) {
            Some(c) if c.width == 0 => screen.cx.saturating_sub(1),
            _ => screen.cx,
        };
        (cx, screen.cy)
    });

    let buf = f.buffer_mut();
    for y in 0..inner.height {
        for x in 0..inner.width {
            let Some(dst) = buf.cell_mut((inner.x + x, inner.y + y)) else {
                continue;
            };
            let Some(sc) = screen.view_cell(x, y) else {
                dst.set_char(' ');
                dst.set_style(Style::default());
                continue;
            };

            // The terminal cursor is drawn as a solid block using a fixed colour
            // so it stays bright regardless of the cell beneath it (e.g. it must
            // not go grey over a dim autosuggestion).
            let style = if cursor == Some((x, y)) {
                Style::default().fg(Color::Black).bg(Color::White)
            } else {
                let mut style = Style::default().fg(sc.fg).bg(sc.bg);
                if sc.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if sc.dim {
                    style = style.add_modifier(Modifier::DIM);
                }
                if sc.italic {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if sc.underline {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if sc.blink {
                    style = style.add_modifier(Modifier::SLOW_BLINK);
                }
                if sc.reverse {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if sc.strike {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }
                style
            };

            match sc.width {
                // Right half of a wide glyph: an empty symbol, which ratatui's
                // diff skips because the lead cell already claimed this column.
                0 => dst.set_symbol(""),
                2 if x + 1 >= inner.width => dst.set_char(' '),
                _ => dst.set_char(sc.ch),
            };
            dst.set_style(style);
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
        Mode::Scroll => {
            let (at, total) = scroll_position(mgr);
            let line = Line::from(vec![
                Span::styled(
                    format!(" SCROLL {at}/{total} "),
                    Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  w/s line · PgUp/PgDn page · g top · G live · Esc exit ",
                    Style::default().fg(Color::Yellow),
                ),
            ]);
            f.render_widget(Paragraph::new(line), area);
        }
        Mode::Normal => {
            let space = mgr.active_space().map(|s| s.name.as_str()).unwrap_or("");
            // While the wheel has you scrolled back, the hint line is the wrong
            // thing to show — say where you are instead.
            let (at, total) = scroll_position(mgr);
            if at > 0 {
                let line = Line::from(vec![
                    Span::styled(
                        format!(" SCROLL {at}/{total} "),
                        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "  wheel or Alt+PgUp to move · type to return to live ",
                        Style::default().fg(Color::Yellow),
                    ),
                ]);
                f.render_widget(Paragraph::new(line), area);
                return;
            }
            let text = format!(
                " [{space}]  Alt+wasd focus · Alt+WASD new pane · q close · t new · r rename · PgUp scroll · 1-9 space "
            );
            let style = Style::default().fg(Color::Black).bg(Color::Cyan);
            f.render_widget(Paragraph::new(text).style(style), area);
        }
    }
}

/// How far the focused pane is scrolled back, and how much history it has.
fn scroll_position(mgr: &Manager) -> (usize, usize) {
    mgr.focused_id()
        .and_then(|id| mgr.panes.get(&id))
        .map(|p| (p.screen.view_offset, p.screen.scrollback_len()))
        .unwrap_or((0, 0))
}
