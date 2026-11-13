//! Native macOS menu bar (muda). On other platforms the egui toolbar is the
//! only menu.

use bw_render::SymbolStyle;
use crossbeam_channel::Receiver;

/// Actions the menu bar can trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    New,
    Open,
    Save,
    SaveAs,
    Quit,
    Run,
    Stop,
    Pause,
    Step,
    StressTest,
    ToggleContinuousRepaint,
    Style(SymbolStyle),
    CreateMacro,
    NewMacro,
    EditMacro,
    UpLevel,
    PartsCentre,
    ImportMacro,
    StressMacros,
}

const ITEMS: &[(&str, MenuAction)] = &[
    ("file.new", MenuAction::New),
    ("file.open", MenuAction::Open),
    ("file.save", MenuAction::Save),
    ("file.save_as", MenuAction::SaveAs),
    ("app.quit", MenuAction::Quit),
    ("circuit.run", MenuAction::Run),
    ("circuit.stop", MenuAction::Stop),
    ("circuit.pause", MenuAction::Pause),
    ("circuit.step", MenuAction::Step),
    ("debug.stress", MenuAction::StressTest),
    ("debug.repaint", MenuAction::ToggleContinuousRepaint),
    ("view.ansi", MenuAction::Style(SymbolStyle::Ansi)),
    ("view.iec", MenuAction::Style(SymbolStyle::Iec)),
    ("macro.create", MenuAction::CreateMacro),
    ("macro.new", MenuAction::NewMacro),
    ("macro.edit", MenuAction::EditMacro),
    ("macro.up", MenuAction::UpLevel),
    ("macro.parts", MenuAction::PartsCentre),
    ("macro.import", MenuAction::ImportMacro),
    ("debug.stress_macros", MenuAction::StressMacros),
];

pub struct NativeMenu {
    #[cfg(target_os = "macos")]
    _menu: muda::Menu,
    /// View → symbol style check items (ANSI, IEC).
    #[cfg(target_os = "macos")]
    style_items: (muda::CheckMenuItem, muda::CheckMenuItem),
    rx: Receiver<MenuAction>,
}

impl NativeMenu {
    /// Installs the menu bar. Returns `None` (and logs) if that fails or the
    /// platform is not macOS.
    pub fn install(ctx: &egui::Context) -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            match build(ctx) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!("native menu unavailable: {e}");
                    None
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = ctx;
            None
        }
    }

    pub fn poll(&self) -> impl Iterator<Item = MenuAction> + '_ {
        self.rx.try_iter()
    }

    /// Shows which symbol style is active (radio-like check marks).
    pub fn sync_style(&self, style: SymbolStyle) {
        #[cfg(target_os = "macos")]
        {
            self.style_items.0.set_checked(style == SymbolStyle::Ansi);
            self.style_items.1.set_checked(style == SymbolStyle::Iec);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = style;
    }
}

#[cfg(target_os = "macos")]
fn build(ctx: &egui::Context) -> Result<NativeMenu, muda::Error> {
    use muda::accelerator::Accelerator;
    use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    let accel = |s: &str| s.parse::<Accelerator>().ok();
    let item = |id: &str, text: &str, acc: Option<&str>| MenuItem::with_id(id, text, true, acc.and_then(accel));

    let app_menu = Submenu::with_items(
        "bitwerk",
        true,
        &[
            &PredefinedMenuItem::about(Some("About bitwerk"), None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::separator(),
            &item("app.quit", "Quit bitwerk", Some("CmdOrCtrl+Q")),
        ],
    )?;
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &item("file.new", "New", Some("CmdOrCtrl+N")),
            &item("file.open", "Open…", Some("CmdOrCtrl+O")),
            &PredefinedMenuItem::separator(),
            &item("file.save", "Save", Some("CmdOrCtrl+S")),
            &item("file.save_as", "Save As…", Some("CmdOrCtrl+Shift+S")),
        ],
    )?;
    let circuit = Submenu::with_items(
        "Circuit",
        true,
        &[
            &item("circuit.run", "Run", Some("F9")),
            &item("circuit.stop", "Stop", Some("F10")),
            &item("circuit.pause", "Pause / Resume", Some("F11")),
            &item("circuit.step", "Step", Some("F12")),
        ],
    )?;
    let macros = Submenu::with_items(
        "Macro",
        true,
        &[
            &item("macro.create", "Create Macro from Selection…", Some("CmdOrCtrl+Shift+M")),
            &item("macro.new", "New Macro", None),
            &item("macro.edit", "Edit Macro", Some("CmdOrCtrl+E")),
            &item("macro.up", "Up One Level", Some("CmdOrCtrl+Up")),
            &PredefinedMenuItem::separator(),
            &item("macro.parts", "Parts Centre", Some("CmdOrCtrl+Shift+P")),
            &item("macro.import", "Import .bwmacro…", None),
        ],
    )?;
    let ansi = CheckMenuItem::with_id("view.ansi", "ANSI Symbols (distinctive shapes)", true, true, None);
    let iec = CheckMenuItem::with_id("view.iec", "IEC Symbols (rectangular)", true, false, None);
    let view = Submenu::with_items("View", true, &[&ansi, &iec])?;
    let debug = Submenu::with_items(
        "Debug",
        true,
        &[
            &item("debug.stress", "Spawn Stress Test (100k components)", Some("CmdOrCtrl+Shift+T")),
            &item("debug.stress_macros", "Spawn Macro Stress Test (2 500 nested cells)", None),
            &CheckMenuItem::with_id("debug.repaint", "Continuous Repaint", true, false, None),
        ],
    )?;
    let menu = Menu::with_items(&[&app_menu, &file, &view, &circuit, &macros, &debug])?;
    menu.init_for_nsapp();

    let (tx, rx) = crossbeam_channel::unbounded();
    let ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        if let Some((_, action)) = ITEMS.iter().find(|(id, _)| ev.id.0 == *id) {
            let _ = tx.send(*action);
            ctx.request_repaint();
        }
    }));
    Ok(NativeMenu { _menu: menu, style_items: (ansi, iec), rx })
}
