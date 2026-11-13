//! Properties of a macro definition: name, pin order and names, template
//! (automatic box or DIL package with assigned pin numbers, with a live
//! preview), datasheet text and an optional local datasheet file.
//!
//! Changes are edited in a draft and stored with one
//! [`bw_core::Edit::DefineMacro`] on "Apply" (every instance follows), so
//! typing does not recompile the simulation on every keystroke.

use bw_core::{DIL_SIZES, MacroDef, MacroId, MacroPin, Package, TagDir};
use bw_render::symbols;

use crate::app::BitwerkApp;

/// Draft of a definition's properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub def: MacroId,
    /// Content hash of the definition the draft started from.
    pub base: u64,
    pub name: String,
    pub pins: Vec<MacroPin>,
    pub package: Package,
    pub datasheet: String,
    pub doc_link: String,
}

impl Draft {
    fn new(id: MacroId, hash: u64, d: &MacroDef) -> Self {
        Self {
            def: id,
            base: hash,
            name: d.name.clone(),
            pins: d.pins.clone(),
            package: d.package,
            datasheet: d.datasheet.clone(),
            doc_link: d.doc_link.clone().unwrap_or_default(),
        }
    }

    fn apply_to(&self, d: &MacroDef) -> MacroDef {
        let mut n = d.clone();
        n.name = self.name.trim().to_string();
        n.pins.clone_from(&self.pins);
        n.package = self.package;
        n.datasheet.clone_from(&self.datasheet);
        let link = self.doc_link.trim();
        n.doc_link = (!link.is_empty()).then(|| link.to_string());
        n
    }
}

/// Opens a datasheet file after the user clicked: only an allow-listed,
/// canonical local file, through `/usr/bin/open` without a shell.
pub fn open_datasheet(app: &mut BitwerkApp, link: &str) {
    let base = app.project_dir();
    match bw_io::datasheet::resolve_link(link, base.as_deref()) {
        Ok(path) => {
            #[cfg(target_os = "macos")]
            {
                match std::process::Command::new("/usr/bin/open").arg(&path).spawn() {
                    Ok(_) => app.status = format!("Opened {}", path.display()),
                    Err(e) => app.status = format!("Could not open the datasheet: {e}"),
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                app.status = format!("Datasheet file: {}", path.display());
            }
        }
        Err(e) => app.status = format!("Datasheet not opened: {e}"),
    }
}

/// The macro section of the Properties panel.
#[allow(clippy::too_many_lines, reason = "one editor")]
pub fn editor(app: &mut BitwerkApp, ui: &mut egui::Ui, id: MacroId) {
    let Some(entry) = app.circuit.macros().get(id).cloned() else {
        ui.weak("Unknown macro definition");
        return;
    };
    if app.macro_draft.as_ref().is_none_or(|d| d.def != id || (d.base != entry.hash && !dirty(d, &entry))) {
        app.macro_draft = Some(Draft::new(id, entry.hash, &entry.def));
    }
    let Some(mut draft) = app.macro_draft.take() else { return };
    let n = app.circuit.total_instances(id);
    ui.label(format!(
        "Macro definition #{} · {n} instance{} · depth {}",
        id.0,
        if n == 1 { "" } else { "s" },
        entry.depth
    ));
    ui.label(format!("{} components when flattened", entry.flat));
    if let Some(o) = &entry.def.origin {
        let lib = app.parts.find(&o.key).map(|e| e.file.hash());
        match lib {
            Some(h) if h != o.hash => {
                ui.colored_label(egui::Color32::YELLOW, format!("Library version changed ({})", o.key));
                if ui.button("Update from library").on_hover_text("Replace this definition by the library's").clicked()
                {
                    app.update_from_library(id);
                }
            }
            Some(_) if entry.hash != o.hash => {
                ui.weak(format!("From {} (modified here)", o.key));
            }
            Some(_) => {
                ui.weak(format!("From {} (up to date)", o.key));
            }
            None => {
                ui.weak(format!("From {} (not in the library)", o.key));
            }
        }
    }
    ui.horizontal(|ui| {
        ui.label("Name");
        ui.text_edit_singleline(&mut draft.name);
    });
    ui.separator();
    ui.label("Pins (instance order: inputs, then outputs)");
    let iface = entry.def.interface();
    let mut swap: Option<(usize, usize)> = None;
    egui::Grid::new("macro-pins").num_columns(5).spacing([4.0, 2.0]).show(ui, |ui| {
        for (k, ip) in iface.iter().enumerate() {
            let Some(pi) = draft.pins.iter().position(|p| p.tag == ip.tag) else { continue };
            ui.label(match (ip.dir, ip.tristate) {
                (TagDir::In, _) => "in",
                (TagDir::Out, false) => "out",
                (TagDir::Out, true) => "out (Z)",
            });
            ui.add(egui::TextEdit::singleline(&mut draft.pins[pi].name).desired_width(70.0));
            if let Package::Dil(size) = draft.package {
                ui.add(egui::DragValue::new(&mut draft.pins[pi].dil).range(0..=size).prefix("pin "));
            } else {
                ui.label("");
            }
            let same_dir = |j: usize| iface.get(j).is_some_and(|o| o.dir == ip.dir);
            if ui.add_enabled(k > 0 && same_dir(k - 1), egui::Button::new("up").small()).clicked() {
                swap = draft.pins.iter().position(|p| p.tag == iface[k - 1].tag).map(|j| (pi, j));
            }
            if ui.add_enabled(same_dir(k + 1), egui::Button::new("dn").small()).clicked() {
                swap = draft.pins.iter().position(|p| p.tag == iface[k + 1].tag).map(|j| (pi, j));
            }
            ui.end_row();
        }
    });
    if let Some((a, b)) = swap {
        draft.pins.swap(a, b);
    }
    if iface.is_empty() {
        ui.weak("No pins yet: open the macro and add In / Out tags.");
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Template");
        let auto = draft.package == Package::Auto;
        if ui.selectable_label(auto, "Box").clicked() {
            draft.package = Package::Auto;
        }
        let pins = iface.len();
        let fits = DIL_SIZES.iter().copied().filter(|&n| usize::from(n) >= pins).collect::<Vec<_>>();
        let current = match draft.package {
            Package::Dil(n) => format!("DIL-{n}"),
            Package::Auto => "DIL…".into(),
        };
        egui::ComboBox::from_id_salt("dil-size").selected_text(current).show_ui(ui, |ui| {
            for n in fits {
                if ui.selectable_label(draft.package == Package::Dil(n), format!("DIL-{n}")).clicked() {
                    draft.package = Package::Dil(n);
                }
            }
        });
    });
    if let Package::Dil(size) = draft.package {
        if ui.button("Auto-assign pin numbers").clicked() {
            for p in &mut draft.pins {
                p.dil = 0;
            }
            let tmp = draft.apply_to(&entry.def);
            let nums = tmp.dil_numbers(size);
            for (ip, n) in tmp.interface().iter().zip(nums) {
                if let Some(p) = draft.pins.iter_mut().find(|p| p.tag == ip.tag) {
                    p.dil = n;
                }
            }
        }
        ui.small("Unassigned (0) pins get the lowest free numbers.");
    }
    // Live preview of the template.
    let preview = draft.apply_to(&entry.def);
    let valid = preview.validate();
    let shape = preview.shape(id);
    let h = 170.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(29, 31, 35));
    if shape.is_valid() {
        let mesh = symbols::macro_symbol(&preview, &shape).tessellate();
        let (w, hh) = bw_core::ComponentKind::Macro(shape).size();
        #[allow(clippy::cast_precision_loss)]
        crate::icons::paint_macro_fit(ui.painter(), rect, &mesh, [w as f32, hh as f32], 1.0);
    }
    if let Err(e) = &valid {
        ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
    }
    ui.separator();
    egui::CollapsingHeader::new("Datasheet").default_open(true).show(ui, |ui| {
        ui.add(
            egui::TextEdit::multiline(&mut draft.datasheet)
                .desired_rows(6)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
        ui.horizontal(|ui| {
            ui.label("File");
            ui.add(egui::TextEdit::singleline(&mut draft.doc_link).hint_text("local .pdf / .html / .txt / .md"));
        });
        let link = entry.def.doc_link.clone();
        if let Some(link) = link {
            if ui
                .button("Open datasheet file")
                .on_hover_text("Opens the local file with the system viewer (pdf, html, htm, txt, md only)")
                .clicked()
            {
                open_datasheet(app, &link);
            }
        }
    });
    let changed = dirty(&draft, &entry);
    ui.horizontal(|ui| {
        if ui.add_enabled(changed && valid.is_ok(), egui::Button::new("Apply")).clicked() {
            let def = draft.apply_to(&entry.def);
            match app.redefine(id, def) {
                Ok(_) => {
                    app.status =
                        format!("Macro '{}' updated ({n} instance{})", draft.name, if n == 1 { "" } else { "s" });
                    if let Some(e) = app.circuit.macros().get(id) {
                        draft = Draft::new(id, e.hash, &e.def);
                    }
                }
                Err(e) => app.status = format!("Macro not changed: {e}"),
            }
        }
        if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
            draft = Draft::new(id, entry.hash, &entry.def);
        }
    });
    if changed && n > 1 {
        ui.colored_label(egui::Color32::YELLOW, format!("Applying changes all {n} instances"));
    }
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        if app.current_def() != Some(id) && ui.button("Open definition").clicked() {
            app.macro_draft = Some(draft.clone());
            app.enter_def(id);
            return;
        }
        if ui.button("Export .bwmacro…").clicked() {
            app.export_macro(id);
        }
        if ui
            .button("Export as Digital Works macro…")
            .on_hover_text("A Digital Works 3.0.5 .dwm macro (lossy, see the export report)")
            .clicked()
        {
            app.export_dw_macro(id);
        }
        if ui.button("Add to my parts").clicked() {
            app.add_to_library(id);
        }
    });
    app.macro_draft = Some(draft);
}

fn dirty(d: &Draft, e: &bw_core::MacroEntry) -> bool {
    *d != Draft::new(d.def, d.base, &e.def)
}
