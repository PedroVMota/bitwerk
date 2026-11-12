//! Application state, panels and command dispatch.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use bw_core::{Applied, Circuit, CircuitError, ComponentId, ComponentKind, Edit, GateOp, GridPos, PinRef, WireId};
use bw_render::{Camera, CanvasRenderer, Change, DrawMode, FrameView, Scene, SymbolKind, SymbolStyle};
use bw_sim::{Command, RunState, SimHandle, StepMode};
use crossbeam_channel::Receiver;

use crate::menu::{MenuAction, NativeMenu};
use crate::scene_worker::SceneWorker;
use crate::sweep::Sweep;

/// MSAA sample count shared by eframe and the canvas pipelines.
pub const MSAA_SAMPLES: u16 = 4;
/// File extension of native projects.
pub const PROJECT_EXT: &str = "bitwerk";
/// Size of the debug stress test.
const STRESS_COMPONENTS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Interact,
    Wire,
    Place(ComponentKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    New,
    Open,
}

/// Debug-only switches.
#[derive(Debug, Default)]
struct DebugOpts {
    /// Repaint every frame (for measuring FPS).
    continuous_repaint: bool,
    /// Log frame statistics periodically.
    log_stats: bool,
}

#[allow(clippy::struct_excessive_bools, reason = "independent UI state flags")]
pub struct BitwerkApp {
    // Model
    pub circuit: Arc<Circuit>,
    pub revision: u64,
    path: Option<PathBuf>,
    name: String,
    dirty: bool,

    // Engines / workers
    pub sim: SimHandle,
    scene_worker: SceneWorker,
    /// Render scene, owned by the UI thread and patched in place by edits.
    pub scene: Box<Scene>,
    /// A full scene build is in flight; edits re-request it instead of
    /// patching the (stale) current scene.
    full_build_pending: bool,
    gpu: Option<egui_wgpu::RenderState>,
    stress_rx: Option<Receiver<Result<Circuit, bw_core::CircuitError>>>,
    pub fit_on_next_scene: bool,
    /// Camera to apply when the next scene arrives (`--view`).
    initial_view: Option<Camera>,

    // UI state
    pub camera: Camera,
    pub tool: Tool,
    pub gate_inputs: u8,
    pub selection: Option<ComponentId>,
    pub wire_start: Option<PinRef>,
    pub drag: Option<crate::canvas::DragState>,
    pub status: String,
    /// Schematic symbol style (View menu / palette). Runtime only: eframe's
    /// persistence is not enabled, so there is no settings store yet.
    pub style: SymbolStyle,
    step_mode: StepMode,
    tick_rate: f64,
    confirm: Option<Pending>,
    debug: DebugOpts,
    menu: Option<NativeMenu>,

    // Stats
    frame_times: VecDeque<f32>,
    frames_since_log: u32,
    /// Draw mode and streaming progress of the last prepared frame.
    canvas_state: (DrawMode, Option<f32>),
    /// Milliseconds spent in the canvas renderer's prepare last frame.
    prepare_ms: f64,
    /// The last frame drew symbol meshes (else far-zoom quads).
    symbols_drawn: bool,
    pub canvas_size: glam::Vec2,
    sweep: Option<Sweep>,
    /// Events of the current frame, for the measurement log.
    frame_events: String,
    /// `ui` (and therefore painting) ran since the last `logic` call.
    ui_ran: bool,
}

/// While the window is occluded or minimized eframe runs only `logic` and
/// never presents, so nothing paces the loop: requested repaints are spaced
/// at least this far apart instead of spinning a core.
const HIDDEN_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

impl BitwerkApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let rs = cc.wgpu_render_state.as_ref().ok_or("bitwerk requires the wgpu (Metal) renderer")?;
        let canvas = CanvasRenderer::new(&rs.device, rs.target_format, u32::from(MSAA_SAMPLES));
        rs.renderer.write().callback_resources.insert(canvas);

        let ctx = cc.egui_ctx.clone();
        let sim = SimHandle::spawn(Box::new(move || ctx.request_repaint()))?;
        let ctx = cc.egui_ctx.clone();
        let scene_worker = SceneWorker::spawn(Box::new(move || ctx.request_repaint()))?;
        let menu = NativeMenu::install(&cc.egui_ctx);

        let mut app = Self {
            circuit: Arc::new(Circuit::new()),
            revision: 0,
            path: None,
            name: "untitled".into(),
            dirty: false,
            sim,
            scene_worker,
            scene: Box::default(),
            full_build_pending: false,
            gpu: cc.wgpu_render_state.clone(),
            stress_rx: None,
            fit_on_next_scene: false,
            initial_view: None,
            camera: Camera::default(),
            tool: Tool::Select,
            gate_inputs: 2,
            selection: None,
            wire_start: None,
            drag: None,
            status: "Ready".into(),
            style: SymbolStyle::Ansi,
            step_mode: StepMode::HalfCycle,
            tick_rate: 20.0,
            confirm: None,
            debug: DebugOpts::default(),
            menu,
            frame_times: VecDeque::with_capacity(120),
            frames_since_log: 0,
            canvas_state: (DrawMode::All(0), None),
            prepare_ms: 0.0,
            symbols_drawn: false,
            canvas_size: glam::Vec2::new(900.0, 650.0),
            sweep: None,
            frame_events: String::new(),
            ui_ran: true,
        };
        app.sim.send(Command::SetTickRate(app.tick_rate));
        app.commit_full();
        // Debug: `bitwerk --stress` loads the 100k stress test at start-up and
        // logs frame statistics; `--continuous` repaints every frame (for
        // measuring; normally the app only repaints when something changed).
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.iter().any(|a| a == "--stress") {
            app.debug.log_stats = true;
            app.spawn_stress(&cc.egui_ctx);
        }
        // `--style iec|ansi`: initial symbol style.
        if let Some(v) = args.iter().position(|a| a == "--style").and_then(|i| args.get(i + 1)) {
            app.set_style(if v.eq_ignore_ascii_case("iec") { SymbolStyle::Iec } else { SymbolStyle::Ansi });
        }
        // `bitwerk <file.bitwerk>` opens a project at start-up.
        if let Some(path) = args.iter().find(|a| {
            std::path::Path::new(a).extension().is_some_and(|e| e == PROJECT_EXT || e.eq_ignore_ascii_case("dwm"))
        }) {
            app.open_path(PathBuf::from(path));
        }
        // Debug: `--view X,Y,ZOOM` sets the camera once the scene is built
        // (reproducible screenshots).
        app.initial_view = args.iter().position(|a| a == "--view").and_then(|i| args.get(i + 1)).and_then(|v| {
            let p: Vec<f32> = v.split(',').filter_map(|t| t.trim().parse().ok()).collect();
            (p.len() == 3).then(|| Camera { center: glam::Vec2::new(p[0], p[1]), zoom: p[2] })
        });
        if args.iter().any(|a| a == "--continuous") {
            app.debug.continuous_repaint = true;
        }
        // `--sweep` (with `--stress`): scripted, self-terminating measurement.
        if args.iter().any(|a| a == "--sweep") {
            let csv = args.iter().position(|a| a == "--sweep-csv").and_then(|i| args.get(i + 1)).map(PathBuf::from);
            app.debug.continuous_repaint = true;
            app.sweep = Some(Sweep::new(csv));
        }
        Ok(app)
    }

    /// Mutable access to the circuit (copy-on-write: only the scene worker
    /// ever shares it, briefly, during a full build).
    pub fn circuit_mut(&mut self) -> &mut Circuit {
        Arc::make_mut(&mut self.circuit)
    }

    /// Publishes a whole new circuit: the simulator reloads and the scene is
    /// rebuilt from scratch on the worker (then streamed to the GPU).
    pub fn commit_full(&mut self) {
        self.revision += 1;
        self.sim.send(Command::Load { circuit: Arc::clone(&self.circuit), revision: self.revision });
        self.scene_worker.request(Arc::clone(&self.circuit), self.revision);
        self.full_build_pending = true;
    }

    /// Applies one model edit everywhere, incrementally: the UI's circuit,
    /// the simulator's copy (asynchronously) and the render scene (patched
    /// in place now, so the edit is visible in this very frame).
    ///
    /// # Errors
    /// The model rejected the edit; nothing changed.
    pub fn apply_edit(&mut self, edit: Edit) -> Result<Applied, CircuitError> {
        let applied = self.circuit_mut().apply(&edit)?;
        self.revision += 1;
        self.sim.send(Command::Edit { edit, revision: self.revision });
        if self.full_build_pending {
            self.scene_worker.request(Arc::clone(&self.circuit), self.revision);
        } else {
            let change = match &applied {
                Applied::Component(id) => Change::Component(*id),
                Applied::Wire { id, .. } => Change::Wire(*id),
            };
            self.scene.apply(&self.circuit, &[change], self.revision);
            // Many deletions leave holes; compact with a background rebuild.
            if self.scene.free_ratio() > 0.5 && self.scene.slot_count() > 50_000 {
                self.commit_rebuild();
            }
        }
        self.mark_dirty();
        Ok(applied)
    }

    fn commit_rebuild(&mut self) {
        self.scene_worker.request(Arc::clone(&self.circuit), self.revision);
        self.full_build_pending = true;
    }

    /// Switches the symbol style: only a uniform changes on the GPU.
    pub fn set_style(&mut self, style: SymbolStyle) {
        self.style = style;
        if let Some(m) = &self.menu {
            m.sync_style(style);
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn replace_circuit(&mut self, circuit: Circuit, name: String, path: Option<PathBuf>) {
        self.circuit = Arc::new(circuit);
        self.name = name;
        self.path = path;
        self.dirty = false;
        self.selection = None;
        self.wire_start = None;
        self.drag = None;
        self.fit_on_next_scene = true;
        self.sim.send(Command::Stop);
        self.commit_full();
    }

    // ---- actions -------------------------------------------------------

    fn action(&mut self, a: MenuAction, ctx: &egui::Context) {
        match a {
            MenuAction::New => self.guarded(Pending::New),
            MenuAction::Open => self.guarded(Pending::Open),
            MenuAction::Save => self.save(false),
            MenuAction::SaveAs => self.save(true),
            MenuAction::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            MenuAction::Run => self.sim.send(Command::Run),
            MenuAction::Stop => self.sim.send(Command::Stop),
            MenuAction::Pause => self.sim.send(Command::Pause),
            MenuAction::Step => self.sim.send(Command::Step(self.step_mode)),
            MenuAction::StressTest => self.spawn_stress(ctx),
            MenuAction::ToggleContinuousRepaint => self.debug.continuous_repaint = !self.debug.continuous_repaint,
            MenuAction::Style(style) => self.set_style(style),
        }
    }

    fn guarded(&mut self, p: Pending) {
        if self.dirty {
            self.confirm = Some(p);
        } else {
            self.run_pending(p);
        }
    }

    fn run_pending(&mut self, p: Pending) {
        match p {
            Pending::New => {
                self.replace_circuit(Circuit::new(), "untitled".into(), None);
                self.camera = Camera::default();
                self.fit_on_next_scene = false;
            }
            Pending::Open => self.open(),
        }
    }

    fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Open circuit")
            .add_filter("bitwerk project", &[PROJECT_EXT])
            .add_filter("Digital Works circuit (legacy)", &["dwm"])
            .pick_file()
        else {
            return;
        };
        self.open_path(path);
    }

    fn open_path(&mut self, path: PathBuf) {
        match bw_io::load_from_path(&path) {
            Ok(p) => {
                let name = if p.name.is_empty() { file_stem(&path) } else { p.name };
                self.status = format!("Opened {}", path.display());
                self.replace_circuit(p.circuit, name, Some(path));
            }
            Err(e) => self.status = format!("Could not open {}: {e}", path.display()),
        }
    }

    fn save(&mut self, save_as: bool) {
        let path = if let (Some(p), false) = (&self.path, save_as) {
            p.clone()
        } else {
            let Some(mut p) = rfd::FileDialog::new()
                .set_title("Save circuit")
                .add_filter("bitwerk project", &[PROJECT_EXT])
                .set_file_name(format!("{}.{PROJECT_EXT}", self.name))
                .save_file()
            else {
                return;
            };
            if p.extension().is_none_or(|e| e != PROJECT_EXT) {
                p.set_extension(PROJECT_EXT);
            }
            p
        };
        let project = bw_io::Project { name: self.name.clone(), circuit: (*self.circuit).clone() };
        match bw_io::save_to_path(&project, &path) {
            Ok(()) => {
                self.status = format!("Saved {}", path.display());
                self.name = file_stem(&path);
                self.path = Some(path);
                self.dirty = false;
            }
            Err(e) => self.status = format!("Could not save: {e}"),
        }
    }

    fn spawn_stress(&mut self, ctx: &egui::Context) {
        if self.stress_rx.is_some() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("bw-stress".into()).spawn(move || {
            let _ = tx.send(crate::stress::generate(STRESS_COMPONENTS));
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => {
                self.stress_rx = Some(rx);
                self.status = format!("Generating {STRESS_COMPONENTS} components…");
            }
            Err(e) => self.status = format!("Could not start stress test: {e}"),
        }
    }

    fn poll_workers(&mut self) {
        if let Some(rx) = &self.stress_rx {
            if let Ok(res) = rx.try_recv() {
                self.stress_rx = None;
                match res {
                    Ok(c) => {
                        self.event("circuit-arrived");
                        self.status =
                            format!("Stress test: {} components, {} wires", c.component_count(), c.wire_count());
                        self.replace_circuit(c, "stress-test".into(), None);
                        self.dirty = true;
                    }
                    Err(e) => self.status = format!("Stress test failed: {e}"),
                }
            }
        }
        let built: Vec<Box<Scene>> = self.scene_worker.poll().collect();
        for scene in built {
            // Only a build of the current revision may replace the live
            // scene; older ones are superseded by a newer request in flight.
            if scene.revision() == self.revision {
                self.event("scene-built");
                let old = std::mem::replace(&mut self.scene, scene);
                self.scene_worker.retire(old);
                self.full_build_pending = false;
                if self.fit_on_next_scene {
                    self.fit_on_next_scene = false;
                    self.fit_view();
                }
                if let Some(cam) = self.initial_view.take() {
                    self.camera = cam;
                }
            } else {
                self.scene_worker.retire(scene);
            }
        }
    }

    fn fit_view(&mut self) {
        let (lo, hi) = self.scene.bounds();
        if hi.cmple(lo).any() {
            return;
        }
        self.camera.center = (lo + hi) * 0.5;
        let ext = (hi - lo).max(glam::Vec2::ONE);
        let c = self.canvas_size.max(glam::Vec2::splat(100.0)) * 0.95;
        self.camera.zoom = (c.x / ext.x).min(c.y / ext.y).clamp(bw_render::camera::MIN_ZOOM, 40.0);
    }

    // ---- panels --------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, snap: &bw_sim::Snapshot) {
        ui.horizontal(|ui| {
            if ui.button("Run").on_hover_text("Run (F9)").clicked() {
                self.sim.send(Command::Run);
            }
            if ui.button("Stop").on_hover_text("Stop (F10)").clicked() {
                self.sim.send(Command::Stop);
            }
            if ui.button("Pause").on_hover_text("Pause / resume (F11)").clicked() {
                self.sim.send(Command::Pause);
            }
            if ui.button("Step").on_hover_text("Step (F12)").clicked() {
                self.sim.send(Command::Step(self.step_mode));
            }
            egui::ComboBox::from_id_salt("step_mode")
                .selected_text(match self.step_mode {
                    StepMode::HalfCycle => "½ cycle",
                    StepMode::FullCycle => "full cycle",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.step_mode, StepMode::HalfCycle, "Half clock cycle");
                    ui.selectable_value(&mut self.step_mode, StepMode::FullCycle, "Full clock cycle");
                });
            ui.separator();
            ui.label("ticks/s");
            let r =
                ui.add(egui::DragValue::new(&mut self.tick_rate).range(0.1..=bw_sim::engine::MAX_TICK_RATE).speed(1.0));
            if r.changed() {
                self.sim.send(Command::SetTickRate(self.tick_rate));
            }
            ui.separator();
            let (txt, color) = match snap.state {
                RunState::Stopped => ("Stopped", ui.visuals().weak_text_color()),
                RunState::Running => ("Running", egui::Color32::LIGHT_GREEN),
                RunState::Paused => ("Paused", egui::Color32::YELLOW),
                RunState::Halted => ("Halted", egui::Color32::LIGHT_RED),
            };
            ui.colored_label(color, txt);
            ui.label(format!("tick {}", snap.tick));
            if let Some(e) = &snap.error {
                ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
            }
            if self.menu.is_none() {
                ui.separator();
                if ui.button("Open…").clicked() {
                    self.guarded(Pending::Open);
                }
                if ui.button("Save").clicked() {
                    self.save(false);
                }
                if ui.button("Stress 100k").clicked() {
                    self.spawn_stress(ui.ctx());
                }
            }
        });
    }

    fn palette(&mut self, ui: &mut egui::Ui) {
        ui.heading("Tools");
        ui.selectable_value(&mut self.tool, Tool::Select, "Select / move");
        ui.selectable_value(&mut self.tool, Tool::Interact, "Interact (toggle switches)");
        ui.selectable_value(&mut self.tool, Tool::Wire, "Wire");
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Symbols");
            let mut style = self.style;
            ui.selectable_value(&mut style, SymbolStyle::Ansi, "ANSI").on_hover_text(SymbolStyle::Ansi.name());
            ui.selectable_value(&mut style, SymbolStyle::Iec, "IEC").on_hover_text(SymbolStyle::Iec.name());
            if style != self.style {
                self.set_style(style);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Gate inputs");
            ui.add(
                egui::DragValue::new(&mut self.gate_inputs).range(bw_core::MIN_GATE_INPUTS..=bw_core::MAX_GATE_INPUTS),
            );
        });
        if let Tool::Place(ComponentKind::Gate { op, inputs }) = self.tool {
            if inputs != self.gate_inputs {
                self.tool = Tool::Place(ComponentKind::Gate { op, inputs: self.gate_inputs });
            }
        }
        let mut kinds: Vec<ComponentKind> =
            GateOp::ALL.iter().map(|&op| ComponentKind::Gate { op, inputs: self.gate_inputs }).collect();
        kinds.extend([
            ComponentKind::Not,
            ComponentKind::Switch { on: false },
            ComponentKind::Clock { half_period: 10 },
            ComponentKind::Led,
        ]);
        let style = self.style;
        egui::Grid::new("palette-components").spacing([4.0, 4.0]).show(ui, |ui| {
            for (i, kind) in kinds.into_iter().enumerate() {
                let selected = matches!(self.tool, Tool::Place(k) if same_palette_entry(k, kind));
                // Icons show the 2-input variant; the placed gate uses the
                // input count above.
                let sym = match kind {
                    ComponentKind::Gate { op, .. } => SymbolKind::Gate(op, 2),
                    k => SymbolKind::of(&k),
                };
                let tip = match kind {
                    ComponentKind::Gate { op, inputs } => format!("{} gate ({inputs} inputs)", op.name()),
                    k => SymbolKind::of(&k).name().to_string(),
                };
                if crate::icons::button(ui, sym, style, kind.label(), &tip, selected, true).clicked() {
                    self.tool = Tool::Place(kind);
                }
                if i % 3 == 2 {
                    ui.end_row();
                }
            }
        });
        egui::CollapsingHeader::new("Coming later").default_open(true).show(ui, |ui| {
            egui::Grid::new("palette-future").spacing([4.0, 4.0]).show(ui, |ui| {
                for (i, kind) in SymbolKind::FUTURE.into_iter().enumerate() {
                    let tip = format!("{} (symbol only, not simulated yet)", kind.name());
                    let _ = crate::icons::button(ui, kind, style, short_name(kind), &tip, false, false);
                    if i % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
        });
        ui.separator();
        ui.small("Scroll wheel / pinch: zoom\nTrackpad scroll or drag empty space: pan\nR: rotate · Del: delete · Esc: cancel");
    }

    fn properties(&mut self, ui: &mut egui::Ui) {
        ui.heading("Properties");
        let Some(id) = self.selection else {
            ui.weak("Nothing selected");
            ui.separator();
            ui.label(format!("Project: {}{}", self.name, if self.dirty { " •" } else { "" }));
            ui.label(format!("{} components", self.circuit.component_count()));
            ui.label(format!("{} wires", self.circuit.wire_count()));
            return;
        };
        let Some(comp) = self.circuit.component(id).cloned() else {
            self.selection = None;
            return;
        };
        ui.label(format!("{} #{}", comp.kind.label(), id.0));
        ui.label(format!("position ({}, {})", comp.pos.x, comp.pos.y));
        ui.label(format!("rotation {:?}", comp.rotation));
        let mut kind = comp.kind;
        match &mut kind {
            ComponentKind::Gate { op, inputs } => {
                egui::ComboBox::from_label("operation").selected_text(op.name()).show_ui(ui, |ui| {
                    for o in GateOp::ALL {
                        ui.selectable_value(op, o, o.name());
                    }
                });
                ui.add(
                    egui::DragValue::new(inputs)
                        .range(bw_core::MIN_GATE_INPUTS..=bw_core::MAX_GATE_INPUTS)
                        .prefix("inputs "),
                );
            }
            ComponentKind::Clock { half_period } => {
                ui.add(
                    egui::DragValue::new(half_period)
                        .range(1..=bw_core::MAX_CLOCK_HALF_PERIOD)
                        .prefix("half period (ticks) "),
                );
            }
            ComponentKind::Switch { on } => {
                ui.checkbox(on, "on");
            }
            ComponentKind::Not | ComponentKind::Led => {}
        }
        if kind != comp.kind {
            let res = if let ComponentKind::Switch { on } = kind {
                // Toggling a switch changes no geometry: the simulator updates
                // its copy and the level arrives with the next snapshot.
                let r = self.circuit_mut().set_kind(id, kind);
                if r.is_ok() {
                    self.sim.send(Command::SetSwitch(id, on));
                    self.mark_dirty();
                }
                r
            } else {
                self.apply_edit(Edit::SetKind(id, kind)).map(|_| ())
            };
            if res.is_err() {
                self.status = "Disconnect the wires before changing the pin count".into();
            }
        }
        ui.separator();
        if ui.button("Rotate (R)").clicked() {
            self.rotate_selected();
        }
        if ui.button("Delete (Del)").clicked() {
            self.delete_selected();
        }
    }

    pub fn rotate_selected(&mut self) {
        if let Some(id) = self.selection {
            self.edit_rotate(id);
        }
    }

    pub fn delete_selected(&mut self) {
        if let Some(id) = self.selection.take() {
            self.edit_delete(id);
        }
    }

    // ---- edits ---------------------------------------------------------
    // Every model change goes through one of these so the simulator and the
    // render scene are kept in sync.

    pub fn edit_move(&mut self, id: ComponentId, to: GridPos) -> bool {
        self.apply_edit(Edit::MoveComponent(id, to)).is_ok()
    }

    pub fn edit_rotate(&mut self, id: ComponentId) -> bool {
        let Some(rot) = self.circuit.component(id).map(|c| c.rotation.rotated_cw()) else { return false };
        self.apply_edit(Edit::SetRotation(id, rot)).is_ok()
    }

    pub fn edit_place(&mut self, kind: ComponentKind, origin: GridPos) -> Result<ComponentId, CircuitError> {
        match self.apply_edit(Edit::AddComponent { kind, pos: origin, rotation: bw_core::Rotation::R0 })? {
            Applied::Component(id) => Ok(id),
            Applied::Wire { .. } => Err(CircuitError::IdSpaceExhausted),
        }
    }

    pub fn edit_connect(&mut self, from: PinRef, to: PinRef) -> Result<WireId, CircuitError> {
        match self.apply_edit(Edit::Connect { from, to, via: Vec::new() })? {
            Applied::Wire { id, .. } => Ok(id),
            Applied::Component(_) => Err(CircuitError::IdSpaceExhausted),
        }
    }

    pub fn edit_delete(&mut self, id: ComponentId) -> bool {
        let ok = self.apply_edit(Edit::RemoveComponent(id)).is_ok();
        if ok && self.selection == Some(id) {
            self.selection = None;
        }
        ok
    }

    /// Uploads this frame's scene changes, net levels and camera to the GPU
    /// (on the UI thread, where the mutable scene lives; the paint callback
    /// only records the culling pass and the draws).
    pub fn prepare_canvas(&mut self, view: &FrameView, snap: &Arc<bw_sim::Snapshot>) {
        let Some(rs) = &self.gpu else { return };
        let t = std::time::Instant::now();
        let mut renderer = rs.renderer.write();
        if let Some(r) = renderer.callback_resources.get_mut::<CanvasRenderer>() {
            let levels = crate::canvas::SnapshotLevels(snap);
            r.set_symbol_style(self.style);
            r.prepare(&rs.device, &rs.queue, &mut self.scene, view, Some(&levels));
            self.canvas_state = (r.draw_mode(), r.stream_progress());
            self.symbols_drawn = r.draws_symbols();
        }
        drop(renderer);
        self.prepare_ms = t.elapsed().as_secs_f64() * 1000.0;
    }

    fn event(&mut self, e: &str) {
        if self.sweep.is_some() {
            if !self.frame_events.is_empty() {
                self.frame_events.push(' ');
            }
            self.frame_events.push_str(e);
        }
    }

    pub fn take_frame_events(&mut self) -> String {
        std::mem::take(&mut self.frame_events)
    }

    /// The scene is still being uploaded to the GPU.
    pub fn streaming(&self) -> bool {
        self.canvas_state.1.is_some()
    }

    // ---- measurement hooks ----------------------------------------------

    /// The render scene matches the current circuit revision and is fully
    /// resident on the GPU.
    pub fn scene_ready(&self) -> bool {
        self.stress_rx.is_none()
            && !self.full_build_pending
            && self.scene.revision() == self.revision
            && !self.streaming()
    }

    pub fn scene_instance_count(&self) -> usize {
        self.scene.instance_count()
    }

    pub fn scene_bounds(&self) -> (glam::Vec2, glam::Vec2) {
        self.scene.bounds()
    }

    pub fn scene_query(&self, lo: glam::Vec2, hi: glam::Vec2, f: impl FnMut(ComponentId)) {
        crate::sweep::bodies_in(&self.scene, lo, hi, f);
    }

    /// Circuit revision of the scene handed to the renderer this frame.
    pub fn displayed_revision(&self) -> u64 {
        self.scene.revision()
    }

    /// (renderer prepare ms, instances submitted) of the last prepared frame.
    pub fn frame_profile(&self) -> (f64, u32) {
        let n = match self.canvas_state.0 {
            DrawMode::All(n) | DrawMode::Culled(n) => n,
        };
        (self.prepare_ms, n)
    }

    fn stats_overlay(&mut self, ui: &egui::Ui, canvas: egui::Rect) {
        let dt = ui.input(|i| i.unstable_dt);
        if self.frame_times.len() == self.frame_times.capacity() {
            self.frame_times.pop_front();
        }
        self.frame_times.push_back(dt);
        #[allow(clippy::cast_precision_loss)]
        let avg = self.frame_times.iter().sum::<f32>() / self.frame_times.len().max(1) as f32;
        let worst = self.frame_times.iter().copied().fold(0.0f32, f32::max);
        self.frames_since_log += 1;
        if self.debug.log_stats && self.frames_since_log >= 120 {
            self.frames_since_log = 0;
            tracing::info!(
                fps = 1.0 / avg.max(1e-6),
                avg_ms = avg * 1000.0,
                worst_ms = worst * 1000.0,
                objects = self.scene.instance_count(),
                zoom = self.camera.zoom,
                "frame stats"
            );
        }
        let drawn = match self.canvas_state.0 {
            DrawMode::All(n) => format!("all {n} drawn"),
            DrawMode::Culled(n) => format!("{n} GPU-culled"),
        };
        let lod = if self.symbols_drawn { "symbols" } else { "quads" };
        let streaming = self.canvas_state.1.map(|p| format!(" · uploading {:.0}%", p * 100.0)).unwrap_or_default();
        let text = format!(
            "{:.0} fps · {:.2} ms avg · {:.2} ms worst · prepare {:.2} ms\n{} objects · {drawn} · {lod} · zoom {:.2}{streaming}",
            1.0 / avg.max(1e-6),
            avg * 1000.0,
            worst * 1000.0,
            self.prepare_ms,
            self.scene.instance_count(),
            self.camera.zoom,
        );
        let painter = ui.painter_at(canvas);
        let pos = canvas.left_top() + egui::vec2(8.0, 8.0);
        let galley = painter.layout_no_wrap(text, egui::FontId::monospace(11.0), egui::Color32::from_gray(220));
        let bg = egui::Rect::from_min_size(pos, galley.size()).expand(4.0);
        painter.rect_filled(bg, 4.0, egui::Color32::from_black_alpha(160));
        painter.galley(pos, galley, egui::Color32::WHITE);
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (del, esc, rot, f9, f10, f11, f12) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::F9),
                i.key_pressed(egui::Key::F10),
                i.key_pressed(egui::Key::F11),
                i.key_pressed(egui::Key::F12),
            )
        });
        if del {
            self.delete_selected();
        }
        if esc {
            if self.wire_start.is_some() {
                self.wire_start = None;
            } else {
                self.tool = Tool::Select;
            }
        }
        if rot {
            self.rotate_selected();
        }
        // With the native menu these come in as menu accelerators instead.
        if self.menu.is_none() {
            if f9 {
                self.sim.send(Command::Run);
            }
            if f10 {
                self.sim.send(Command::Stop);
            }
            if f11 {
                self.sim.send(Command::Pause);
            }
            if f12 {
                self.sim.send(Command::Step(self.step_mode));
            }
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = self.confirm else { return };
        let mut close = false;
        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("The current circuit has unsaved changes. Discard them?");
                ui.horizontal(|ui| {
                    if ui.button("Discard").clicked() {
                        close = true;
                        self.dirty = false;
                        self.run_pending(p);
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.confirm = None;
        }
    }
}

/// Same palette entry (gates by operation, others by kind)?
fn same_palette_entry(a: ComponentKind, b: ComponentKind) -> bool {
    match (a, b) {
        (ComponentKind::Gate { op: x, .. }, ComponentKind::Gate { op: y, .. }) => x == y,
        _ => std::mem::discriminant(&a) == std::mem::discriminant(&b),
    }
}

/// Caption under a palette icon.
fn short_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::DFlipFlop => "D-FF",
        SymbolKind::RsFlipFlop => "RS-FF",
        SymbolKind::JkFlipFlop => "JK-FF",
        SymbolKind::TriState => "Tri-state",
        SymbolKind::SevenSegment => "7-seg",
        SymbolKind::NumericOutput => "Number",
        SymbolKind::Vcc => "VCC",
        SymbolKind::SequenceGenerator => "Sequence",
        SymbolKind::InteractiveInput => "Input",
        SymbolKind::Macro => "Macro",
        SymbolKind::Junction => "Junction",
        k => k.name(),
    }
}

fn file_stem(p: &std::path::Path) -> String {
    p.file_stem().map_or_else(|| "untitled".into(), |s| s.to_string_lossy().into_owned())
}

impl eframe::App for BitwerkApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Was the previous pass presented? eframe skips `ui` and painting for
        // occluded/minimized windows (and when macOS withholds redraws).
        let presented = std::mem::replace(&mut self.ui_ran, false);
        let (occluded, minimized) =
            ctx.input(|i| (i.viewport().occluded.unwrap_or(false), i.viewport().minimized.unwrap_or(false)));
        let hidden = occluded || minimized || !presented;
        if hidden {
            self.event(if occluded {
                "occluded"
            } else if minimized {
                "minimized"
            } else {
                "not-presented"
            });
        }
        if let Some(mut sweep) = self.sweep.take() {
            if sweep.begin_frame(self, frame.info().cpu_usage, self.canvas_size, presented) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.sweep = Some(sweep);
        }
        let actions: Vec<MenuAction> = self.menu.as_ref().map(|m| m.poll().collect()).unwrap_or_default();
        for a in actions {
            self.action(a, ctx);
        }
        self.poll_workers();
        // Repaint on demand: input, simulator snapshots and finished workers
        // request repaints themselves; only keep going while the scene is
        // still streaming to the GPU (or when measuring).
        // When hidden, nothing is presented (and no vsync paces the loop):
        // throttle instead of busy-spinning.
        if self.streaming() || self.debug.continuous_repaint {
            if hidden {
                ctx.request_repaint_after(HIDDEN_REPAINT_INTERVAL);
            } else {
                ctx.request_repaint();
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_ran = true;
        let ctx = ui.ctx().clone();
        let snap = self.sim.snapshot();
        self.handle_keys(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui, &snap));
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
            });
        });
        egui::Panel::left("palette").resizable(false).default_size(170.0).show(ui, |ui| self.palette(ui));
        egui::Panel::right("properties").resizable(true).default_size(220.0).show(ui, |ui| self.properties(ui));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = crate::canvas::canvas_ui(self, ui, &snap);
            self.canvas_size = glam::Vec2::new(rect.width(), rect.height());
            self.stats_overlay(ui, rect);
        });
        self.confirm_dialog(&ctx);
        if let Some(mut sweep) = self.sweep.take() {
            sweep.end_frame(self);
            self.sweep = Some(sweep);
        }
    }
}
