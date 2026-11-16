//! Undo / redo and the History panel.
//!
//! The history is a linear timeline of **states**: `states[0]` is how the
//! project looked when it was opened (or created), every later entry is the
//! whole root circuit right after one user action. The cursor marks the state
//! on the canvas: Undo moves it back, Redo forward, and clicking an entry in
//! the panel jumps straight to it. A new action made after going back
//! discards the states ahead (the classic redo rule).
//!
//! States are `Arc<Circuit>` snapshots. The root circuit holds the macro
//! definitions too, so an edit made inside a macro is covered like any other,
//! and restoring a state needs no inverse operations: the circuit is swapped
//! in, the simulator reloads it and the scene is rebuilt (see
//! [`BitwerkApp::history_goto`]).
//!
//! What counts as one action: every model edit made within one UI pass
//! (a drag, "Delete" on a multi-selection, creating a macro, …) is merged
//! into one entry, committed at the end of the pass. Typing into a memory or
//! a note is merged further: consecutive edits of the same kind within
//! [`COALESCE`] replace the last entry instead of adding one.
//!
//! Memory is bounded: at most [`MAX_STATES`] entries and about
//! [`MAX_WEIGHT`] components + wire segments in total; the oldest states go
//! first (the current one is never dropped).
//!
//! Thumbnails are drawn straight from the snapshot with the egui painter
//! (components as boxes, wires as lines), computed once per state and capped
//! at [`THUMB_ITEMS`] items so that a 100k-component circuit stays cheap.

use std::cell::OnceCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bw_core::{Circuit, ComponentId, ComponentKind, Edit, MacroId};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::app::BitwerkApp;

/// Most entries kept (including the initial state).
const MAX_STATES: usize = 200;
/// Budget of retained (components + wire segments) summed over all states.
const MAX_WEIGHT: usize = 1_500_000;
/// Typing-like edits closer together than this share one entry.
const COALESCE: Duration = Duration::from_secs(2);
/// Items (boxes or wires) a thumbnail draws at most; more are sampled.
const THUMB_ITEMS: usize = 3000;
/// Size of a thumbnail in the panel.
const THUMB_SIZE: Vec2 = Vec2::new(216.0, 120.0);
/// Height of one row of the panel (thumbnail, caption, spacing).
const ROW_HEIGHT: f32 = 120.0 + 20.0 + 14.0;

/// Pre-computed geometry of a state, in grid units.
struct Thumb {
    lo: Pos2,
    hi: Pos2,
    /// `(min x, min y, max x, max y, is a macro instance)`.
    boxes: Vec<(i32, i32, i32, i32, bool)>,
    /// Wire segments `(x1, y1, x2, y2)`.
    wires: Vec<(i32, i32, i32, i32)>,
}

impl Thumb {
    fn build(c: &Circuit) -> Self {
        let mut lo = (i32::MAX, i32::MAX);
        let mut hi = (i32::MIN, i32::MIN);
        let mut grow = |x: i32, y: i32| {
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        };
        let stride_c = c.component_count().div_ceil(THUMB_ITEMS).max(1);
        let mut boxes = Vec::new();
        for (i, comp) in c.components().enumerate() {
            let b = comp.bounds();
            grow(b.min.x, b.min.y);
            grow(b.max.x, b.max.y);
            if i % stride_c == 0 {
                boxes.push((b.min.x, b.min.y, b.max.x, b.max.y, matches!(comp.kind, ComponentKind::Macro(_))));
            }
        }
        let stride_w = c.segment_count().div_ceil(THUMB_ITEMS).max(1);
        let mut wires = Vec::new();
        for (i, (id, _)) in c.segments().enumerate() {
            let Some((a, b)) = c.segment_ends(id) else { continue };
            grow(a.x, a.y);
            grow(b.x, b.y);
            if i % stride_w == 0 {
                wires.push((a.x, a.y, b.x, b.y));
            }
        }
        if lo.0 > hi.0 {
            (lo, hi) = ((0, 0), (1, 1));
        }
        #[allow(clippy::cast_precision_loss, reason = "grid coordinates are far below 2^24 in practice")]
        let p = |t: (i32, i32)| Pos2::new(t.0 as f32, t.1 as f32);
        Self { lo: p(lo), hi: p(hi), boxes, wires }
    }

    /// Paints the snapshot fitted into `rect`.
    fn paint(&self, painter: &egui::Painter, rect: Rect, dark: bool) {
        let (bg, wire, body, mac) = if dark {
            (
                Color32::from_gray(22),
                Color32::from_rgb(90, 190, 120),
                Color32::from_gray(170),
                Color32::from_rgb(230, 170, 70),
            )
        } else {
            (
                Color32::from_gray(246),
                Color32::from_rgb(30, 140, 70),
                Color32::from_gray(80),
                Color32::from_rgb(200, 120, 20),
            )
        };
        painter.rect_filled(rect, 3.0, bg);
        let inner = rect.shrink(6.0);
        let ext = (self.hi - self.lo).max(Vec2::splat(1.0));
        let scale = (inner.width() / ext.x).min(inner.height() / ext.y);
        let off = inner.center().to_vec2() - ((self.lo.to_vec2() + self.hi.to_vec2()) * 0.5) * scale;
        let map = |x: i32, y: i32| {
            #[allow(clippy::cast_precision_loss, reason = "grid coordinates are far below 2^24 in practice")]
            Pos2::new(x as f32 * scale + off.x, y as f32 * scale + off.y)
        };
        let painter = painter.with_clip_rect(rect);
        let stroke = Stroke::new((scale * 0.18).clamp(0.6, 1.4), wire);
        for &(x1, y1, x2, y2) in &self.wires {
            painter.line_segment([map(x1, y1), map(x2, y2)], stroke);
        }
        for &(x1, y1, x2, y2, is_macro) in &self.boxes {
            let r = Rect::from_min_max(map(x1, y1), map(x2, y2));
            let r = if r.width() < 1.5 || r.height() < 1.5 {
                Rect::from_center_size(r.center(), Vec2::splat(1.5))
            } else {
                r
            };
            painter.rect_filled(r, 0.0, if is_macro { mac } else { body }.gamma_multiply(0.85));
        }
        painter.rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_gray(110)), egui::StrokeKind::Inside);
    }
}

struct State {
    label: String,
    circuit: Arc<Circuit>,
    /// Retained size, for the memory budget.
    weight: usize,
    /// Edit kind that produced it, for merging typing-like edits.
    merge_key: Option<&'static str>,
    thumb: OnceCell<Thumb>,
}

impl State {
    fn new(label: String, circuit: Arc<Circuit>, merge_key: Option<&'static str>) -> Self {
        let weight = circuit.component_count() + circuit.segment_count() + 1;
        Self { label, circuit, weight, merge_key, thumb: OnceCell::new() }
    }
}

/// Edit made in the current UI pass, committed at its end.
struct Pending {
    label: String,
    count: u32,
    merge_key: Option<&'static str>,
}

/// The undo / redo timeline.
pub struct History {
    states: Vec<State>,
    cursor: usize,
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
    /// Scroll the panel to the current entry on the next frame.
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
        Self {
            states: vec![State::new("Initial state".into(), circuit, None)],
            cursor: 0,
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
        self.states = vec![State::new(label.into(), circuit, None)];
        self.cursor = 0;
        self.pending = None;
        self.hint = None;
        self.next_name = None;
        self.scroll_to_cursor = true;
    }

    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.cursor > 0 || self.pending.is_some()
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.pending.is_none() && self.cursor + 1 < self.states.len()
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

    /// Closes the current action: the circuit as it is now becomes the newest
    /// state. Called at the end of every UI pass.
    pub fn commit(&mut self, circuit: &Arc<Circuit>) {
        let Some(p) = self.pending.take() else { return };
        let label =
            self.next_name.take().unwrap_or(if p.count > 1 { format!("{} (×{})", p.label, p.count) } else { p.label });
        self.states.truncate(self.cursor + 1);
        let now = Instant::now();
        let merge = p.merge_key.filter(|k| {
            self.cursor > 0 && self.states[self.cursor].merge_key == Some(*k) && now - self.committed_at < COALESCE
        });
        if merge.is_some() {
            self.states[self.cursor] = State::new(label, Arc::clone(circuit), p.merge_key);
        } else {
            self.states.push(State::new(label, Arc::clone(circuit), p.merge_key));
            self.cursor += 1;
        }
        self.committed_at = now;
        self.scroll_to_cursor = true;
        self.trim();
    }

    /// Drops the oldest states while over the entry or memory budget.
    fn trim(&mut self) {
        let mut weight: usize = self.states.iter().map(|s| s.weight).sum();
        let mut drop_n = 0;
        while drop_n < self.cursor && (self.states.len() - drop_n > MAX_STATES || weight > MAX_WEIGHT) {
            weight -= self.states[drop_n].weight;
            drop_n += 1;
        }
        if drop_n > 0 {
            let gone: Vec<State> = self.states.drain(..drop_n).collect();
            self.cursor -= drop_n;
            // Freeing big circuits can take a while: not on the UI thread.
            if gone.iter().map(|s| s.weight).sum::<usize>() > 20_000 {
                let _ = std::thread::Builder::new().name("bw-history-drop".into()).spawn(move || drop(gone));
            }
        }
    }

    fn state_circuit(&self, i: usize) -> Option<Arc<Circuit>> {
        self.states.get(i).map(|s| Arc::clone(&s.circuit))
    }
}

impl BitwerkApp {
    /// Undo the last action.
    pub fn undo(&mut self) {
        self.history_commit();
        if self.history.cursor > 0 {
            let to = self.history.cursor - 1;
            let what = self.history.states[self.history.cursor].label.clone();
            self.history_goto(to);
            self.status = format!("Undid: {what}");
        } else {
            self.status = "Nothing to undo".into();
        }
    }

    /// Redo the action that was undone.
    pub fn redo(&mut self) {
        self.history_commit();
        if self.history.cursor + 1 < self.history.states.len() {
            let to = self.history.cursor + 1;
            self.history_goto(to);
            let what = self.history.states[to].label.clone();
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

    /// Makes state `idx` the current circuit.
    ///
    /// The state replaces the root circuit wholesale: the simulator reloads
    /// it (run state is kept, flip-flop state restarts) and the scene is
    /// rebuilt. Macro levels that were open are re-entered as far as their
    /// definitions and instances still exist.
    pub fn history_goto(&mut self, idx: usize) {
        self.history_commit();
        let Some(target) = self.history.state_circuit(idx) else { return };
        if idx == self.history.cursor {
            return;
        }
        let was_inside: Vec<(MacroId, Option<ComponentId>)> = self.levels.iter().map(|l| (l.def, l.inst)).collect();
        let level_camera = self.camera;

        self.drop_levels();
        self.history.cursor = idx;
        self.history.scroll_to_cursor = true;
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

    /// The History panel: one entry per state, newest at the bottom, with a
    /// thumbnail of the circuit as it was. Click an entry to go there.
    pub fn history_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("History");
            if ui.add_enabled(self.history.can_undo(), egui::Button::new("↶")).on_hover_text("Undo").clicked() {
                self.undo();
            }
            if ui.add_enabled(self.history.can_redo(), egui::Button::new("↷")).on_hover_text("Redo").clicked() {
                self.redo();
            }
        });
        let n = self.history.states.len();
        ui.weak(format!("{} step{} · current: {}", n - 1, if n == 2 { "" } else { "s" }, self.history.cursor));
        ui.separator();
        let dark = ui.visuals().dark_mode;
        let scroll_to = std::mem::take(&mut self.history.scroll_to_cursor);
        let mut jump = None;
        let cursor = self.history.cursor;
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if scroll_to {
            // Rows outside the viewport are not built, so scroll by offset.
            #[allow(clippy::cast_precision_loss, reason = "a few hundred rows at most")]
            let y = cursor as f32 * ROW_HEIGHT - (ui.available_height() - ROW_HEIGHT) * 0.5;
            area = area.vertical_scroll_offset(y.max(0.0));
        }
        area.show_rows(ui, ROW_HEIGHT, n, |ui, rows| {
            for i in rows {
                let state = &self.history.states[i];
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
                let future = i > cursor;
                let painter = ui.painter_at(rect);
                if i == cursor {
                    painter.rect_filled(rect, 4.0, ui.visuals().selection.bg_fill);
                } else if resp.hovered() {
                    painter.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
                }
                let thumb_rect =
                    Rect::from_min_size(rect.min + Vec2::new(4.0, 4.0), THUMB_SIZE.min(rect.size() - Vec2::splat(8.0)));
                let thumb = state.thumb.get_or_init(|| Thumb::build(&state.circuit));
                let mut thumb_painter = painter.clone();
                if future {
                    thumb_painter.multiply_opacity(0.45);
                }
                thumb.paint(&thumb_painter, thumb_rect, dark);
                let text_color = if future { ui.visuals().weak_text_color() } else { ui.visuals().text_color() };
                let caption = if i == 0 { state.label.clone() } else { format!("{i}. {}", state.label) };
                painter.text(
                    Pos2::new(thumb_rect.left(), thumb_rect.bottom() + 3.0),
                    egui::Align2::LEFT_TOP,
                    caption,
                    egui::FontId::proportional(13.0),
                    text_color,
                );
                if resp.clicked() {
                    jump = Some(i);
                }
            }
        });
        if let Some(i) = jump {
            self.history_goto(i);
            self.status = format!("Went to step {i}: {}", self.history.states[i].label);
        }
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn timeline_undo_redo_and_discard() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        assert!(!h.can_undo() && !h.can_redo());

        add(&mut c, &mut h, 0);
        assert!(h.can_undo());
        h.commit(&c);
        add(&mut c, &mut h, 10);
        h.commit(&c);
        assert_eq!((h.states.len(), h.cursor), (3, 2));
        assert_eq!(h.states[2].circuit.component_count(), 2);

        // Going back keeps the future; a new action discards it.
        h.cursor = 1;
        assert!(h.can_redo());
        c = h.state_circuit(1).expect("state exists");
        add(&mut c, &mut h, 20);
        h.commit(&c);
        assert_eq!((h.states.len(), h.cursor), (3, 2));
        assert!(!h.can_redo());
    }

    #[test]
    fn several_edits_in_one_pass_are_one_entry() {
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        add(&mut c, &mut h, 0);
        add(&mut c, &mut h, 10);
        add(&mut c, &mut h, 20);
        h.commit(&c);
        assert_eq!(h.states.len(), 2);
        assert_eq!(h.states[1].label, "Add component (×3)");
    }

    #[test]
    fn budget_drops_oldest_but_never_the_current() {
        const { assert!(MAX_STATES < 220) };
        let mut c = Arc::new(Circuit::new());
        let mut h = History::new(Arc::clone(&c));
        for i in 0..220 {
            add(&mut c, &mut h, i * 10);
            h.commit(&c);
        }
        assert_eq!(h.states.len(), MAX_STATES);
        assert_eq!(h.cursor, MAX_STATES - 1);
        assert_eq!(h.states[h.cursor].circuit.component_count(), 220);
    }

    #[test]
    fn thumbnail_covers_components_and_handles_empty() {
        let empty = Thumb::build(&Circuit::new());
        assert!(empty.hi.x > empty.lo.x && empty.boxes.is_empty());
        let mut c = Circuit::new();
        c.apply(&Edit::AddComponent {
            kind: ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle },
            pos: GridPos::new(5, 7),
            rotation: Rotation::R0,
        })
        .expect("edit applies");
        let t = Thumb::build(&c);
        assert_eq!(t.boxes.len(), 1);
        assert!(t.lo.x <= 5.0 && t.lo.y <= 7.0);
    }
}
