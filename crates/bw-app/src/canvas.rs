//! The circuit canvas: egui-wgpu paint callback + mouse interaction.

use std::sync::Arc;

use bw_core::{ComponentId, ComponentKind, GridPos, PinDir, PinRef};
use bw_render::{CanvasRenderer, FrameView, Item, NetLevels, SymbolKind};
use bw_sim::{Command, Snapshot};
use egui::{Color32, Pos2, Rect, Sense, Stroke};
use glam::Vec2;

use crate::app::{BitwerkApp, Tool};

/// In-progress drag of a component.
#[derive(Debug, Clone, Copy)]
pub struct DragState {
    pub id: ComponentId,
    /// Grab offset (world) from the component origin.
    pub grab: Vec2,
    pub to: GridPos,
}

/// Simulator snapshot as seen by the renderer.
pub struct SnapshotLevels<'a>(pub &'a Arc<Snapshot>);

impl NetLevels for SnapshotLevels<'_> {
    fn key(&self) -> u64 {
        self.0.generation
    }
    fn revision(&self) -> u64 {
        self.0.revision
    }
    fn levels(&self) -> &[u8] {
        &self.0.values
    }
    fn net_components(&self) -> &[ComponentId] {
        &self.0.net_components
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

/// Draws the canvas and handles its input. Returns the canvas rect.
pub fn canvas_ui(app: &mut BitwerkApp, ui: &mut egui::Ui, snap_shot: &Arc<Snapshot>) -> Rect {
    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let viewport = v(rect.size());
    let local = |p: Pos2| v(p - rect.min);
    let hover = resp.hover_pos().map(local);

    navigate(app, ui, &resp, rect);

    let world_at = |cam: &bw_render::Camera, p: Vec2| cam.screen_to_world(p, viewport);

    // ---- primary-button interaction ----------------------------------
    if resp.drag_started_by(egui::PointerButton::Primary) && app.tool == Tool::Select {
        if let Some(p) = resp.interact_pointer_pos().map(local) {
            let w = world_at(&app.camera, p);
            if let Some(id) = pick_component(app, w) {
                if let Some(c) = app.circuit.component(id) {
                    app.selection = Some(id);
                    app.drag = Some(DragState { id, grab: w - gp(c.pos), to: c.pos });
                }
            }
        }
    }
    if resp.dragged_by(egui::PointerButton::Primary) {
        if let Some(d) = app.drag.as_mut() {
            if let Some(p) = resp.interact_pointer_pos().map(local) {
                d.to = snap(app.camera.screen_to_world(p, viewport) - d.grab);
            }
        } else {
            // Dragging empty space pans.
            app.camera.pan_by(v(resp.drag_delta()));
        }
    }
    if resp.drag_stopped() {
        if let Some(d) = app.drag.take() {
            if app.circuit.component(d.id).is_some_and(|c| c.pos != d.to) {
                app.edit_move(d.id, d.to);
            }
        }
    }
    if resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos().map(local) {
            click(app, world_at(&app.camera, p));
        }
    }

    // ---- GPU canvas --------------------------------------------------
    let view =
        FrameView { camera: app.camera, viewport_points: viewport, pixels_per_point: ui.ctx().pixels_per_point() };
    app.prepare_canvas(&view, snap_shot);
    ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, CanvasCallback));

    draw_overlays(app, ui, rect, hover);
    rect
}

/// Selection, drag ghost, wiring rubber band and placement preview, drawn
/// with the egui painter (a bounded number of shapes). Component kinds need
/// no text labels: the GPU draws their schematic symbols.
fn draw_overlays(app: &BitwerkApp, ui: &egui::Ui, rect: Rect, hover: Option<Vec2>) {
    let viewport = v(rect.size());
    let world_at = |cam: &bw_render::Camera, p: Vec2| cam.screen_to_world(p, viewport);
    let painter = ui.painter_at(rect);
    let to_screen = |w: Vec2| {
        let s = app.camera.world_to_screen(w, viewport);
        rect.min + egui::vec2(s.x, s.y)
    };

    if let Some(id) = app.selection {
        if let Some(c) = app.circuit.component(id) {
            let b = c.bounds();
            let r = Rect::from_two_pos(to_screen(gp(b.min)), to_screen(gp(b.max))).expand(3.0);
            painter.rect_stroke(r, 2.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Outside);
        }
    }
    if let Some(d) = app.drag {
        if let Some(c) = app.circuit.component(d.id) {
            let b = c.bounds();
            let size = gp(b.max) - gp(b.min);
            let r = Rect::from_two_pos(to_screen(gp(d.to)), to_screen(gp(d.to) + size));
            painter.rect_stroke(r, 2.0, Stroke::new(1.5, Color32::from_rgb(120, 180, 255)), egui::StrokeKind::Middle);
        }
    }
    if let Some(cursor) = hover {
        let w = world_at(&app.camera, cursor);
        match app.tool {
            Tool::Wire => {
                let hovered = pick_pin(app, w);
                if let Some((_, pos, _)) = hovered {
                    painter.circle_stroke(to_screen(gp(pos)), 5.0, Stroke::new(1.5, Color32::YELLOW));
                }
                if let Some(start) = app.wire_start {
                    if let Some(a) = app.circuit.component(start.component).and_then(|c| c.pin_pos(start.pin)) {
                        painter.line_segment(
                            [to_screen(gp(a)), rect.min + egui::vec2(cursor.x, cursor.y)],
                            Stroke::new(1.5, Color32::YELLOW),
                        );
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
                crate::icons::paint_with(&painter, SymbolKind::of(&kind), app.style, 2, 0.55, app.camera.zoom, |p| {
                    to_screen(o + Vec2::new(p[0], p[1]))
                });
            }
            Tool::Select | Tool::Interact => {}
        }
    }
}

/// Component under a world point, checked against the authoritative circuit
/// (the scene may lag one rebuild behind).
fn pick_component(app: &BitwerkApp, w: Vec2) -> Option<ComponentId> {
    let id = app.scene.pick_body(w)?;
    let c = app.circuit.component(id)?;
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
        let Some(c) = app.circuit.component(id) else { return };
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

fn click(app: &mut BitwerkApp, w: Vec2) {
    match app.tool {
        Tool::Select => app.selection = pick_component(app, w),
        Tool::Interact => {
            if let Some(id) = pick_component(app, w) {
                if let Some(ComponentKind::Switch { on }) = app.circuit.component(id).map(|c| c.kind) {
                    let kind = ComponentKind::Switch { on: !on };
                    if app.circuit_mut().set_kind(id, kind).is_ok() {
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
        Tool::Wire => match (app.wire_start, pick_pin(app, w)) {
            (None, Some((pin, _, PinDir::Output))) => app.wire_start = Some(pin),
            (None, Some((_, _, PinDir::Input))) => app.status = "Start a wire at an output pin".into(),
            (Some(from), Some((to, _, _))) => match app.edit_connect(from, to) {
                Ok(_) => app.wire_start = None,
                Err(e) => app.status = e.to_string(),
            },
            (_, None) => {}
        },
    }
}
