//! Text annotations: the text and style of a [`crate::ComponentKind::Text`]
//! component, kept in the circuit next to it (like memory contents), so the
//! component kind stays small and `Copy`.

use serde::{Deserialize, Serialize};

/// Longest annotation text in bytes.
pub const MAX_NOTE_BYTES: usize = 1024;
/// Most lines of one annotation.
pub const MAX_NOTE_LINES: usize = 16;
/// Text height limits, in tenths of a grid unit.
pub const MIN_NOTE_HEIGHT: u8 = 4;
pub const MAX_NOTE_HEIGHT: u8 = 120;
/// Default text height (tenths of a grid unit).
pub const DEFAULT_NOTE_HEIGHT: u8 = 12;

/// Horizontal advance of one character, as a fraction of the text height
/// (the stroke font's glyph cell plus its gap; mirrored by the renderer).
pub const NOTE_ADVANCE: f32 = 0.9;
/// Line pitch, as a fraction of the text height (room for descenders and
/// the overbar).
pub const NOTE_LINE: f32 = 1.6;

/// Text and style of one annotation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    /// The text (printable characters; `\n` separates lines).
    pub text: String,
    /// Cap height in tenths of a grid unit.
    #[serde(default = "default_height")]
    pub height: u8,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    /// A bar over the whole text (active-low signal names).
    #[serde(default, skip_serializing_if = "is_false")]
    pub overbar: bool,
}

const fn default_height() -> u8 {
    DEFAULT_NOTE_HEIGHT
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde skip predicate signature")]
const fn is_false(b: &bool) -> bool {
    !*b
}

impl Default for Note {
    fn default() -> Self {
        Self { text: "Text".into(), height: DEFAULT_NOTE_HEIGHT, bold: false, italic: false, overbar: false }
    }
}

impl Note {
    /// A note with the default style.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    /// Lines of the text.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.split('\n')
    }

    /// Is the note within the limits (length, lines, printable text,
    /// height)?
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.text.len() <= MAX_NOTE_BYTES
            && self.lines().count() <= MAX_NOTE_LINES
            && self.text.chars().all(|c| c == '\n' || !c.is_control())
            && (MIN_NOTE_HEIGHT..=MAX_NOTE_HEIGHT).contains(&self.height)
    }

    /// Text made valid: control characters dropped (tabs become spaces),
    /// cut to the length and line limits on character boundaries.
    #[must_use]
    pub fn sanitize_text(text: &str) -> String {
        let mut out = String::new();
        let mut lines = 1;
        for c in text.chars() {
            let c = match c {
                '\t' => ' ',
                '\r' => continue,
                '\n' => {
                    if lines >= MAX_NOTE_LINES {
                        break;
                    }
                    lines += 1;
                    '\n'
                }
                c if c.is_control() => continue,
                c => c,
            };
            if out.len() + c.len_utf8() > MAX_NOTE_BYTES {
                break;
            }
            out.push(c);
        }
        out
    }

    /// Size of the annotation box in grid units (width, height), as stored
    /// in its [`crate::ComponentKind::Text`] kind.
    #[must_use]
    pub fn box_size(&self) -> (u8, u8) {
        let h = f32::from(self.height) / 10.0;
        let cols = self.lines().map(|l| l.chars().count()).max().unwrap_or(0).max(1);
        let rows = self.lines().count().max(1);
        #[allow(clippy::cast_precision_loss)]
        let w = (cols as f32 * NOTE_ADVANCE * h + 0.4 * h).ceil();
        #[allow(clippy::cast_precision_loss)]
        let hh = (rows as f32 * NOTE_LINE * h).ceil();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let clamp = |v: f32| v.clamp(1.0, 255.0) as u8;
        (clamp(w), clamp(hh))
    }

    /// The component kind of this note.
    #[must_use]
    pub fn kind(&self) -> crate::ComponentKind {
        let (w, h) = self.box_size();
        crate::ComponentKind::Text { w, h }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_and_size() {
        let n = Note::new(&Note::sanitize_text("A\tB\r\nc\u{7}d"));
        assert_eq!(n.text, "A B\ncd");
        assert!(n.is_valid());
        let (w, h) = n.box_size();
        assert!(w >= 4 && h >= 3, "{w}x{h}");
        let long = "x".repeat(5000);
        assert_eq!(Note::sanitize_text(&long).len(), MAX_NOTE_BYTES);
        let many = "a\n".repeat(40);
        assert_eq!(Note::sanitize_text(&many).lines().count(), MAX_NOTE_LINES);
        assert!(!Note { height: 0, ..Note::default() }.is_valid());
        assert!(!Note::new("bell\u{7}").is_valid());
    }
}
