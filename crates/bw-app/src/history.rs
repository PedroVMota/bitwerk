//! Undo / redo and the History panel.
//!
//! The history is a **tree** of states. The root is how the project looked
//! when it was opened (or created); every other node is the whole root
//! circuit right after one user action, child of the state the action was
//! made from. The cursor marks the state on the canvas: Undo goes to its
//! parent, Redo to the child last visited, and clicking a node in the panel
//! jumps straight to it. Going back and making a *different* change does not
//! discard what was there: it starts a new branch, and the old one stays in
//! the tree.
//!
//! States are `Arc<Circuit>` snapshots. The root circuit holds the macro
//! definitions too, so an edit made inside a macro is covered like any other,
//! and restoring a state needs no inverse operations: the circuit is swapped
//! in, the simulator reloads it and the scene is rebuilt (see
//! [`BitwerkApp::history_goto`]).
//!
//! What counts as one action: every model edit made within one UI pass
//! (a drag, "Delete" on a multi-selection, creating a macro, …) is merged
//! into one node, committed at the end of the pass. Typing into a memory or
//! a note is merged further: consecutive edits of the same kind within
//! [`COALESCE`] replace the last node (while it has no children) instead of
//! adding one.
//!
//! Memory is bounded: at most [`MAX_STATES`] nodes and about [`MAX_WEIGHT`]
//! components + wire segments in total. The oldest node goes first among the
//! branch tips (leaves other than the current state) and the root when it has
//! a single child, so the current state and the path to it are never dropped.
//!
//! The panel draws the tree like a commit graph: one row per node in
//! creation order, branches in lanes to the right of the trunk. Thumbnails
//! live in [`crate::thumb`].

use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bw_core::{Circuit, ComponentId, ComponentKind, Edit, MacroId};
use egui::{Color32, Pos2, Sense, Stroke, Vec2};

use crate::app::BitwerkApp;
use crate::thumb::Thumb;

/// Most nodes kept (including the root).
const MAX_STATES: usize = 200;
/// Budget of retained (components + wire segments) summed over all nodes.
const MAX_WEIGHT: usize = 1_500_000;
/// Typing-like edits closer together than this share one node.
const COALESCE: Duration = Duration::from_secs(2);
/// Size of the thumbnail in the hover tooltip.
const PREVIEW_SIZE: Vec2 = Vec2::new(420.0, 280.0);
/// Colours of the branch lanes.
const LANE_COLORS: [Color32; 6] = [
    Color32::from_rgb(110, 170, 255),
    Color32::from_rgb(240, 170, 80),
    Color32::from_rgb(120, 210, 140),
    Color32::from_rgb(230, 120, 170),
    Color32::from_rgb(180, 140, 240),
    Color32::from_rgb(90, 200, 210),
];

/// Stable identifier of a history node (shown as `#id`; ids grow with time
/// and are not reused, so dropping old nodes leaves gaps).
pub type StateId = u32;

struct State {
    parent: Option<StateId>,
    /// Oldest first.
    children: Vec<StateId>,
    /// Child last visited (the Redo target).
    resume: Option<StateId>,
    label: String,
    circuit: Arc<Circuit>,
    /// Retained size, for the memory budget.
    weight: usize,
    /// Edit kind that produced it, for merging typing-like edits.
    merge_key: Option<&'static str>,
    thumb: OnceCell<Thumb>,
}

impl State {
    fn new(parent: Option<StateId>, label: String, circuit: Arc<Circuit>, merge_key: Option<&'static str>) -> Self {
        let weight = circuit.component_count() + circuit.segment_count() + 1;
        Self { parent, children: Vec::new(), resume: None, label, circuit, weight, merge_key, thumb: OnceCell::new() }
    }
}

/// Edit made in the current UI pass, committed at its end.
struct Pending {
    label: String,
    count: u32,
    merge_key: Option<&'static str>,
}

/// The undo / redo tree.
pub struct History {
    nodes: BTreeMap<StateId, State>,
    cursor: StateId,
    next_id: StateId,
    pending: Option<Pending>,
    committed_at: Instant,
    /// Label for the macro-body edit being routed through the root (so the
    /// entry reads "Move" rather than "Edit macro definition").
    hint: Option<(String, Option<&'static str>)>,
    /// Name for the next entry, overriding the edits' own (multi-step
    /// actions such as creating a macro).
    next_name: Option<String>,
    /// The panel is shown.
    pub open: bool,
    /// Scroll the panel to the current node on the next frame.
    scroll_to_cursor: bool,
}

/// Wording and merge behaviour of an edit in the entry list.
fn describe(edit: &Edit) -> (&'static str, Option<&'static str>) {
    match edit {
        Edit::AddComponent { .. } => ("Add component", None),
        Edit::RemoveComponent(_) => ("Delete component", None),
        Edit::MoveComponent(..) => ("Move component", None),
        Edit::SetRotation(..) => ("Rotate component", None),
        Edit::SetKind(..) => ("Change component", None),
        Edit::AddWire { .. } => ("Draw wire", None),
        Edit::RemoveSegment(_) => ("Delete wire", None),
        Edit::MoveNode(..) => ("Move wire junction", None),
        Edit::DragSegment(..) => ("Drag wire", None),
        Edit::SetMemoryWord { .. } | Edit::SetMemory { .. } => ("Edit memory contents", Some("memory")),
        Edit::DefineMacro { .. } => ("Edit macro definition", None),
        Edit::RemoveMacro(_) => ("Remove macro definition", None),
        Edit::SetColor(..) => ("Change colour", Some("colour")),
        Edit::SetNote { .. } => ("Edit note", Some("note")),
        Edit::SetEvalMode(_) => ("Change simulation mode", None),
    }
}

impl History {
    /// A history whose only state is `circuit`.
    #[must_use]
    pub fn new(circuit: Arc<Circuit>) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(0, State::new(None, "Initial state".into(), circuit, None));
        Self {
            nodes,
            cursor: 0,
            next_id: 1,
            pending: None,
            committed_at: Instant::now(),
            hint: None,
            next_name: None,
            open: false,
            scroll_to_cursor: false,
        }
    }

    /// Starts over from `circuit` (a project was opened, created or
    /// imported).
    pub fn reset(&mut self, circuit: Arc<Circuit>, label: &str) {
        self.nodes.clear();
        self.nodes.insert(0, State::new(None, label.into(), circuit, None));
        self.cursor = 0;
        self.next_id = 1;
        self.pending = None;
        self.hint = None;
        self.next_name = None;
        self.scroll_to_cursor = true;
    }

    fn node(&self, id: StateId) -> &State {
        // Every id held by `cursor`, `parent` and `children` is in the map.
        &self.nodes[&id]
    }

    /// State Undo goes to.
    #[must_use]
    fn undo_target(&self) -> Option<StateId> {
        self.node(self.cursor).parent
    }

    /// State Redo goes to: the child last visited, else the newest.
    #[must_use]
    fn redo_target(&self) -> Option<StateId> {
        let n = self.node(self.cursor);
        n.resume.filter(|r| n.children.contains(r)).or_else(|| n.children.last().copied())
    }

    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.undo_target().is_some() || self.pending.is_some()
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.pending.is_none() && self.redo_target().is_some()
    }

    /// Names the entry the edits of this pass will form.
    pub fn name_next(&mut self, label: &str) {
        self.next_name = Some(label.to_string());
    }

    /// Words the macro-body edit about to be routed through the root.
    pub(crate) fn set_hint(&mut self, edit: Option<&Edit>) {
        self.hint = edit.map(|e| {
            let (l, k) = describe(e);
            (l.to_string(), k)
        });
    }

    /// An edit was applied to the root circuit.
    pub(crate) fn note_edit(&mut self, edit: &Edit) {
        let (label, key) = self.hint.clone().unwrap_or_else(|| {
            let (l, k) = describe(edit);
            (l.to_string(), k)
        });
        match &mut self.pending {
            Some(p) => {
                p.count += 1;
                if p.merge_key != key {
                    p.merge_key = None;
                }
            }
            None => self.pending = Some(Pending { label, count: 1, merge_key: key }),
        }
    }

    /// Closes the current action: the circuit as it is now becomes a new
    /// child of the current state (a new branch if that state already has
    /// children). Called at the end of every UI pass.
    pub fn commit(&mut self, circuit: &Arc<Circuit>) {
        let Some(p) = self.pending.take() else { return };
        let label =
            self.next_name.take().unwrap_or(if p.count > 1 { format!("{} (×{})", p.label, p.count) } else { p.label });
        let now = Instant::now();
        let cur = self.cursor;
        let merge = p.merge_key.is_some_and(|k| {
            let n = self.node(cur);
            n.parent.is_some() && n.children.is_empty() && n.merge_key == Some(k) && now - self.committed_at < COALESCE
        });
        if merge {
            let parent = self.node(cur).parent;
            self.nodes.insert(cur, State::new(parent, label, Arc::clone(circuit), p.merge_key));
        } else {
            let id = self.next_id;
            self.next_id += 1;
            self.nodes.insert(id, State::new(Some(cur), label, Arc::clone(circuit), p.merge_key));
            if let Some(parent) = self.nodes.get_mut(&cur) {
                parent.children.push(id);
                parent.resume = Some(id);
            }
            self.cursor = id;
        }
        self.committed_at = now;
        self.scroll_to_cursor = true;
        self.trim();
    }

    /// Makes `id` the current state and remembers the path to it, so Redo
    /// from any ancestor follows it. Returns its circuit (`None`: unknown or
    /// already current).
    fn go(&mut self, id: StateId) -> Option<Arc<Circuit>> {
        if id == self.cursor || !self.nodes.contains_key(&id) {
            return None;
        }
        self.cursor = id;
        self.scroll_to_cursor = true;
        let mut c = id;
        while let Some(p) = self.node(c).parent {
            if let Some(n) = self.nodes.get_mut(&p) {
                n.resume = Some(c);
            }
            c = p;
        }
        Some(Arc::clone(&self.node(id).circuit))
    }

    /// Drops nodes while over the node or memory budget (see the module
    /// docs for the order).
    fn trim(&mut self) {
        let mut gone = Vec::new();
        loop {
            let weight: usize = self.nodes.values().map(|n| n.weight).sum();
            if self.nodes.len() <= MAX_STATES && weight <= MAX_WEIGHT {
                break;
            }
            let victim = self
                .nodes
                .iter()
                .filter(|(id, n)| {
                    **id != self.cursor && (n.children.is_empty() || (n.parent.is_none() && n.children.len() == 1))
                })
                .map(|(id, _)| *id)
                .next();
            let Some(id) = victim else { break };
            if let Some(n) = self.remove(id) {
                gone.push(n);
            }
        }
        // Freeing big circuits can take a while: not on the UI thread.
        if gone.iter().map(|s| s.weight).sum::<usize>() > 20_000 {
            let _ = std::thread::Builder::new().name("bw-history-drop".into()).spawn(move || drop(gone));
        }
    }

    /// Removes a leaf or a root with one child (which becomes the root).
    fn remove(&mut self, id: StateId) -> Option<State> {
        let node = self.nodes.remove(&id)?;
        match node.parent {
            Some(p) => {
                if let Some(parent) = self.nodes.get_mut(&p) {
                    parent.children.retain(|c| *c != id);
                    if parent.resume == Some(id) {
                        parent.resume = None;
                    }
                }
            }
            None => {
                for c in &node.children {
                    if let Some(child) = self.nodes.get_mut(c) {
                        child.parent = None;
                    }
                }
            }
        }
        Some(node)
    }

    /// Ancestors of the current state and the state itself.
    fn active_path(&self) -> HashSet<StateId> {
        let mut set = HashSet::new();
        let mut c = Some(self.cursor);
        while let Some(id) = c {
            set.insert(id);
            c = self.node(id).parent;
        }
        set
    }

    /// Rows (one per node, oldest first) and the number of lanes. The oldest
    /// child continues its parent's lane; later children start new lanes
    /// to the right, in the lowest lane that is free from the parent's row to
    /// the end of the new chain.
    fn layout(&self) -> (Vec<Row>, usize) {
        let ids: Vec<StateId> = self.nodes.keys().copied().collect();
        let row_of: HashMap<StateId, usize> = ids.iter().enumerate().map(|(r, id)| (*id, r)).collect();
        // Last row of the chain that starts at each row (oldest children).
        let mut chain_end = vec![0; ids.len()];
        for r in (0..ids.len()).rev() {
            chain_end[r] = self.node(ids[r]).children.first().map_or(r, |c| chain_end[row_of[c]]);
        }
        let mut busy_until: Vec<usize> = Vec::new();
        let mut rows: Vec<Row> = Vec::with_capacity(ids.len());
        for (r, id) in ids.iter().enumerate() {
            let n = self.node(*id);
            let parent = n.parent.map(|p| row_of[&p]);
            let inherits = n.parent.is_some_and(|p| self.node(p).children.first() == Some(id));
            let lane = if let (true, Some(pr)) = (inherits, parent) {
                rows[pr].lane
            } else {
                let from = parent.unwrap_or(r);
                let free = busy_until.iter().position(|b| *b < from);
                let lane = free.unwrap_or(busy_until.len());
                if lane == busy_until.len() {
                    busy_until.push(0);
                }
                busy_until[lane] = chain_end[r];
                lane
            };
            rows.push(Row { id: *id, lane, parent });
        }
        (rows, busy_until.len().max(1))
    }
}

/// One row of the history graph.
struct Row {
    id: StateId,
    lane: usize,
    parent: Option<usize>,
}

impl BitwerkApp {
    /// Undo the last action.
    pub fn undo(&mut self) {
        self.history_commit();
        if let Some(to) = self.history.undo_target() {
            let what = self.history.node(self.history.cursor).label.clone();
            self.history_goto(to);
            self.status = format!("Undid: {what}");
        } else {
            self.status = "Nothing to undo".into();
        }
    }

    /// Redo the action that was undone.
    pub fn redo(&mut self) {
        self.history_commit();
        if let Some(to) = self.history.redo_target() {
            self.history_goto(to);
            let what = self.history.node(to).label.clone();
            self.status = format!("Redid: {what}");
        } else {
            self.status = "Nothing to redo".into();
        }
    }

    /// Commits the edits of this UI pass as one history entry.
    pub fn history_commit(&mut self) {
        let circuit = Arc::clone(&self.circuit);
        self.history.commit(&circuit);
    }

    /// Makes state `id` the current circuit.
    ///
    /// The state replaces the root circuit wholesale: the simulator reloads
    /// it (run state is kept, flip-flop state restarts) and the scene is
    /// rebuilt. Macro levels that were open are re-entered as far as their
    /// definitions and instances still exist.
    pub fn history_goto(&mut self, id: StateId) {
        self.history_commit();
        let Some(target) = self.history.go(id) else { return };
        let was_inside: Vec<(MacroId, Option<ComponentId>)> = self.levels.iter().map(|l| (l.def, l.inst)).collect();
        let level_camera = self.camera;

        self.drop_levels();
        self.circuit = target;
        self.multi.clear();
        self.clear_selection();
        self.macro_draft = None;
        self.pressed = None;
        self.wire_draft = None;
        self.drag = None;
        self.hover_net = None;
        crate::memwin::close(self);
        if let Some(m) = &self.menu {
            m.sync_eval_mode(self.circuit.eval_mode());
        }
        self.dirty = true;
        self.commit_full();
        // Small circuits: show the restored state in this very frame instead
        // of waiting for the scene worker.
        if self.circuit.component_count() + self.circuit.segment_count() <= 4000 {
            let scene = Box::new(bw_render::Scene::build(&self.circuit, self.revision));
            let old = std::mem::replace(&mut self.scene, scene);
            self.scene_worker.retire(old);
        }

        let mut reentered = false;
        for (def, inst) in was_inside {
            let as_instance = inst.filter(
                |i| matches!(self.shown().component(*i).map(|c| c.kind), Some(ComponentKind::Macro(s)) if s.def == def),
            );
            if let Some(i) = as_instance {
                self.enter_instance(i);
            } else if self.circuit.macros().def(def).is_some() && self.levels.is_empty() {
                self.enter_def(def);
            } else {
                break;
            }
            reentered = true;
        }
        if reentered {
            self.camera = level_camera;
            self.fit_pending = false;
        }
    }

    /// The History panel: the tree of states drawn like a commit graph, one
    /// row per state (`#3  Move component`). Hovering a row previews the
    /// circuit as it was then; clicking it goes there.
    #[allow(clippy::too_many_lines, reason = "one egui panel: header, graph edges, rows")]
    pub fn history_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("History");
        ui.horizontal(|ui| {
            if ui.add_enabled(self.history.can_undo(), egui::Button::new("Undo")).clicked() {
                self.undo();
            }
            if ui.add_enabled(self.history.can_redo(), egui::Button::new("Redo")).clicked() {
                self.redo();
            }
        });
        let n = self.history.nodes.len();
        let branches = self.history.nodes.values().filter(|s| s.children.is_empty()).count();
        ui.label(format!(
            "{} step{} · {branches} branch{} · at #{}",
            n - 1,
            if n == 2 { "" } else { "s" },
            if branches == 1 { "" } else { "es" },
            self.history.cursor
        ));
        ui.separator();

        let (rows, lanes) = self.history.layout();
        let active = self.history.active_path();
        let cursor = self.history.cursor;
        let (dark, style) = (ui.visuals().dark_mode, self.style);
        let row_h = ui.spacing().interact_size.y;
        ui.spacing_mut().item_spacing.y = 0.0;
        let pad = 8.0;
        #[allow(clippy::cast_precision_loss, reason = "a handful of lanes")]
        let lane_w = ((ui.available_width() * 0.5 - 2.0 * pad) / lanes as f32).clamp(6.0, 14.0);
        #[allow(clippy::cast_precision_loss, reason = "a handful of lanes")]
        let graph_w = lanes as f32 * lane_w + 2.0 * pad;

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if std::mem::take(&mut self.history.scroll_to_cursor) {
            // Rows outside the viewport are not built, so scroll by offset.
            let at = rows.iter().position(|r| r.id == cursor).unwrap_or(0);
            #[allow(clippy::cast_precision_loss, reason = "a few hundred rows at most")]
            let y = at as f32 * row_h - ui.available_height() * 0.5;
            area = area.vertical_scroll_offset(y.max(0.0));
        }
        let mut jump = None;
        area.show_rows(ui, row_h, rows.len(), |ui, visible| {
            let top = ui.cursor().top();
            let left = ui.cursor().left();
            let width = ui.available_width();
            #[allow(clippy::cast_precision_loss, reason = "a few hundred rows at most")]
            let y_of = |r: usize| top + (r as f32 - visible.start as f32 + 0.5) * row_h;
            #[allow(clippy::cast_precision_loss, reason = "a handful of lanes")]
            let x_of = |lane: usize| left + pad + (lane as f32 + 0.5) * lane_w;
            let lane_color = |lane: usize| LANE_COLORS[lane % LANE_COLORS.len()];
            let painter = ui.painter().clone();
            // Edges first (dots go on top): from each node up to its parent.
            for (r, row) in rows.iter().enumerate() {
                let Some(pr) = row.parent else { continue };
                if r < visible.start || pr >= visible.end {
                    continue;
                }
                let on_path = active.contains(&row.id);
                let color = if on_path { lane_color(row.lane) } else { lane_color(row.lane).gamma_multiply(0.45) };
                let stroke = Stroke::new(if on_path { 2.0 } else { 1.3 }, color);
                let (xp, xc) = (x_of(rows[pr].lane), x_of(row.lane));
                painter.line_segment([Pos2::new(xp, y_of(pr)), Pos2::new(xc, y_of(pr))], stroke);
                painter.line_segment([Pos2::new(xc, y_of(pr)), Pos2::new(xc, y_of(r))], stroke);
            }
            for r in visible {
                let row = &rows[r];
                let state = self.history.node(row.id);
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, row_h), Sense::click());
                let on_path = active.contains(&row.id);
                if row.id == cursor {
                    painter.rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
                } else if resp.hovered() {
                    painter.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
                }
                let center = Pos2::new(x_of(row.lane), rect.center().y);
                let dot = lane_color(row.lane);
                if row.id == cursor {
                    painter.circle_filled(center, 5.5, ui.visuals().strong_text_color());
                    painter.circle_stroke(center, 5.5, Stroke::new(2.0, dot));
                } else {
                    painter.circle_filled(center, 4.0, if on_path { dot } else { dot.gamma_multiply(0.5) });
                }
                let text_color = if on_path { ui.visuals().text_color() } else { ui.visuals().weak_text_color() };
                let galley = painter.layout_no_wrap(
                    format!("#{}  {}", row.id, state.label),
                    egui::FontId::proportional(13.0),
                    text_color,
                );
                let text_pos = Pos2::new(left + graph_w + 4.0, rect.center().y - galley.size().y * 0.5);
                painter.with_clip_rect(rect).galley(text_pos, galley, text_color);
                resp.clone().on_hover_ui(|ui| {
                    ui.label(format!("#{}  {}", row.id, state.label));
                    let (rect, _) = ui.allocate_exact_size(PREVIEW_SIZE, Sense::hover());
                    let thumb = state.thumb.get_or_init(|| Thumb::build(&state.circuit));
                    thumb.paint(ui.painter(), rect, style, dark);
                });
                if resp.clicked() {
                    jump = Some(row.id);
                }
            }
        });
        if let Some(id) = jump {
            self.history_goto(id);
            self.status = format!("Went to #{id}: {}", self.history.node(id).label);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::many_single_char_names, reason = "short test fixtures")]
    use super::*;
    use bw_core::{GridPos, Rotation};

    fn add(c: &mut Arc<Circuit>, h: &mut History, x: i32) {
        let e = Edit::AddComponent {
            kind: ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle },
            pos: GridPos::new(x, 0),
            rotation: Rotation::R0,
        };
        Arc::make_mut(c).apply(&e).expect("edit applies");
        h.note_edit(&e);
    }

    /// One committed action adding a component at `x`.
    fn step(c: &mut Arc<Circuit>, h: &mut History, x: i32) -> StateId {
        add(c, h, x);
        h.commit(c);
        h.cursor
    }

    fn undo(c: &mut Arc<Circuit>, h: &mut History) {
        let to = h.undo_target().expect("can undo");
        *c = h.go(to).expect("moves");
    }

    #[test]
    fn undo_redo_walk_the_chain() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        assert!(!h.can_undo() && !h.can_redo());
        let a = step(&mut c, &mut h, 0);
        let b = step(&mut c, &mut h, 10);
        assert_eq!((a, b, h.nodes.len()), (1, 2, 3));
        assert!(h.can_undo() && !h.can_redo());
        undo(&mut c, &mut h);
        assert_eq!(c.component_count(), 1);
        assert_eq!(h.redo_target(), Some(b));
        undo(&mut c, &mut h);
        assert_eq!((c.component_count(), h.cursor, h.undo_target()), (0, 0, None));
    }

    #[test]
    fn a_change_after_going_back_starts_a_branch_and_keeps_the_old_one() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        let a = step(&mut c, &mut h, 0);
        let b = step(&mut c, &mut h, 10);
        undo(&mut c, &mut h);
        let d = step(&mut c, &mut h, 20);
        // Both branches hang off `a`; nothing was discarded.
        assert_eq!(h.node(a).children, vec![b, d]);
        assert_eq!(h.nodes.len(), 4);
        assert_eq!(h.node(b).circuit.component_count(), 2);
        // Redo from `a` follows the branch last visited; jumping to the old
        // branch and back up makes it the default again.
        h.go(a);
        assert_eq!(h.redo_target(), Some(d));
        c = h.go(b).expect("old branch is reachable");
        assert_eq!(c.component_count(), 2);
        h.go(a);
        assert_eq!(h.redo_target(), Some(b));
    }

    #[test]
    fn graph_layout_puts_branches_in_their_own_lanes() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        let a = step(&mut c, &mut h, 0);
        let b = step(&mut c, &mut h, 10);
        undo(&mut c, &mut h);
        let d = step(&mut c, &mut h, 20);
        let e = step(&mut c, &mut h, 30);
        let (rows, lanes) = h.layout();
        let lane = |id| rows.iter().find(|r| r.id == id).expect("row").lane;
        assert_eq!(lanes, 2);
        assert_eq!([lane(0), lane(a), lane(b)], [0, 0, 0], "the first line stays on the trunk");
        assert_eq!([lane(d), lane(e)], [1, 1], "the new branch moves right");
        assert_eq!(rows[usize::try_from(d).expect("small")].parent, Some(1));
    }

    #[test]
    fn several_edits_in_one_pass_are_one_node() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        add(&mut c, &mut h, 0);
        add(&mut c, &mut h, 10);
        add(&mut c, &mut h, 20);
        h.commit(&c);
        assert_eq!(h.nodes.len(), 2);
        assert_eq!(h.node(1).label, "Add component (×3)");
    }

    #[test]
    fn budget_drops_old_nodes_but_never_the_current_path() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        const { assert!(MAX_STATES < 220) };
        for i in 0..220 {
            step(&mut c, &mut h, i * 10);
        }
        assert_eq!(h.nodes.len(), MAX_STATES);
        assert_eq!(h.node(h.cursor).circuit.component_count(), 220);
        // The oldest went first and the next one became the root.
        let root = h.nodes.iter().find(|(_, n)| n.parent.is_none()).map(|(id, _)| *id).expect("a root");
        assert_eq!(usize::try_from(root).expect("small"), 220 - MAX_STATES + 1);
    }

    #[test]
    fn budget_prefers_old_branch_tips_over_the_trunk() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        step(&mut c, &mut h, 0);
        let b = step(&mut c, &mut h, 10);
        undo(&mut c, &mut h);
        for i in 0..MAX_STATES {
            step(&mut c, &mut h, 100 + i32::try_from(i).expect("small") * 10);
        }
        assert!(!h.nodes.contains_key(&b), "the abandoned tip was dropped first");
        assert!(h.nodes.len() <= MAX_STATES);
        assert!(h.nodes.contains_key(&h.cursor));
    }
}
