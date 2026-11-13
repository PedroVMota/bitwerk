//! Properties of the input / display / annotation kinds: switch mode,
//! power level, sequence pattern (binary or hex, with a waveform preview),
//! numeric output layout, 7-segment polarity, display colour and text
//! annotations.
//!
//! Kind parameters are edited on a copy of the kind (applied by the caller
//! with one `SetKind`); colours and notes are separate edits
//! ([`Edit::SetColor`], [`Edit::SetNote`]). Free text (notes, sequence
//! patterns) is edited in a draft and applied on Enter / focus loss /
//! Apply, so typing does not produce an edit per keystroke.

use bw_core::{
    Color, ComponentId, ComponentKind, Edit, MAX_CLOCK_HALF_PERIOD, MAX_NUMERIC_BITS, MAX_SEQ_LEN, MIN_NUMERIC_BITS,
    Note, NumFormat, SwitchMode,
};
use egui::{Color32, Stroke};

use crate::app::BitwerkApp;

/// Drafts of the free-text properties of the selected component.
#[derive(Debug, Default)]
pub struct Drafts {
    note: Option<NoteDraft>,
    seq: Option<SeqDraft>,
}

#[derive(Debug)]
struct NoteDraft {
    id: ComponentId,
    /// Note the draft started from (a different stored note resets it).
    base: Note,
    text: String,
    height: u8,
}

#[derive(Debug)]
struct SeqDraft {
    id: ComponentId,
    base: (u64, u8),
    hex: bool,
    text: String,
    /// Length used in hex mode.
    len: u8,
    error: Option<String>,
}

/// Steps as text, step 0 first (`"0110"`).
#[must_use]
pub fn pattern_to_steps(pattern: u64, len: u8) -> String {
    (0..len.min(MAX_SEQ_LEN)).map(|i| if pattern >> i & 1 == 1 { '1' } else { '0' }).collect()
}

/// Parses steps (`0` / `1`, step 0 first; spaces and `_` ignored).
///
/// # Errors
/// Other characters, no steps or more than [`MAX_SEQ_LEN`].
pub fn parse_steps(text: &str) -> Result<(u64, u8), String> {
    let mut pattern = 0u64;
    let mut len = 0u8;
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '_') {
        let bit = match c {
            '0' => 0,
            '1' => 1,
            _ => return Err(format!("'{c}' is not a step (use 0 and 1)")),
        };
        if len >= MAX_SEQ_LEN {
            return Err(format!("At most {MAX_SEQ_LEN} steps"));
        }
        pattern |= bit << len;
        len += 1;
    }
    if len == 0 {
        return Err("Enter at least one step".into());
    }
    Ok((pattern, len))
}

/// Pattern as hex (bit `i` = step `i`).
#[must_use]
pub fn pattern_to_hex(pattern: u64) -> String {
    format!("{pattern:X}")
}

/// Parses a hex pattern for `len` steps.
///
/// # Errors
/// Not hex, or bits set beyond the length.
pub fn parse_hex_pattern(text: &str, len: u8) -> Result<u64, String> {
    let t: String = text.trim().trim_start_matches("0x").trim_start_matches("0X").trim_start_matches('$').into();
    let t: String = t.chars().filter(|c| !c.is_whitespace() && *c != '_').collect();
    if t.is_empty() {
        return Err("Enter a hex value".into());
    }
    if t.len() > 16 {
        return Err("At most 16 hex digits".into());
    }
    let v = u64::from_str_radix(&t, 16).map_err(|_| "Not a hex number".to_string())?;
    if len < MAX_SEQ_LEN && v >> len != 0 {
        return Err(format!("The pattern has bits beyond step {}", len.saturating_sub(1)));
    }
    Ok(v)
}

/// Draws a small waveform of the pattern (step 0 at the left).
fn waveform(ui: &mut egui::Ui, pattern: u64, len: u8) {
    let w = ui.available_width().max(60.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, Color32::from_rgb(29, 31, 35));
    let n = f32::from(len.max(1));
    let step_w = (rect.width() - 8.0) / n;
    let (hi, lo) = (rect.top() + 5.0, rect.bottom() - 5.0);
    let mut pts = Vec::with_capacity(usize::from(len) * 2 + 2);
    for i in 0..len {
        let x0 = rect.left() + 4.0 + f32::from(i) * step_w;
        let y = if pattern >> i & 1 == 1 { hi } else { lo };
        pts.push(egui::pos2(x0, y));
        pts.push(egui::pos2(x0 + step_w, y));
        if step_w >= 6.0 && i > 0 {
            painter.line_segment(
                [egui::pos2(x0, rect.top() + 2.0), egui::pos2(x0, rect.bottom() - 2.0)],
                Stroke::new(1.0, Color32::from_white_alpha(14)),
            );
        }
    }
    painter.add(egui::Shape::line(pts, Stroke::new(1.5, Color32::from_rgb(120, 220, 120))));
}

/// Does this kind take a display colour?
#[must_use]
pub const fn has_color(kind: &ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Led
            | ComponentKind::Clock { .. }
            | ComponentKind::SeqGen { .. }
            | ComponentKind::SevenSeg { .. }
            | ComponentKind::Numeric { .. }
            | ComponentKind::Text { .. }
    )
}

fn swatch(ui: &mut egui::Ui, c: Option<Color>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let edge = Stroke::new(1.0, Color32::GRAY);
    if let Some(c) = c {
        let [r, g, b] = c.rgb();
        ui.painter().rect(rect, 2.0, Color32::from_rgb(r, g, b), edge, egui::StrokeKind::Inside);
    } else {
        // "Default": an empty, struck-through box.
        ui.painter().rect_stroke(rect, 2.0, edge, egui::StrokeKind::Inside);
        ui.painter().line_segment([rect.left_bottom(), rect.right_top()], edge);
    }
}

/// A colour choice of the picker (`None` = the kind's default colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Choice(Option<Color>);

/// Colour picker: the 16-entry palette plus "Default". Returns the new
/// choice when it changed.
fn color_combo(ui: &mut egui::Ui, current: Option<Color>) -> Option<Choice> {
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.label("colour");
        swatch(ui, current);
        egui::ComboBox::from_id_salt("component-colour")
            .selected_text(current.map_or("Default", Color::name))
            .height(400.0)
            .show_ui(ui, |ui| {
                for c in std::iter::once(None).chain(Color::ALL.into_iter().map(Some)) {
                    ui.horizontal(|ui| {
                        swatch(ui, c);
                        if ui.selectable_label(c == current, c.map_or("Default", Color::name)).clicked() && c != current
                        {
                            chosen = Some(Choice(c));
                        }
                    });
                }
            });
    });
    chosen
}

/// Editors of the kind parameters of the newer kinds (`kind` is the
/// caller's copy, applied with one `SetKind` if it changed).
pub fn kind_editor(app: &mut BitwerkApp, ui: &mut egui::Ui, id: ComponentId, kind: &mut ComponentKind) {
    match kind {
        ComponentKind::Switch { on, mode } => {
            egui::ComboBox::from_label("mode").selected_text(mode.name()).show_ui(ui, |ui| {
                for m in SwitchMode::ALL {
                    ui.selectable_value(mode, m, m.name());
                }
            });
            match mode {
                SwitchMode::Toggle => {
                    ui.checkbox(on, "on");
                }
                SwitchMode::Momentary => {
                    ui.checkbox(on, "push-to-break (on until pressed)");
                    ui.small("Hold it down with the Interact tool.");
                }
                SwitchMode::Pulse => {
                    ui.checkbox(on, "starts high");
                    ui.small(format!(
                        "Square wave, {} ticks per half period. Click it with the Interact tool to set its level.",
                        bw_core::PULSE_HALF_PERIOD
                    ));
                }
            }
        }
        ComponentKind::Const { high } => {
            ui.horizontal(|ui| {
                ui.selectable_value(high, true, "VCC (1)");
                ui.selectable_value(high, false, "Ground (0)");
            });
        }
        ComponentKind::SevenSeg { common_anode } => {
            ui.horizontal(|ui| {
                ui.selectable_value(common_anode, false, "Common cathode");
                ui.selectable_value(common_anode, true, "Common anode");
            });
            ui.small(if *common_anode { "A segment lights at 0." } else { "A segment lights at 1." });
        }
        ComponentKind::Numeric { bits, format, signed } => {
            ui.add(egui::DragValue::new(bits).range(MIN_NUMERIC_BITS..=MAX_NUMERIC_BITS).prefix("input bits "));
            egui::ComboBox::from_label("format").selected_text(format.name()).show_ui(ui, |ui| {
                for f in NumFormat::ALL {
                    ui.selectable_value(format, f, f.name());
                }
            });
            ui.add_enabled(*format == NumFormat::Dec, egui::Checkbox::new(signed, "signed (two's complement)"));
        }
        ComponentKind::SeqGen { pattern, len, half_period } => {
            ui.add(egui::DragValue::new(half_period).range(1..=MAX_CLOCK_HALF_PERIOD).prefix("half period (ticks) "));
            if let Some((p, l)) = seq_editor(app, ui, id, *pattern, *len) {
                *pattern = p;
                *len = l;
            }
        }
        _ => {}
    }
}

/// Pattern editor of a sequence generator; returns a new (pattern, length)
/// when one was applied.
fn seq_editor(app: &mut BitwerkApp, ui: &mut egui::Ui, id: ComponentId, pattern: u64, len: u8) -> Option<(u64, u8)> {
    let stale = app.prop_drafts.seq.as_ref().is_none_or(|d| d.id != id || d.base != (pattern, len));
    if stale {
        let hex = app.prop_drafts.seq.as_ref().is_some_and(|d| d.hex);
        let text = if hex { pattern_to_hex(pattern) } else { pattern_to_steps(pattern, len) };
        app.prop_drafts.seq = Some(SeqDraft { id, base: (pattern, len), hex, text, len, error: None });
    }
    let d = app.prop_drafts.seq.as_mut()?;
    let mut result = None;
    ui.label(format!("pattern ({len} steps, one every half period, repeating)"));
    ui.horizontal(|ui| {
        let was_hex = d.hex;
        ui.selectable_value(&mut d.hex, false, "Steps").on_hover_text("0 / 1 per step, step 0 first");
        ui.selectable_value(&mut d.hex, true, "Hex").on_hover_text("bit i = step i");
        if d.hex != was_hex {
            d.text = if d.hex { pattern_to_hex(pattern) } else { pattern_to_steps(pattern, len) };
            d.len = len;
            d.error = None;
        }
    });
    let r = ui.add(
        egui::TextEdit::singleline(&mut d.text)
            .font(egui::TextStyle::Monospace)
            .char_limit(80)
            .desired_width(f32::INFINITY),
    );
    let mut apply = r.lost_focus() && d.text != text_of(d, pattern, len);
    if d.hex {
        let lr = ui.add(egui::DragValue::new(&mut d.len).range(1..=MAX_SEQ_LEN).prefix("steps "));
        apply |= lr.changed() && !lr.dragged();
        apply |= lr.drag_stopped();
    }
    if ui.button("Apply pattern").clicked() {
        apply = true;
    }
    if apply {
        let parsed = if d.hex { parse_hex_pattern(&d.text, d.len).map(|p| (p, d.len)) } else { parse_steps(&d.text) };
        match parsed {
            Ok((p, l)) => {
                d.error = None;
                if (p, l) != (pattern, len) {
                    result = Some((p, l));
                }
            }
            Err(e) => d.error = Some(e),
        }
    }
    if let Some(e) = &d.error {
        ui.colored_label(Color32::LIGHT_RED, e);
    }
    let (pp, pl) = result.unwrap_or((pattern, len));
    waveform(ui, pp, pl);
    result
}

fn text_of(d: &SeqDraft, pattern: u64, len: u8) -> String {
    if d.hex { pattern_to_hex(pattern) } else { pattern_to_steps(pattern, len) }
}

/// Colour and text-annotation editors (separate edits, applied here).
pub fn extra_editor(
    app: &mut BitwerkApp,
    ui: &mut egui::Ui,
    id: ComponentId,
    kind: &ComponentKind,
    color: Option<Color>,
) {
    if has_color(kind) {
        if let Some(Choice(c)) = color_combo(ui, color) {
            if let Err(e) = app.apply_edit(Edit::SetColor(id, c)) {
                app.status = e.to_string();
            }
        }
    }
    if matches!(kind, ComponentKind::Text { .. }) {
        note_editor(app, ui, id);
    }
}

fn note_editor(app: &mut BitwerkApp, ui: &mut egui::Ui, id: ComponentId) {
    let Some(note) = app.shown().note(id).cloned() else { return };
    if app.prop_drafts.note.as_ref().is_none_or(|d| d.id != id || d.base != note) {
        app.prop_drafts.note = Some(NoteDraft { id, base: note.clone(), text: note.text.clone(), height: note.height });
    }
    let Some(d) = app.prop_drafts.note.as_mut() else { return };
    ui.label("text");
    let r = ui.add(
        egui::TextEdit::multiline(&mut d.text)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .char_limit(bw_core::note::MAX_NOTE_BYTES),
    );
    ui.small(format!("Applied when you click elsewhere · at most {} lines", bw_core::note::MAX_NOTE_LINES));
    let mut changed = r.lost_focus() && d.text != note.text;
    ui.horizontal(|ui| {
        ui.label("height");
        let hr = ui.add(
            egui::DragValue::new(&mut d.height)
                .range(bw_core::note::MIN_NOTE_HEIGHT..=bw_core::note::MAX_NOTE_HEIGHT)
                .custom_formatter(|v, _| format!("{:.1}", v / 10.0))
                .custom_parser(|s| s.trim().parse::<f64>().ok().map(|v| (v * 10.0).round()))
                .suffix(" grid"),
        );
        // Every step resizes the box: apply when a drag ends.
        changed |= (hr.changed() && !hr.dragged()) || hr.drag_stopped();
    });
    let mut new = note.clone();
    ui.horizontal(|ui| {
        changed |= ui.checkbox(&mut new.bold, "bold").changed();
        changed |= ui.checkbox(&mut new.italic, "italic").changed();
        changed |= ui.checkbox(&mut new.overbar, "overbar").changed();
    });
    if !changed {
        return;
    }
    new.height = d.height;
    new.text = Note::sanitize_text(&d.text);
    d.text.clone_from(&new.text);
    if new != note && new.is_valid() {
        if let Err(e) = app.apply_edit(Edit::SetNote { id, note: new }) {
            app.status = e.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_round_trip() {
        assert_eq!(parse_steps("0110"), Ok((0b0110, 4)));
        assert_eq!(parse_steps("1 0_0"), Ok((0b001, 3)));
        assert_eq!(pattern_to_steps(0b0110, 4), "0110");
        assert_eq!(pattern_to_steps(0b1, 3), "100");
        assert!(parse_steps("").is_err());
        assert!(parse_steps("012").is_err());
        let long = "1".repeat(64);
        assert_eq!(parse_steps(&long), Ok((u64::MAX, 64)));
        assert!(parse_steps(&"1".repeat(65)).is_err());
        let (p, l) = parse_steps("1010").unwrap_or_default();
        assert_eq!(parse_steps(&pattern_to_steps(p, l)), Ok((p, l)));
    }

    #[test]
    fn hex_patterns() {
        assert_eq!(parse_hex_pattern("0x6", 4), Ok(6));
        assert_eq!(parse_hex_pattern("$f", 4), Ok(15));
        assert!(parse_hex_pattern("1f", 4).is_err(), "bit beyond the length");
        assert!(parse_hex_pattern("g", 4).is_err());
        assert!(parse_hex_pattern("", 4).is_err());
        assert_eq!(parse_hex_pattern("FFFFFFFFFFFFFFFF", 64), Ok(u64::MAX));
        assert!(parse_hex_pattern("1FFFFFFFFFFFFFFFF", 64).is_err());
        assert_eq!(pattern_to_hex(0xab), "AB");
    }

    #[test]
    fn colour_kinds() {
        assert!(has_color(&ComponentKind::Led));
        assert!(has_color(&ComponentKind::Text { w: 2, h: 2 }));
        assert!(!has_color(&ComponentKind::Not));
    }
}
