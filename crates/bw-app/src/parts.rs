//! The Parts Centre: browse macros by folder (this project's definitions,
//! the built-in library and the user's own parts), search, preview the
//! symbol and pins, read the datasheet, and place a part by clicking
//! "Place" (then clicking on the canvas) or by dragging it onto the canvas.
//! User parts can be added (from a project definition), renamed, moved to
//! another folder and deleted.

use std::path::PathBuf;

use bw_core::{ComponentKind, Edit, LibraryRef, MacroId};
use bw_io::library::{self, BadPart, LibEntry};

use crate::app::{BitwerkApp, Tool};

/// What a Parts Centre row refers to (also the drag-and-drop payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartRef {
    /// A library part, by key.
    Lib(String),
    /// A definition of the open project.
    Project(MacroId),
}

#[derive(Default)]
pub struct PartsCentre {
    pub open: bool,
    /// Shown as a side panel instead of a floating window.
    pub docked: bool,
    search: String,
    selected: Option<PartRef>,
    user: Vec<LibEntry>,
    user_bad: Vec<BadPart>,
    scanned: bool,
    new_name: String,
    new_folder: String,
    confirm_delete: Option<PathBuf>,
}

impl PartsCentre {
    /// The user library folder (created on first use).
    fn dir() -> Option<PathBuf> {
        library::user_dir()
    }

    pub fn rescan(&mut self) {
        self.scanned = true;
        if let Some(dir) = Self::dir() {
            let _ = library::ensure_dir(&dir);
            (self.user, self.user_bad) = library::scan(&dir);
        }
    }

    pub fn select(&mut self, part: PartRef) {
        self.selected = Some(part);
    }

    /// A library part by key (built-in or user).
    #[must_use]
    pub fn find(&self, key: &str) -> Option<&LibEntry> {
        library::builtin().iter().chain(&self.user).find(|e| e.key == key)
    }
}

impl BitwerkApp {
    /// Folder of the open project file (relative datasheet links).
    #[must_use]
    pub fn project_dir(&self) -> Option<PathBuf> {
        self.path.as_ref().and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    }

    /// Brings a library part (and everything it uses) into the project;
    /// identical definitions already present are reused.
    pub fn import_part(&mut self, key: &str) -> Option<MacroId> {
        if !self.parts.scanned {
            self.parts.rescan();
        }
        let e = self.parts.find(key)?.clone();
        let plan = self.circuit.macros().import_plan(&e.file.table, e.file.root);
        let (defs, root) = match plan {
            Ok(p) => p,
            Err(err) => {
                self.status = format!("Could not import {}: {err}", e.name);
                return None;
            }
        };
        for (id, def) in defs {
            let mut def = (*def).clone();
            if id == root {
                def.origin = Some(LibraryRef { key: e.key.clone(), hash: e.file.hash() });
            }
            if let Err(err) = self.apply_root_edit(Edit::DefineMacro { id, def: std::sync::Arc::new(def) }) {
                self.status = format!("Could not import {}: {err}", e.name);
                return None;
            }
        }
        Some(root)
    }

    /// Selects the Place tool for a part (importing it first).
    pub fn place_part(&mut self, part: &PartRef) -> Option<ComponentKind> {
        let id = match part {
            PartRef::Lib(key) => self.import_part(key)?,
            PartRef::Project(id) => *id,
        };
        if self.current_def().is_some_and(|d| d == id || self.circuit.macros().reaches(id, d)) {
            self.status = "A macro cannot contain itself".into();
            return None;
        }
        let kind = ComponentKind::Macro(self.circuit.macros().get(id)?.shape);
        self.tool = Tool::Place(kind);
        self.status = "Click on the canvas to place the part".into();
        Some(kind)
    }

    /// Replaces a definition imported from the library by the library's
    /// current version (same id: every instance follows).
    pub fn update_from_library(&mut self, id: MacroId) {
        let Some(key) = self.circuit.macros().def(id).and_then(|d| d.origin.clone()).map(|o| o.key) else { return };
        let Some(e) = self.parts.find(&key).cloned() else { return };
        let Ok((defs, root)) = self.circuit.macros().import_plan(&e.file.table, e.file.root) else { return };
        for (nid, def) in defs {
            let mut def = (*def).clone();
            let target = if nid == root {
                def.origin = Some(LibraryRef { key: key.clone(), hash: e.file.hash() });
                id
            } else {
                nid
            };
            if let Err(err) = self.redefine(target, def) {
                self.status = format!("Update refused: {err}");
                return;
            }
        }
        if root != id && self.circuit.macros().get(root).is_some() {
            let _ = self.apply_root_edit(Edit::RemoveMacro(root));
        }
        self.status = format!("Updated '{}' from the library", e.name);
    }

    pub fn export_macro(&mut self, id: MacroId) {
        let name = self.circuit.macros().def(id).map_or_else(|| "macro".into(), |d| d.name.clone());
        let Some(mut path) = rfd::FileDialog::new()
            .set_title("Export macro")
            .add_filter("bitwerk macro", &[bw_io::bwmacro::MACRO_EXT])
            .set_file_name(format!("{name}.{}", bw_io::bwmacro::MACRO_EXT))
            .save_file()
        else {
            return;
        };
        if path.extension().is_none_or(|e| e != bw_io::bwmacro::MACRO_EXT) {
            path.set_extension(bw_io::bwmacro::MACRO_EXT);
        }
        self.status = match bw_io::bwmacro::save_to_path(self.circuit.macros(), id, &path) {
            Ok(()) => format!("Exported {}", path.display()),
            Err(e) => format!("Could not export: {e}"),
        };
    }

    pub fn add_to_library(&mut self, id: MacroId) {
        let Some(dir) = PartsCentre::dir() else {
            self.status = "No user library folder (HOME not set)".into();
            return;
        };
        let name = self.circuit.macros().def(id).map_or_else(|| "macro".into(), |d| d.name.clone());
        self.status = match library::save_user(&dir, "My parts", &name, self.circuit.macros(), id) {
            Ok(p) => format!("Saved to {}", p.display()),
            Err(e) => format!("Could not save to the library: {e}"),
        };
        self.parts.rescan();
    }

    /// Imports a `.bwmacro` file chosen by the user into the project.
    pub fn import_macro_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Import macro")
            .add_filter("bitwerk macro", &[bw_io::bwmacro::MACRO_EXT])
            .pick_file()
        else {
            return;
        };
        self.import_macro_path(&path);
    }

    /// Imports a `.bwmacro` file into the project (its definitions are
    /// added; identical ones are reused).
    pub fn import_macro_path(&mut self, path: &std::path::Path) {
        match bw_io::bwmacro::load_from_path(path) {
            Ok(f) => match self.circuit.macros().import_plan(&f.table, f.root) {
                Ok((defs, root)) => {
                    for (id, def) in defs {
                        if let Err(e) = self.apply_root_edit(Edit::DefineMacro { id, def }) {
                            self.status = format!("Could not import: {e}");
                            return;
                        }
                    }
                    self.parts.selected = Some(PartRef::Project(root));
                    self.parts.open = true;
                    self.status = format!("Imported {}", path.display());
                }
                Err(e) => self.status = format!("Could not import: {e}"),
            },
            Err(e) => self.status = format!("Could not read {}: {e}", path.display()),
        }
    }
}

struct Row {
    part: PartRef,
    folder: String,
    name: String,
    detail: String,
}

/// The Parts Centre, as a window or docked side panel.
pub fn show(app: &mut BitwerkApp, ui: &mut egui::Ui) {
    if !app.parts.open {
        return;
    }
    if !app.parts.scanned {
        app.parts.rescan();
    }
    if app.parts.docked {
        egui::Panel::right("parts-centre").resizable(true).default_size(330.0).show(ui, |ui| contents(app, ui));
    } else {
        let mut open = app.parts.open;
        egui::Window::new("Parts Centre")
            .open(&mut open)
            .default_size([660.0, 540.0])
            .resizable(true)
            .show(ui.ctx(), |ui| contents(app, ui));
        app.parts.open = open && app.parts.open;
    }
}

#[allow(clippy::too_many_lines, reason = "one panel")]
fn contents(app: &mut BitwerkApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut app.parts.search).hint_text("Search parts").desired_width(170.0));
        if ui.small_button("x").clicked() {
            app.parts.search.clear();
        }
        ui.checkbox(&mut app.parts.docked, "Docked");
        if ui.small_button("Rescan").on_hover_text("Re-read the user library folder").clicked() {
            app.parts.rescan();
        }
        if ui.small_button("Import…").on_hover_text("Import a .bwmacro file into this project").clicked() {
            app.import_macro_file();
        }
    });
    if ui
        .small_button("Import Digital Works folder…")
        .on_hover_text("Convert every Digital Works macro (.dwm) below a folder into My parts › Imported")
        .clicked()
    {
        app.import_dw_folder_dialog();
    }
    crate::dwimport::parts_section(app, ui);
    let q = app.parts.search.to_lowercase();
    let mut rows: Vec<Row> = Vec::new();
    for (id, e) in app.circuit.macros().iter() {
        let n = app.circuit.total_instances(id);
        rows.push(Row {
            part: PartRef::Project(id),
            folder: "This project".into(),
            name: e.def.name.clone(),
            detail: format!("{n} in use"),
        });
    }
    for e in library::builtin().iter().chain(&app.parts.user) {
        rows.push(Row {
            part: PartRef::Lib(e.key.clone()),
            folder: if e.is_builtin() {
                format!("Built-in › {}", e.folder)
            } else {
                format!("My parts › {}", e.folder)
            },
            name: e.name.clone(),
            detail: String::new(),
        });
    }
    rows.retain(|r| q.is_empty() || r.name.to_lowercase().contains(&q) || r.folder.to_lowercase().contains(&q));
    let mut clicked: Option<PartRef> = None;
    let mut place: Option<PartRef> = None;
    ui.columns(2, |cols| {
        egui::ScrollArea::vertical().id_salt("parts-list").show(&mut cols[0], |ui| {
            let mut folder = String::new();
            for r in &rows {
                if r.folder != folder {
                    folder.clone_from(&r.folder);
                    ui.add_space(4.0);
                    ui.strong(&folder);
                }
                let sel = app.parts.selected.as_ref() == Some(&r.part);
                let text = if r.detail.is_empty() { r.name.clone() } else { format!("{}  ({})", r.name, r.detail) };
                let resp = ui
                    .dnd_drag_source(ui.scope_id().with(("part", &r.folder, &r.name)), r.part.clone(), |ui| {
                        ui.selectable_label(sel, text)
                    })
                    .response;
                if resp.clicked() {
                    clicked = Some(r.part.clone());
                }
                if resp.double_clicked() {
                    place = Some(r.part.clone());
                }
            }
            if rows.is_empty() {
                ui.weak("No parts match.");
            }
            for b in &app.parts.user_bad {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("{}: {}", b.path.display(), b.error));
            }
        });
        details(app, &mut cols[1], &mut place);
    });
    if let Some(p) = clicked {
        app.parts.selected = Some(p);
    }
    if let Some(p) = place {
        app.place_part(&p);
    }
}

#[allow(clippy::too_many_lines, reason = "one panel")]
fn details(app: &mut BitwerkApp, ui: &mut egui::Ui, place: &mut Option<PartRef>) {
    let Some(sel) = app.parts.selected.clone() else {
        ui.weak("Select a part to see its symbol, pins and datasheet. Drag it onto the canvas or press Place.");
        return;
    };
    // The definition to show (library parts from their own table).
    let (def, shape, hash, user_path) = match &sel {
        PartRef::Project(id) => match app.circuit.macros().get(*id) {
            Some(e) => (std::sync::Arc::clone(&e.def), e.shape, e.hash, None),
            None => return,
        },
        PartRef::Lib(key) => match app.parts.find(key) {
            Some(e) => match e.file.table.get(e.file.root) {
                Some(m) => (std::sync::Arc::clone(&m.def), m.shape, m.hash, e.path.clone()),
                None => return,
            },
            None => return,
        },
    };
    ui.heading(&def.name);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 150.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(29, 31, 35));
    let mesh = crate::icons::macro_mesh(&def, &shape, hash);
    let (w, h) = ComponentKind::Macro(shape).size();
    #[allow(clippy::cast_precision_loss)]
    crate::icons::paint_macro_fit(ui.painter(), rect, &mesh, [w as f32, h as f32], 1.0);
    ui.horizontal(|ui| {
        if ui.button("Place").clicked() {
            *place = Some(sel.clone());
        }
        if let PartRef::Project(id) = sel {
            if ui.button("Open").clicked() {
                app.enter_def(id);
            }
            let used = app.circuit.total_instances(id) > 0 || !app.circuit.macros().users(id).is_empty();
            if ui.add_enabled(!used, egui::Button::new("Remove")).on_hover_text("Remove an unused definition").clicked()
            {
                if let Err(e) = app.apply_root_edit(Edit::RemoveMacro(id)) {
                    app.status = e.to_string();
                }
                app.parts.selected = None;
            }
        }
    });
    let pins: Vec<String> = def
        .interface()
        .iter()
        .map(|p| {
            let dir = if p.dir == bw_core::TagDir::In { "in" } else { "out" };
            let num = if shape.dil > 0 { format!(" · {}", def.pins[p.index].dil) } else { String::new() };
            format!("{} ({dir}{num})", def.pins[p.index].name)
        })
        .collect();
    ui.label(format!("Pins: {}", pins.join(", ")));
    if shape.dil > 0 {
        ui.label(format!("Template: DIL-{}", shape.dil));
    }
    egui::ScrollArea::vertical().id_salt("datasheet").max_height(180.0).show(ui, |ui| {
        if def.datasheet.is_empty() {
            ui.weak("No datasheet.");
        } else {
            let mut text = def.datasheet.as_str();
            ui.add(egui::TextEdit::multiline(&mut text).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
        }
    });
    if let Some(link) = def.doc_link.clone() {
        if ui.button("Open datasheet file").clicked() {
            crate::macro_props::open_datasheet(app, &link);
        }
    }
    if let (Some(path), Some(dir)) = (user_path, PartsCentre::dir()) {
        ui.separator();
        ui.small(path.display().to_string());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.parts.new_name).hint_text("new name").desired_width(100.0));
            if ui.button("Rename").clicked() {
                let r = library::rename_user(&dir, &path, &app.parts.new_name);
                app.status = r.map_or_else(|e| e.to_string(), |p| format!("Renamed to {}", p.display()));
                app.parts.rescan();
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.parts.new_folder).hint_text("folder").desired_width(100.0));
            if ui.button("Move").clicked() {
                let r = library::move_user(&dir, &path, &app.parts.new_folder);
                app.status = r.map_or_else(|e| e.to_string(), |p| format!("Moved to {}", p.display()));
                app.parts.rescan();
            }
        });
        if app.parts.confirm_delete.as_ref() == Some(&path) {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::LIGHT_RED, "Delete this part file?");
                if ui.button("Delete").clicked() {
                    let r = library::delete_user(&dir, &path);
                    app.status = r.map_or_else(|e| e.to_string(), |()| "Part deleted".into());
                    app.parts.confirm_delete = None;
                    app.parts.selected = None;
                    app.parts.rescan();
                }
                if ui.button("Cancel").clicked() {
                    app.parts.confirm_delete = None;
                }
            });
        } else if ui.button("Delete…").clicked() {
            app.parts.confirm_delete = Some(path);
        }
    }
}
