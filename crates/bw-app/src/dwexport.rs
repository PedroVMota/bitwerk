//! Digital Works export in the app:
//!
//! * File → Save As… (⌘⇧S) offers "Digital Works 3.0.5 (.dwm)" next to the
//!   bitwerk format, and File → Export as Digital Works… asks for a `.dwm`
//!   directly. The export is lossy, so it is an *export*: the document keeps
//!   its own path (⌘S still saves the `.bitwerk` project) and its unsaved
//!   state; the status bar says so.
//! * Macro properties → "Export as Digital Works macro…" writes one
//!   definition as a Digital Works macro (its tags are the interface).
//! * The export (graph build, serialisation, read-back check, atomic write)
//!   runs on a worker thread (`bw-dw-export`) with a shared copy of the
//!   circuit; the Export report window then shows the [`ExportReport`]:
//!   what was written, what was approximated or dropped, "Copy as text".
//! * `bitwerk FILE --export-dwm OUT` exports without opening a window and
//!   prints the report (automation, tests).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bw_core::{Circuit, MacroId, MacroTable};
use bw_io::dwm::{ExportReport, Severity};
use crossbeam_channel::Receiver;
use egui::Color32;

use crate::app::BitwerkApp;

/// Extension of Digital Works circuits and macros.
pub const DWM_EXT: &str = "dwm";

/// What a finished export worker sends back.
type ExportResult = (PathBuf, Result<ExportReport, String>);

/// Digital Works export state of the app.
#[derive(Default)]
pub struct DwExportState {
    job: Option<Receiver<ExportResult>>,
    /// The last export's file and report (or error).
    pub report: Option<(PathBuf, Result<ExportReport, String>)>,
    pub report_open: bool,
}

/// What to export.
enum Job {
    Circuit { circuit: Arc<Circuit>, name: String },
    Macro { table: MacroTable, id: MacroId },
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(String::new, |f| f.to_string_lossy().into_owned())
}

/// Builds, checks and atomically writes one export (worker thread or CLI).
fn run(job: &Job, path: &Path) -> Result<ExportReport, String> {
    let name = file_name(path);
    let (bytes, report) = match job {
        Job::Circuit { circuit, name: n } => bw_io::dwm::export_circuit(circuit, n, &name),
        Job::Macro { table, id } => bw_io::dwm::export_macro(table, *id, &name),
    }
    .map_err(|e| e.to_string())?;
    bw_io::write_bytes_atomic(&bytes, path).map_err(|e| e.to_string())?;
    Ok(report)
}

/// `path` with the `.dwm` extension.
fn with_dwm_ext(mut path: PathBuf) -> PathBuf {
    if path.extension().is_none_or(|e| !e.eq_ignore_ascii_case(DWM_EXT)) {
        path.set_extension(DWM_EXT);
    }
    path
}

/// Is this a path the user chose for a Digital Works file?
#[must_use]
pub fn is_dwm(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(DWM_EXT))
}

impl BitwerkApp {
    /// File → Export as Digital Works…: asks for a `.dwm` file.
    pub fn export_dw_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export as Digital Works 3.0.5 circuit")
            .add_filter("Digital Works 3.0.5 (.dwm)", &[DWM_EXT])
            .set_file_name(format!("{}.{DWM_EXT}", self.name))
            .save_file()
        else {
            return;
        };
        self.export_dw_path(&with_dwm_ext(path));
    }

    /// Exports the project (root circuit) to `path` in the background.
    pub fn export_dw_path(&mut self, path: &Path) {
        let job = Job::Circuit { circuit: Arc::clone(&self.circuit), name: self.name.clone() };
        self.spawn_export(job, path);
    }

    /// Macro properties → Export as Digital Works macro…
    pub fn export_dw_macro(&mut self, id: MacroId) {
        let name = self.circuit.macros().def(id).map_or_else(|| "macro".into(), |d| d.name.clone());
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export macro as Digital Works 3.0.5 macro")
            .add_filter("Digital Works 3.0.5 macro (.dwm)", &[DWM_EXT])
            .set_file_name(format!(
                "{}.{DWM_EXT}",
                bw_io::library::sanitize_name(&name).unwrap_or_else(|| "macro".into())
            ))
            .save_file()
        else {
            return;
        };
        let job = Job::Macro { table: self.circuit.macros().clone(), id };
        self.spawn_export(job, &with_dwm_ext(path));
    }

    fn spawn_export(&mut self, job: Job, path: &Path) {
        if self.dw_export.job.is_some() {
            self.status = "A Digital Works export is already running".into();
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = self.egui_ctx.clone();
        let target = path.to_path_buf();
        let spawned = std::thread::Builder::new().name("bw-dw-export".into()).spawn(move || {
            let r = run(&job, &target);
            let _ = tx.send((target, r));
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => {
                self.dw_export.job = Some(rx);
                self.status = format!("Exporting {} as Digital Works…", path.display());
            }
            Err(e) => self.status = format!("Could not start the export: {e}"),
        }
    }

    /// Collects a finished export (called once per frame; O(1)).
    pub(crate) fn poll_dw_export(&mut self) {
        let Some(rx) = &self.dw_export.job else { return };
        let Ok((path, r)) = rx.try_recv() else { return };
        self.dw_export.job = None;
        self.status = match &r {
            Ok(rep) => {
                let lossy = rep.total(Severity::Unsupported) + rep.total(Severity::Approximation);
                format!(
                    "Exported a Digital Works copy to {}{} — the document is unchanged{} (Cmd+S saves the bitwerk project)",
                    path.display(),
                    if lossy > 0 {
                        format!(" ({lossy} approximations / omissions, see the report)")
                    } else {
                        String::new()
                    },
                    if self.dirty { " and still has unsaved changes" } else { "" },
                )
            }
            Err(e) => format!("Could not export {}: {e}", path.display()),
        };
        tracing::info!("{}", self.status);
        self.dw_export.report = Some((path, r));
        self.dw_export.report_open = true;
    }
}

/// `--export-dwm OUT`: exports `input` (a `.bitwerk` project or a `.dwm`
/// import) without a window and prints the report. Returns the exit code.
#[must_use]
pub fn cli(input: Option<&str>, out: &str) -> i32 {
    let Some(input) = input else {
        eprintln!("--export-dwm needs an input file (.bitwerk or .dwm)");
        return 2;
    };
    let input = Path::new(input);
    let loaded = if is_dwm(input) {
        bw_io::read_capped(input)
            .and_then(|b| bw_io::dwm::import_project(&b, &file_name(input)))
            .map(|(p, _)| p)
            .map_err(|e| e.to_string())
    } else {
        bw_io::load_from_path(input).map_err(|e| e.to_string())
    };
    let project = match loaded {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}: {e}", input.display());
            return 1;
        }
    };
    let name = if project.name.is_empty() {
        input.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned())
    } else {
        project.name
    };
    let job = Job::Circuit { circuit: Arc::new(project.circuit), name };
    match run(&job, &with_dwm_ext(PathBuf::from(out))) {
        Ok(report) => {
            print!("{}", report.to_text());
            0
        }
        Err(e) => {
            eprintln!("export failed: {e}");
            1
        }
    }
}

/// Note severities, most severe first.
const SEVERITIES: [(Severity, &str, Color32); 3] = [
    (Severity::Unsupported, "Dropped (Digital Works cannot represent it)", Color32::from_rgb(255, 130, 110)),
    (Severity::Warning, "Warnings", Color32::from_rgb(240, 200, 100)),
    (Severity::Approximation, "Approximations", Color32::from_rgb(150, 190, 240)),
];

/// The Export report window.
pub fn show_report(app: &mut BitwerkApp, ctx: &egui::Context) {
    if !app.dw_export.report_open {
        return;
    }
    let Some((path, report)) = app.dw_export.report.as_ref() else {
        app.dw_export.report_open = false;
        return;
    };
    let mut open = true;
    let mut close = false;
    egui::Window::new("Digital Works export")
        .open(&mut open)
        .default_size([480.0, 440.0])
        .default_pos(egui::pos2(320.0, 110.0))
        .resizable(true)
        .show(ctx, |ui| {
            match report {
                Ok(r) => report_body(ui, path, r),
                Err(e) => {
                    ui.colored_label(Color32::LIGHT_RED, "The circuit could not be exported.");
                    ui.label(e);
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                if let Ok(r) = report {
                    if ui.button("Copy as text").clicked() {
                        ui.ctx().copy_text(r.to_text());
                    }
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
    app.dw_export.report_open = open && !close;
}

fn report_body(ui: &mut egui::Ui, path: &Path, r: &ExportReport) {
    ui.heading(r.name.as_str());
    ui.add(egui::Label::new(egui::RichText::new(path.display().to_string()).small().weak()).truncate());
    ui.label(format!(
        "Digital Works 3.x format (version {}) · {} bytes · {:.1} ms",
        r.version,
        r.bytes,
        r.elapsed.as_secs_f64() * 1000.0
    ));
    ui.horizontal_wrapped(|ui| {
        ui.weak(
            "This is a lossy copy: the document keeps its own file, and Cmd+S still saves it as a bitwerk project.",
        );
    });
    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
        egui::CollapsingHeader::new("Written").default_open(true).show(ui, |ui| {
            egui::Grid::new("dwx-counts").num_columns(2).striped(true).show(ui, |ui| {
                for (what, n) in &r.counts {
                    ui.label(what);
                    ui.label(n.to_string());
                    ui.end_row();
                }
                if r.counts.is_empty() {
                    ui.weak("nothing");
                    ui.end_row();
                }
            });
            if r.macro_copies > 0 {
                ui.label(format!(
                    "{} embedded macro cop{} of {} definition{} (Digital Works copies a macro into every instance)",
                    r.macro_copies,
                    if r.macro_copies == 1 { "y" } else { "ies" },
                    r.macro_defs,
                    if r.macro_defs == 1 { "" } else { "s" }
                ));
            }
        });
        if r.notes.is_empty() {
            ui.label("Nothing was approximated or dropped.");
        }
        for (sev, title, color) in SEVERITIES {
            let notes: Vec<_> = r.notes.iter().filter(|n| n.severity == sev).collect();
            if notes.is_empty() {
                continue;
            }
            let total: usize = notes.iter().map(|n| n.count).sum();
            let header = egui::RichText::new(format!("{title}: {total}")).color(color);
            egui::CollapsingHeader::new(header).id_salt(title).default_open(true).show(ui, |ui| {
                for n in notes {
                    ui.horizontal_wrapped(|ui| {
                        if n.count > 1 {
                            ui.weak(format!("{}x", n.count));
                        }
                        ui.label(&n.message);
                    });
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dwm_paths() {
        assert_eq!(with_dwm_ext(PathBuf::from("/a/b")), PathBuf::from("/a/b.dwm"));
        assert_eq!(with_dwm_ext(PathBuf::from("/a/b.DWM")), PathBuf::from("/a/b.DWM"));
        assert_eq!(with_dwm_ext(PathBuf::from("/a/b.bitwerk")), PathBuf::from("/a/b.dwm"));
        assert!(is_dwm(Path::new("x.Dwm")));
        assert!(!is_dwm(Path::new("x.bitwerk")));
    }

    #[test]
    fn cli_exports_an_example() {
        let dir = std::env::temp_dir().join(format!("bw-dwx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/half-adder.bitwerk");
        let out = dir.join("ha.dwm");
        assert_eq!(cli(input.to_str(), out.to_str().unwrap()), 0);
        let bytes = std::fs::read(&out).unwrap();
        assert!(bw_io::dwm::is_dwm(&bytes));
        assert!(bw_io::dwm::import_project(&bytes, "ha.dwm").is_ok());
        assert_eq!(cli(None, "x"), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
