//! Digital Works import in the app:
//!
//! * File → Import Digital Works… (and `.dwm` files dropped on the window or
//!   named on the command line): the file is read with a size cap and
//!   imported by [`bw_io::dwm::import_project`] on a worker thread (the
//!   command line imports synchronously, so the other start-up flags apply
//!   to the imported circuit). The result replaces the open project (after
//!   the usual unsaved-changes question); it has no path, so Save asks
//!   where to save it as a `.bitwerk` file.
//! * The Import report window shows the [`ImportReport`]: counts, macro
//!   copies vs definitions, the notes grouped by severity, and a "Copy as
//!   text" button.
//! * Parts Centre → Import Digital Works folder…: every `.dwm` macro below a
//!   folder is converted into the user library (`<library>/Imported`) by
//!   [`bw_io::dwm::import_folder`] on a worker thread, with a progress bar,
//!   Cancel, and a summary; the library is rescanned at the end.
//! * Drag and drop: `.dwm` → import, `.bitwerk` → open, `.bwmacro` →
//!   import into the project, a folder → folder import into the library.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bw_io::dwm::{FolderImport, ImportReport, Severity};
use crossbeam_channel::Receiver;
use egui::Color32;

use crate::app::BitwerkApp;

/// Result of a single-file import worker.
type FileResult = (PathBuf, Result<(bw_io::Project, ImportReport), String>);

/// Digital Works import state of the app.
#[derive(Default)]
pub struct DwState {
    /// The last import's report (or error), shown in the report window.
    pub report: Option<Result<ImportReport, String>>,
    pub report_open: bool,
    /// `--no-report`: imports do not open the report window.
    pub suppress_report: bool,
    file_job: Option<Receiver<FileResult>>,
    folder: Option<FolderJob>,
    folder_result: Option<FolderOutcome>,
}

impl DwState {
    /// A single-file import is running.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.file_job.is_some()
    }
}

#[derive(Debug, Clone, Default)]
struct Progress {
    done: usize,
    total: usize,
    current: String,
}

/// A folder import running on a worker thread.
struct FolderJob {
    src: PathBuf,
    dest: PathBuf,
    progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<FolderImport, String>>,
    started: Instant,
}

/// A finished folder import, summarised in the Parts Centre.
struct FolderOutcome {
    src: PathBuf,
    dest: PathBuf,
    result: Result<FolderImport, String>,
    elapsed: Duration,
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(String::new, |f| f.to_string_lossy().into_owned())
}

/// Reads and imports one `.dwm` file (worker thread or start-up).
fn import_file(path: &Path) -> Result<(bw_io::Project, ImportReport), String> {
    let bytes = bw_io::read_capped(path).map_err(|e| e.to_string())?;
    bw_io::dwm::import_project(&bytes, &file_name(path)).map_err(|e| e.to_string())
}

/// A project name usable as a default file name (no path separators).
fn project_name(report: &ImportReport, project: &bw_io::Project, path: &Path) -> String {
    let raw = [report.name.as_str(), project.name.as_str()].into_iter().find(|s| !s.trim().is_empty()).map_or_else(
        || path.file_stem().map_or_else(|| "imported".into(), |s| s.to_string_lossy().into_owned()),
        str::to_string,
    );
    let clean: String =
        raw.chars().map(|c| if matches!(c, '/' | '\\' | ':') || c.is_control() { '_' } else { c }).take(80).collect();
    let clean = clean.trim().to_string();
    if clean.is_empty() { "imported".into() } else { clean }
}

impl BitwerkApp {
    /// File → Import Digital Works…: asks for a `.dwm` file and imports it
    /// in the background.
    pub fn import_dw_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Import Digital Works circuit")
            .add_filter("Digital Works circuit", &["dwm", "DWM"])
            .pick_file()
        else {
            return;
        };
        self.import_dw_path(&path);
    }

    /// Imports a `.dwm` file on a worker thread; the result arrives in
    /// [`poll_dw`](Self::poll_dw).
    pub fn import_dw_path(&mut self, path: &Path) {
        if self.dw.file_job.is_some() {
            self.status = "An import is already running".into();
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = self.egui_ctx.clone();
        let p = path.to_path_buf();
        let spawned = std::thread::Builder::new().name("bw-dw-import".into()).spawn(move || {
            let r = import_file(&p);
            let _ = tx.send((p, r));
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => {
                self.dw.file_job = Some(rx);
                self.status = format!("Importing {}…", path.display());
            }
            Err(e) => self.status = format!("Could not start the import: {e}"),
        }
    }

    /// Imports a `.dwm` file right away (start-up: the other command-line
    /// options then apply to the imported circuit).
    pub fn import_dw_now(&mut self, path: &Path) {
        let r = import_file(path);
        self.finish_dw_import(path, r);
    }

    fn finish_dw_import(&mut self, path: &Path, r: Result<(bw_io::Project, ImportReport), String>) {
        match r {
            Ok((project, report)) => {
                let name = project_name(&report, &project, path);
                self.replace_circuit(project.circuit, name, None);
                // Not saved as a bitwerk project yet: New / Open ask first.
                self.mark_dirty();
                // Not a bitwerk file: Save asks for a new name.
                self.status = format!(
                    "Imported {} ({} mode) — Save stores it as a .bitwerk project",
                    path.display(),
                    self.circuit.eval_mode().name()
                );
                tracing::info!(file = %path.display(), notes = report.notes.len(), "Digital Works import");
                self.dw.report = Some(Ok(report));
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), "Digital Works import failed: {e}");
                self.status = format!("Could not import {}: {e}", path.display());
                self.dw.report = Some(Err(format!("{}: {e}", file_name(path))));
            }
        }
        self.dw.report_open = !self.dw.suppress_report;
    }

    /// Parts Centre → Import Digital Works folder…
    pub fn import_dw_folder_dialog(&mut self) {
        let Some(src) = rfd::FileDialog::new().set_title("Import a folder of Digital Works macros").pick_folder()
        else {
            return;
        };
        self.start_folder_import(src);
    }

    /// Converts every `.dwm` macro below `src` into the user library
    /// (`<library>/Imported`) on a worker thread.
    pub fn start_folder_import(&mut self, src: PathBuf) {
        if self.dw.folder.is_some() {
            self.status = "A folder import is already running".into();
            return;
        }
        let Some(dest) = bw_io::library::user_dir().map(|d| d.join("Imported")) else {
            self.status = "No user library folder (HOME not set)".into();
            return;
        };
        self.parts.open = true;
        let progress = Arc::new(Mutex::new(Progress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = self.egui_ctx.clone();
        let (shared, stop) = (Arc::clone(&progress), Arc::clone(&cancel));
        let (from, into) = (src.clone(), dest.clone());
        let spawned = std::thread::Builder::new().name("bw-dw-folder".into()).spawn(move || {
            let mut on_file = |done: usize, total: usize, current: &Path| {
                if let Ok(mut g) = shared.lock() {
                    g.done = done;
                    g.total = total;
                    g.current = current.strip_prefix(&from).unwrap_or(current).display().to_string();
                }
                ctx.request_repaint();
                !stop.load(Ordering::Relaxed)
            };
            let result = bw_io::dwm::import_folder(&from, &into, &mut on_file).map_err(|e| e.to_string());
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => {
                self.dw.folder_result = None;
                self.dw.folder = Some(FolderJob { src, dest, progress, cancel, rx, started: Instant::now() });
                self.status = "Importing Digital Works macros into the library…".into();
            }
            Err(e) => self.status = format!("Could not start the folder import: {e}"),
        }
    }

    /// Collects finished import workers (called once per frame; O(1)).
    pub(crate) fn poll_dw(&mut self) {
        if let Some(rx) = &self.dw.file_job {
            if let Ok((path, r)) = rx.try_recv() {
                self.dw.file_job = None;
                self.finish_dw_import(&path, r);
            }
        }
        if let Some(job) = &self.dw.folder {
            if let Ok(result) = job.rx.try_recv() {
                let Some(job) = self.dw.folder.take() else { return };
                self.status = match &result {
                    Ok(f) => format!(
                        "Digital Works folder import{}: {} written, {} duplicates, {} not macros, {} failed",
                        if f.cancelled { " cancelled" } else { "" },
                        f.written.len(),
                        f.duplicates.len(),
                        f.not_macros.len(),
                        f.failed.len()
                    ),
                    Err(e) => format!("Folder import failed: {e}"),
                };
                tracing::info!("{}", self.status);
                self.dw.folder_result =
                    Some(FolderOutcome { src: job.src, dest: job.dest, result, elapsed: job.started.elapsed() });
                self.parts.rescan();
            }
        }
    }

    /// Files dropped on the window: `.dwm` → import, `.bitwerk` → open,
    /// `.bwmacro` → import into the project, a folder → library import.
    pub(crate) fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        // One file at a time: opening or importing replaces the project.
        let Some(path) = dropped.into_iter().next() else { return };
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if path.is_dir() {
            self.start_folder_import(path);
        } else if ext == "dwm" {
            self.request_import_dw(path);
        } else if ext == crate::app::PROJECT_EXT {
            self.request_open_path(path);
        } else if ext == bw_io::bwmacro::MACRO_EXT {
            self.import_macro_path(&path);
        } else {
            self.status = format!("Cannot open {} (drop a .bitwerk, .dwm or .bwmacro file)", path.display());
        }
    }
}

/// "Drop to open" hint while files are dragged over the window.
pub fn drop_hint(ctx: &egui::Context) {
    let hovering = ctx.input(|i| i.raw.hovered_files.first().map(|f| f.path.clone()));
    let Some(path) = hovering else { return };
    let what = match path.as_ref().and_then(|p| p.extension()).map(|e| e.to_string_lossy().to_ascii_lowercase()) {
        Some(e) if e == "dwm" => "Drop to import the Digital Works circuit",
        Some(e) if e == crate::app::PROJECT_EXT => "Drop to open the project",
        Some(e) if e == bw_io::bwmacro::MACRO_EXT => "Drop to import the macro into this project",
        _ => "Drop a .bitwerk, .dwm or .bwmacro file, or a folder of Digital Works macros",
    };
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop-hint")));
    let rect = ctx.content_rect();
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(140));
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, what, egui::FontId::proportional(22.0), Color32::WHITE);
}

/// Display name of a note severity, most severe first.
const SEVERITIES: [(Severity, &str, Color32); 3] = [
    (Severity::Unsupported, "Unsupported (imported as something else or left out)", Color32::from_rgb(255, 130, 110)),
    (Severity::Warning, "Warnings (ignored or repaired data)", Color32::from_rgb(240, 200, 100)),
    (Severity::Approximation, "Approximations", Color32::from_rgb(150, 190, 240)),
];

/// The Import report window.
pub fn show_report(app: &mut BitwerkApp, ctx: &egui::Context) {
    if !app.dw.report_open {
        return;
    }
    let Some(report) = app.dw.report.as_ref() else {
        app.dw.report_open = false;
        return;
    };
    let mut open = true;
    let mut close = false;
    let mode = app.circuit.eval_mode().name();
    egui::Window::new("Import report")
        .open(&mut open)
        .default_size([460.0, 420.0])
        .default_pos(egui::pos2(300.0, 120.0))
        .resizable(true)
        .show(ctx, |ui| {
            match report {
                Ok(r) => report_body(ui, r, mode),
                Err(e) => {
                    ui.colored_label(Color32::LIGHT_RED, "The file could not be imported.");
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
    app.dw.report_open = open && !close;
}

fn report_body(ui: &mut egui::Ui, r: &ImportReport, mode: &str) {
    ui.heading(if r.name.is_empty() { r.source.as_str() } else { r.name.as_str() });
    ui.label(format!("{} · Digital Works format version {}", r.source, r.version));
    ui.label(format!("Imported in {:.1} ms · simulation mode: {mode}", r.elapsed.as_secs_f64() * 1000.0));
    if let Some(ds) = &r.datasheet {
        ui.horizontal_wrapped(|ui| {
            ui.weak("Datasheet (kept as a note, not opened):");
            ui.label(ds);
        });
    }
    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
        egui::CollapsingHeader::new("Imported objects").default_open(true).show(ui, |ui| {
            egui::Grid::new("dw-counts").num_columns(2).striped(true).show(ui, |ui| {
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
            if r.macro_copies > 0 || r.macro_defs > 0 {
                ui.label(format!(
                    "{} embedded macro cop{} → {} definition{}",
                    r.macro_copies,
                    if r.macro_copies == 1 { "y" } else { "ies" },
                    r.macro_defs,
                    if r.macro_defs == 1 { "" } else { "s" }
                ));
            }
        });
        if r.notes.is_empty() {
            ui.label("No approximations, warnings or unsupported features.");
        }
        for (sev, title, color) in SEVERITIES {
            let notes: Vec<_> = r.notes.iter().filter(|n| n.severity == sev).collect();
            if notes.is_empty() {
                continue;
            }
            let total: usize = notes.iter().map(|n| n.count).sum();
            let header = egui::RichText::new(format!("{title}: {total}")).color(color);
            egui::CollapsingHeader::new(header).id_salt(title).default_open(sev != Severity::Approximation).show(
                ui,
                |ui| {
                    for n in notes {
                        ui.horizontal_wrapped(|ui| {
                            if n.count > 1 {
                                ui.weak(format!("{}×", n.count));
                            }
                            ui.label(&n.message);
                        });
                    }
                },
            );
        }
    });
}

/// The Digital Works section at the top of the Parts Centre: the folder
/// import button, its progress while running, then the summary.
pub fn parts_section(app: &mut BitwerkApp, ui: &mut egui::Ui) {
    if let Some(job) = &app.dw.folder {
        let p = job.progress.lock().map(|g| g.clone()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("Importing Digital Works macros…");
            if ui.button("Cancel").clicked() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        });
        #[allow(clippy::cast_precision_loss)]
        let frac = if p.total == 0 { 0.0 } else { p.done as f32 / p.total as f32 };
        ui.add(egui::ProgressBar::new(frac).text(format!("{} / {}", p.done, p.total)).desired_width(f32::INFINITY));
        ui.add(egui::Label::new(egui::RichText::new(&p.current).small().weak()).truncate());
        if job.cancel.load(Ordering::Relaxed) {
            ui.weak("Cancelling after the current file…");
        }
        ui.separator();
        return;
    }
    let mut dismiss = false;
    if let Some(o) = &app.dw.folder_result {
        folder_summary(ui, o, &mut dismiss);
        ui.separator();
    }
    if dismiss {
        app.dw.folder_result = None;
    }
}

fn path_list(ui: &mut egui::Ui, id: &str, title: &str, rows: &[String]) {
    if rows.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(format!("{title} ({})", rows.len())).id_salt(id).show(ui, |ui| {
        let h = ui.text_style_height(&egui::TextStyle::Small) + 2.0;
        egui::ScrollArea::vertical().id_salt(id).max_height(160.0).show_rows(ui, h, rows.len(), |ui, range| {
            for r in &rows[range] {
                ui.add(egui::Label::new(egui::RichText::new(r).small()).truncate());
            }
        });
    });
}

fn folder_summary(ui: &mut egui::Ui, o: &FolderOutcome, dismiss: &mut bool) {
    ui.horizontal(|ui| {
        ui.strong("Digital Works folder import");
        if ui.small_button("Dismiss").clicked() {
            *dismiss = true;
        }
    });
    ui.add(egui::Label::new(egui::RichText::new(o.src.display().to_string()).small().weak()).truncate());
    let f = match &o.result {
        Ok(f) => f,
        Err(e) => {
            ui.colored_label(Color32::LIGHT_RED, e);
            return;
        }
    };
    if f.cancelled {
        ui.colored_label(Color32::from_rgb(240, 200, 100), "Cancelled (the files written so far stay)");
    }
    ui.label(format!(
        "{} written · {} duplicates · {} not macros · {} failed · {:.1} s",
        f.written.len(),
        f.duplicates.len(),
        f.not_macros.len(),
        f.failed.len(),
        o.elapsed.as_secs_f64()
    ));
    ui.add(egui::Label::new(egui::RichText::new(format!("into {}", o.dest.display())).small().weak()).truncate());
    let rel =
        |p: &PathBuf| p.strip_prefix(&o.src).or_else(|_| p.strip_prefix(&o.dest)).unwrap_or(p).display().to_string();
    let written: Vec<String> = f.written.iter().map(rel).collect();
    let dups: Vec<String> = f.duplicates.iter().map(rel).collect();
    let not_macros: Vec<String> = f.not_macros.iter().map(rel).collect();
    let failed: Vec<String> = f.failed.iter().map(|(p, e)| format!("{}: {e}", rel(p))).collect();
    path_list(ui, "dw-failed", "Failed", &failed);
    path_list(ui, "dw-written", "Written", &written);
    path_list(ui, "dw-dups", "Already in the library", &dups);
    path_list(ui, "dw-notmacros", "Not macros (circuits without tags)", &not_macros);
    let notes: usize = f.reports.iter().map(|r| r.notes.len()).sum();
    if notes > 0 {
        ui.weak(format!(
            "{notes} import notes in {} files (import a file on its own to see its report)",
            f.reports.len()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names_are_file_safe() {
        let p = bw_io::Project { name: String::new(), circuit: bw_core::Circuit::new() };
        let r = ImportReport { name: "a/b:c".into(), ..ImportReport::default() };
        assert_eq!(project_name(&r, &p, Path::new("/x/y.dwm")), "a_b_c");
        let r = ImportReport::default();
        assert_eq!(project_name(&r, &p, Path::new("/x/counter.dwm")), "counter");
        let r = ImportReport { name: "  \n ".into(), ..ImportReport::default() };
        assert_eq!(project_name(&r, &p, Path::new("/x/q.dwm")), "q");
    }
}
