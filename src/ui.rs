//! All rendering. Reads world state and paints it into ratatui's back buffer:
//! the Spaces tab bar, the pane layout tree (blitting each [`crate::screen::Screen`]
//! cell-for-cell, edge to edge with no frames), a status bar that appears only
//! while renaming or scrolling, and the `Alt+h` keybinding popup.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::Mode;
use crate::state::Manager;

pub fn draw(f: &mut Frame, mgr: &mut Manager, mode: &Mode, session: &str) {
    let area = f.area();
    let tabs = Rect { height: area.height.min(1), ..area };
    let body = Rect {
        y: area.y + tabs.height,
        height: area.height - tabs.height,
        ..area
    };

    draw_tabs(f, mgr, session, tabs);
    draw_panes(f, mgr, body);
    // The status bar is drawn *over* the last pane row rather than given a row
    // of its own: reserving space would resize every pane (and SIGWINCH its
    // app) each time the bar came or went.
    if let Some(bar) = status_bar(mgr, mode) {
        if body.height > 0 {
            let row = Rect { y: body.bottom() - 1, height: 1, ..body };
            f.render_widget(Clear, row);
            f.render_widget(bar, row);
        }
    }
    if matches!(mode, Mode::Help) {
        draw_help(f, body);
    }
}

/// A `w`x`h` rect centred in `area`, shrunk to fit if `area` is smaller.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
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

    // Panes tile the body edge to edge — no frame. The focused pane is the one
    // showing a cursor.
    for (id, rect) in rects {
        let Some(pane) = mgr.panes.get_mut(&id) else {
            continue;
        };
        // Recorded for focus navigation, PTY resize and mouse hit-testing.
        pane.rect = rect;
        blit_screen(f, &pane.screen, rect, Some(id) == focused);
    }
}

/// Copy a screen's cell grid directly into the ratatui buffer.
///
/// Double-width glyphs need care in both directions: the continuation cell must
/// be emitted as an empty symbol (ratatui's diff then skips the column the glyph
/// already covers), and a glyph whose second half would land outside `inner` is
/// replaced by a space so it cannot bleed into the neighbouring pane.
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

/// The transient bottom bar: the rename field, or where you are in scrollback.
/// `None` the rest of the time — there is no permanent hint line; `Alt+h` shows
/// the keys on demand.
fn status_bar(mgr: &Manager, mode: &Mode) -> Option<Paragraph<'static>> {
    let scroll = |hint: &'static str| {
        let (at, total) = scroll_position(mgr);
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" SCROLL {at}/{total} "),
                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(hint, Style::default().fg(Color::Yellow)),
        ]))
    };
    match mode {
        Mode::Rename(buf) => {
            // A clean input bar: dim prompt, white text, and a block cursor
            // rendered as an inverse-video cell like a real terminal cursor.
            let bar = Style::default().fg(Color::White).bg(Color::Rgb(30, 32, 40));
            let line = Line::from(vec![
                Span::styled(" rename space ", Style::default().fg(Color::Black).bg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(buf.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(" ", Style::default().fg(Color::Rgb(30, 32, 40)).bg(Color::White)),
            ]);
            Some(Paragraph::new(line).style(bar))
        }
        Mode::Scroll => Some(scroll("  w/s line · PgUp/PgDn page · g top · G live · Esc exit ")),
        // Scrolled back with the wheel: say so, since the view is history.
        Mode::Normal | Mode::Help if scroll_position(mgr).0 > 0 => {
            Some(scroll("  wheel or Alt+PgUp to move · type to return to live "))
        }
        Mode::Normal | Mode::Help => None,
    }
}

/// Every binding, client-side chords included, for the `Alt+h` popup.
const HELP: &[(&str, &str)] = &[
    ("", "Panes"),
    ("Alt+w a s d", "focus up / left / down / right"),
    ("Alt+W A S D", "new pane up / left / down / right"),
    ("Alt+q", "close pane"),
    ("", "Spaces"),
    ("Alt+t", "new space"),
    ("Alt+r", "rename space"),
    ("Alt+1..9", "go to space"),
    ("Alt+← →", "previous / next space"),
    ("", "Scrollback"),
    ("Mouse wheel", "scroll history"),
    ("Alt+PgUp", "scroll mode (w/s, PgUp/PgDn, g/G, Esc)"),
    ("", "Sessions"),
    ("Alt+Enter", "detach"),
    ("Alt+.", "session list"),
    ("Alt+/", "session search"),
    ("Alt+h", "this help"),
];

fn draw_help(f: &mut Frame, area: Rect) {
    let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let head = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = HELP
        .iter()
        .map(|&(k, what)| {
            if k.is_empty() {
                Line::styled(format!(" {what}"), head)
            } else {
                Line::from(vec![
                    Span::styled(format!("   {k:<13}"), key),
                    Span::raw(what),
                ])
            }
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(" any key closes", Style::default().fg(Color::DarkGray)));

    // 2 for the frame; 56 fits the longest line with a margin.
    let popup = centered(area, 56, lines.len() as u16 + 2);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(" keys ", head));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// How far the focused pane is scrolled back, and how much history it has.
fn scroll_position(mgr: &Manager) -> (usize, usize) {
    mgr.focused_id()
        .and_then(|id| mgr.panes.get(&id))
        .map(|p| (p.screen.view_offset, p.screen.scrollback_len()))
        .unwrap_or((0, 0))
}
