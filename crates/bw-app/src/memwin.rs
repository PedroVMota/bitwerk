//! Memory Contents window: a hex / binary editor of one RAM or ROM.
//!
//! * Rows are virtualized (`ScrollArea::show_rows`): only the visible rows
//!   are laid out, so a 16K-word memory costs the same as a 16-word one.
//! * While the window is open the simulator publishes the memory's live
//!   contents through a separate, throttled channel (`WatchMemory`, at most
//!   every 100 ms and only when they change); snapshots never carry memory.
//! * Editing a cell (click, type, Enter) sends a `SetMemoryWord` edit:
//!   the UI's circuit, the simulator's copy and the running memory change
//!   together. Enter moves to the next word, Escape or clicking elsewhere
//!   cancels.
//! * Clear (with confirmation), Load and Save of `.map` files
//!   ([`bw_io::memmap`]; Load also reads Digital Works binary maps, detected
//!   by their magic number, and says which format it loaded). A larger map is truncated, a smaller one
//!   zero-filled, wider words are masked to the data width.

use std::sync::Arc;

use bw_core::memenc::width_mask;
use bw_core::{ComponentId, ComponentKind, Edit};
use bw_io::memmap::{self, MemoryMap};
use bw_sim::Command;
use egui::{Color32, RichText, Sense, TextStyle};

use crate::app::BitwerkApp;

/// File extension of memory maps.
pub const MAP_EXT: &str = "map";

pub struct MemoryWindow {
    pub id: ComponentId,
    hex: bool,
    /// Address being edited and its text.
    editing: Option<(usize, String)>,
    /// Focus the editor on the next frame.
    focus: bool,
    confirm_clear: bool,
    message: String,
    /// Contents with this window's latest edits applied, shown until the
    /// simulator publishes a newer view (so an edited cell never flickers
    /// back for the up to 100 ms the live feed takes).
    shadow: Option<(Option<u64>, Arc<[u16]>)>,
}

impl MemoryWindow {
    pub fn new(id: ComponentId) -> Self {
        Self { id, hex: true, editing: None, focus: false, confirm_clear: false, message: String::new(), shadow: None }
    }
}

/// Opens the window for `id` (one at a time) and starts the live feed.
pub fn open(app: &mut BitwerkApp, id: ComponentId) {
    if app.memory_window.as_ref().is_some_and(|w| w.id == id) {
        return;
    }
    app.memory_window = Some(MemoryWindow::new(id));
    app.sim.send(Command::WatchMemory(Some(id)));
}

pub fn close(app: &mut BitwerkApp) {
    if app.memory_window.take().is_some() {
        app.sim.send(Command::WatchMemory(None));
    }
}

fn format_word(v: u16, bits: u8, hex: bool) -> String {
    if hex {
        let digits = usize::from(bits).div_ceil(4);
        format!("{v:0digits$X}")
    } else {
        let digits = usize::from(bits);
        format!("{v:0digits$b}")
    }
}

fn parse_word(text: &str, bits: u8, hex: bool) -> Result<u16, String> {
    let t = text.trim();
    let t = if hex { t.trim_start_matches('$').trim_start_matches("0x").trim_start_matches("0X") } else { t };
    let radix = if hex { 16 } else { 2 };
    if t.is_empty() || t.len() > 16 {
        return Err("Enter a value".into());
    }
    let v =
        u32::from_str_radix(t, radix).map_err(|_| format!("Not a {} number", if hex { "hex" } else { "binary" }))?;
    if v > u32::from(width_mask(bits)) {
        return Err(format!("Value does not fit in {bits} bits"));
    }
    u16::try_from(v).map_err(|_| "Value too large".into())
}

/// Draws the window (if open). Returns nothing; edits go through the app.
#[allow(clippy::too_many_lines, reason = "one window: toolbar, grid, dialogs")]
pub fn show(app: &mut BitwerkApp, ctx: &egui::Context) {
    let Some(mut win) = app.memory_window.take() else { return };
    let id = win.id;
    let Some((kind, label)) = app.circuit.component(id).map(|c| (c.kind, c.kind.label())) else {
        app.sim.send(Command::WatchMemory(None));
        return;
    };
    let ComponentKind::Memory { data_bits, addr_bits, .. } = kind else {
        app.sim.send(Command::WatchMemory(None));
        return;
    };
    // Live contents if the simulator has published them, else the design.
    let live = app.sim.memory_view().filter(|v| v.id == id && v.words.len() == kind.memory_words());
    let generation = live.as_ref().map(|v| v.generation);
    let words: Arc<[u16]> = match (&win.shadow, &live) {
        (Some((g, w)), _) if *g == generation => Arc::clone(w),
        (_, Some(v)) => Arc::clone(&v.words),
        (_, None) => app
            .circuit
            .memory(id)
            .map_or_else(|| Arc::from(vec![0u16; kind.memory_words()]), |m| Arc::from(m.as_slice())),
    };
    if win.shadow.as_ref().is_some_and(|(g, _)| *g != generation) {
        win.shadow = None;
    }
    let mut open = true;
    let mut edit: Option<Edit> = None;
    let title = format!("Memory Contents — {label} #{} ({}×{data_bits})", id.0, 1u32 << addr_bits);
    egui::Window::new(title)
        .id(egui::Id::unique("memory-contents"))
        .open(&mut open)
        .default_size([520.0, 420.0])
        .default_pos(egui::pos2(560.0, 90.0))
        .resizable(true)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Clear…").on_hover_text("Set every word to zero").clicked() {
                    win.confirm_clear = true;
                }
                if ui.button("Load…").on_hover_text("Load a memory map (.map)").clicked() {
                    match load_map(kind) {
                        Ok(Some((w, note))) => {
                            edit = Some(Edit::SetMemory { id, words: Arc::from(w) });
                            win.message = note;
                        }
                        Ok(None) => {}
                        Err(e) => win.message = e,
                    }
                }
                if ui.button("Save…").on_hover_text("Save as a memory map (.map)").clicked() {
                    win.message = save_map(&words, data_bits, label);
                }
                ui.separator();
                ui.selectable_value(&mut win.hex, true, "Hex");
                ui.selectable_value(&mut win.hex, false, "Binary");
                ui.separator();
                ui.weak(if live.is_some() { "live" } else { "design" });
            });
            if !win.message.is_empty() {
                ui.colored_label(Color32::from_rgb(230, 200, 120), &win.message);
            }
            ui.separator();
            let per_row = if win.hex || data_bits <= 4 {
                8
            } else if data_bits <= 8 {
                4
            } else {
                2
            };
            let rows = words.len().div_ceil(per_row);
            let row_h = ui.text_style_height(&TextStyle::Monospace) + 6.0;
            let digits = if win.hex { usize::from(data_bits).div_ceil(4) } else { usize::from(data_bits) };
            #[allow(clippy::cast_precision_loss)]
            let cell_w = (digits as f32 * 7.5 + 12.0).max(26.0);
            let mut commit: Option<(usize, String)> = None;
            let mut cancel = false;
            let mut start: Option<usize> = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, rows, |ui, range| {
                for row in range {
                    ui.horizontal(|ui| {
                        ui.set_height(row_h);
                        ui.label(RichText::new(format!("${:04X}", row * per_row)).monospace().weak());
                        for a in row * per_row..((row + 1) * per_row).min(words.len()) {
                            let editing_this = win.editing.as_ref().is_some_and(|(e, _)| *e == a);
                            if editing_this {
                                let Some((_, text)) = win.editing.as_mut() else { continue };
                                let r = ui.add_sized(
                                    [cell_w, row_h - 2.0],
                                    egui::TextEdit::singleline(text).font(TextStyle::Monospace).char_limit(16),
                                );
                                if win.focus {
                                    r.request_focus();
                                    win.focus = false;
                                }
                                if r.lost_focus() {
                                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                        commit = Some((a, text.clone()));
                                    } else {
                                        cancel = true;
                                    }
                                }
                            } else {
                                let txt = RichText::new(format_word(words[a], data_bits, win.hex)).monospace();
                                let r = ui
                                    .add_sized([cell_w, row_h - 2.0], egui::Label::new(txt).sense(Sense::click()))
                                    .on_hover_text(format!("${a:04X}"));
                                if r.hovered() {
                                    ui.painter().rect_stroke(
                                        r.rect,
                                        2.0,
                                        egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                if r.clicked() {
                                    start = Some(a);
                                }
                            }
                        }
                    });
                }
            });
            if let Some((a, text)) = commit {
                match parse_word(&text, data_bits, win.hex) {
                    Ok(v) => {
                        win.message.clear();
                        if words.get(a) != Some(&v) {
                            edit =
                                Some(Edit::SetMemoryWord { id, addr: u32::try_from(a).unwrap_or(u32::MAX), value: v });
                        }
                        // Enter moves on to the next word.
                        start = (a + 1 < words.len()).then_some(a + 1);
                        win.editing = None;
                    }
                    Err(e) => {
                        win.message = format!("${a:04X}: {e}");
                        win.focus = true;
                    }
                }
            } else if cancel {
                win.editing = None;
            }
            if let Some(a) = start {
                win.editing = Some((a, format_word(words[a], data_bits, win.hex)));
                win.focus = true;
            }
        });
    if win.confirm_clear {
        egui::Window::new("Clear memory")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("Clear all memory contents?");
                ui.horizontal(|ui| {
                    if ui.button("Clear").clicked() {
                        edit = Some(Edit::SetMemory { id, words: Arc::from(Vec::new()) });
                        win.confirm_clear = false;
                    }
                    if ui.button("Cancel").clicked() {
                        win.confirm_clear = false;
                    }
                });
            });
    }
    if let Some(e) = edit {
        let mut shown = words.to_vec();
        match &e {
            Edit::SetMemoryWord { addr, value, .. } => {
                if let Some(w) = shown.get_mut(*addr as usize) {
                    *w = *value;
                }
            }
            Edit::SetMemory { words: new, .. } => {
                shown.iter_mut().enumerate().for_each(|(i, w)| *w = new.get(i).copied().unwrap_or(0));
            }
            _ => {}
        }
        match app.apply_edit(e) {
            Ok(_) => win.shadow = Some((generation, Arc::from(shown))),
            Err(err) => win.message = err.to_string(),
        }
    }
    if open {
        app.memory_window = Some(win);
    } else {
        app.sim.send(Command::WatchMemory(None));
    }
}

/// Asks for a `.map` file and adapts it to `kind`. `Ok(None)`: cancelled.
fn load_map(kind: ComponentKind) -> Result<Option<(Vec<u16>, String)>, String> {
    let ComponentKind::Memory { data_bits, .. } = kind else { return Ok(None) };
    let Some(path) = rfd::FileDialog::new()
        .set_title("Load memory map")
        .add_filter("Memory map (bitwerk or Digital Works)", &[MAP_EXT, "MAP"])
        .pick_file()
    else {
        return Ok(None);
    };
    let bytes = read_map_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let map = memmap::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let format = if memmap::is_dw_map(&bytes) { "Digital Works map" } else { "bitwerk map" };
    let n = kind.memory_words();
    let mut notes = Vec::new();
    if map.words.len() > n {
        notes.push(format!("truncated from {} to {n} words", map.words.len()));
    } else if map.words.len() < n {
        notes.push(format!("{} words, zero-filled to {n}", map.words.len()));
    }
    if map.data_bits > data_bits {
        notes.push(format!("{}-bit words masked to {data_bits} bits", map.data_bits));
    }
    let mask = width_mask(data_bits);
    let mut words: Vec<u16> = map.words.iter().take(n).map(|&w| w & mask).collect();
    words.resize(n, 0);
    let file = path.file_name().map_or_else(String::new, |f| f.to_string_lossy().into_owned());
    let note = if notes.is_empty() {
        format!("Loaded {file} ({format})")
    } else {
        format!("Loaded {file} ({format}): {}", notes.join(", "))
    };
    Ok(Some((words, note)))
}

/// Reads a map file, refusing anything larger than a map can be (checked
/// before and while reading).
fn read_map_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    let cap = memmap::MAX_MAP_SIZE as u64;
    if meta.len() > cap {
        return Err("file too large for a memory map".into());
    }
    let mut buf = Vec::new();
    file.take(cap + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn save_map(words: &[u16], data_bits: u8, label: &str) -> String {
    let Some(mut path) = rfd::FileDialog::new()
        .set_title("Save memory map")
        .add_filter("Memory map", &[MAP_EXT])
        .set_file_name(format!("{}.{MAP_EXT}", label.to_ascii_lowercase()))
        .save_file()
    else {
        return String::new();
    };
    if path.extension().is_none_or(|e| e != MAP_EXT) {
        path.set_extension(MAP_EXT);
    }
    save_map_to(&path, words, data_bits)
}

fn save_map_to(path: &std::path::Path, words: &[u16], data_bits: u8) -> String {
    let map = MemoryMap { data_bits, words: words.to_vec() };
    match memmap::save_path(&map, path) {
        Ok(()) => format!("Saved {}", path.display()),
        Err(e) => format!("Could not save: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_formatting_and_parsing() {
        assert_eq!(format_word(0xa, 8, true), "0A");
        assert_eq!(format_word(5, 4, false), "0101");
        assert_eq!(parse_word("$1f", 8, true), Ok(0x1f));
        assert_eq!(parse_word("0x1F", 8, true), Ok(0x1f));
        assert_eq!(parse_word("101", 4, false), Ok(5));
        assert!(parse_word("100", 8, true).is_err(), "wider than 8 bits");
        assert!(parse_word("2", 4, false).is_err());
        assert!(parse_word("", 4, true).is_err());
        assert!(parse_word("ffffffffffffffffff", 16, true).is_err());
    }
}
