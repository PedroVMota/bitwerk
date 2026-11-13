//! The circuit canvas: egui-wgpu paint callback + mouse interaction.

use std::sync::Arc;

use bw_core::{ComponentId, ComponentKind, GridPos, NodeId, PinDir, PinRef, SegmentId, WireEnd};
use bw_render::{CanvasRenderer, FrameView, Item, NetLevels, SymbolKind};
use bw_sim::{Command, Snapshot};
use egui::{Color32, Pos2, Rect, Sense, Stroke};
use glam::Vec2;

use crate::app::{BitwerkApp, Tool};

/// In-progress drag with the Select tool (applied as one edit on release).
#[derive(Debug, Clone, Copy)]
pub enum DragState {
    Component {
        id: ComponentId,
        /// Grab offset (world) from the component origin.
        grab: Vec2,
        to: GridPos,
    },
    /// A junction or bend.
    Node { id: NodeId, to: GridPos },
    /// A segment, sideways (perpendicular to itself).
    Segment { id: SegmentId, from: GridPos, offset: i32 },
}

/// A wire being drawn with the Wire tool.
#[derive(Debug, Clone)]
pub struct WireDraft {
    pub start: WireEnd,
    pub start_pos: GridPos,
    /// Bend points clicked so far.
    pub bends: Vec<GridPos>,
    /// The L of the leg under the cursor goes vertical first ('/' or Space).
    pub vertical_first: bool,
}

impl WireDraft {
    fn last(&self) -> GridPos {
        self.bends.last().copied().unwrap_or(self.start_pos)
    }
}

/// Corner of the L from `a` to `b` (none if they are aligned).
fn corner(a: GridPos, b: GridPos, vertical_first: bool) -> Option<GridPos> {
    if a.x == b.x || a.y == b.y {
        None
    } else if vertical_first {
        Some(GridPos::new(a.x, b.y))
    } else {
        Some(GridPos::new(b.x, a.y))
    }
}

/// Simulator snapshot as seen by the renderer.
pub struct SnapshotLevels<'a>(pub &'a Arc<Snapshot>);

impl NetLevels for SnapshotLevels<'_> {
    fn key(&self) -> u64 {
        self.0.generation
    }
    fn compile_key(&self) -> u64 {
        self.0.compile_id
    }
    fn levels(&self) -> &[u32] {
        &self.0.values
    }
    fn net_count(&self) -> usize {
        self.0.net_count
    }
    fn driver_start(&self) -> &[u32] {
        &self.0.net_map.start
    }
    fn driver_pins(&self) -> &[(ComponentId, u8)] {
        &self.0.net_map.pins
    }
}

/// Paint callback. All uploads already happened in `ui` (see
/// [`BitwerkApp::prepare_canvas`]); here we only submit the GPU culling pass
/// (when the lists must be recomputed) and record the draws.
struct CanvasCallback;

impl egui_wgpu::CallbackTrait for CanvasCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        // Submitted right away rather than into egui's encoder, which egui
        // drops unsubmitted when it gets no surface texture this frame.
        if let Some(r) = resources.get_mut::<CanvasRenderer>() {
            r.submit_culling(device, queue);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(r) = resources.get::<CanvasRenderer>() {
            r.paint(pass);
        }
    }
}

fn v(p: egui::Vec2) -> Vec2 {
    Vec2::new(p.x, p.y)
}

#[allow(clippy::cast_possible_truncation)]
fn snap(w: Vec2) -> GridPos {
    GridPos::new(w.x.round() as i32, w.y.round() as i32)
}

#[allow(clippy::cast_precision_loss)]
fn gp(p: GridPos) -> Vec2 {
    Vec2::new(p.x as f32, p.y as f32)
}

/// Zoom (wheel, pinch, Cmd/Ctrl+scroll) around the cursor and pan (trackpad
/// scroll, middle/right drag). Only the camera changes — nothing is rebuilt.
fn navigate(app: &mut BitwerkApp, ui: &egui::Ui, resp: &egui::Response, rect: Rect) {
    let viewport = v(rect.size());
    let hover = resp.hover_pos().map(|p| v(p - rect.min));
    if let Some(cursor) = hover {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match ev {
                egui::Event::MouseWheel { unit, delta, modifiers, .. } => {
                    let trackpad_pan = unit == egui::MouseWheelUnit::Point && !modifiers.command && !modifiers.ctrl;
                    if trackpad_pan {
                        app.camera.pan_by(v(delta));
                    } else {
                        let step = match unit {
                            egui::MouseWheelUnit::Point => delta.y * 0.0025,
                            egui::MouseWheelUnit::Line => delta.y * 0.12,
                            egui::MouseWheelUnit::Page => delta.y * 0.5,
                        };
                        app.camera.zoom_at(cursor, step.exp(), viewport);
                    }
                }
                egui::Event::Zoom(f) => app.camera.zoom_at(cursor, f, viewport),
                _ => {}
            }
        }
    }
    let pan_button = resp.dragged_by(egui::PointerButton::Middle) || resp.dragged_by(egui::PointerButton::Secondary);
    if pan_button {
        app.camera.pan_by(v(resp.drag_delta()));
    }
}

/// Picking tolerance in world units: about 6 points on screen.
fn reach(app: &BitwerkApp) -> f32 {
    (6.0 / app.camera.zoom.max(1e-3)).clamp(0.2, 3.0)
}

/// Draws the canvas and handles its input. Returns the canvas rect.
#[allow(clippy::too_many_lines, reason = "one input handler per gesture")]
pub fn canvas_ui(app: &mut BitwerkApp, ui: &mut egui::Ui, snap_shot: &Arc<Snapshot>) -> Rect {
    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let viewport = v(rect.size());
    let local = |p: Pos2| v(p - rect.min);
    let hover = resp.hover_pos().map(local);

    navigate(app, ui, &resp, rect);
    if app.tool != Tool::Wire {
        app.wire_draft = None;
    }

    let world_at = |cam: &bw_render::Camera, p: Vec2| cam.screen_to_world(p, viewport);

    // ---- primary-button interaction ----------------------------------
    let shift = ui.input(|i| i.modifiers.shift);
    if resp.drag_started_by(egui::PointerButton::Primary) && app.tool == Tool::Select {
        if let Some(p) = resp.interact_pointer_pos().map(local) {
            let w = world_at(&app.camera, p);
            app.drag = if shift { None } else { start_drag(app, w) };
            if app.drag.is_none() {
                // Dragging empty space draws a selection box.
                app.box_select = Some((w, w));
            }
        }
    }
    if resp.dragged_by(egui::PointerButton::Primary) {
        let w = resp.interact_pointer_pos().map(|p| app.camera.screen_to_world(local(p), viewport));
        let seg_h = match app.drag {
            Some(DragState::Segment { id, .. }) => app.shown().segment_ends(id).is_some_and(|(a, b)| a.y == b.y),
            _ => false,
        };
        match (app.drag.as_mut(), w) {
            (Some(DragState::Component { grab, to, .. }), Some(w)) => *to = snap(w - *grab),
            (Some(DragState::Node { to, .. }), Some(w)) => *to = snap(w),
            (Some(DragState::Segment { from, offset, .. }), Some(w)) => {
                let p = snap(w);
                *offset = if seg_h { p.y - from.y } else { p.x - from.x };
            }
            (None, Some(w)) => {
                if let Some(b) = app.box_select.as_mut() {
                    b.1 = w;
                } else {
                    app.camera.pan_by(v(resp.drag_delta()));
                }
            }
            (Some(_) | None, None) => {}
        }
    }
    if resp.drag_stopped() {
        if let Some(d) = app.drag.take() {
            finish_drag(app, d);
        }
        if let Some((a, b)) = app.box_select.take() {
            finish_box(app, a, b, shift);
        }
    }
    if resp.double_clicked() && matches!(app.tool, Tool::Select | Tool::Interact) {
        // Double-click a macro instance: open it in place.
        if let Some(p) = resp.interact_pointer_pos().map(local) {
            if let Some(id) = pick_component(app, world_at(&app.camera, p)) {
                if matches!(app.shown().component(id).map(|c| c.kind), Some(ComponentKind::Macro(_))) {
                    app.enter_instance(id);
                    return rect;
                }
            }
        }
    } else if resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos().map(local) {
            let w = world_at(&app.camera, p);
            if shift && app.tool == Tool::Select {
                if let Some(id) = pick_component(app, w) {
                    if !app.multi.remove(&id) {
                        app.multi.insert(id);
                        if let Some(s) = app.selection.take() {
                            app.multi.insert(s);
                        }
                    }
                }
            } else {
                app.multi.clear();
                click(app, w);
            }
        }
    }
    // Context menu: Edit Macro, Create Macro from Selection, Up one level.
    if resp.secondary_clicked() {
        app.ctx_target =
            resp.interact_pointer_pos().map(local).and_then(|p| pick_component(app, world_at(&app.camera, p)));
    }
    resp.context_menu(|ui| context_menu(app, ui));
    // A part dragged from the Parts Centre and dropped here is placed.
    if let Some(part) = resp.dnd_release_payload::<crate::parts::PartRef>() {
        if let (Some(kind), Some(p)) = (app.place_part(&part), resp.hover_pos().map(local)) {
            let (sw, sh) = kind.size();
            let origin = snap(world_at(&app.camera, p) - gp(GridPos::new(sw, sh)) * 0.5);
            match app.edit_place(kind, origin) {
                Ok(id) => {
                    app.selection = Some(id);
                    app.tool = Tool::Select;
                }
                Err(e) => app.status = e.to_string(),
            }
        }
    }
    // Hovered wire → its whole net is highlighted (cached per revision).
    let hovered_seg = hover
        .filter(|_| app.drag.is_none() && matches!(app.tool, Tool::Select | Tool::Wire))
        .and_then(|c| app.scene.pick_wire(world_at(&app.camera, c), reach(app)))
        .filter(|&s| app.shown().segment(s).is_some());
    update_hover_net(app, hovered_seg);

    // ---- GPU canvas --------------------------------------------------
    let view =
        FrameView { camera: app.camera, viewport_points: viewport, pixels_per_point: ui.ctx().pixels_per_point() };
    app.prepare_canvas(&view, snap_shot);
    ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, CanvasCallback));

    draw_overlays(app, ui, rect, hover, snap_shot);
    rect
}

/// What a Select-tool drag grabs: a junction / bend, a component, a wire
/// segment (to move sideways), or nothing (pan).
fn start_drag(app: &mut BitwerkApp, w: Vec2) -> Option<DragState> {
    let r = reach(app);
    if let Some((id, pos)) = pick_node(app, w, r, true) {
        app.selection = None;
        app.selected_segment = None;
        return Some(DragState::Node { id, to: pos });
    }
    if let Some(id) = pick_component(app, w) {
        let pos = app.shown().component(id)?.pos;
        app.selection = Some(id);
        app.selected_segment = None;
        return Some(DragState::Component { id, grab: w - gp(pos), to: pos });
    }
    let seg = app.scene.pick_wire(w, r)?;
    let (a, b) = app.shown().segment_ends(seg)?;
    app.selection = None;
    app.selected_segment = Some(seg);
    (a != b).then(|| DragState::Segment { id: seg, from: snap(w), offset: 0 })
}

/// Selects every component touching the box (Shift adds to the selection).
fn finish_box(app: &mut BitwerkApp, a: Vec2, b: Vec2, add: bool) {
    let (lo, hi) = (a.min(b), a.max(b));
    if (hi - lo).length() < 0.3 {
        return;
    }
    if !add {
        app.clear_selection();
    }
    let mut found = Vec::new();
    crate::sweep::bodies_in(&app.scene, lo, hi, |id| found.push(id));
    found.retain(|id| app.shown().component(*id).is_some());
    app.multi.extend(found);
    if app.multi.len() == 1 {
        app.selection = app.multi.pop_first();
    }
    let n = app.selected_ids().len();
    app.status = format!("{n} component{} selected", if n == 1 { "" } else { "s" });
}

fn context_menu(app: &mut BitwerkApp, ui: &mut egui::Ui) {
    let target = app.ctx_target;
    let is_macro =
        target.is_some_and(|id| matches!(app.shown().component(id).map(|c| c.kind), Some(ComponentKind::Macro(_))));
    if is_macro {
        if ui.button("Edit Macro (open inside)").clicked() {
            if let Some(id) = target {
                app.enter_instance(id);
            }
            ui.close();
        }
        if ui.button("Macro Properties").clicked() {
            app.clear_selection();
            app.selection = target;
            ui.close();
        }
        ui.separator();
    }
    if ui.add_enabled(!app.selected_ids().is_empty(), egui::Button::new("Create Macro from Selection…")).clicked() {
        app.prompt_macro_name();
        ui.close();
    }
    if ui.button("New Macro").clicked() {
        app.new_macro();
        ui.close();
    }
    if ui.button("Parts Centre").clicked() {
        app.parts.open = true;
        ui.close();
    }
    if !app.levels.is_empty() && ui.button("Up One Level").clicked() {
        app.go_up();
        ui.close();
    }
}

fn finish_drag(app: &mut BitwerkApp, d: DragState) {
    let r = match d {
        DragState::Component { id, to, .. } => {
            if app.shown().component(id).is_some_and(|c| c.pos != to) {
                app.edit_move(id, to);
            }
            return;
        }
        DragState::Node { id, to } => {
            if app.shown().node(id).is_none_or(|n| n.pos == to) {
                return;
            }
            app.apply_edit(bw_core::Edit::MoveNode(id, to))
        }
        DragState::Segment { id, offset, .. } => {
            if offset == 0 {
                return;
            }
            app.apply_edit(bw_core::Edit::DragSegment(id, offset))
        }
    };
    if let Err(e) = r {
        app.status = e.to_string();
    }
    // Segment ids may be reused by the normalisation: drop a stale selection.
    if app.selected_segment.is_some_and(|s| app.shown().segment(s).is_none()) {
        app.selected_segment = None;
    }
}

fn update_hover_net(app: &mut BitwerkApp, seg: Option<SegmentId>) {
    let Some(seg) = seg else {
        app.hover_net = None;
        return;
    };
    if app.hover_net.as_ref().is_some_and(|(rev, s, _)| *rev == app.revision && *s == seg) {
        return;
    }
    let Some(start) = app.shown().segment(seg).map(|s| s.a) else { return };
    let mut segs = Vec::new();
    app.shown().visit_net(start, |_, _| {}, |s| segs.push(s));
    app.hover_net = Some((app.revision, seg, segs));
}

/// Selection, drag ghosts, the wire being drawn, the hovered net, the
/// placement preview, the name of the hovered pin and the components in a
/// bus contention, drawn with the egui painter (a bounded number of
/// shapes). Component kinds need no text labels: the GPU draws their
/// schematic symbols.
#[allow(clippy::too_many_lines, reason = "one overlay per tool / state")]
fn draw_overlays(app: &BitwerkApp, ui: &egui::Ui, rect: Rect, hover: Option<Vec2>, snap_shot: &Snapshot) {
    /// At most this many highlighted segments are painted per frame.
    const MAX_HIGHLIGHT: usize = 4_000;
    let viewport = v(rect.size());
    let world_at = |cam: &bw_render::Camera, p: Vec2| cam.screen_to_world(p, viewport);
    let painter = ui.painter_at(rect);
    let to_screen = |w: Vec2| {
        let s = app.camera.world_to_screen(w, viewport);
        rect.min + egui::vec2(s.x, s.y)
    };
    let seg_line = |id: SegmentId| app.shown().segment_ends(id).map(|(a, b)| [to_screen(gp(a)), to_screen(gp(b))]);
    let (vis_lo, vis_hi) = app.camera.visible_rect(viewport);
    let visible = |a: GridPos, b: GridPos| {
        let (lo, hi) = (gp(a).min(gp(b)), gp(a).max(gp(b)));
        hi.x >= vis_lo.x && hi.y >= vis_lo.y && lo.x <= vis_hi.x && lo.y <= vis_hi.y
    };
    let wire_px = (0.12 * app.camera.zoom).max(1.0);

    // Hovered net: a soft glow over every segment of it.
    if let Some((_, _, segs)) = &app.hover_net {
        let glow = Stroke::new(wire_px + 5.0, Color32::from_rgba_unmultiplied(120, 200, 255, 70));
        let mut n = 0;
        for &s in segs {
            let Some((a, b)) = app.shown().segment_ends(s) else { continue };
            if !visible(a, b) {
                continue;
            }
            painter.line_segment([to_screen(gp(a)), to_screen(gp(b))], glow);
            n += 1;
            if n >= MAX_HIGHLIGHT {
                break;
            }
        }
    }
    for id in app.multi.iter().take(4_000) {
        if let Some(c) = app.shown().component(*id) {
            let b = c.bounds();
            let r = Rect::from_two_pos(to_screen(gp(b.min)), to_screen(gp(b.max))).expand(3.0);
            painter.rect_stroke(r, 2.0, Stroke::new(1.5, Color32::from_rgb(140, 200, 255)), egui::StrokeKind::Outside);
        }
    }
    if let Some((a, b)) = app.box_select {
        let r = Rect::from_two_pos(to_screen(a), to_screen(b));
        painter.rect(
            r,
            0.0,
            Color32::from_rgba_unmultiplied(120, 180, 255, 30),
            Stroke::new(1.0, Color32::from_rgb(120, 180, 255)),
            egui::StrokeKind::Middle,
        );
    }
    // Inside a macro: the interface pin name next to each tag.
    if let Some(def) = app.current_def().and_then(|d| app.circuit.macros().def(d)) {
        if app.camera.zoom >= 4.0 {
            for p in def.pins.iter().take(256) {
                let Some(c) = app.shown().component(p.tag) else { continue };
                let b = c.bounds();
                let (anchor, align) = match c.kind {
                    ComponentKind::Tag { dir: bw_core::TagDir::In, .. } => {
                        (gp(b.min) + Vec2::new(-0.3, 1.0), egui::Align2::RIGHT_CENTER)
                    }
                    _ => (gp(b.max) + Vec2::new(0.3, -1.0), egui::Align2::LEFT_CENTER),
                };
                let size = (0.8 * app.camera.zoom).clamp(9.0, 22.0);
                painter.text(
                    to_screen(anchor),
                    align,
                    &p.name,
                    egui::FontId::proportional(size),
                    Color32::from_rgb(255, 214, 120),
                );
            }
        }
    }
    if let Some(id) = app.selection {
        if let Some(c) = app.shown().component(id) {
            let b = c.bounds();
            let r = Rect::from_two_pos(to_screen(gp(b.min)), to_screen(gp(b.max))).expand(3.0);
            painter.rect_stroke(r, 2.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Outside);
        }
    }
    if let Some(line) = app.selected_segment.and_then(seg_line) {
        painter.line_segment(line, Stroke::new(wire_px + 3.0, Color32::from_white_alpha(170)));
    }
    // Bus contention: the drivers fighting over the net, outlined in red
    // (inside a macro: the instance containing them, at this level).
    if let Some(bw_sim::SimError::BusContention { drivers, .. }) = &snap_shot.error {
        let here = app.view_path().unwrap_or_default();
        let ids: Vec<ComponentId> = drivers
            .iter()
            .filter(|p| p.len() > here.len() && p[..here.len()] == here[..])
            .map(|p| p[here.len()])
            .collect();
        for id in ids.iter().take(64) {
            if let Some(c) = app.shown().component(*id) {
                let b = c.bounds();
                let r = Rect::from_two_pos(to_screen(gp(b.min)), to_screen(gp(b.max))).expand(5.0);
                painter.rect_stroke(
                    r,
                    3.0,
                    Stroke::new(2.5, Color32::from_rgb(255, 70, 60)),
                    egui::StrokeKind::Outside,
                );
            }
        }
    }
    let ghost = Stroke::new(1.5, Color32::from_rgb(120, 180, 255));
    match app.drag {
        Some(DragState::Component { id, to, .. }) => {
            if let Some(c) = app.shown().component(id) {
                let b = c.bounds();
                let size = gp(b.max) - gp(b.min);
                let r = Rect::from_two_pos(to_screen(gp(to)), to_screen(gp(to) + size));
                painter.rect_stroke(r, 2.0, ghost, egui::StrokeKind::Middle);
            }
        }
        Some(DragState::Node { id, to }) => {
            if let Some(n) = app.shown().node(id) {
                for &s in n.segments() {
                    let Some(o) = app.shown().segment(s).and_then(|sg| app.shown().node(sg.other(id))) else {
                        continue;
                    };
                    // The L the rubber band will make.
                    let mid = corner(o.pos, to, false).unwrap_or(to);
                    painter.line_segment([to_screen(gp(o.pos)), to_screen(gp(mid))], ghost);
                    painter.line_segment([to_screen(gp(mid)), to_screen(gp(to))], ghost);
                }
                painter.circle_filled(to_screen(gp(to)), 4.0, Color32::from_rgb(120, 180, 255));
            }
        }
        Some(DragState::Segment { id, offset, .. }) => {
            if let Some((a, b)) = app.shown().segment_ends(id) {
                let shift = |p: GridPos| if a.y == b.y { p.offset(0, offset) } else { p.offset(offset, 0) };
                painter.line_segment([to_screen(gp(shift(a))), to_screen(gp(shift(b)))], Stroke::new(2.0, ghost.color));
                painter.line_segment([to_screen(gp(a)), to_screen(gp(shift(a)))], ghost);
                painter.line_segment([to_screen(gp(b)), to_screen(gp(shift(b)))], ghost);
            }
        }
        None => {}
    }
    if let Some(cursor) = hover {
        let w = world_at(&app.camera, cursor);
        // Name of the pin under the cursor (memories have dozens).
        if app.camera.zoom >= 6.0 {
            if let Some((pin, pos, _)) = pick_pin(app, w) {
                if let Some(c) = app.shown().component(pin.component) {
                    let name = match c.kind {
                        ComponentKind::Macro(s) => app
                            .circuit
                            .macros()
                            .def(s.def)
                            .and_then(|d| d.interface().get(usize::from(pin.pin)).map(|p| d.pins[p.index].name.clone()))
                            .unwrap_or_default(),
                        k => k.pin_name(pin.pin),
                    };
                    let at = to_screen(gp(pos)) + egui::vec2(8.0, -14.0);
                    let galley = painter.layout_no_wrap(name, egui::FontId::monospace(11.0), Color32::from_gray(235));
                    let bg = Rect::from_min_size(at, galley.size()).expand(3.0);
                    painter.rect_filled(bg, 3.0, Color32::from_black_alpha(190));
                    painter.galley(at, galley, Color32::WHITE);
                }
            }
        }
        match app.tool {
            Tool::Wire => {
                let yellow = Color32::YELLOW;
                let target = wire_target(app, w);
                if let Some((end, pos)) = target {
                    let at = to_screen(gp(pos));
                    match end {
                        WireEnd::Pin(_) => {
                            painter.circle_stroke(at, 5.0, Stroke::new(1.5, yellow));
                        }
                        WireEnd::Node(_) => {
                            painter.circle_filled(at, 4.5, yellow);
                        }
                        WireEnd::OnSegment(..) => {
                            // A junction will be created here.
                            painter.circle_stroke(at, 5.0, Stroke::new(1.5, yellow));
                            painter.circle_filled(at, 3.0, yellow);
                        }
                    }
                }
                if let Some(d) = &app.wire_draft {
                    let end = target.map_or_else(|| snap(w), |(_, p)| p);
                    let mut pts = vec![d.start_pos];
                    pts.extend(&d.bends);
                    if let Some(c) = corner(d.last(), end, d.vertical_first) {
                        pts.push(c);
                    }
                    pts.push(end);
                    let screen: Vec<Pos2> = pts.iter().map(|&p| to_screen(gp(p))).collect();
                    painter.add(egui::Shape::line(screen, Stroke::new(2.0, yellow)));
                    for &b in &d.bends {
                        painter.circle_filled(to_screen(gp(b)), 2.5, yellow);
                    }
                }
            }
            Tool::Place(kind) => {
                let (sw, sh) = kind.size();
                let size = gp(GridPos::new(sw, sh));
                let origin = snap(w - size * 0.5);
                let r = Rect::from_two_pos(to_screen(gp(origin)), to_screen(gp(origin) + size));
                painter.rect_stroke(r, 2.0, Stroke::new(1.0, Color32::from_white_alpha(60)), egui::StrokeKind::Middle);
                // Ghost of the symbol about to be placed (same mesh as the canvas).
                let o = gp(origin);
                if let ComponentKind::Macro(shape) = kind {
                    if let Some(e) = app.circuit.macros().get(shape.def) {
                        let mesh = crate::icons::macro_mesh(&e.def, &shape, e.hash);
                        crate::icons::paint_vertices(&painter, &mesh, app.camera.zoom, 0.55, |p| {
                            to_screen(o + Vec2::new(p[0], p[1]))
                        });
                    }
                } else {
                    crate::icons::paint_with(
                        &painter,
                        SymbolKind::of(&kind),
                        app.style,
                        2,
                        0.55,
                        app.camera.zoom,
                        |p| to_screen(o + Vec2::new(p[0], p[1])),
                    );
                }
            }
            Tool::Select => {
                // Junctions and bends are grabbable: show it.
                if app.drag.is_none() {
                    if let Some((_, pos)) = pick_node(app, w, reach(app), true) {
                        painter.circle_stroke(
                            to_screen(gp(pos)),
                            5.0,
                            Stroke::new(1.5, Color32::from_rgb(120, 180, 255)),
                        );
                    }
                }
            }
            Tool::Interact => {}
        }
    }
}

/// Component under a world point, checked against the authoritative circuit
/// (the scene may lag one rebuild behind).
fn pick_component(app: &BitwerkApp, w: Vec2) -> Option<ComponentId> {
    let id = app.scene.pick_body(w)?;
    let c = app.shown().component(id)?;
    let b = c.bounds();
    let inside = w.x >= gp(b.min).x && w.y >= gp(b.min).y && w.x <= gp(b.max).x && w.y <= gp(b.max).y;
    inside.then_some(id)
}

/// Nearest pin within reach of a world point.
fn pick_pin(app: &BitwerkApp, w: Vec2) -> Option<(PinRef, GridPos, PinDir)> {
    const REACH: f32 = 0.75;
    let mut best: Option<(f32, PinRef, GridPos, PinDir)> = None;
    app.scene.query(w - Vec2::splat(REACH), w + Vec2::splat(REACH), |it| {
        let Item::Body(i) = it else { return };
        let Some(id) = app.scene.body_id(i) else { return };
        let Some(c) = app.shown().component(id) else { return };
        for pin in 0..c.kind.pin_count() {
            let (Some(pos), Some(dir)) = (c.pin_pos(pin), c.pin_dir(pin)) else { continue };
            let d = (gp(pos) - w).length();
            if d <= REACH && best.is_none_or(|b| d < b.0) {
                best = Some((d, PinRef { component: id, pin }, pos, dir));
            }
        }
    });
    best.map(|(_, p, pos, dir)| (p, pos, dir))
}

/// Nearest wire node (an end of a segment near the point) within `reach`;
/// with `free_only`, junctions and bends only (pins are not draggable).
fn pick_node(app: &BitwerkApp, w: Vec2, reach: f32, free_only: bool) -> Option<(NodeId, GridPos)> {
    let mut best: Option<(f32, NodeId, GridPos)> = None;
    app.scene.query(w - Vec2::splat(reach), w + Vec2::splat(reach), |it| {
        let Item::Wire(i) = it else { return };
        let Some(seg) = app.scene.segment_at(i).and_then(|s| app.shown().segment(s)) else { return };
        for n in [seg.a, seg.b] {
            let Some(node) = app.shown().node(n) else { continue };
            if free_only && !node.is_free() {
                continue;
            }
            let d = (gp(node.pos) - w).length();
            if d <= reach && best.is_none_or(|b| d < b.0) {
                best = Some((d, n, node.pos));
            }
        }
    });
    best.map(|(_, n, p)| (n, p))
}

/// Grid point of segment `seg` nearest to `w`.
fn snap_on_segment(app: &BitwerkApp, seg: SegmentId, w: Vec2) -> Option<GridPos> {
    let (a, b) = app.shown().segment_ends(seg)?;
    let p = snap(w);
    Some(if a.y == b.y {
        GridPos::new(p.x.clamp(a.x.min(b.x), a.x.max(b.x)), a.y)
    } else {
        GridPos::new(a.x, p.y.clamp(a.y.min(b.y), a.y.max(b.y)))
    })
}

/// Where a wire would start or end under the cursor: a pin, a wire node
/// (junction / bend / wired pin), or a point on a segment (a new junction).
fn wire_target(app: &BitwerkApp, w: Vec2) -> Option<(WireEnd, GridPos)> {
    if let Some((pin, pos, _)) = pick_pin(app, w) {
        return Some((WireEnd::Pin(pin), pos));
    }
    let r = reach(app);
    if let Some((n, pos)) = pick_node(app, w, r * 0.8, false) {
        return Some((WireEnd::Node(n), pos));
    }
    let seg = app.scene.pick_wire(w, r)?;
    let p = snap_on_segment(app, seg, w)?;
    Some((WireEnd::OnSegment(seg, p), p))
}

fn click(app: &mut BitwerkApp, w: Vec2) {
    match app.tool {
        Tool::Select => {
            app.selection = pick_component(app, w);
            app.selected_segment = if app.selection.is_none() { app.scene.pick_wire(w, reach(app)) } else { None };
        }
        Tool::Interact => {
            if let Some(id) = pick_component(app, w) {
                if let Some(ComponentKind::Switch { on }) = app.shown().component(id).map(|c| c.kind) {
                    let kind = ComponentKind::Switch { on: !on };
                    if !app.levels.is_empty() {
                        // Inside a macro a switch is part of the definition.
                        if let Err(e) = app.apply_edit(bw_core::Edit::SetKind(id, kind)) {
                            app.status = e.to_string();
                        }
                    } else if app.circuit_mut().set_kind(id, kind).is_ok() {
                        app.sim.send(Command::SetSwitch(id, !on));
                        app.mark_dirty();
                    }
                }
            }
        }
        Tool::Place(kind) => {
            let (sw, sh) = kind.size();
            let origin = snap(w - gp(GridPos::new(sw, sh)) * 0.5);
            match app.edit_place(kind, origin) {
                Ok(id) => app.selection = Some(id),
                Err(e) => app.status = e.to_string(),
            }
        }
        Tool::Wire => wire_click(app, w),
    }
}

/// Wire tool: the first click picks the start (a pin or a wire), clicks in
/// empty space add bends (with the L of the current leg), a click on a pin
/// or a wire finishes.
fn wire_click(app: &mut BitwerkApp, w: Vec2) {
    let target = wire_target(app, w);
    let Some(draft) = app.wire_draft.as_mut() else {
        match target {
            Some((start, start_pos)) => {
                app.wire_draft = Some(WireDraft { start, start_pos, bends: Vec::new(), vertical_first: false });
                app.status =
                    "Wire: click to bend, click a pin or a wire to finish (/ or Space flips the L, Esc cancels)".into();
            }
            None => app.status = "Start a wire on a pin or on an existing wire".into(),
        }
        return;
    };
    if let Some((end, pos)) = target {
        let mut via = draft.bends.clone();
        via.extend(corner(draft.last(), pos, draft.vertical_first));
        let start = draft.start;
        match app.edit_wire(start, end, via) {
            Ok(_) => {
                app.wire_draft = None;
                app.status = "Wire added".into();
            }
            Err(e) => app.status = e.to_string(),
        }
    } else {
        let p = snap(w);
        let last = draft.last();
        if p != last && draft.bends.len() < bw_core::MAX_WIRE_VERTICES - 2 {
            draft.bends.extend(corner(last, p, draft.vertical_first));
            draft.bends.push(p);
        }
    }
}
