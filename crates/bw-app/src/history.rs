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

use std::cell::{OnceCell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bw_core::{Circuit, ComponentId, ComponentKind, Edit, MacroId, MacroShape, Rotation};
use bw_render::symbols::{SymbolKind, SymbolStyle};
use egui::{Color32, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::app::BitwerkApp;

/// Most entries kept (including the initial state).
const MAX_STATES: usize = 200;
/// Budget of retained (components + wire segments) summed over all states.
const MAX_WEIGHT: usize = 1_500_000;
/// Typing-like edits closer together than this share one entry.
const COALESCE: Duration = Duration::from_secs(2);
/// Items (boxes or wires) a thumbnail draws at most; more are sampled.
const THUMB_ITEMS: usize = 3000;
/// Size of the thumbnail in the hover tooltip.
const PREVIEW_SIZE: Vec2 = Vec2::new(420.0, 280.0);

/// A component of a thumbnail: where it is and how to draw its symbol.
struct Item {
    /// Top-left of the (rotated) bounds.
    min: (i32, i32),
    /// Unrotated size.
    size: (i32, i32),
    rot: Rotation,
    sym: SymbolKind,
    /// Macro instances draw their definition's symbol.
    mac: Option<(Arc<bw_core::MacroDef>, MacroShape, u64)>,
}

/// Pre-computed geometry of a state, in grid units, and the shapes of the
/// last size it was painted at.
struct Thumb {
    lo: Pos2,
    hi: Pos2,
    items: Vec<Item>,
    /// Wire segments `(x1, y1, x2, y2)`.
    wires: Vec<(i32, i32, i32, i32)>,
    /// Shapes relative to the rectangle's top-left, for `(size, style, dark)`.
    cache: RefCell<Option<(CacheKey, Arc<Vec<Shape>>)>>,
}

type CacheKey = ([i32; 2], SymbolStyle, bool);

/// Symbols drawn as real symbols (the rest become plain boxes).
const THUMB_SYMBOLS: usize = 600;

impl Thumb {
    fn build(c: &Circuit) -> Self {
        let mut lo = (i32::MAX, i32::MAX);
        let mut hi = (i32::MIN, i32::MIN);
        let mut grow = |x: i32, y: i32| {
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        };
        let stride_c = c.component_count().div_ceil(THUMB_ITEMS).max(1);
        let mut items = Vec::new();
        for (i, comp) in c.components().enumerate() {
            let b = comp.bounds();
            grow(b.min.x, b.min.y);
            grow(b.max.x, b.max.y);
            if i % stride_c == 0 {
                let mac = match comp.kind {
                    ComponentKind::Macro(shape) => {
                        c.macros().get(shape.def).map(|e| (Arc::clone(&e.def), shape, e.hash))
                    }
                    _ => None,
                };
                items.push(Item {
                    min: (b.min.x, b.min.y),
                    size: comp.kind.size(),
                    rot: comp.rotation,
                    sym: SymbolKind::of(&comp.kind),
                    mac,
                });
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
        Self { lo: p(lo), hi: p(hi), items, wires, cache: RefCell::new(None) }
    }

    /// Paints the snapshot fitted into `rect` with the real symbols.
    fn paint(&self, painter: &egui::Painter, rect: Rect, style: SymbolStyle, dark: bool) {
        #[allow(clippy::cast_possible_truncation, reason = "a panel size in points")]
        let key = ([rect.width().round() as i32, rect.height().round() as i32], style, dark);
        let shapes = {
            let mut cache = self.cache.borrow_mut();
            if let Some((_, s)) = cache.as_ref().filter(|(k, _)| *k == key) {
                Arc::clone(s)
            } else {
                let s = Arc::new(self.shapes(Rect::from_min_size(Pos2::ZERO, rect.size()), style, dark));
                *cache = Some((key, Arc::clone(&s)));
                s
            }
        };
        let painter = painter.with_clip_rect(rect);
        for shape in shapes.iter() {
            let mut shape = shape.clone();
            shape.translate(rect.min.to_vec2());
            painter.add(shape);
        }
    }

    /// Background, wires and components fitted into `rect`.
    fn shapes(&self, rect: Rect, style: SymbolStyle, dark: bool) -> Vec<Shape> {
        let (bg, wire, body) = if dark {
            (Color32::from_gray(22), Color32::from_rgb(90, 190, 120), Color32::from_gray(170))
        } else {
            (Color32::from_gray(246), Color32::from_rgb(30, 140, 70), Color32::from_gray(80))
        };
        let mut out = vec![Shape::rect_filled(rect, 3.0, bg)];
        let inner = rect.shrink(8.0);
        let ext = (self.hi - self.lo).max(Vec2::splat(1.0));
        let scale = (inner.width() / ext.x).min(inner.height() / ext.y).min(24.0);
        let off = inner.center().to_vec2() - ((self.lo.to_vec2() + self.hi.to_vec2()) * 0.5) * scale;
        #[allow(clippy::cast_precision_loss, reason = "grid coordinates are far below 2^24 in practice")]
        let map = |x: f32, y: f32| Pos2::new(x * scale + off.x, y * scale + off.y);
        let stroke = Stroke::new((scale * 0.12).clamp(0.6, 1.4), wire);
        #[allow(clippy::cast_precision_loss, reason = "grid coordinates are far below 2^24 in practice")]
        for &(x1, y1, x2, y2) in &self.wires {
            out.push(Shape::line_segment([map(x1 as f32, y1 as f32), map(x2 as f32, y2 as f32)], stroke));
        }
        let detailed = self.items.len() <= THUMB_SYMBOLS;
        for it in &self.items {
            #[allow(clippy::cast_precision_loss, reason = "grid coordinates are far below 2^24 in practice")]
            let (mx, my, w, h) = (it.min.0 as f32, it.min.1 as f32, it.size.0 as f32, it.size.1 as f32);
            let (rw, rh) = if matches!(it.rot, Rotation::R90 | Rotation::R270) { (h, w) } else { (w, h) };
            let box_rect = Rect::from_min_max(map(mx, my), map(mx + rw, my + rh));
            if !detailed || scale * w.min(h) < 4.0 {
                let r = if box_rect.width() < 1.5 || box_rect.height() < 1.5 {
                    Rect::from_center_size(box_rect.center(), Vec2::splat(1.5))
                } else {
                    box_rect
                };
                out.push(Shape::rect_filled(r, 0.0, body.gamma_multiply(0.8)));
                continue;
            }
            // Same mapping as the canvas shader (`sym_to_world`).
            let to_screen = |p: [f32; 2]| {
                let r = match it.rot {
                    Rotation::R0 => p,
                    Rotation::R90 => [h - p[1], p[0]],
                    Rotation::R180 => [w - p[0], h - p[1]],
                    Rotation::R270 => [p[1], w - p[0]],
                };
                map(mx + r[0], my + r[1])
            };
            let mesh = match &it.mac {
                Some((def, shape, hash)) => {
                    let verts = crate::icons::macro_mesh(def, shape, *hash);
                    crate::icons::vertices_mesh(&verts, scale, 1.0, to_screen)
                }
                None => crate::icons::build_mesh(it.sym, style, 2, 1.0, scale, to_screen),
            };
            if !mesh.is_empty() {
                out.push(Shape::mesh(mesh));
            }
        }
        out.push(Shape::rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_gray(110)), egui::StrokeKind::Inside));
        out
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

    /// The History panel: one compact row per state (`#3  Move component`),
    /// like the other panels. Hovering a row previews the circuit as it was
    /// then; clicking it goes there.
    pub fn history_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("History");
        let n = self.history.states.len();
        let cursor = self.history.cursor;
        ui.horizontal(|ui| {
            if ui.add_enabled(self.history.can_undo(), egui::Button::new("Undo")).clicked() {
                self.undo();
            }
            if ui.add_enabled(self.history.can_redo(), egui::Button::new("Redo")).clicked() {
                self.redo();
            }
        });
        ui.label(format!("{} step{} · at #{cursor}", n - 1, if n == 2 { "" } else { "s" }));
        ui.separator();
        let dark = ui.visuals().dark_mode;
        let style = self.style;
        let row_h = ui.spacing().interact_size.y;
        let scroll_to = std::mem::take(&mut self.history.scroll_to_cursor);
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if scroll_to {
            // Rows outside the viewport are not built, so scroll by offset.
            #[allow(clippy::cast_precision_loss, reason = "a few hundred rows at most")]
            let y = cursor as f32 * (row_h + ui.spacing().item_spacing.y) - ui.available_height() * 0.5;
            area = area.vertical_scroll_offset(y.max(0.0));
        }
        let mut jump = None;
        area.show_rows(ui, row_h, n, |ui, rows| {
            for i in rows {
                let state = &self.history.states[i];
                let text = format!("#{i}  {}", state.label);
                let text = if i > cursor { egui::RichText::new(text).weak() } else { egui::RichText::new(text) };
                let resp =
                    ui.add_sized([ui.available_width(), row_h], egui::Button::selectable(i == cursor, text).truncate());
                resp.clone().on_hover_ui(|ui| {
                    ui.label(format!("#{i}  {}", state.label));
                    let (rect, _) = ui.allocate_exact_size(PREVIEW_SIZE, Sense::hover());
                    let thumb = state.thumb.get_or_init(|| Thumb::build(&state.circuit));
                    thumb.paint(ui.painter(), rect, style, dark);
                });
                if resp.clicked() {
                    jump = Some(i);
                }
            }
        });
        if let Some(i) = jump {
            self.history_goto(i);
            self.status = format!("Went to #{i}: {}", self.history.states[i].label);
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
        assert!(empty.hi.x > empty.lo.x && empty.items.is_empty());
        let mut c = Circuit::new();
        c.apply(&Edit::AddComponent {
            kind: ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle },
            pos: GridPos::new(5, 7),
            rotation: Rotation::R0,
        })
        .expect("edit applies");
        let t = Thumb::build(&c);
        assert_eq!(t.items.len(), 1);
        assert!(t.lo.x <= 5.0 && t.lo.y <= 7.0);
    }

    #[test]
    fn thumbnail_draws_real_symbols_in_every_rotation() {
        let mut c = Circuit::new();
        for (i, rot) in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270].into_iter().enumerate() {
            let x = i32::try_from(i).expect("small") * 8;
            c.apply(&Edit::AddComponent {
                kind: ComponentKind::Gate { op: bw_core::GateOp::Or, inputs: 2 },
                pos: GridPos::new(x, 0),
                rotation: rot,
            })
            .expect("edit applies");
        }
        let t = Thumb::build(&c);
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0));
        let shapes = t.shapes(rect, SymbolStyle::Ansi, true);
        let meshes: Vec<_> =
            shapes.iter().filter_map(|s| if let Shape::Mesh(m) = s { Some(m) } else { None }).collect();
        assert_eq!(meshes.len(), 4, "one symbol mesh per gate");
        for m in meshes {
            assert!(!m.is_empty());
            // Inside the thumbnail.
            assert!(m.vertices.iter().all(|v| rect.expand(2.0).contains(v.pos)), "symbol stays in the frame");
        }
    }
}
