//! Symbol icons for the palette (and the placement ghost), painted with
//! egui from the very same tessellated meshes the GPU canvas draws
//! (`bw_render::symbols::atlas`): one source of truth, no image files.
//! A symbol is a few hundred vertices, so painting them as an egui mesh
//! whenever the UI repaints is cheaper than managing textures.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use bw_render::symbols::{self, Role, SymbolKind, SymbolStyle, Width};
use egui::{Color32, Pos2, Rect, Sense, Stroke};

/// 0..1 → 0..255.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to 0..=255 first")]
fn byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Paints `kind` with a local→screen mapping and `scale` screen points per
/// grid unit. `level` picks the state shown (0 low, 1 high, 2 unknown).
pub fn paint_with(
    painter: &egui::Painter,
    kind: SymbolKind,
    style: SymbolStyle,
    level: u8,
    opacity: f32,
    scale: f32,
    to_screen: impl Fn([f32; 2]) -> Pos2,
) {
    let mesh = build_mesh(kind, style, level, opacity, scale, to_screen);
    if !mesh.is_empty() {
        painter.add(egui::Shape::mesh(mesh));
    }
}

fn build_mesh(
    kind: SymbolKind,
    style: SymbolStyle,
    level: u8,
    opacity: f32,
    scale: f32,
    to_screen: impl Fn([f32; 2]) -> Pos2,
) -> egui::Mesh {
    let mesh = symbols::atlas().mesh(kind, style);
    let (accent, ink) = (kind.accent(), kind.ink());
    // Displays: the level lights the segments / digits, not the body.
    let display = matches!(kind, SymbolKind::SevenSegment | SymbolKind::Numeric(..));
    let hw = |w: Width| w.half_width(scale);
    let mut out = egui::Mesh::default();
    out.vertices.reserve(mesh.len());
    for tri in mesh.chunks_exact(3) {
        if !tri[0].layer().shows(level) {
            continue;
        }
        let role = tri[0].role();
        let lvl = if display && role == Role::Fill { 2 } else { level };
        let [r, g, b] = symbols::shade_ink(role, accent, ink, lvl).map(byte);
        let color = Color32::from_rgba_unmultiplied(r, g, b, byte(opacity));
        for v in tri {
            let idx = u32::try_from(out.vertices.len()).unwrap_or(u32::MAX);
            out.vertices.push(egui::epaint::Vertex {
                pos: to_screen(v.at(hw(v.width()))),
                uv: egui::epaint::WHITE_UV,
                color,
            });
            out.indices.push(idx);
        }
    }
    out
}

type IconKey = (SymbolKind, SymbolStyle, u8, u8, [i32; 4]);

thread_local! {
    /// Built palette icon meshes (UI thread only). The palette does not
    /// move, so steady-state frames only clone an `Arc` per icon.
    static ICON_CACHE: RefCell<HashMap<IconKey, Arc<egui::Mesh>>> = RefCell::new(HashMap::new());
}

/// Paints `kind` fitted (centred, aspect kept) into `rect`; cached.
pub fn paint_fit(painter: &egui::Painter, rect: Rect, kind: SymbolKind, style: SymbolStyle, level: u8, opacity: f32) {
    #[allow(clippy::cast_possible_truncation, reason = "cache key: rect in quarter points")]
    let q = |v: f32| (v * 4.0).round() as i32;
    let key = (kind, style, level, byte(opacity), [q(rect.min.x), q(rect.min.y), q(rect.max.x), q(rect.max.y)]);
    let mesh = ICON_CACHE.with_borrow_mut(|cache| {
        if cache.len() > 1024 {
            cache.clear();
        }
        Arc::clone(cache.entry(key).or_insert_with(|| Arc::new(fit_mesh(rect, kind, style, level, opacity))))
    });
    if !mesh.is_empty() {
        painter.add(egui::Shape::Mesh(mesh));
    }
}

fn fit_mesh(rect: Rect, kind: SymbolKind, style: SymbolStyle, level: u8, opacity: f32) -> egui::Mesh {
    let Some((lo, hi)) = symbols::atlas().bounds(kind, style) else { return egui::Mesh::default() };
    let ext = egui::vec2((hi[0] - lo[0]).max(0.5), (hi[1] - lo[1]).max(0.5));
    let inner = rect.shrink(3.0);
    // Cap the scale so tiny symbols (junction) do not balloon.
    let scale = (inner.width() / ext.x).min(inner.height() / ext.y).min(14.0);
    let mid = egui::vec2((lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5);
    let c = rect.center();
    build_mesh(kind, style, level, opacity, scale, |p| c + (egui::vec2(p[0], p[1]) - mid) * scale)
}

/// State shown by a palette icon: lit LED, clock, sequence, displays and
/// VCC (high output), ground low, open switches, neutral pins elsewhere.
pub fn palette_level(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Led
        | SymbolKind::Clock
        | SymbolKind::SevenSegment
        | SymbolKind::Numeric(..)
        | SymbolKind::SequenceGenerator
        | SymbolKind::Vcc => 1,
        SymbolKind::Ground => 0,
        _ => 2,
    }
}

/// A palette button: the symbol with a short caption, a hover tooltip and a
/// selection highlight. Disabled buttons are dimmed and only show their
/// tooltip.
pub fn button(
    ui: &mut egui::Ui,
    kind: SymbolKind,
    style: SymbolStyle,
    caption: &str,
    tooltip: &str,
    selected: bool,
    enabled: bool,
) -> egui::Response {
    let size = egui::vec2(48.0, 50.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&resp, selected);
        let painter = ui.painter();
        if selected || (enabled && resp.hovered()) {
            painter.rect(rect, 4.0, visuals.weak_bg_fill, visuals.bg_stroke, egui::StrokeKind::Inside);
        } else {
            painter.rect_stroke(rect, 4.0, Stroke::new(1.0, ui.visuals().faint_bg_color), egui::StrokeKind::Inside);
        }
        let icon = Rect::from_min_max(rect.min + egui::vec2(2.0, 2.0), egui::pos2(rect.max.x - 2.0, rect.max.y - 14.0));
        paint_fit(painter, icon, kind, style, palette_level(kind), if enabled { 1.0 } else { 0.4 });
        let text_color = if enabled { visuals.text_color() } else { ui.visuals().weak_text_color() };
        painter.text(
            egui::pos2(rect.center().x, rect.max.y - 7.0),
            egui::Align2::CENTER_CENTER,
            caption,
            egui::FontId::proportional(9.5),
            text_color,
        );
    }
    resp.on_hover_text(tooltip)
}

type MacroMeshCache = HashMap<(u64, bw_core::MacroShape), Arc<Vec<symbols::SymVertex>>>;

thread_local! {
    /// Tessellated macro symbols for previews, by (definition hash, shape).
    static MACRO_MESHES: RefCell<MacroMeshCache> =
        RefCell::new(HashMap::new());
}

/// The tessellated symbol of a macro definition (cached).
pub fn macro_mesh(def: &bw_core::MacroDef, shape: &bw_core::MacroShape, hash: u64) -> Arc<Vec<symbols::SymVertex>> {
    MACRO_MESHES.with_borrow_mut(|cache| {
        if cache.len() > 256 {
            cache.clear();
        }
        Arc::clone(
            cache.entry((hash, *shape)).or_insert_with(|| Arc::new(symbols::macro_symbol(def, shape).tessellate())),
        )
    })
}

/// Paints a macro symbol fitted into `rect` (previews, placement ghost).
pub fn paint_macro_fit(
    painter: &egui::Painter,
    rect: Rect,
    verts: &[symbols::SymVertex],
    size: [f32; 2],
    opacity: f32,
) {
    let inner = rect.shrink(6.0);
    let scale = (inner.width() / size[0].max(1.0)).min(inner.height() / size[1].max(1.0)).min(28.0);
    let origin = rect.center() - egui::vec2(size[0], size[1]) * scale * 0.5;
    paint_vertices(painter, verts, scale, opacity, |p| origin + egui::vec2(p[0], p[1]) * scale);
}

/// Paints symbol vertices with a local→screen mapping (`scale` points per
/// grid unit), macro accent colours, neutral state.
pub fn paint_vertices(
    painter: &egui::Painter,
    verts: &[symbols::SymVertex],
    scale: f32,
    opacity: f32,
    to_screen: impl Fn([f32; 2]) -> Pos2,
) {
    let accent = SymbolKind::Macro.accent();
    let level = 2;
    let hw = |w: Width| w.half_width(scale);
    let mut out = egui::Mesh::default();
    out.vertices.reserve(verts.len());
    for tri in verts.chunks_exact(3) {
        if !tri[0].layer().shows(level) {
            continue;
        }
        let [r, g, b] = symbols::shade(tri[0].role(), accent, level).map(byte);
        let color = Color32::from_rgba_unmultiplied(r, g, b, byte(opacity));
        for v in tri {
            let idx = u32::try_from(out.vertices.len()).unwrap_or(u32::MAX);
            out.vertices.push(egui::epaint::Vertex {
                pos: to_screen(v.at(hw(v.width()))),
                uv: egui::epaint::WHITE_UV,
                color,
            });
            out.indices.push(idx);
        }
    }
    if !out.is_empty() {
        painter.add(egui::Shape::mesh(out));
    }
}
