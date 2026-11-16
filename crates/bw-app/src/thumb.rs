//! Thumbnails of circuit snapshots for the History panel, drawn with the egui
//! painter straight from the snapshot: wires as lines, components with their
//! real symbols (the same meshes as the canvas, in every rotation; macros
//! with their definition's symbol).
//!
//! The geometry is computed once per state and capped at [`THUMB_ITEMS`]
//! items so that a 100k-component circuit stays cheap; the shapes of the last
//! size painted are cached.

use std::cell::RefCell;
use std::sync::Arc;

use bw_core::{Circuit, ComponentKind, MacroShape, Rotation};
use bw_render::symbols::{SymbolKind, SymbolStyle};
use egui::{Color32, Pos2, Rect, Shape, Stroke, Vec2};

/// Items (boxes or wires) a thumbnail draws at most; more are sampled.
const THUMB_ITEMS: usize = 3000;

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
pub(crate) struct Thumb {
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
    pub(crate) fn build(c: &Circuit) -> Self {
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
    pub(crate) fn paint(&self, painter: &egui::Painter, rect: Rect, style: SymbolStyle, dark: bool) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bw_core::{ComponentKind, Edit, GridPos, Rotation};

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
