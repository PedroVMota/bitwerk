//! Application state, panels and command dispatch.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use bw_core::{
    Applied, Circuit, CircuitError, ComponentId, ComponentKind, DATA_WIDTHS, Edit, EvalMode, FfTrigger, FlipFlopType,
    GateOp, GridPos, MemoryType, NumFormat, SegmentId, SwitchMode, WireEnd,
};
use bw_render::{Camera, CanvasRenderer, DrawMode, FrameView, Scene, SymbolKind, SymbolStyle};
use bw_sim::{Command, RunState, SimError, SimHandle, StepMode};
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
/// Cells of the macro stress test (each a counter + 4-bit adder, four
/// levels of macros: 2 500 cells ≈ 37 500 instances, 70 000 primitives).
const STRESS_MACRO_CELLS: usize = 2_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Interact,
    Wire,
    Place(ComponentKind),
}

/// An action waiting for the unsaved-changes question.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    New,
    Open,
    /// Import a Digital Works file (`None`: ask for it).
    ImportDw(Option<PathBuf>),
    /// Open this project file (dropped on the window).
    OpenPath(PathBuf),
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
    pub(crate) path: Option<PathBuf>,
    pub(crate) name: String,
    pub(crate) dirty: bool,

    // Engines / workers
    pub sim: SimHandle,
    pub(crate) scene_worker: SceneWorker,
    /// Render scene, owned by the UI thread and patched in place by edits.
    pub scene: Box<Scene>,
    /// A full scene build is in flight; edits re-request it instead of
    /// patching the (stale) current scene.
    pub(crate) full_build_pending: bool,
    gpu: Option<egui_wgpu::RenderState>,
    stress_rx: Option<Receiver<Result<Circuit, bw_core::CircuitError>>>,
    /// Name of the stress circuit being generated.
    stress_name: &'static str,
    pub fit_on_next_scene: bool,
    /// Camera to apply when the next scene arrives (`--view`).
    initial_view: Option<Camera>,

    // UI state
    pub camera: Camera,
    pub tool: Tool,
    pub gate_inputs: u8,
    pub selection: Option<ComponentId>,
    /// Selected wire segment (exclusive with `selection`).
    pub selected_segment: Option<SegmentId>,
    /// Wire being drawn with the Wire tool.
    pub wire_draft: Option<crate::canvas::WireDraft>,
    pub drag: Option<crate::canvas::DragState>,
    /// Net highlighted under the cursor: (revision, hovered segment, its
    /// segments), recomputed only when either changes.
    pub hover_net: Option<(u64, SegmentId, Vec<SegmentId>)>,
    pub status: String,
    /// Schematic symbol style (View menu / palette). Runtime only: eframe's
    /// persistence is not enabled, so there is no settings store yet.
    pub style: SymbolStyle,
    step_mode: StepMode,
    tick_rate: f64,
    confirm: Option<Pending>,
    debug: DebugOpts,
    menu: Option<NativeMenu>,
    /// The Memory Contents window, if open.
    pub memory_window: Option<crate::memwin::MemoryWindow>,
    /// Macro levels opened below the root (see `hier`).
    pub levels: Vec<crate::hier::Level>,
    /// Further selected components (box select / Shift-click), besides
    /// `selection`.
    pub multi: BTreeSet<ComponentId>,
    /// Box selection being dragged (world corners).
    pub box_select: Option<(glam::Vec2, glam::Vec2)>,
    pub parts: crate::parts::PartsCentre,
    pub macro_draft: Option<crate::macro_props::Draft>,
    /// "Create Macro from Selection" name prompt.
    pub macro_name_prompt: Option<String>,
    /// Component under the cursor when the context menu opened.
    pub ctx_target: Option<ComponentId>,
    /// `--enter A/B/…`: instance path to open once loaded.
    enter_path: Vec<ComponentId>,
    /// Fit the view to the scene once the canvas size is known.
    pub fit_pending: bool,
    /// For worker threads that need to wake the UI.
    pub(crate) egui_ctx: egui::Context,
    /// Digital Works import (report window, workers).
    pub dw: crate::dwimport::DwState,
    /// Digital Works export (Save As… `.dwm`, macro export) state.
    pub dw_export: crate::dwexport::DwExportState,
    /// Drafts of free-text properties (notes, sequence patterns).
    pub prop_drafts: crate::kind_props::Drafts,
    /// Push button / pulse input held down with the Interact tool.
    pub pressed: Option<crate::canvas::Pressed>,

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
    #[allow(clippy::too_many_lines, reason = "start-up options, one after the other")]
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
            stress_name: "stress-test",
            fit_on_next_scene: false,
            initial_view: None,
            camera: Camera::default(),
            tool: Tool::Select,
            gate_inputs: 2,
            selection: None,
            selected_segment: None,
            wire_draft: None,
            drag: None,
            hover_net: None,
            status: "Ready".into(),
            style: SymbolStyle::Ansi,
            step_mode: StepMode::HalfCycle,
            tick_rate: 20.0,
            confirm: None,
            debug: DebugOpts::default(),
            menu,
            memory_window: None,
            levels: Vec::new(),
            multi: BTreeSet::new(),
            box_select: None,
            parts: crate::parts::PartsCentre::default(),
            macro_draft: None,
            macro_name_prompt: None,
            ctx_target: None,
            enter_path: Vec::new(),
            fit_pending: false,
            egui_ctx: cc.egui_ctx.clone(),
            dw: crate::dwimport::DwState::default(),
            dw_export: crate::dwexport::DwExportState::default(),
            prop_drafts: crate::kind_props::Drafts::default(),
            pressed: None,
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
        app.commit_full();
        let args: Vec<String> = std::env::args().skip(1).collect();
        // `--tick-rate N`: simulation speed (ticks per second) at start-up,
        // e.g. to watch a large example run without touching the toolbar.
        if let Some(rate) = args
            .iter()
            .position(|a| a == "--tick-rate")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|r| r.is_finite())
        {
            app.tick_rate = rate.clamp(0.1, bw_sim::engine::MAX_TICK_RATE);
        }
        app.sim.send(Command::SetTickRate(app.tick_rate));
        // Debug: `bitwerk --stress` loads the 100k stress test at start-up and
        // logs frame statistics; `--continuous` repaints every frame (for
        // measuring; normally the app only repaints when something changed).
        if args.iter().any(|a| a == "--stress") {
            app.debug.log_stats = true;
            app.spawn_stress(&cc.egui_ctx);
        }
        // `--stress-macros`: thousands of nested macro instances.
        if args.iter().any(|a| a == "--stress-macros") {
            app.debug.log_stats = true;
            app.spawn_stress_macros(&cc.egui_ctx);
        }
        // `--parts`: open the Parts Centre (screenshots / demos).
        if let Some(i) = args.iter().position(|a| a == "--parts") {
            app.parts.open = true;
            if let Some(key) = args.get(i + 1).filter(|k| !k.starts_with("--")) {
                app.parts.select(crate::parts::PartRef::Lib(key.clone()));
            }
        }
        // `--enter ID/ID/…`: open that macro instance path once loaded.
        if let Some(v) = args.iter().position(|a| a == "--enter").and_then(|i| args.get(i + 1)) {
            app.enter_path = v.split('/').filter_map(|t| t.trim().parse().ok()).map(ComponentId).collect();
        }
        // `--style iec|ansi`: initial symbol style.
        if let Some(v) = args.iter().position(|a| a == "--style").and_then(|i| args.get(i + 1)) {
            app.set_style(if v.eq_ignore_ascii_case("iec") { SymbolStyle::Iec } else { SymbolStyle::Ansi });
        }
        // `--no-report`: Digital Works imports do not open the report
        // window (`--report`, the default, shows it).
        app.dw.suppress_report = args.iter().any(|a| a == "--no-report") && !args.iter().any(|a| a == "--report");
        // `bitwerk <file.bitwerk>` opens a project at start-up; `bitwerk
        // <file.dwm>` imports a Digital Works circuit (synchronously, so the
        // options below apply to it) and shows the import report.
        if let Some(path) = args.iter().find(|a| {
            std::path::Path::new(a).extension().is_some_and(|e| e == PROJECT_EXT || e.eq_ignore_ascii_case("dwm"))
        }) {
            app.open_path(PathBuf::from(path));
        }
        // `--save-as-dwm OUT`: export the opened circuit as a Digital Works
        // file and show the Export report (automation, screenshots).
        if let Some(out) = args.iter().position(|a| a == "--save-as-dwm").and_then(|i| args.get(i + 1)) {
            app.export_dw_path(std::path::Path::new(out));
        }
        // `--import-dw-folder DIR`: converts the Digital Works macros below
        // DIR into the user library (Parts Centre, with progress).
        if let Some(dir) = args.iter().position(|a| a == "--import-dw-folder").and_then(|i| args.get(i + 1)) {
            app.start_folder_import(PathBuf::from(dir));
        }
        // `--select ID`: select a component (screenshots).
        if let Some(id) = args.iter().position(|a| a == "--select").and_then(|i| args.get(i + 1)) {
            app.selection = id.parse().ok().map(ComponentId);
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
        // `--run`: start the simulation right away (automation, demos).
        if args.iter().any(|a| a == "--run") {
            app.sim.send(Command::Run);
        }
        // `--memory-window ID`: open the Memory Contents window of component
        // `ID` (automation / screenshots).
        if let Some(id) = args.iter().position(|a| a == "--memory-window").and_then(|i| args.get(i + 1)) {
            if let Ok(id) = id.parse::<u32>() {
                crate::memwin::open(&mut app, ComponentId(id));
            }
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
    ///
    /// Inside a macro the edit changes the definition (see `hier`).
    pub fn apply_edit(&mut self, edit: Edit) -> Result<Applied, CircuitError> {
        if self.levels.is_empty() { self.apply_root_edit(edit) } else { self.apply_body_edit(&edit) }
    }

    /// Selected components (primary + box / Shift selection).
    #[must_use]
    pub fn selected_ids(&self) -> Vec<ComponentId> {
        let mut v: Vec<ComponentId> = self.multi.iter().copied().collect();
        if let Some(s) = self.selection {
            if !self.multi.contains(&s) {
                v.push(s);
            }
        }
        v.sort_unstable();
        v
    }

    /// Turns the selection into a macro named `name` (see
    /// [`bw_core::Circuit::plan_macro`]).
    pub fn create_macro_from_selection(&mut self, name: &str) {
        struct Target<'a>(&'a mut BitwerkApp);
        impl bw_core::PlanTarget for Target<'_> {
            fn define(&mut self, e: Edit) -> Result<Applied, CircuitError> {
                self.0.apply_root_edit(e)
            }
            fn edit(&mut self, e: Edit) -> Result<Applied, CircuitError> {
                self.0.apply_edit(e)
            }
            fn circuit(&self) -> &Circuit {
                self.0.shown()
            }
            fn table(&self) -> &bw_core::MacroTable {
                self.0.circuit.macros()
            }
        }
        let sel = self.selected_ids();
        let plan = match self.shown().plan_macro(&sel, name) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Cannot create the macro: {e}");
                return;
            }
        };
        let Some(id) = self.circuit.macros().free_id() else {
            self.status = "Too many macro definitions".into();
            return;
        };
        let pins = plan.links.len();
        self.clear_selection();
        match bw_core::run_macro_plan(&mut Target(self), plan, id) {
            Ok(inst) => {
                self.selection = Some(inst);
                self.status = format!("Created macro '{name}' with {pins} pins (double-click it to open)");
            }
            Err(e) => self.status = format!("Macro creation stopped: {e}"),
        }
    }

    /// Creates an empty macro definition and opens it.
    pub fn new_macro(&mut self) {
        let Some(id) = self.circuit.macros().free_id() else { return };
        let name = format!("Macro {}", id.0 + 1);
        match self.apply_root_edit(Edit::DefineMacro { id, def: Arc::new(bw_core::MacroDef::new(&name)) }) {
            Ok(_) => {
                self.enter_def(id);
                self.status = format!(
                    "New macro '{name}': add components and In / Out tags, then place it from the Parts Centre"
                );
            }
            Err(e) => self.status = e.to_string(),
        }
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

    pub(crate) fn replace_circuit(&mut self, circuit: Circuit, name: String, path: Option<PathBuf>) {
        self.drop_levels();
        self.pressed = None;
        if let Some(m) = &self.menu {
            m.sync_eval_mode(circuit.eval_mode());
        }
        self.multi.clear();
        self.macro_draft = None;
        self.circuit = Arc::new(circuit);
        self.name = name;
        self.path = path;
        self.dirty = false;
        crate::memwin::close(self);
        self.selection = None;
        self.selected_segment = None;
        self.wire_draft = None;
        self.drag = None;
        self.hover_net = None;
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
            MenuAction::CreateMacro => self.prompt_macro_name(),
            MenuAction::NewMacro => self.new_macro(),
            MenuAction::EditMacro => {
                if let Some(id) = self.selection {
                    self.enter_instance(id);
                }
            }
            MenuAction::UpLevel => self.go_up(),
            MenuAction::PartsCentre => self.parts.open = !self.parts.open,
            MenuAction::ImportMacro => self.import_macro_file(),
            MenuAction::StressMacros => self.spawn_stress_macros(ctx),
            MenuAction::ImportDw => self.guarded(Pending::ImportDw(None)),
            MenuAction::ImportDwFolder => self.import_dw_folder_dialog(),
            MenuAction::ExportDw => self.export_dw_dialog(),
            MenuAction::EvalMode(mode) => self.set_eval_mode(mode),
        }
    }

    /// Asks for a name, then creates a macro from the selection.
    pub fn prompt_macro_name(&mut self) {
        if self.selected_ids().is_empty() {
            self.status = "Select the components to turn into a macro first (drag a box or Shift-click)".into();
            return;
        }
        let n = self.circuit.macros().free_id().map_or(0, |m| m.0 + 1);
        self.macro_name_prompt = Some(format!("Macro {n}"));
    }

    /// Imports a Digital Works file (after the unsaved-changes question).
    pub fn request_import_dw(&mut self, path: PathBuf) {
        self.guarded(Pending::ImportDw(Some(path)));
    }

    /// Opens a project file (after the unsaved-changes question).
    pub fn request_open_path(&mut self, path: PathBuf) {
        self.guarded(Pending::OpenPath(path));
    }

    /// Chooses the simulator's propagation semantics (a root edit, also
    /// while a macro is open).
    pub fn set_eval_mode(&mut self, mode: EvalMode) {
        if self.circuit.eval_mode() != mode {
            match self.apply_root_edit(Edit::SetEvalMode(mode)) {
                Ok(_) => self.status = format!("Simulation mode: {}", mode.name()),
                Err(e) => self.status = e.to_string(),
            }
        }
        if let Some(m) = &self.menu {
            m.sync_eval_mode(self.circuit.eval_mode());
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
            Pending::ImportDw(None) => self.import_dw_dialog(),
            Pending::ImportDw(Some(path)) => self.import_dw_path(&path),
            Pending::OpenPath(path) => self.open_path(path),
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
        if is_dwm_path(&path) {
            self.import_dw_path(&path);
        } else {
            self.open_path(path);
        }
    }

    /// Opens a project file now (a `.dwm` file is imported, synchronously).
    fn open_path(&mut self, path: PathBuf) {
        if is_dwm_path(&path) {
            self.import_dw_now(&path);
            return;
        }
        match bw_io::load_from_path(&path) {
            Ok(p) => {
                let name = if p.name.is_empty() { file_stem(&path) } else { p.name };
                self.status = format!("Opened {}", path.display());
                self.replace_circuit(p.circuit, name, Some(path));
            }
            Err(e) => self.status = format!("Could not open {}: {e}", path.display()),
        }
    }

    /// Save (to the document's file, else as Save As…) or Save As…,
    /// which also offers the Digital Works format: choosing `.dwm` exports
    /// a lossy copy and leaves the document (its file, its unsaved state)
    /// as it was.
    fn save(&mut self, save_as: bool) {
        let path = if let (Some(p), false) = (&self.path, save_as) {
            p.clone()
        } else {
            let Some(mut p) = rfd::FileDialog::new()
                .set_title("Save circuit")
                .add_filter("bitwerk project", &[PROJECT_EXT])
                .add_filter("Digital Works 3.0.5 (.dwm, lossy export)", &[crate::dwexport::DWM_EXT])
                .set_file_name(format!("{}.{PROJECT_EXT}", self.name))
                .save_file()
            else {
                return;
            };
            if crate::dwexport::is_dwm(&p) {
                self.export_dw_path(&p);
                return;
            }
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
        self.spawn_generator(ctx, "stress-test", || crate::stress::generate(STRESS_COMPONENTS));
    }

    fn spawn_stress_macros(&mut self, ctx: &egui::Context) {
        self.spawn_generator(ctx, "stress-macros", || bw_io::library::stress_macros(STRESS_MACRO_CELLS));
    }

    fn spawn_generator(
        &mut self,
        ctx: &egui::Context,
        name: &'static str,
        f: impl FnOnce() -> Result<Circuit, CircuitError> + Send + 'static,
    ) {
        if self.stress_rx.is_some() {
            return;
        }
        self.stress_name = name;
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("bw-stress".into()).spawn(move || {
            let _ = tx.send(f());
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
                        self.status = format!(
                            "Stress test: {} components, {} wire segments",
                            c.component_count(),
                            c.segment_count()
                        );
                        self.replace_circuit(c, self.stress_name.into(), None);
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
                let old = std::mem::replace(self.root_scene_mut(), scene);
                self.scene_worker.retire(old);
                self.full_build_pending = false;
                if !self.enter_path.is_empty() {
                    self.fit_on_next_scene = false;
                    for id in std::mem::take(&mut self.enter_path) {
                        self.enter_instance(id);
                    }
                }
                if self.fit_on_next_scene && self.levels.is_empty() {
                    self.fit_on_next_scene = false;
                    self.fit_pending = true;
                }
                if let Some(cam) = self.initial_view.take() {
                    self.fit_pending = false;
                    self.camera = cam;
                }
            } else {
                self.scene_worker.retire(scene);
            }
        }
    }

    pub(crate) fn fit_view(&mut self) {
        let (lo, hi) = self.scene.bounds();
        if hi.cmple(lo).any() {
            return;
        }
        self.camera.center = (lo + hi) * 0.5;
        // Inside a macro leave room for the tag names drawn beside the tags.
        let margin = if self.levels.is_empty() { 0.0 } else { 6.0 };
        let ext = (hi - lo + glam::Vec2::splat(margin)).max(glam::Vec2::ONE);
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
            let mode = self.circuit.eval_mode();
            let mut chosen = mode;
            egui::ComboBox::from_id_salt("eval_mode")
                .selected_text(match mode {
                    EvalMode::UnitDelay => "Unit delay",
                    EvalMode::Sequential => "Sequential (DW)",
                })
                .show_ui(ui, |ui| {
                    for m in EvalMode::ALL {
                        ui.selectable_value(&mut chosen, m, m.name());
                    }
                })
                .response
                .on_hover_text(
                    "Simulation mode. Unit delay: every gate takes one step. Sequential: changes propagate \
                     immediately, as in Digital Works (imported circuits use it).",
                );
            if chosen != mode {
                self.set_eval_mode(chosen);
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
                ui.colored_label(egui::Color32::LIGHT_RED, self.describe_error(e));
            }
            if self.dw.busy() {
                ui.separator();
                ui.spinner();
                ui.label("Importing…");
            }
            if self.menu.is_none() {
                ui.separator();
                if ui.button("Open…").clicked() {
                    self.guarded(Pending::Open);
                }
                if ui.button("Save").clicked() {
                    self.save(false);
                }
                if ui.button("Save As…").clicked() {
                    self.save(true);
                }
                if ui.button("Import Digital Works…").clicked() {
                    self.guarded(Pending::ImportDw(None));
                }
                if ui.button("Stress 100k").clicked() {
                    self.spawn_stress(ui.ctx());
                }
            }
        });
    }

    /// Error message with component names (bus contention lists the
    /// drivers by kind and id).
    #[allow(clippy::unused_self)]
    fn describe_error(&self, e: &SimError) -> String {
        // The message names the place, with the macro instance path
        // (`root › Counter#3 › NAND#2`) when it is inside a macro.
        let s = e.to_string();
        let mut c = s.chars();
        c.next().map_or_else(String::new, |f| f.to_uppercase().collect::<String>() + c.as_str())
    }

    #[allow(clippy::too_many_lines, reason = "one palette section after the other")]
    fn palette(&mut self, ui: &mut egui::Ui) {
        ui.heading("Tools");
        ui.selectable_value(&mut self.tool, Tool::Select, "Select / move");
        ui.selectable_value(&mut self.tool, Tool::Interact, "Interact (switches, buttons)");
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
        let gates: Vec<ComponentKind> = GateOp::ALL
            .iter()
            .map(|&op| ComponentKind::Gate { op, inputs: self.gate_inputs })
            .chain([ComponentKind::Not, ComponentKind::TriState])
            .collect();
        self.palette_group(ui, "Gates", &gates);
        self.palette_group(
            ui,
            "Inputs",
            &[
                ComponentKind::Switch { on: false, mode: SwitchMode::Toggle },
                ComponentKind::Switch { on: false, mode: SwitchMode::Momentary },
                ComponentKind::Switch { on: false, mode: SwitchMode::Pulse },
                ComponentKind::Clock { half_period: 10 },
                ComponentKind::SeqGen { pattern: 0b0110, len: 4, half_period: 10 },
                ComponentKind::Const { high: true },
                ComponentKind::Const { high: false },
            ],
        );
        self.palette_group(
            ui,
            "Outputs",
            &[
                ComponentKind::Led,
                ComponentKind::SevenSeg { common_anode: false },
                ComponentKind::Numeric { bits: 4, format: NumFormat::Hex, signed: false },
            ],
        );
        self.palette_group(
            ui,
            "Flip-flops & memory",
            &[
                ComponentKind::flip_flop(FlipFlopType::D),
                ComponentKind::flip_flop(FlipFlopType::Rs),
                ComponentKind::flip_flop(FlipFlopType::Jk),
                ComponentKind::memory(MemoryType::Ram),
                ComponentKind::memory(MemoryType::Rom),
            ],
        );
        self.palette_group(ui, "Notes", &[bw_core::Note::default().kind()]);
        let style = self.style;
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Macros");
            if ui.button("Parts Centre").on_hover_text("Browse and place macros (Shift+Cmd+P)").clicked() {
                self.parts.open = !self.parts.open;
            }
        });
        egui::Grid::new("palette-macros").spacing([4.0, 4.0]).show(ui, |ui| {
            for (kind, caption, tip) in [
                (
                    ComponentKind::Tag { dir: bw_core::TagDir::In, tristate: false },
                    "In tag",
                    "Macro input pin (use inside a macro definition)",
                ),
                (
                    ComponentKind::Tag { dir: bw_core::TagDir::Out, tristate: false },
                    "Out tag",
                    "Macro output pin (use inside a macro definition)",
                ),
            ] {
                let selected = matches!(self.tool, Tool::Place(k) if k == kind);
                if crate::icons::button(ui, SymbolKind::of(&kind), style, caption, tip, selected, true).clicked() {
                    self.tool = Tool::Place(kind);
                }
            }
            if crate::icons::button(ui, SymbolKind::Macro, style, "New", "New empty macro definition", false, true)
                .clicked()
            {
                self.new_macro();
            }
            ui.end_row();
        });
        if ui.button("Create Macro from Selection…").on_hover_text("Shift+Cmd+M").clicked() {
            self.prompt_macro_name();
        }
        ui.separator();
        ui.small(
            "Scroll wheel / pinch: zoom\nTrackpad scroll or drag empty space: pan\nR: rotate · Del: delete · Esc: cancel\n\
             Wire: click a pin or a wire, click to bend, finish on a pin or a wire · / or Space: flip the L",
        );
    }

    /// One palette section: a titled grid of symbol buttons, three per row.
    fn palette_group(&mut self, ui: &mut egui::Ui, title: &str, kinds: &[ComponentKind]) {
        let style = self.style;
        egui::CollapsingHeader::new(title).default_open(true).show(ui, |ui| {
            egui::Grid::new(("palette", title)).spacing([4.0, 4.0]).show(ui, |ui| {
                for (i, &kind) in kinds.iter().enumerate() {
                    let selected = matches!(self.tool, Tool::Place(k) if same_palette_entry(k, kind));
                    if crate::icons::button(
                        ui,
                        palette_symbol(kind),
                        style,
                        palette_caption(kind),
                        &palette_tip(kind),
                        selected,
                        true,
                    )
                    .clicked()
                    {
                        self.tool = Tool::Place(kind);
                    }
                    if i % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
        });
    }

    #[allow(clippy::too_many_lines, reason = "one editor per component kind")]
    fn properties(&mut self, ui: &mut egui::Ui) {
        ui.heading("Properties");
        if let Some(seg) = self.selected_segment {
            self.segment_properties(ui, seg);
            return;
        }
        let selected = self.selected_ids();
        if selected.len() > 1 {
            ui.label(format!("{} components selected", selected.len()));
            if ui.button("Create Macro from Selection…").clicked() {
                self.prompt_macro_name();
            }
            if ui.button("Delete all").clicked() {
                for id in selected {
                    self.edit_delete(id);
                }
                self.clear_selection();
            }
            return;
        }
        let Some(id) = self.selection else {
            if let Some(def) = self.current_def() {
                crate::macro_props::editor(self, ui, def);
                return;
            }
            ui.weak("Nothing selected");
            ui.separator();
            ui.label(format!("Project: {}{}", self.name, if self.dirty { " •" } else { "" }));
            ui.label(format!("{} components", self.circuit.component_count()));
            ui.label(format!("{} wire segments", self.circuit.segment_count()));
            ui.label(format!("{} junctions", self.scene.dot_count()));
            if !self.circuit.macros().is_empty() {
                ui.label(format!(
                    "{} macro definitions · {} components flattened",
                    self.circuit.macros().len(),
                    self.circuit.flat_size()
                ));
            }
            return;
        };
        let Some(comp) = self.shown().component(id).cloned() else {
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
            ComponentKind::FlipFlop { ty, trigger, preset_clear } => {
                egui::ComboBox::from_label("type").selected_text(ty.name()).show_ui(ui, |ui| {
                    for t in FlipFlopType::ALL {
                        ui.selectable_value(ty, t, t.name());
                    }
                });
                egui::ComboBox::from_label("trigger").selected_text(trigger.name()).show_ui(ui, |ui| {
                    for t in FfTrigger::ALL {
                        let ok =
                            ComponentKind::FlipFlop { ty: *ty, trigger: t, preset_clear: *preset_clear }.is_valid();
                        ui.add_enabled_ui(ok, |ui| ui.selectable_value(trigger, t, t.name()));
                    }
                });
                if *ty == FlipFlopType::Jk && *trigger == FfTrigger::Latch {
                    *trigger = FfTrigger::MasterSlave;
                }
                ui.checkbox(preset_clear, "preset / clear pins");
            }
            ComponentKind::Memory { ty, addr_bits, data_bits } => {
                egui::ComboBox::from_label("type").selected_text(ty.name()).show_ui(ui, |ui| {
                    ui.selectable_value(ty, MemoryType::Ram, "RAM");
                    ui.selectable_value(ty, MemoryType::Rom, "ROM");
                });
                ui.add(
                    egui::DragValue::new(addr_bits)
                        .range(bw_core::MIN_ADDR_BITS..=bw_core::MAX_ADDR_BITS)
                        .prefix("address lines "),
                );
                egui::ComboBox::from_label("data bits").selected_text(data_bits.to_string()).show_ui(ui, |ui| {
                    for d in DATA_WIDTHS {
                        ui.selectable_value(data_bits, d, d.to_string());
                    }
                });
                ui.label(format!("{} words × {} bits", 1u32 << *addr_bits, *data_bits));
                let root = self.levels.is_empty();
                if ui
                    .add_enabled(root, egui::Button::new("Edit contents…"))
                    .on_disabled_hover_text("Memory contents can be edited at the root level only")
                    .clicked()
                {
                    crate::memwin::open(self, id);
                }
            }
            ComponentKind::Tag { dir, tristate } => {
                let def = self.current_def();
                let pin = def
                    .and_then(|d| self.circuit.macros().def(d))
                    .and_then(|d| d.pins.iter().find(|p| p.tag == id).cloned());
                match (def, pin) {
                    (Some(_), Some(p)) => {
                        ui.label(format!("Interface pin '{}'", p.name));
                        ui.small("Rename and reorder pins in the macro's properties (click empty space).");
                    }
                    _ => {
                        ui.weak("Tags only matter inside a macro definition.");
                    }
                }
                if *dir == bw_core::TagDir::Out {
                    ui.checkbox(tristate, "tri-state output (may share a bus)");
                }
            }
            ComponentKind::Macro(shape) => {
                let def = shape.def;
                if ui.button("Open (Edit Macro)").clicked() {
                    self.enter_instance(id);
                    return;
                }
                ui.separator();
                crate::macro_props::editor(self, ui, def);
            }
            ComponentKind::Not | ComponentKind::Led | ComponentKind::TriState => {}
            k => crate::kind_props::kind_editor(self, ui, id, k),
        }
        if kind != comp.kind {
            let same_mode = matches!((kind, comp.kind),
                (ComponentKind::Switch { mode: a, .. }, ComponentKind::Switch { mode: b, .. }) if a == b);
            let res = if let (ComponentKind::Switch { on, .. }, true) = (kind, self.levels.is_empty() && same_mode) {
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
                self.status = "Disconnect the wires before changing the pins of this component".into();
            }
        }
        crate::kind_props::extra_editor(self, ui, id, &comp.kind, comp.color);
        ui.separator();
        if ui.button("Rotate (R)").clicked() {
            self.rotate_selected();
        }
        if ui.button("Delete (Del)").clicked() {
            self.delete_selected();
        }
    }

    /// Properties of a selected wire segment.
    fn segment_properties(&mut self, ui: &mut egui::Ui, seg: SegmentId) {
        let (Some(sg), Some((a, b))) = (self.shown().segment(seg).copied(), self.shown().segment_ends(seg)) else {
            self.selected_segment = None;
            return;
        };
        ui.label("Wire segment");
        ui.label(format!("({}, {}) → ({}, {})", a.x, a.y, b.x, b.y));
        let len = (b.x - a.x).abs() + (b.y - a.y).abs();
        ui.label(format!("length {len}"));
        let drivers = self.shown().net_drivers(sg.a);
        let net = match drivers.as_slice() {
            [] => "net: no driver (floating)".to_string(),
            [d] => {
                let label = self.shown().component(d.component).map_or("?", |c| c.kind.label());
                format!("net driven by {label} #{}", d.component.0)
            }
            many => format!("net: bus of {} tri-state drivers", many.len()),
        };
        ui.label(net);
        ui.separator();
        ui.small("Drag it to move it sideways.\nDrag a junction or bend to move it.");
        if ui.button("Delete segment (Del)").clicked() {
            self.delete_selected();
        }
    }

    pub fn rotate_selected(&mut self) {
        if let Some(id) = self.selection {
            self.edit_rotate(id);
        }
    }

    pub fn delete_selected(&mut self) {
        if let Some(seg) = self.selected_segment.take() {
            if let Err(e) = self.apply_edit(Edit::RemoveSegment(seg)) {
                self.status = e.to_string();
            }
        } else {
            for id in self.selected_ids() {
                self.edit_delete(id);
            }
            self.clear_selection();
        }
    }

    // ---- edits ---------------------------------------------------------
    // Every model change goes through one of these so the simulator and the
    // render scene are kept in sync.

    pub fn edit_move(&mut self, id: ComponentId, to: GridPos) -> bool {
        self.apply_edit(Edit::MoveComponent(id, to)).is_ok()
    }

    pub fn edit_rotate(&mut self, id: ComponentId) -> bool {
        let Some(rot) = self.shown().component(id).map(|c| c.rotation.rotated_cw()) else { return false };
        self.apply_edit(Edit::SetRotation(id, rot)).is_ok()
    }

    pub fn edit_place(&mut self, kind: ComponentKind, origin: GridPos) -> Result<ComponentId, CircuitError> {
        self.apply_edit(Edit::AddComponent { kind, pos: origin, rotation: bw_core::Rotation::R0 })?
            .component()
            .ok_or(CircuitError::IdSpaceExhausted)
    }

    /// Draws a wire between two ends through `via` (see [`Edit::AddWire`]).
    pub fn edit_wire(&mut self, from: WireEnd, to: WireEnd, via: Vec<GridPos>) -> Result<Applied, CircuitError> {
        self.apply_edit(Edit::AddWire { from, to, via })
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
            r.sync_macros(&rs.device, &rs.queue, self.circuit.macros());
            // Live levels only when the simulator maps nets onto the circuit
            // being shown (the root, or that very instance).
            let path = self.view_path();
            let matches = match (&snap.view, &path) {
                (Some(v), Some(p)) => **v == **p,
                _ => false,
            };
            r.prepare(
                &rs.device,
                &rs.queue,
                &mut self.scene,
                view,
                matches.then_some(&levels as &dyn bw_render::NetLevels),
            );
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
        // Wire tool: '/' or Space flips the L of the leg being drawn.
        let flip = ctx.input(|i| i.key_pressed(egui::Key::Slash) || i.key_pressed(egui::Key::Space));
        if let Some(d) = self.wire_draft.as_mut() {
            if flip {
                d.vertical_first = !d.vertical_first;
            }
            if del {
                // While drawing, Delete / Backspace removes the last bend.
                if d.bends.pop().is_none() {
                    self.wire_draft = None;
                }
            }
        } else if del {
            self.delete_selected();
        }
        if esc {
            if self.wire_draft.is_some() {
                self.wire_draft = None;
            } else if self.drag.is_some() {
                self.drag = None;
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

    /// Breadcrumbs (`root › Counter#3 › …`), "Up one level" and the
    /// all-instances warning.
    fn breadcrumb_bar(&mut self, ui: &mut egui::Ui) {
        let crumbs = self.breadcrumbs();
        let mut go = None;
        ui.horizontal(|ui| {
            if ui.button("Up one level").on_hover_text("Cmd+Up").clicked() {
                go = Some(self.levels.len() - 1);
            }
            ui.separator();
            for (i, c) in crumbs.iter().enumerate() {
                if i > 0 {
                    ui.label("›");
                }
                let last = i + 1 == crumbs.len();
                if ui.add_enabled(!last, egui::Button::new(c).small()).clicked() {
                    go = Some(i);
                }
            }
            if let Some(def) = self.current_def() {
                let n = self.circuit.total_instances(def);
                ui.separator();
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 90),
                    format!("Editing the definition: changes affect all {n} instance{}", if n == 1 { "" } else { "s" }),
                );
                if self.view_path().is_none() {
                    ui.weak("(no live state: opened as a definition)");
                }
            }
        });
        if let Some(d) = go {
            self.go_to_depth(d);
        }
    }

    fn macro_name_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut name) = self.macro_name_prompt.take() else { return };
        let mut keep = true;
        let mut create = false;
        egui::Window::new("Create Macro from Selection")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} components become a macro; wires crossing the selection become its pins.",
                    self.selected_ids().len()
                ));
                let r = ui.text_edit_singleline(&mut name);
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("Create").clicked() {
                        create = true;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            });
        if create && !name.trim().is_empty() {
            self.create_macro_from_selection(name.trim());
        } else if keep {
            self.macro_name_prompt = Some(name);
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = self.confirm.clone() else { return };
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
                        self.run_pending(p.clone());
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

/// Same palette entry (gates by operation, flip-flops and memories by
/// type, switches by mode, power by level, others by kind)?
fn same_palette_entry(a: ComponentKind, b: ComponentKind) -> bool {
    match (a, b) {
        (ComponentKind::Gate { op: x, .. }, ComponentKind::Gate { op: y, .. }) => x == y,
        (ComponentKind::FlipFlop { ty: x, .. }, ComponentKind::FlipFlop { ty: y, .. }) => x == y,
        (ComponentKind::Memory { ty: x, .. }, ComponentKind::Memory { ty: y, .. }) => x == y,
        (ComponentKind::Switch { mode: x, .. }, ComponentKind::Switch { mode: y, .. }) => x == y,
        (ComponentKind::Const { high: x }, ComponentKind::Const { high: y }) => x == y,
        _ => std::mem::discriminant(&a) == std::mem::discriminant(&b),
    }
}

/// Icon of a palette entry: gates show the 2-input variant (the placed
/// gate uses the input count setting), memories a small variant (the
/// 256 × 8 default is very tall for an icon).
fn palette_symbol(kind: ComponentKind) -> SymbolKind {
    match kind {
        ComponentKind::Gate { op, .. } => SymbolKind::Gate(op, 2),
        ComponentKind::Memory { ty, .. } => SymbolKind::Memory(ty, 4, 1),
        k => SymbolKind::of(&k),
    }
}

/// Caption under a palette icon.
fn palette_caption(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Switch { mode: SwitchMode::Toggle, .. } => "Switch",
        ComponentKind::Switch { mode: SwitchMode::Momentary, .. } => "Push",
        ComponentKind::Switch { mode: SwitchMode::Pulse, .. } => "Pulse",
        ComponentKind::Const { high: false } => "GND",
        ComponentKind::SevenSeg { .. } => "7-seg",
        ComponentKind::Numeric { .. } => "Number",
        k => k.label(),
    }
}

/// Tooltip of a palette entry.
fn palette_tip(kind: ComponentKind) -> String {
    let name = SymbolKind::of(&kind).name();
    match kind {
        ComponentKind::Gate { op, inputs } => format!("{} gate ({inputs} inputs)", op.name()),
        ComponentKind::FlipFlop { trigger, .. } => format!("{name} ({}; change it in Properties)", trigger.name()),
        ComponentKind::Memory { addr_bits, data_bits, .. } => {
            format!("{name} ({} words × {data_bits} bits; change it in Properties)", 1u32 << addr_bits)
        }
        ComponentKind::Switch { mode: SwitchMode::Toggle, .. } => {
            "Toggle switch (click it with the Interact tool)".into()
        }
        ComponentKind::Switch { mode: SwitchMode::Momentary, .. } => {
            "Push button: on while held down with the Interact tool".into()
        }
        ComponentKind::Switch { mode: SwitchMode::Pulse, .. } => {
            format!("Pulse input: square wave, {} ticks per half period", bw_core::PULSE_HALF_PERIOD)
        }
        ComponentKind::SeqGen { .. } => "Sequence generator: a repeating pattern of up to 64 steps".into(),
        ComponentKind::Const { high: true } => "Power supply (VCC, logic 1)".into(),
        ComponentKind::Const { high: false } => "Ground (logic 0)".into(),
        ComponentKind::Numeric { .. } => "Numeric output (binary, hex or decimal; set the bits in Properties)".into(),
        ComponentKind::Text { .. } => "Text annotation (edit it in Properties)".into(),
        _ => name.to_string(),
    }
}

/// A Digital Works circuit file (by extension)?
fn is_dwm_path(p: &std::path::Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dwm"))
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
        self.poll_dw();
        self.poll_dw_export();
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
        if !self.levels.is_empty() {
            egui::Panel::top("breadcrumbs").show(ui, |ui| self.breadcrumb_bar(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
            });
        });
        egui::Panel::left("palette").resizable(false).default_size(170.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.palette(ui));
        });
        egui::Panel::right("properties").resizable(true).default_size(250.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.properties(ui));
        });
        crate::parts::show(self, ui);
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = crate::canvas::canvas_ui(self, ui, &snap);
            self.canvas_size = glam::Vec2::new(rect.width(), rect.height());
            if std::mem::take(&mut self.fit_pending) {
                self.fit_view();
                ctx.request_repaint();
            }
            self.stats_overlay(ui, rect);
        });
        crate::memwin::show(self, &ctx);
        crate::dwimport::show_report(self, &ctx);
        crate::dwexport::show_report(self, &ctx);
        self.handle_dropped_files(&ctx);
        crate::dwimport::drop_hint(&ctx);
        self.confirm_dialog(&ctx);
        self.macro_name_dialog(&ctx);
        if let Some(mut sweep) = self.sweep.take() {
            sweep.end_frame(self);
            self.sweep = Some(sweep);
        }
    }
}
