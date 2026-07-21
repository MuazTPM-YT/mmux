//! Workspace state: Spaces (tabs), a binary split tree of Panes per Space, and
//! the pane registry that owns each PTY-backed terminal.
//!
//! The layout is a binary tree so that arbitrary nested splits compose cleanly.
//! Directional (WASD) focus navigation works off the pane rectangles captured
//! during the previous render, which makes it correct for any tree shape.

use std::collections::HashMap;

use anyhow::Result;
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;
use vte::Parser;

use crate::pty::{PtyEvent, PtyProcess};
use crate::screen::Screen;

/// How a split arranges its two children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    /// Children side by side (left | right) — bound to `Prefix + h`.
    Horizontal,
    /// Children stacked (top / bottom) — bound to `Prefix + v`.
    Vertical,
}

/// A node in a Space's layout tree.
#[derive(Clone, Debug)]
pub enum Node {
    Leaf(u64),
    Split {
        dir: SplitDir,
        /// Percentage of the parent given to child `a` (1..=99).
        ratio: u16,
        a: Box<Node>,
        b: Box<Node>,
    },
}

impl Node {
    fn first_leaf(&self) -> u64 {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { a, .. } => a.first_leaf(),
        }
    }

    fn leaves(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    /// Replace the leaf `target` with `repl` in place. Returns true if found.
    fn replace_leaf(&mut self, target: u64, repl: Node) -> bool {
        match self {
            Node::Leaf(id) => {
                if *id == target {
                    *self = repl;
                    true
                } else {
                    false
                }
            }
            Node::Split { a, b, .. } => {
                // Try `a`; only descend into `b` if not yet consumed.
                if a.replace_leaf(target, repl.clone()) {
                    true
                } else {
                    b.replace_leaf(target, repl)
                }
            }
        }
    }

    /// Remove leaf `target`, collapsing a split down to its surviving child.
    /// Returns the rewritten subtree, or `None` if this whole node was removed.
    fn remove_leaf(self, target: u64) -> Option<Node> {
        match self {
            Node::Leaf(id) => {
                if id == target {
                    None
                } else {
                    Some(Node::Leaf(id))
                }
            }
            Node::Split { dir, ratio, a, b } => {
                let a2 = a.remove_leaf(target);
                let b2 = b.remove_leaf(target);
                match (a2, b2) {
                    (Some(a), Some(b)) => Some(Node::Split {
                        dir,
                        ratio,
                        a: Box::new(a),
                        b: Box::new(b),
                    }),
                    (Some(x), None) | (None, Some(x)) => Some(x),
                    (None, None) => None,
                }
            }
        }
    }

    /// Compute the on-screen rectangle for every leaf within `area`.
    pub fn compute_rects(&self, area: Rect, out: &mut Vec<(u64, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, area)),
            Node::Split { dir, ratio, a, b } => {
                let (ra, rb) = split_rect(area, *dir, *ratio);
                a.compute_rects(ra, out);
                b.compute_rects(rb, out);
            }
        }
    }
}

/// Divide `area` into two child rects according to `dir` and `ratio`.
pub fn split_rect(area: Rect, dir: SplitDir, ratio: u16) -> (Rect, Rect) {
    let ratio = ratio.clamp(1, 99);
    match dir {
        SplitDir::Horizontal => {
            let wa = (area.width as u32 * ratio as u32 / 100) as u16;
            let wa = wa.clamp(1, area.width.saturating_sub(1).max(1));
            let a = Rect { width: wa, ..area };
            let b = Rect {
                x: area.x + wa,
                width: area.width.saturating_sub(wa),
                ..area
            };
            (a, b)
        }
        SplitDir::Vertical => {
            let ha = (area.height as u32 * ratio as u32 / 100) as u16;
            let ha = ha.clamp(1, area.height.saturating_sub(1).max(1));
            let a = Rect { height: ha, ..area };
            let b = Rect {
                y: area.y + ha,
                height: area.height.saturating_sub(ha),
                ..area
            };
            (a, b)
        }
    }
}

/// One terminal: a VTE parser feeding a [`Screen`], backed by a [`PtyProcess`].
pub struct Pane {
    pub id: u64,
    pub screen: Screen,
    parser: Parser,
    pub process: PtyProcess,
    /// Inner (content) rect from the last render — used for focus nav & resize.
    pub rect: Rect,
}

impl Pane {
    /// Feed a chunk of PTY output through the parser into the screen, then send
    /// any query replies (DA/DSR) the emulator produced straight back to the PTY.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Disjoint field borrows: parser drives, screen receives.
        for &byte in bytes {
            self.parser.advance(&mut self.screen, byte);
        }
        if !self.screen.responses.is_empty() {
            let reply = std::mem::take(&mut self.screen.responses);
            self.process.write(&reply);
        }
    }
}

pub struct Space {
    pub name: String,
    pub root: Node,
    pub focused: u64,
}

pub struct Manager {
    pub spaces: Vec<Space>,
    pub active: usize,
    pub panes: HashMap<u64, Pane>,
    next_id: u64,
    /// Bumped whenever the set of space *names* changes, so the server can skip
    /// re-persisting (and cloning every name) on the hot PTY-output path.
    spaces_gen: u64,
    tx: UnboundedSender<PtyEvent>,
}

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

impl Manager {
    /// Build a manager whose spaces are named by `space_names` (each with a
    /// single fresh pane). An empty list yields one space called "main".
    pub fn new(tx: UnboundedSender<PtyEvent>, space_names: &[String]) -> Result<Self> {
        let mut mgr = Manager {
            spaces: Vec::new(),
            active: 0,
            panes: HashMap::new(),
            next_id: 1,
            spaces_gen: 0,
            tx,
        };
        let names: Vec<String> = if space_names.is_empty() {
            vec!["main".to_string()]
        } else {
            space_names.to_vec()
        };
        for name in names {
            let id = mgr.spawn_pane()?;
            mgr.spaces.push(Space {
                name,
                root: Node::Leaf(id),
                focused: id,
            });
        }
        Ok(mgr)
    }

    /// The names of all spaces, for persistence.
    pub fn space_names(&self) -> Vec<String> {
        self.spaces.iter().map(|s| s.name.clone()).collect()
    }

    /// Generation of the space-name set; changes only when a name is added,
    /// removed, or renamed.
    pub fn spaces_gen(&self) -> u64 {
        self.spaces_gen
    }

    fn spawn_pane(&mut self) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let process = PtyProcess::spawn(id, DEFAULT_COLS, DEFAULT_ROWS, self.tx.clone())?;
        self.panes.insert(
            id,
            Pane {
                id,
                screen: Screen::new(DEFAULT_COLS, DEFAULT_ROWS),
                parser: Parser::new(),
                process,
                rect: Rect::default(),
            },
        );
        Ok(id)
    }

    pub fn active_space(&self) -> &Space {
        let i = self.active.min(self.spaces.len().saturating_sub(1));
        &self.spaces[i]
    }

    fn active_space_mut(&mut self) -> &mut Space {
        let i = self.active.min(self.spaces.len().saturating_sub(1));
        &mut self.spaces[i]
    }

    pub fn focused_id(&self) -> Option<u64> {
        self.spaces.get(self.active).map(|s| s.focused)
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.focused_id()?;
        self.panes.get_mut(&id)
    }

    /// True once every Space has been closed — the app should exit.
    pub fn is_empty(&self) -> bool {
        self.spaces.is_empty()
    }

    /// Split the focused pane, placing a new pane on the given side of it, and
    /// move focus to the new pane. Left/Right split horizontally (side by side);
    /// Up/Down split vertically (stacked). The side determines child order.
    pub fn new_pane(&mut self, dir: Direction) -> Result<()> {
        let new_id = self.spawn_pane()?;
        let target = self.active_space().focused;
        let (split_dir, new_first) = match dir {
            Direction::Left => (SplitDir::Horizontal, true),
            Direction::Right => (SplitDir::Horizontal, false),
            Direction::Up => (SplitDir::Vertical, true),
            Direction::Down => (SplitDir::Vertical, false),
        };
        let (a, b) = if new_first {
            (Node::Leaf(new_id), Node::Leaf(target))
        } else {
            (Node::Leaf(target), Node::Leaf(new_id))
        };
        let repl = Node::Split {
            dir: split_dir,
            ratio: 50,
            a: Box::new(a),
            b: Box::new(b),
        };
        let space = self.active_space_mut();
        space.root.replace_leaf(target, repl);
        space.focused = new_id;
        Ok(())
    }

    /// Close the currently focused pane.
    pub fn close_focused(&mut self) {
        if let Some(id) = self.focused_id() {
            self.close_pane(id);
        }
    }

    /// Close pane `id` wherever it lives. Collapses its split; removes the Space
    /// if it was that Space's last pane. Also used when a shell exits on its own.
    pub fn close_pane(&mut self, id: u64) {
        // Removing the pane drops its `PtyProcess`, whose `Drop` kills + reaps
        // the shell.
        self.panes.remove(&id);
        // Locate the Space that owns this pane.
        let owner = self.spaces.iter().position(|s| {
            let mut leaves = Vec::new();
            s.root.leaves(&mut leaves);
            leaves.contains(&id)
        });
        let Some(i) = owner else {
            return;
        };
        let space = &mut self.spaces[i];
        let root = std::mem::replace(&mut space.root, Node::Leaf(0));
        match root.remove_leaf(id) {
            Some(new_root) => {
                if space.focused == id {
                    space.focused = new_root.first_leaf();
                }
                space.root = new_root;
            }
            None => {
                // Space is now empty — drop it and keep `active` pointing at the
                // same logical space (indices before `active` shift left by one).
                self.spaces.remove(i);
                self.spaces_gen += 1;
                if i < self.active {
                    self.active -= 1;
                }
                if self.active >= self.spaces.len() {
                    self.active = self.spaces.len().saturating_sub(1);
                }
            }
        }
    }

    /// Create a new Space (with one pane) and switch to it.
    pub fn new_space(&mut self) -> Result<()> {
        let id = self.spawn_pane()?;
        let n = self.spaces.len() + 1;
        self.spaces.push(Space {
            name: format!("space{n}"),
            root: Node::Leaf(id),
            focused: id,
        });
        self.active = self.spaces.len() - 1;
        self.spaces_gen += 1;
        Ok(())
    }

    pub fn rename_active(&mut self, name: String) {
        if let Some(space) = self.spaces.get_mut(self.active) {
            space.name = name;
            self.spaces_gen += 1;
        }
    }

    pub fn switch_to(&mut self, index: usize) {
        if index < self.spaces.len() {
            self.active = index;
        }
    }

    pub fn next_space(&mut self) {
        if !self.spaces.is_empty() {
            self.active = (self.active + 1) % self.spaces.len();
        }
    }

    pub fn prev_space(&mut self) {
        if !self.spaces.is_empty() {
            self.active = (self.active + self.spaces.len() - 1) % self.spaces.len();
        }
    }

    /// Move focus to the nearest pane in a cardinal direction, using the rects
    /// captured at the last render.
    pub fn focus_dir(&mut self, dir: Direction) {
        let Some(cur_id) = self.focused_id() else {
            return;
        };
        let Some(cur) = self.panes.get(&cur_id).map(|p| p.rect) else {
            return;
        };
        let (cx, cy) = center(cur);

        let mut ids = Vec::new();
        self.active_space().root.leaves(&mut ids);

        let mut best: Option<(u64, i64)> = None;
        for id in ids {
            if id == cur_id {
                continue;
            }
            let Some(r) = self.panes.get(&id).map(|p| p.rect) else {
                continue;
            };
            let (rx, ry) = center(r);
            let (dx, dy) = (rx - cx, ry - cy);
            let in_dir = match dir {
                Direction::Left => dx < 0,
                Direction::Right => dx > 0,
                Direction::Up => dy < 0,
                Direction::Down => dy > 0,
            };
            if !in_dir {
                continue;
            }
            // Weight the primary axis heavily so we prefer true neighbours.
            let score = match dir {
                Direction::Left | Direction::Right => dx.unsigned_abs() as i64 * 4 + dy.unsigned_abs() as i64,
                Direction::Up | Direction::Down => dy.unsigned_abs() as i64 * 4 + dx.unsigned_abs() as i64,
            };
            if best.is_none_or(|(_, s)| score < s) {
                best = Some((id, score));
            }
        }

        if let Some((id, _)) = best {
            self.active_space_mut().focused = id;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn center(r: Rect) -> (i64, i64) {
    (
        r.x as i64 + r.width as i64 / 2,
        r.y as i64 + r.height as i64 / 2,
    )
}

#[cfg(test)]
mod tests {
    //! These cover the pure layout-tree logic (no PTYs), which is where the
    //! fiddly correctness lives.
    use super::*;

    #[test]
    fn split_then_remove_collapses_tree() {
        // Start: Leaf(1). Split -> Split(Leaf(1), Leaf(2)).
        let mut root = Node::Leaf(1);
        root.replace_leaf(
            1,
            Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 50,
                a: Box::new(Node::Leaf(1)),
                b: Box::new(Node::Leaf(2)),
            },
        );
        let mut leaves = Vec::new();
        root.leaves(&mut leaves);
        assert_eq!(leaves, vec![1, 2]);

        // Removing pane 2 collapses the split back to a bare Leaf(1).
        let collapsed = root.remove_leaf(2).unwrap();
        assert!(matches!(collapsed, Node::Leaf(1)));
    }

    #[test]
    fn remove_last_leaf_yields_none() {
        assert!(Node::Leaf(7).remove_leaf(7).is_none());
    }

    #[test]
    fn horizontal_split_places_children_side_by_side() {
        let area = Rect { x: 0, y: 0, width: 100, height: 40 };
        let (a, b) = split_rect(area, SplitDir::Horizontal, 50);
        assert_eq!(a.x, 0);
        assert_eq!(b.x, 50);
        assert_eq!(a.height, 40);
        assert_eq!(a.width + b.width, 100);
    }

    #[test]
    fn vertical_split_stacks_children() {
        let area = Rect { x: 0, y: 0, width: 100, height: 40 };
        let (a, b) = split_rect(area, SplitDir::Vertical, 50);
        assert_eq!(a.y, 0);
        assert_eq!(b.y, 20);
        assert_eq!(a.width, 100);
        assert_eq!(a.height + b.height, 40);
    }

    #[test]
    fn compute_rects_covers_all_leaves() {
        let root = Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 50,
            a: Box::new(Node::Leaf(1)),
            b: Box::new(Node::Split {
                dir: SplitDir::Vertical,
                ratio: 50,
                a: Box::new(Node::Leaf(2)),
                b: Box::new(Node::Leaf(3)),
            }),
        };
        let mut rects = Vec::new();
        root.compute_rects(Rect { x: 0, y: 0, width: 80, height: 24 }, &mut rects);
        let ids: Vec<u64> = rects.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
