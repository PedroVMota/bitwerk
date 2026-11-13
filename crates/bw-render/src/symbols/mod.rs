//! Schematic symbols: original vector drawings of every component, in two
//! styles (ANSI/IEEE distinctive shapes and IEC rectangular), compiled into
//! the binary.
//!
//! * [`Symbol::build`] describes a symbol as outline paths plus fills in its
//!   own unrotated local frame (grid units, origin at the body's top-left,
//!   y down), together with its pin anchors. Pins follow `bw-core`'s pin
//!   geometry exactly ([`bw_core::ComponentKind::pin_offset`]: inputs at
//!   `(0, 2i + 1)`, a single output at `(w, h / 2)`, enables / preset /
//!   clear on the top or bottom edge), so wires end precisely on the
//!   symbol's pin stubs.
//! * [`tess`] turns the paths into triangles once (fills by ear clipping,
//!   strokes as mitred strips with a unit *extrude* vector per vertex).
//! * [`atlas()`] holds every kind × input count × style in one shared,
//!   static vertex array with a sub-range per symbol. The GPU renderer
//!   uploads it once; the app's palette paints the very same vertices with
//!   egui; the app icon rasterizes them on the CPU ([`raster`]).
//!
//! State never changes a mesh: each vertex has a colour *role* and a *layer*
//! (always / only when high / only when not high) that the shader resolves
//! against the per-slot four-state levels (LED lit, switch lever, clock
//! trace). Output pin stubs also carry their output index, so each stub of
//! a multi-output part (Q / Q̄, memory data) shows its own net.

#![allow(clippy::many_single_char_names, reason = "geometry code: points, radii, curve parameters")]

pub mod atlas;
pub mod font;
pub mod path;
pub mod raster;
pub mod tess;

use std::f32::consts::PI;

use bw_core::{
    Color, ComponentKind, DATA_WIDTHS, FfTrigger, FlipFlopType, GateOp, MAX_ADDR_BITS, MAX_NUMERIC_BITS, MIN_ADDR_BITS,
    MIN_GATE_INPUTS, MIN_NUMERIC_BITS, MacroDef, MacroShape, MemoryType, Note, NumFormat, PinDir, SEGMENTS, SwitchMode,
    TagDir, data_width_index, numeric_digits,
};

pub use atlas::{Atlas, MeshRange, atlas};
use path::{P, arc, circle, cubic, quad, rect};
pub use tess::SymVertex;

/// Drawing convention for logic gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SymbolStyle {
    /// ANSI / IEEE Std 91 distinctive shapes.
    #[default]
    Ansi,
    /// IEC 60617 rectangular shapes with qualifying symbols (`&`, `≥1`, `=1`, `1`).
    Iec,
}

impl SymbolStyle {
    pub const ALL: [Self; 2] = [Self::Ansi, Self::Iec];

    /// Index used by the GPU mesh table.
    #[must_use]
    pub const fn index(self) -> u32 {
        match self {
            Self::Ansi => 0,
            Self::Iec => 1,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ansi => "ANSI (distinctive shapes)",
            Self::Iec => "IEC (rectangular)",
        }
    }
}

/// Colour role of a vertex, resolved per instance by the shader / painter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Role {
    /// Body outline (kind accent, lightened).
    Outline = 0,
    /// Body interior (dark; tinted green while the output is high).
    Fill = 1,
    /// State indicator (LED lamp, clock trace): bright / dim / grey.
    State = 2,
    /// Output pin stub (and the switch lever): the net colour, like wires.
    PinOut = 3,
    /// Input pin stub.
    PinIn = 4,
    /// Labels and qualifying symbols.
    Text = 5,
    /// 7-segment display segment `pin` (lit by display signal `pin`, with
    /// the polarity of the instance's variant); unlit segments are ghosts.
    Segment = 6,
    /// Numeric output: segment `pin` of digit `extra` (0 = rightmost). The
    /// shader computes the shown value from the display signals.
    Digit = 7,
    /// Annotation ink: the component's colour (or the kind's default).
    Ink = 8,
}

impl Role {
    #[must_use]
    pub const fn from_bits(b: u32) -> Self {
        match b {
            1 => Self::Fill,
            2 => Self::State,
            3 => Self::PinOut,
            4 => Self::PinIn,
            5 => Self::Text,
            6 => Self::Segment,
            7 => Self::Digit,
            8 => Self::Ink,
            _ => Self::Outline,
        }
    }
}

/// Stroke width class (fills use `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Width {
    None = 0,
    Outline = 1,
    /// Same as wires, so pin stubs continue them seamlessly.
    Pin = 2,
    Thin = 3,
}

impl Width {
    #[must_use]
    pub const fn from_bits(b: u32) -> Self {
        match b {
            1 => Self::Outline,
            2 => Self::Pin,
            3 => Self::Thin,
            _ => Self::None,
        }
    }

    /// Half line width in grid units at a zoom of `px_per_unit` physical
    /// pixels per unit: a world width, never thinner than about one pixel.
    /// Mirrors `sym_half_width` in `symbol.wgsl`.
    #[must_use]
    pub fn half_width(self, px_per_unit: f32) -> f32 {
        let px = px_per_unit.max(1e-3);
        match self {
            Self::None => 0.0,
            Self::Outline => 0.055f32.max(0.6 / px),
            Self::Pin => 0.06f32.max(0.5 / px),
            Self::Thin => 0.04f32.max(0.5 / px),
        }
    }
}

/// When a vertex is shown, depending on the instance's net level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Layer {
    Always = 0,
    High = 1,
    NotHigh = 2,
}

impl Layer {
    #[must_use]
    pub const fn from_bits(b: u32) -> Self {
        match b {
            1 => Self::High,
            2 => Self::NotHigh,
            _ => Self::Always,
        }
    }

    /// Visible at `level` (0 low, 1 high, 2 floating, 3 unknown)?
    #[must_use]
    pub const fn shows(self, level: u8) -> bool {
        match self {
            Self::Always => true,
            Self::High => level == 1,
            Self::NotHigh => level != 1,
        }
    }
}

/// Every symbol the app knows: one per mesh of the components of `bw-core`
/// (plus the junction dot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Gate(GateOp, u8),
    Not,
    /// Toggle switch, push button or pulse input.
    Switch(SwitchMode),
    Clock,
    Led,
    TriState,
    /// Type, trigger, preset/clear pins.
    FlipFlop(FlipFlopType, FfTrigger, bool),
    /// Type, address lines, data bits.
    Memory(MemoryType, u8, u8),
    /// Macro tag (interface pin inside a definition): direction, tri-state.
    Tag(TagDir, bool),
    /// Macro instance / generic IC. Instances are drawn with their own
    /// definition's mesh ([`macro_symbol`], appended to the GPU atlas when
    /// the definition appears or changes); the static mesh is a generic IC
    /// box used for icons.
    Macro,
    /// 7-segment display (both polarities share the mesh; the polarity is
    /// a shader parameter in the instance variant).
    SevenSegment,
    /// Numeric output: bits, format, signed (each its own mesh: the digit
    /// count sets the size).
    Numeric(u8, NumFormat, bool),
    Vcc,
    Ground,
    SequenceGenerator,
    /// Text annotation. Instances are drawn with their own note's mesh
    /// (see [`note_symbol`]); the static mesh is the palette icon.
    Text,
    /// Solder dot where three or more wires meet (drawn automatically, not
    /// a component; its own GPU bucket).
    Junction,
}

/// Number of GPU kind codes (see [`crate::kind_code`]): the 16 original
/// primitive kinds, macro tags, macro instances, VCC / ground, sequence
/// generator, 7-segment display, numeric output, text and the junction dot.
pub const KIND_CODES: u32 = 24;
/// Kind code of macro instances (their mesh comes from the per-definition
/// table, see `symbols::atlas::INFO_MACRO`).
pub const MACRO_CODE: u32 = 17;
/// Kind code of power and ground (variant 1 = VCC, 0 = ground).
pub const CONST_CODE: u32 = 18;
/// Kind code of sequence generators.
pub const SEQ_CODE: u32 = 19;
/// Kind code of 7-segment displays (variant bit 0: common anode).
pub const SEVEN_SEG_CODE: u32 = 20;
/// Kind code of numeric outputs (variant: see [`numeric_variant`]).
pub const NUMERIC_CODE: u32 = 21;
/// Kind code of text annotations (their mesh comes from the per-note table,
/// see `symbols::atlas::INFO_NOTE`).
pub const TEXT_CODE: u32 = 22;
/// Kind code of junction dots (the last bucket).
pub const JUNCTION_CODE: u32 = 23;

/// Variant of a numeric output: `bits - 1` (bits 0..4), format (4..6),
/// signed (6).
#[must_use]
pub fn numeric_variant(bits: u8, format: NumFormat, signed: bool) -> u32 {
    let bits = bits.clamp(MIN_NUMERIC_BITS, MAX_NUMERIC_BITS);
    u32::from(bits - 1) | (u32::from(format.index()) << 4) | (u32::from(signed) << 6)
}

/// Display colour of a palette entry on the dark canvas (0..1 RGB; mirrors
/// `palette_color` in `common.wgsl`). The entries are the nominal colours,
/// brightened where they would vanish on the dark background (navy, the
/// darker greens); **black is drawn as the default foreground** (light
/// grey), the canvas' equivalent of black ink on white paper.
#[must_use]
pub const fn palette_rgb(c: Color) -> [f32; 3] {
    match c {
        Color::Red => [0.95, 0.25, 0.22],
        Color::Orange => [1.0, 0.58, 0.12],
        Color::Yellow => [0.98, 0.92, 0.20],
        Color::Lime => [0.45, 0.98, 0.30],
        Color::Green => [0.18, 0.72, 0.28],
        Color::Cyan => [0.25, 0.92, 0.95],
        Color::Blue => [0.32, 0.48, 1.0],
        Color::Navy => [0.32, 0.38, 0.82],
        Color::Purple => [0.70, 0.38, 0.88],
        Color::Magenta => [0.98, 0.32, 0.92],
        Color::White => [1.0, 1.0, 1.0],
        Color::Grey => [0.62, 0.63, 0.66],
        Color::Black => [0.86, 0.88, 0.92],
        Color::Brown => [0.74, 0.47, 0.22],
        Color::Olive => [0.68, 0.68, 0.34],
        Color::Teal => [0.16, 0.70, 0.68],
    }
}

/// Does a kind show a user-chosen colour (LED, clock, sequence generator,
/// 7-segment display, numeric output, text)?
#[must_use]
pub const fn takes_color(kind: &ComponentKind) -> bool {
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
/// Variant slots per kind in the GPU mesh table: the gate input count, the
/// flip-flop trigger + preset/clear, or the memory size (see
/// [`SymbolKind::variant`]); 8 bits of the packed instance kind.
pub const VARIANT_SLOTS: u32 = 256;

/// Variant index of a memory size: `(addr_bits - 4) * 5 + width index`.
#[must_use]
pub fn memory_variant(addr_bits: u8, data_bits: u8) -> u32 {
    let w = data_width_index(data_bits).unwrap_or(0);
    let widths = u32::try_from(DATA_WIDTHS.len()).unwrap_or(u32::MAX);
    u32::from(addr_bits.saturating_sub(MIN_ADDR_BITS)) * widths + u32::from(w)
}

impl SymbolKind {
    #[must_use]
    pub fn of(kind: &ComponentKind) -> Self {
        match *kind {
            ComponentKind::Gate { op, inputs } => {
                Self::Gate(op, inputs.clamp(MIN_GATE_INPUTS, bw_core::MAX_GATE_INPUTS))
            }
            ComponentKind::Not => Self::Not,
            ComponentKind::Switch { mode, .. } => Self::Switch(mode),
            ComponentKind::Clock { .. } => Self::Clock,
            ComponentKind::Led => Self::Led,
            ComponentKind::TriState => Self::TriState,
            ComponentKind::FlipFlop { ty, trigger, preset_clear } => Self::FlipFlop(ty, trigger, preset_clear),
            ComponentKind::Memory { ty, addr_bits, data_bits } => Self::Memory(ty, addr_bits, data_bits),
            ComponentKind::Tag { dir, tristate } => Self::Tag(dir, tristate && dir == TagDir::Out),
            ComponentKind::Macro(_) => Self::Macro,
            ComponentKind::Const { high: true } => Self::Vcc,
            ComponentKind::Const { high: false } => Self::Ground,
            ComponentKind::SeqGen { .. } => Self::SequenceGenerator,
            ComponentKind::SevenSeg { .. } => Self::SevenSegment,
            ComponentKind::Numeric { bits, format, signed } => {
                Self::Numeric(bits.clamp(MIN_NUMERIC_BITS, MAX_NUMERIC_BITS), format, signed)
            }
            ComponentKind::Text { .. } => Self::Text,
        }
    }

    /// Every symbol variant: gates for each input count, the other
    /// components (every switch mode, flip-flop variant, memory size and
    /// numeric output layout), tags, macros, the junction dot and text.
    #[must_use]
    pub fn all() -> Vec<Self> {
        let mut v = Vec::new();
        for op in GateOp::ALL {
            for n in MIN_GATE_INPUTS..=bw_core::MAX_GATE_INPUTS {
                v.push(Self::Gate(op, n));
            }
        }
        v.push(Self::Not);
        v.extend(SwitchMode::ALL.map(Self::Switch));
        v.extend([Self::Clock, Self::Led, Self::TriState]);
        for ty in FlipFlopType::ALL {
            for trigger in FfTrigger::ALL {
                for pc in [false, true] {
                    if (ComponentKind::FlipFlop { ty, trigger, preset_clear: pc }).is_valid() {
                        v.push(Self::FlipFlop(ty, trigger, pc));
                    }
                }
            }
        }
        for ty in [MemoryType::Ram, MemoryType::Rom] {
            for a in MIN_ADDR_BITS..=MAX_ADDR_BITS {
                for d in DATA_WIDTHS {
                    v.push(Self::Memory(ty, a, d));
                }
            }
        }
        v.extend([Self::Tag(TagDir::In, false), Self::Tag(TagDir::Out, false), Self::Tag(TagDir::Out, true)]);
        v.push(Self::Macro);
        v.push(Self::Junction);
        v.extend([Self::Vcc, Self::Ground, Self::SequenceGenerator, Self::SevenSegment]);
        for bits in MIN_NUMERIC_BITS..=MAX_NUMERIC_BITS {
            for format in NumFormat::ALL {
                for signed in [false, true] {
                    v.push(Self::Numeric(bits, format, signed));
                }
            }
        }
        v.push(Self::Text);
        v
    }

    /// Mesh variant within the kind code (see [`VARIANT_SLOTS`]); matches
    /// [`crate::body_kind`].
    #[must_use]
    pub fn variant(self) -> u32 {
        match self {
            Self::Gate(_, n) => u32::from(n),
            Self::FlipFlop(_, trigger, pc) => u32::from(trigger.index()) | (u32::from(pc) << 2),
            Self::Memory(_, a, d) => memory_variant(a, d),
            Self::Tag(TagDir::Out, tri) => 1 + u32::from(tri),
            Self::Switch(mode) => switch_variant(mode),
            Self::Vcc => 1,
            Self::Numeric(bits, format, signed) => numeric_variant(bits, format, signed),
            _ => 0,
        }
    }

    /// Does each variant have its own mesh (else one mesh serves every
    /// variant slot of the kind code)?
    #[must_use]
    pub const fn per_variant_mesh(self) -> bool {
        matches!(
            self,
            Self::Gate(..)
                | Self::FlipFlop(..)
                | Self::Memory(..)
                | Self::Tag(..)
                | Self::Switch(..)
                | Self::Vcc
                | Self::Ground
                | Self::Numeric(..)
        )
    }

    /// GPU kind code; matches [`crate::kind_code`].
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Gate(op, _) => match op {
                GateOp::And => 0,
                GateOp::Or => 1,
                GateOp::Nand => 2,
                GateOp::Nor => 3,
                GateOp::Xor => 4,
                GateOp::Xnor => 5,
            },
            Self::Not => 6,
            Self::Switch(_) => 7,
            Self::Clock => 8,
            Self::Led => 9,
            Self::TriState => 10,
            Self::FlipFlop(FlipFlopType::D, ..) => 11,
            Self::FlipFlop(FlipFlopType::Rs, ..) => 12,
            Self::FlipFlop(FlipFlopType::Jk, ..) => 13,
            Self::Memory(MemoryType::Ram, ..) => 14,
            Self::Memory(MemoryType::Rom, ..) => 15,
            Self::Tag(..) => 16,
            Self::Macro => MACRO_CODE,
            Self::Vcc | Self::Ground => CONST_CODE,
            Self::SequenceGenerator => SEQ_CODE,
            Self::SevenSegment => SEVEN_SEG_CODE,
            Self::Numeric(..) => NUMERIC_CODE,
            Self::Text => TEXT_CODE,
            Self::Junction => JUNCTION_CODE,
        }
    }

    /// Does the IEC drawing differ from the ANSI one?
    #[must_use]
    pub const fn style_dependent(self) -> bool {
        matches!(self, Self::Gate(..) | Self::Not | Self::TriState)
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gate(op, _) => op.name(),
            Self::Not => "NOT",
            Self::Switch(SwitchMode::Toggle) => "Switch",
            Self::Switch(SwitchMode::Momentary) => "Push button",
            Self::Switch(SwitchMode::Pulse) => "Pulse input",
            Self::Clock => "Clock",
            Self::Led => "LED",
            Self::FlipFlop(FlipFlopType::D, ..) => "D flip-flop",
            Self::FlipFlop(FlipFlopType::Rs, ..) => "RS flip-flop",
            Self::FlipFlop(FlipFlopType::Jk, ..) => "JK flip-flop",
            Self::TriState => "Tri-state buffer",
            Self::Memory(MemoryType::Ram, ..) => "RAM",
            Self::Memory(MemoryType::Rom, ..) => "ROM",
            Self::SevenSegment => "7-segment display",
            Self::Numeric(..) => "Numeric output",
            Self::Vcc => "Power (VCC)",
            Self::Ground => "Ground",
            Self::SequenceGenerator => "Sequence generator",
            Self::Macro => "Macro / IC",
            Self::Tag(TagDir::In, _) => "Macro tag (input)",
            Self::Tag(TagDir::Out, false) => "Macro tag (output)",
            Self::Tag(TagDir::Out, true) => "Macro tag (tri-state output)",
            Self::Junction => "Wire junction",
            Self::Text => "Text",
        }
    }

    /// Accent colour (linear-ish RGB, 0..1); mirrors `kind_accent` in the
    /// shaders (one per kind code).
    #[must_use]
    pub const fn accent(self) -> [f32; 3] {
        match self {
            Self::Gate(GateOp::And | GateOp::Nand, _) => [0.35, 0.62, 0.95],
            Self::Gate(GateOp::Or | GateOp::Nor, _) => [0.95, 0.62, 0.30],
            Self::Gate(GateOp::Xor | GateOp::Xnor, _) => [0.70, 0.45, 0.95],
            Self::Not | Self::TriState => [0.85, 0.85, 0.40],
            Self::Switch(_) => [0.40, 0.85, 0.55],
            Self::Clock | Self::SequenceGenerator => [0.40, 0.85, 0.85],
            Self::Led | Self::SevenSegment | Self::Numeric(..) => [0.95, 0.35, 0.35],
            Self::FlipFlop(..) => [0.95, 0.55, 0.78],
            Self::Memory(..) => [0.55, 0.78, 0.95],
            Self::Vcc | Self::Ground => [0.92, 0.66, 0.38],
            Self::Macro => [0.78, 0.78, 0.84],
            Self::Tag(..) => [0.95, 0.80, 0.35],
            Self::Junction => [0.45, 0.50, 0.58],
            Self::Text => [0.80, 0.82, 0.86],
        }
    }

    /// Default display colour (lit LED, clock and sequence traces, lit
    /// segments, digits, annotation text) when the component has no colour
    /// of its own; mirrors `kind_ink` in the shaders.
    #[must_use]
    pub const fn ink(self) -> [f32; 3] {
        match self {
            Self::SequenceGenerator | Self::SevenSegment => [0.95, 0.35, 0.35],
            Self::Numeric(..) | Self::Text => [0.86, 0.88, 0.92],
            k => k.accent(),
        }
    }
}

/// Variant (and mesh) of a switch mode.
#[must_use]
pub const fn switch_variant(mode: SwitchMode) -> u32 {
    match mode {
        SwitchMode::Toggle => 0,
        SwitchMode::Momentary => 1,
        SwitchMode::Pulse => 2,
    }
}

/// Colour of the unknown (X) level, on wires and state indicators.
pub const X_COLOR: [f32; 3] = [1.0, 0.55, 0.10];

/// Wire / pin colour of a net level (0 low, 1 high, 2 floating, 3
/// unknown). Mirrors `level_color` in `common.wgsl`.
#[must_use]
pub const fn level_color(level: u8) -> [f32; 3] {
    match level {
        1 => [0.30, 0.95, 0.40],
        0 => [0.12, 0.42, 0.18],
        3 => X_COLOR,
        _ => [0.45, 0.50, 0.58],
    }
}

/// Colour of a role for a symbol with `accent` at `level` (the accent is
/// also the display colour). See [`shade_ink`].
#[must_use]
pub fn shade(role: Role, accent: [f32; 3], level: u8) -> [f32; 3] {
    shade_ink(role, accent, accent, level)
}

/// Unlit segment / digit ("ghost") colour for display colour `ink`.
#[must_use]
pub fn ghost(ink: [f32; 3]) -> [f32; 3] {
    ink.map(|c| 0.2 + c * 0.1)
}

/// Colour of a role for a symbol with `accent` and display colour `ink` at
/// `level` (segments and digits: lit at 1). Mirrors `sym_color` in
/// `symbol.wgsl`, which also resolves the display logic of segments and
/// digits per instance.
#[must_use]
pub fn shade_ink(role: Role, accent: [f32; 3], ink: [f32; 3], level: u8) -> [f32; 3] {
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t);
    let outline = mix(accent, [0.92; 3], 0.45);
    match role {
        Role::Segment | Role::Digit => match level {
            1 => ink,
            3 => X_COLOR,
            _ => ghost(ink),
        },
        Role::Ink => ink,
        Role::Outline | Role::Text | Role::PinIn => outline,
        Role::Fill => {
            if level == 1 {
                [0.19, 0.25, 0.21]
            } else {
                [0.16, 0.17, 0.20]
            }
        }
        Role::State => match level {
            1 => ink,
            0 => ink.map(|c| c * 0.28),
            3 => X_COLOR,
            _ => [0.32; 3],
        },
        Role::PinOut => level_color(level),
    }
}

/// One path of a symbol: an optional fill and an optional stroke.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub points: Vec<P>,
    pub closed: bool,
    pub fill: Option<Role>,
    pub stroke: Option<(Role, Width)>,
    pub layer: Layer,
    /// Labels: drawn upright around this point whatever the rotation.
    pub anchor: Option<P>,
    /// Output index whose net colours a [`Role::PinOut`] stroke; the
    /// segment of a [`Role::Segment`] or [`Role::Digit`] shape.
    pub pin: u8,
    /// Digit position of a [`Role::Digit`] shape (0 = rightmost).
    pub extra: u8,
    /// Stroke half-width baked into the geometry (world units), on top of
    /// the width class (annotation text scales with its height).
    pub bake: f32,
}

/// A pin anchor of a symbol (local, unrotated grid units).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymPin {
    pub at: P,
    pub dir: PinDir,
}

/// Vector description of one symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    /// Body size (w, h) in grid units, unrotated (as `ComponentKind::size`).
    pub size: P,
    /// Painter's order: later shapes are drawn on top.
    pub shapes: Vec<Shape>,
    /// Inputs first, then outputs (the `bw-core` pin numbering).
    pub pins: Vec<SymPin>,
}

/// Bubble (inversion circle) radius.
const BUBBLE_R: f32 = 0.25;
/// Gate body: half height of the core shape, back x and tip x.
const GATE_HALF: f32 = 1.75;
const GATE_BACK: f32 = 0.5;
const GATE_FRONT: f32 = 2.9;
/// Width of a gate (matches `ComponentKind::size`).
const GATE_W: f32 = 4.0;

struct B {
    size: P,
    shapes: Vec<Shape>,
    inputs: Vec<SymPin>,
    outputs: Vec<SymPin>,
}

impl B {
    fn new(w: f32, h: f32) -> Self {
        Self { size: [w, h], shapes: Vec::new(), inputs: Vec::new(), outputs: Vec::new() }
    }

    fn push(&mut self, points: Vec<P>, closed: bool, fill: Option<Role>, stroke: Option<(Role, Width)>) -> &mut Shape {
        self.shapes.push(Shape {
            points,
            closed,
            fill,
            stroke,
            layer: Layer::Always,
            anchor: None,
            pin: 0,
            extra: 0,
            bake: 0.0,
        });
        let n = self.shapes.len() - 1;
        &mut self.shapes[n]
    }

    /// Filled + outlined closed body.
    fn body(&mut self, points: Vec<P>) {
        self.push(points, true, Some(Role::Fill), Some((Role::Outline, Width::Outline)));
    }

    fn line(&mut self, points: Vec<P>, role: Role, width: Width) -> &mut Shape {
        self.push(points, false, None, Some((role, width)))
    }

    fn loop_line(&mut self, points: Vec<P>, role: Role, width: Width) {
        self.push(points, true, None, Some((role, width)));
    }

    fn dot(&mut self, c: P, r: f32, role: Role) -> &mut Shape {
        self.push(circle(c, r, 16), true, Some(role), None)
    }

    fn bubble(&mut self, cx: f32, cy: f32) {
        self.push(circle([cx, cy], BUBBLE_R, 20), true, Some(Role::Fill), Some((Role::Outline, Width::Outline)));
    }

    fn text(&mut self, s: &str, cx: f32, cy: f32, h: f32) {
        for l in font::layout(s, cx, cy, h) {
            self.line(l, Role::Text, Width::Thin).anchor = Some([cx, cy]);
        }
    }

    /// Upright label keeping its case (segment names a … g).
    fn text_cased(&mut self, s: &str, cx: f32, cy: f32, h: f32) {
        for l in font::layout_centered_cased(s, cx, cy, h) {
            self.line(l, Role::Text, Width::Thin).anchor = Some([cx, cy]);
        }
    }

    /// Text with an overline (e.g. Q̄).
    fn text_bar(&mut self, s: &str, cx: f32, cy: f32, h: f32) {
        self.text(s, cx, cy, h);
        let w = font::text_width(s.chars().count(), h);
        let y = cy - h / 2.0 - h * 0.28;
        self.line(vec![[cx - w / 2.0, y], [cx + w / 2.0, y]], Role::Text, Width::Thin).anchor = Some([cx, cy]);
    }

    /// Input pin at the left edge, stub to `x_end`.
    fn input(&mut self, y: f32, x_end: f32) {
        self.line(vec![[0.0, y], [x_end, y]], Role::PinIn, Width::Pin);
        self.inputs.push(SymPin { at: [0.0, y], dir: PinDir::Input });
    }

    /// Input pin at the top edge (e.g. an enable), stub down to `y_end`.
    fn input_top(&mut self, x: f32, y_end: f32) {
        self.line(vec![[x, 0.0], [x, y_end]], Role::PinIn, Width::Pin);
        self.inputs.push(SymPin { at: [x, 0.0], dir: PinDir::Input });
    }

    /// Input pin at the bottom edge (e.g. a clear), stub up to `y_end`.
    fn input_bottom(&mut self, x: f32, y_end: f32) {
        let h = self.size[1];
        self.line(vec![[x, h], [x, y_end]], Role::PinIn, Width::Pin);
        self.inputs.push(SymPin { at: [x, h], dir: PinDir::Input });
    }

    /// Output pin at the right edge, stub from `x_start`, coloured by its
    /// own net.
    fn output(&mut self, y: f32, x_start: f32) {
        let w = self.size[0];
        let pin = u8::try_from(self.outputs.len()).unwrap_or(u8::MAX);
        self.line(vec![[x_start, y], [w, y]], Role::PinOut, Width::Pin).pin = pin;
        self.outputs.push(SymPin { at: [w, y], dir: PinDir::Output });
    }

    /// Input pin on the right edge (DIL packages), stub from `x_start`.
    fn input_right(&mut self, y: f32, x_start: f32) {
        let w = self.size[0];
        self.line(vec![[w, y], [x_start, y]], Role::PinIn, Width::Pin);
        self.inputs.push(SymPin { at: [w, y], dir: PinDir::Input });
    }

    /// Output pin on the right edge with an explicit output index.
    fn output_at(&mut self, y: f32, x_start: f32, k: u8) {
        let w = self.size[0];
        self.line(vec![[x_start, y], [w, y]], Role::PinOut, Width::Pin).pin = k;
        self.outputs.push(SymPin { at: [w, y], dir: PinDir::Output });
    }

    /// Output pin on the left edge (DIL packages).
    fn output_left(&mut self, y: f32, x_start: f32, k: u8) {
        self.line(vec![[x_start, y], [0.0, y]], Role::PinOut, Width::Pin).pin = k;
        self.outputs.push(SymPin { at: [0.0, y], dir: PinDir::Output });
    }

    /// Text stacked vertically (one upright glyph per line) centred on
    /// `(cx, cy)`: names along the long axis of narrow DIL packages.
    #[allow(clippy::cast_precision_loss)]
    fn text_vertical(&mut self, s: &str, cx: f32, cy: f32, h: f32) {
        let step = h * 1.35;
        let n = s.chars().count();
        let y0 = cy - step * (n as f32 - 1.0) / 2.0;
        for (i, c) in s.chars().enumerate() {
            if c != ' ' {
                self.text(&c.to_string(), cx, y0 + step * i as f32, h);
            }
        }
    }

    fn finish(self) -> Symbol {
        let mut pins = self.inputs;
        pins.extend(self.outputs);
        Symbol { size: self.size, shapes: self.shapes, pins }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    And,
    Or,
    Xor,
}

fn family(op: GateOp) -> (Family, bool) {
    match op {
        GateOp::And => (Family::And, false),
        GateOp::Nand => (Family::And, true),
        GateOp::Or => (Family::Or, false),
        GateOp::Nor => (Family::Or, true),
        GateOp::Xor => (Family::Xor, false),
        GateOp::Xnor => (Family::Xor, true),
    }
}

/// Bulge of the OR/XOR back curve (quadratic control offset).
const OR_BULGE: f32 = 0.55;

/// x of a concave back curve starting at `x0` between `yt` and `yb`.
fn back_curve_x(x0: f32, y: f32, yt: f32, yb: f32) -> f32 {
    if y <= yt || y >= yb {
        return x0;
    }
    let t = (y - yt) / (yb - yt);
    x0 + 2.0 * t * (1.0 - t) * OR_BULGE
}

/// The output side of a gate (and NOT): plain stub or bubble + stub.
fn gate_output(b: &mut B, front: f32, c: f32, invert: bool) {
    if invert {
        b.bubble(front + BUBBLE_R, c);
        b.output(c, front + 2.0 * BUBBLE_R);
    } else {
        b.output(c, front);
    }
}

#[allow(clippy::cast_precision_loss)]
fn gate(op: GateOp, n: u8, style: SymbolStyle) -> Symbol {
    let n = n.clamp(MIN_GATE_INPUTS, bw_core::MAX_GATE_INPUTS);
    let h = 2.0 * f32::from(n);
    let c = f32::from(n);
    let (fam, invert) = family(op);
    let mut b = B::new(GATE_W, h);
    let pin_y = |i: u8| 2.0 * f32::from(i) + 1.0;
    match style {
        SymbolStyle::Iec => {
            // Rectangle growing with the input count, qualifier on top.
            for i in 0..n {
                b.input(pin_y(i), GATE_BACK);
            }
            let (top, bot) = (0.25, h - 0.25);
            b.body(rect(GATE_BACK, top, GATE_FRONT, bot));
            let label = match fam {
                Family::And => "&",
                Family::Or => "≥1",
                Family::Xor => "=1",
            };
            b.text(label, f32::midpoint(GATE_BACK, GATE_FRONT), top + 0.75, 0.8);
            gate_output(&mut b, GATE_FRONT, c, invert);
        }
        SymbolStyle::Ansi => {
            let (yt, yb) = (c - GATE_HALF, c + GATE_HALF);
            let outer = GATE_BACK;
            let back = if fam == Family::Xor { GATE_BACK + 0.35 } else { GATE_BACK };
            // Input stubs end on the (outer) back line or curve.
            for i in 0..n {
                let y = pin_y(i);
                let x_end = if fam == Family::And { outer } else { back_curve_x(outer, y, yt, yb) };
                b.input(y, x_end);
            }
            // The input side grows with the input count (IEEE: extended back line).
            let (ext_top, ext_bot) = (0.5, h - 0.5);
            if ext_top < yt - 1e-3 {
                b.line(vec![[outer, ext_top], [outer, yt]], Role::Outline, Width::Outline);
                b.line(vec![[outer, yb], [outer, ext_bot]], Role::Outline, Width::Outline);
            }
            let shape = match fam {
                Family::And => {
                    let flat = GATE_FRONT - GATE_HALF;
                    let mut v = vec![[back, yb], [back, yt], [flat, yt]];
                    path::join(&mut v, &arc([flat, c], GATE_HALF, -PI / 2.0, PI / 2.0, 28));
                    v
                }
                Family::Or | Family::Xor => {
                    let mut v = cubic([back, yt], [back + 1.1, yt], [2.45, c - 0.9], [GATE_FRONT, c], 18);
                    path::join(&mut v, &cubic([GATE_FRONT, c], [2.45, c + 0.9], [back + 1.1, yb], [back, yb], 18));
                    let bk = quad([back, yb], [back + OR_BULGE, c], [back, yt], 14);
                    path::join(&mut v, &bk[..bk.len() - 1]);
                    v
                }
            };
            b.body(shape);
            if fam == Family::Xor {
                b.line(quad([outer, yt], [outer + OR_BULGE, c], [outer, yb], 14), Role::Outline, Width::Outline);
            }
            gate_output(&mut b, GATE_FRONT, c, invert);
        }
    }
    b.finish()
}

fn not_gate(style: SymbolStyle) -> Symbol {
    let mut b = B::new(4.0, 2.0);
    let front = 2.65;
    match style {
        SymbolStyle::Ansi => {
            b.input(1.0, 0.6);
            b.body(vec![[0.6, 0.2], [front, 1.0], [0.6, 1.8]]);
        }
        SymbolStyle::Iec => {
            b.input(1.0, 0.6);
            b.body(rect(0.6, 0.15, front, 1.85));
            b.text("1", f32::midpoint(0.6, front), 1.0, 0.8);
        }
    }
    gate_output(&mut b, front, 1.0, true);
    b.finish()
}

/// Interactive input: the lever / button / pulse is drawn active (net
/// colour) when the output is high and at rest otherwise; both states live
/// in the mesh, the net bit picks one.
fn switch(mode: SwitchMode) -> Symbol {
    let mut b = B::new(2.0, 2.0);
    b.body(rect(0.15, 0.15, 1.65, 1.85));
    match mode {
        SwitchMode::Toggle => {
            // SPST lever: closed along the contact line when on, raised
            // otherwise.
            b.output(1.0, 1.35);
            b.line(vec![[0.45, 1.0], [1.35, 1.0]], Role::PinOut, Width::Outline).layer = Layer::High;
            b.line(vec![[0.45, 1.0], [1.22, 0.42]], Role::Outline, Width::Outline).layer = Layer::NotHigh;
            b.dot([0.45, 1.0], 0.13, Role::Outline);
            b.dot([1.35, 1.0], 0.13, Role::Outline);
        }
        SwitchMode::Momentary => {
            // Push button: a bridge on a plunger, resting on both contacts
            // (net colour) while the output is high, lifted otherwise.
            b.output(1.0, 1.35);
            b.line(vec![[0.66, 0.38], [1.14, 0.38]], Role::Outline, Width::Outline);
            b.line(vec![[0.9, 0.38], [0.9, 0.84]], Role::Outline, Width::Thin).layer = Layer::High;
            b.line(vec![[0.32, 0.84], [1.48, 0.84]], Role::PinOut, Width::Outline).layer = Layer::High;
            b.line(vec![[0.9, 0.38], [0.9, 0.6]], Role::Outline, Width::Thin).layer = Layer::NotHigh;
            b.line(vec![[0.32, 0.6], [1.48, 0.6]], Role::Outline, Width::Outline).layer = Layer::NotHigh;
            b.dot([0.45, 1.0], 0.13, Role::Outline);
            b.dot([1.35, 1.0], 0.13, Role::Outline);
            b.line(vec![[0.45, 1.0], [0.45, 1.55]], Role::Outline, Width::Thin);
        }
        SwitchMode::Pulse => {
            // One pulse with an arrow on its rising edge (a clock has two
            // periods and no arrow).
            let (lo, hi) = (1.38, 0.62);
            let wave = vec![[0.32, lo], [0.68, lo], [0.68, hi], [1.12, hi], [1.12, lo], [1.45, lo]];
            b.line(wave.clone(), Role::Outline, Width::Outline);
            b.line(wave, Role::State, Width::Outline).layer = Layer::High;
            b.line(vec![[0.53, 1.08], [0.68, 0.86], [0.83, 1.08]], Role::Outline, Width::Thin);
            b.output(1.0, 1.65);
        }
    }
    b.finish()
}

/// Clock: box with a square-wave glyph whose colour follows the level.
fn clock() -> Symbol {
    let mut b = B::new(2.0, 2.0);
    b.body(rect(0.15, 0.15, 1.65, 1.85));
    let (lo, hi) = (1.35, 0.65);
    let wave = vec![[0.35, lo], [0.6, lo], [0.6, hi], [0.95, hi], [0.95, lo], [1.3, lo], [1.3, hi], [1.45, hi]];
    // Always visible; drawn again in the bright state colour while high.
    b.line(wave.clone(), Role::Outline, Width::Outline);
    b.line(wave, Role::State, Width::Outline).layer = Layer::High;
    b.output(1.0, 1.65);
    b.finish()
}

/// LED: a round lamp whose fill is the lit / dark / floating colour.
fn led() -> Symbol {
    let mut b = B::new(2.0, 2.0);
    let (c, r) = ([1.12, 1.0], 0.72);
    // The input stub shows the driving net (the LED's net is its driver).
    b.line(vec![[0.0, 1.0], [c[0] - r, 1.0]], Role::PinOut, Width::Pin);
    b.inputs.push(SymPin { at: [0.0, 1.0], dir: PinDir::Input });
    b.push(circle(c, r, 32), true, Some(Role::State), Some((Role::Outline, Width::Outline)));
    // Highlight glint while lit.
    b.line(arc(c, r * 0.6, PI * 1.05, PI * 1.45, 6), Role::Text, Width::Thin).layer = Layer::High;
    b.finish()
}

/// Flip-flop box (IEC-style qualifiers, same in both styles): data inputs
/// and the clock on the left, Q / Q̄ on the right, optional preset (top)
/// and clear (bottom). The clock input shows the trigger: a plain `C` for
/// the level-sensitive latch, the dynamic triangle for edge triggering
/// (with an inversion bubble for the falling edge), and for master-slave a
/// plain `C` plus the postponed-output mark (⌐) at both outputs.
fn flip_flop(ty: FlipFlopType, trigger: FfTrigger, preset_clear: bool) -> Symbol {
    let kind = ComponentKind::FlipFlop { ty, trigger, preset_clear };
    #[allow(clippy::cast_precision_loss)]
    let (w, h) = (kind.size().0 as f32, kind.size().1 as f32);
    let mut b = B::new(w, h);
    let (x0, x1) = (0.6, w - 0.6);
    let labels = ty.input_labels();
    let clock = usize::from(FlipFlopType::CLOCK_INPUT);
    let falling = trigger == FfTrigger::FallingEdge;
    for i in 0..labels.len() {
        #[allow(clippy::cast_precision_loss)]
        let y = 2.0 * i as f32 + 1.0;
        let end = if i == clock && falling { x0 - 2.0 * BUBBLE_R } else { x0 };
        b.input(y, end);
    }
    if preset_clear {
        b.input_top(w / 2.0, 0.2);
        b.input_bottom(w / 2.0, h - 0.2);
    }
    b.body(rect(x0, 0.2, x1, h - 0.2));
    for (i, l) in labels.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let y = 2.0 * i as f32 + 1.0;
        if i == clock && matches!(trigger, FfTrigger::RisingEdge | FfTrigger::FallingEdge) {
            b.line(vec![[x0, y - 0.4], [x0 + 0.5, y], [x0, y + 0.4]], Role::Outline, Width::Outline);
            b.text(l, x0 + 1.1, y, 0.6);
            if falling {
                b.bubble(x0 - BUBBLE_R, y);
            }
        } else {
            b.text(l, x0 + 0.65, y, 0.7);
        }
    }
    if preset_clear {
        b.text("PRE", w / 2.0, 0.85, 0.45);
        b.text("CLR", w / 2.0, h - 0.85, 0.45);
    }
    let (qy, nqy) = (1.0, h - 1.0);
    b.text("Q", x1 - 0.6, qy, 0.7);
    b.text_bar("Q", x1 - 0.6, nqy, 0.7);
    if trigger == FfTrigger::MasterSlave {
        // Postponed output: ⌐ left of each output label.
        for y in [qy, nqy] {
            let x = x1 - 1.55;
            b.line(vec![[x, y - 0.3], [x + 0.45, y - 0.3], [x + 0.45, y + 0.15]], Role::Text, Width::Thin);
        }
    }
    b.output(qy, x1);
    b.output(nqy, x1);
    b.finish()
}

/// Tri-state buffer: IN on the left (y = 2), EN on the top (x = 2), OUT on
/// the right (y = 2), as `ComponentKind::pin_offset` places them.
fn tri_state(style: SymbolStyle) -> Symbol {
    let mut b = B::new(4.0, 3.0);
    let y = 2.0;
    let ex = 2.0;
    match style {
        SymbolStyle::Ansi => {
            b.input(y, 0.6);
            let (x0, x1, half) = (0.6, 2.9, 0.85);
            // Enable stub ends on the triangle's upper edge.
            let ey = (y - half) + (ex - x0) / (x1 - x0) * half;
            b.input_top(ex, ey);
            b.body(vec![[x0, y - half], [x1, y], [x0, y + half]]);
            b.output(y, x1);
        }
        SymbolStyle::Iec => {
            b.input(y, 0.6);
            b.input_top(ex, 1.1);
            b.body(rect(0.6, 1.1, 2.9, 2.9));
            b.text("EN", 1.3, 1.45, 0.3);
            b.text("1", 1.25, 2.2, 0.65);
            // Three-state output qualifier (inverted triangle).
            b.loop_line(vec![[2.2, 2.0], [2.65, 2.0], [2.425, 2.4]], Role::Text, Width::Thin);
            b.output(y, 2.9);
        }
    }
    b.finish()
}

/// RAM / ROM: address lines A0.. then (RAM) data inputs D0.., R/W̄, then
/// C̄S̄ on the left; data outputs on the right; the type and size in the
/// middle. Pins are labelled with small text (and the active-low controls
/// with an overline); the output stubs carry their own net colours.
fn memory(ty: MemoryType, addr_bits: u8, data_bits: u8) -> Symbol {
    let kind = ComponentKind::Memory { ty, addr_bits, data_bits };
    #[allow(clippy::cast_precision_loss)]
    let (w, h) = (kind.size().0 as f32, kind.size().1 as f32);
    let mut b = B::new(w, h);
    let (x0, x1) = (0.6, w - 0.6);
    let pin_y = |i: u8| 2.0 * f32::from(i) + 1.0;
    let ins = kind.input_count();
    for i in 0..ins {
        b.input(pin_y(i), x0);
    }
    b.body(rect(x0, 0.2, x1, h - 0.2));
    // Title band.
    b.line(vec![[2.6, 0.2], [2.6, h - 0.2]], Role::Outline, Width::Thin);
    b.line(vec![[5.4, 0.2], [5.4, h - 0.2]], Role::Outline, Width::Thin);
    b.text(ty.name(), 4.0, h / 2.0 - 0.7, 0.9);
    b.text(&format!("{}X{}", 1u32 << addr_bits, data_bits), 4.0, h / 2.0 + 0.7, 0.45);
    let th = 0.5;
    for i in 0..ins {
        let (y, lx) = (pin_y(i), x0 + 0.95);
        if i < addr_bits {
            b.text(&format!("A{i}"), lx, y, th);
        } else if ty == MemoryType::Ram && i < addr_bits + data_bits {
            b.text(&format!("D{}", i - addr_bits), lx, y, th);
        } else if ty == MemoryType::Ram && i == addr_bits + data_bits {
            // R/W̄: overline on the W only.
            b.text("R/W", lx, y, th);
            let cw = font::text_width(1, th);
            let gx = lx + font::text_width(3, th) / 2.0 - cw / 2.0;
            let oy = y - th / 2.0 - th * 0.28;
            b.line(vec![[gx - cw / 2.0, oy], [gx + cw / 2.0, oy]], Role::Text, Width::Thin).anchor = Some([lx, y]);
        } else {
            b.text_bar("CS", lx, y, th);
        }
    }
    for k in 0..data_bits {
        b.text(&format!("Q{k}"), x1 - 0.95, pin_y(k), th);
        b.output(pin_y(k), x1);
    }
    b.finish()
}

/// A display segment from `a` to `z` as a pointed bar of thickness `t`
/// (a hexagon), with a small gap at both ends.
fn segment_hex(a: P, z: P, t: f32) -> Vec<P> {
    let len = path::dist(a, z).max(1e-4);
    let d = [(z[0] - a[0]) / len, (z[1] - a[1]) / len];
    let n = [-d[1], d[0]];
    let g = t * 0.12;
    let h = t / 2.0;
    let at = |p: P, along: f32, across: f32| [p[0] + d[0] * along + n[0] * across, p[1] + d[1] * along + n[1] * across];
    vec![at(a, g, 0.0), at(a, g + h, h), at(z, -g - h, h), at(z, -g, 0.0), at(z, -g - h, -h), at(a, g + h, -h)]
}

/// Segments a … g of a slanted digit whose box (before the slant) is
/// `l..r` × `t..bo`: endpoint pairs in the usual a … g order.
fn digit_segments(l: f32, r: f32, t: f32, bo: f32, slant: f32) -> [(P, P); 7] {
    let m = f32::midpoint(t, bo);
    let sk = |x: f32, y: f32| [x + (bo - y) * slant, y];
    [
        (sk(l, t), sk(r, t)),
        (sk(r, t), sk(r, m)),
        (sk(r, m), sk(r, bo)),
        (sk(l, bo), sk(r, bo)),
        (sk(l, m), sk(l, bo)),
        (sk(l, t), sk(l, m)),
        (sk(l, m), sk(r, m)),
    ]
}

/// 7-segment display: inputs a … g along the bottom edge (at `x = 1 + i`,
/// labelled), a slanted digit whose segments light from the display
/// signals (polarity per instance).
fn seven_segment() -> Symbol {
    let kind = ComponentKind::SevenSeg { common_anode: false };
    #[allow(clippy::cast_precision_loss)]
    let (w, h) = (kind.size().0 as f32, kind.size().1 as f32);
    let mut b = B::new(w, h);
    let bottom = h - 0.8;
    for i in 0..SEGMENTS {
        b.input_bottom(1.0 + f32::from(i), bottom);
    }
    b.body(rect(0.2, 0.2, w - 0.2, bottom));
    b.push(rect(1.0, 0.8, w - 1.0, bottom - 1.2), true, Some(Role::Fill), Some((Role::Outline, Width::Thin)));
    for (k, (a, z)) in digit_segments(2.3, 5.2, 1.5, 9.0, 0.09).into_iter().enumerate() {
        let s = b.push(segment_hex(a, z, 0.62), true, Some(Role::Segment), None);
        s.pin = u8::try_from(k).unwrap_or(0);
    }
    for (i, l) in ["a", "b", "c", "d", "e", "f", "g"].iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        b.text_cased(l, 1.0 + i as f32, bottom - 0.45, 0.5);
    }
    b.finish()
}

/// Numeric output: input `k` (bit `k`) on the left at `y = 2k + 1`, the
/// value as 7-segment digits (computed in the shader from the display
/// signals), and the format above them.
#[allow(clippy::cast_precision_loss)]
fn numeric_output(bits: u8, format: NumFormat, signed: bool) -> Symbol {
    let bits = bits.clamp(MIN_NUMERIC_BITS, MAX_NUMERIC_BITS);
    let kind = ComponentKind::Numeric { bits, format, signed };
    let (w, h) = (kind.size().0 as f32, kind.size().1 as f32);
    let mut b = B::new(w, h);
    let x0 = 0.6;
    for k in 0..bits {
        let y = 2.0 * f32::from(k) + 1.0;
        b.input(y, x0);
    }
    b.body(rect(x0, 0.2, w - 0.2, h - 0.2));
    for k in 0..bits {
        b.text(&k.to_string(), x0 + 0.45, 2.0 * f32::from(k) + 1.0, 0.36);
    }
    let n = numeric_digits(bits, format, signed);
    let pitch = 1.2;
    let (dw, dh) = (0.78, 1.5);
    let cx = f32::midpoint(x0 + 0.9, w - 0.2);
    let cy = h / 2.0;
    let span = pitch * f32::from(n);
    b.push(
        rect(cx - span / 2.0 - 0.15, cy - dh / 2.0 - 0.3, cx + span / 2.0 + 0.15, cy + dh / 2.0 + 0.3),
        true,
        Some(Role::Fill),
        Some((Role::Outline, Width::Thin)),
    );
    for d in 0..n {
        let dcx = cx + span / 2.0 - pitch * (f32::from(d) + 0.5);
        let (l, r) = (dcx - dw / 2.0 - 0.05, dcx + dw / 2.0 - 0.05);
        for (k, (a, z)) in digit_segments(l, r, cy - dh / 2.0, cy + dh / 2.0, 0.07).into_iter().enumerate() {
            let s = b.push(segment_hex(a, z, 0.2), true, Some(Role::Digit), None);
            s.pin = u8::try_from(k).unwrap_or(0);
            s.extra = d;
        }
    }
    let tag = match (format, signed) {
        (NumFormat::Bin, _) => "BIN",
        (NumFormat::Hex, _) => "HEX",
        (NumFormat::Dec, false) => "DEC",
        (NumFormat::Dec, true) => "+/-",
    };
    b.text(tag, cx, (cy - dh / 2.0 - 0.3) / 2.0 + 0.1, 0.32);
    b.finish()
}

/// Power (VCC): a supply rail with the output stub; ground: the usual
/// three bars, here turned to face the output on the right.
fn constant(high: bool) -> Symbol {
    let mut b = B::new(2.0, 2.0);
    if high {
        b.output(1.0, 0.6);
        b.push(rect(0.42, 0.3, 0.62, 1.7), true, Some(Role::Outline), None);
        b.line(vec![[1.05, 0.42], [1.05, 0.72]], Role::Outline, Width::Thin);
        b.line(vec![[0.9, 0.57], [1.2, 0.57]], Role::Outline, Width::Thin);
    } else {
        b.output(1.0, 1.05);
        for (x, half) in [(1.05, 0.68), (0.75, 0.44), (0.45, 0.2)] {
            b.line(vec![[x, 1.0 - half], [x, 1.0 + half]], Role::Outline, Width::Outline);
        }
    }
    b.finish()
}

/// Sequence generator: a box with an irregular bit pattern (always drawn,
/// bright in the display colour while the output is high).
fn sequence_generator() -> Symbol {
    let mut b = B::new(4.0, 2.0);
    b.body(rect(0.15, 0.15, 3.65, 1.85));
    let (lo, hi) = (1.4, 0.6);
    let mut wave = vec![[0.4, lo]];
    let mut level = false;
    for (i, bit) in [false, true, true, false, true, false].into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = 0.55 + 0.45 * i as f32;
        if bit != level {
            let y0 = if level { hi } else { lo };
            wave.push([x, y0]);
            wave.push([x, if bit { hi } else { lo }]);
            level = bit;
        }
    }
    wave.push([3.35, if level { hi } else { lo }]);
    b.line(wave.clone(), Role::Outline, Width::Outline);
    b.line(wave, Role::State, Width::Outline).layer = Layer::High;
    b.output(1.0, 3.65);
    b.finish()
}

/// Solder dot: a filled disc in the net colour, centred in a small body
/// (`crate::scene::DOT_HALF` on each side of the node).
fn junction() -> Symbol {
    let h = crate::scene::DOT_HALF;
    let mut b = B::new(2.0 * h, 2.0 * h);
    b.dot([h, h], 0.23, Role::PinOut);
    b.finish()
}

/// Dashed frame of an annotation (not a component body).
fn dashed_frame(b: &mut B, r: &[P]) {
    for k in 0..r.len() {
        let (a, z) = (r[k], r[(k + 1) % r.len()]);
        let len = path::dist(a, z);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let dashes = (len / 0.5).floor().max(1.0) as usize;
        for d in 0..dashes {
            #[allow(clippy::cast_precision_loss)]
            let (t0, t1) = (d as f32 / dashes as f32, (d as f32 + 0.55) / dashes as f32);
            let lerp = |t: f32| [a[0] + (z[0] - a[0]) * t, a[1] + (z[1] - a[1]) * t];
            b.line(vec![lerp(t0), lerp(t1)], Role::Outline, Width::Thin);
        }
    }
}

/// Palette icon of a text annotation (instances use [`note_symbol`]).
fn text_note() -> Symbol {
    let mut b = B::new(4.0, 2.0);
    dashed_frame(&mut b, &rect(0.1, 0.2, 3.9, 1.8));
    for l in font::layout_cased("Abc", 0.75, 0.62, 0.75, false) {
        b.line(l, Role::Ink, Width::Outline);
    }
    b.finish()
}

/// Most vertices of one annotation mesh (the GPU vertex-index packing).
pub const MAX_NOTE_VERTICES: usize = (1 << atlas::LOCAL_BITS) - 1;

/// The symbol of a text annotation: its lines in the stroke font (case
/// kept), left-aligned in the box `bw_core::Note::box_size` computes. Bold
/// draws thicker strokes, italic shears the glyphs, the overbar is a line
/// over each line of text. Strokes scale with the text height. Unlike pin
/// labels, the text turns with the component.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn note_symbol(note: &Note) -> Symbol {
    let (w, h) = note.box_size();
    let mut b = B::new(f32::from(w), f32::from(h));
    let th = f32::from(note.height) / 10.0;
    // Half stroke width: a fraction of the height, on top of the thin class.
    let hw = th * if note.bold { 0.085 } else { 0.042 };
    let bake = (hw - 0.04).max(0.0);
    let x0 = 0.2 * th;
    // The box is capped at 255 units: text beyond it is cut.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cols = ((f32::from(w) - 0.25 * th) / (font::ADVANCE * th)).floor().max(0.0) as usize;
    for (i, line) in note.lines().enumerate() {
        let top = i as f32 * bw_core::note::NOTE_LINE * th + 0.3 * th;
        if top + 1.3 * th > f32::from(h) {
            break;
        }
        let cut: String;
        let line = if line.chars().count() > cols {
            cut = line.chars().take(cols).collect();
            cut.as_str()
        } else {
            line
        };
        for l in font::layout_cased(line, x0, top, th, note.italic) {
            b.line(l, Role::Ink, Width::Thin).bake = bake;
        }
        let n = line.chars().count();
        if note.overbar && n > 0 {
            let y = top - 0.18 * th;
            let shift = if note.italic { (th + 0.18 * th) * font::ITALIC_SLANT } else { 0.0 };
            let tw = font::text_width(n, th);
            b.line(vec![[x0 + shift, y], [x0 + shift + tw, y]], Role::Ink, Width::Thin).bake = bake;
        }
    }
    b.finish()
}

/// Tessellated annotation mesh, cut to [`MAX_NOTE_VERTICES`] (whole
/// triangles).
#[must_use]
pub fn note_mesh(note: &Note) -> Vec<SymVertex> {
    let mut mesh = note_symbol(note).tessellate();
    if mesh.len() > MAX_NOTE_VERTICES {
        mesh.truncate(MAX_NOTE_VERTICES - MAX_NOTE_VERTICES % 3);
    }
    mesh
}

/// A macro tag: a flag pointing out of the body (input tags) or into it
/// (output tags), filled with the level of its net; tri-state output tags
/// carry a small ▽.
fn tag(dir: TagDir, tristate: bool) -> Symbol {
    let mut b = B::new(4.0, 2.0);
    match dir {
        TagDir::In => {
            b.push(
                vec![[0.2, 0.3], [2.6, 0.3], [3.3, 1.0], [2.6, 1.7], [0.2, 1.7]],
                true,
                Some(Role::State),
                Some((Role::Outline, Width::Outline)),
            );
            b.output(1.0, 3.3);
        }
        TagDir::Out => {
            b.input(1.0, 0.7);
            b.push(
                vec![[0.7, 1.0], [1.4, 0.3], [3.8, 0.3], [3.8, 1.7], [1.4, 1.7]],
                true,
                Some(Role::State),
                Some((Role::Outline, Width::Outline)),
            );
            if tristate {
                b.loop_line(vec![[2.2, 0.75], [3.2, 0.75], [2.7, 1.3]], Role::Text, Width::Thin);
            }
        }
    }
    b.finish()
}

/// Longest label that fits `w` units at cap height `h` (characters).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn fit_chars(w: f32, h: f32) -> usize {
    ((w / (0.9 * h)).floor().max(1.0)) as usize
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The symbol of a macro definition's instances: the automatic box (inputs
/// left, outputs right, name in the middle, pin names inside the edges) or
/// a DIL package (notch, pin 1 dot, package pin numbers outside the body
/// and pin names inside, unassigned package pins as short dim stubs).
/// Pins match [`bw_core::ComponentKind::pin_offset`] for `shape`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn macro_symbol(def: &MacroDef, shape: &MacroShape) -> Symbol {
    let kind = ComponentKind::Macro(*shape);
    let (w, h) = kind.size();
    let (w, h) = (w as f32, h as f32);
    let mut b = B::new(w, h);
    let iface = def.interface();
    let ins = usize::from(shape.inputs);
    let name_of = |k: usize| iface.get(k).map_or(String::new(), |p| def.pins[p.index].name.clone());
    let th = 0.55;
    let (x0, x1) = (0.6, w - 0.6);
    if shape.dil > 0 {
        let mut body = vec![[x0, 0.2]];
        path::join(&mut body, &arc([w / 2.0, 0.2], 0.45, PI, 0.0, 10));
        path::join(&mut body, &[[x1, 0.2], [x1, h - 0.2], [x0, h - 0.2]]);
        b.body(body);
        b.dot([x0 + 0.45, 0.75], 0.12, Role::Outline);
        let half = shape.dil / 2;
        let at = |n: u8| -> (f32, bool) {
            if n <= half { (2.0 * f32::from(n) - 1.0, true) } else { (2.0 * f32::from(shape.dil - n) + 1.0, false) }
        };
        let used: Vec<u8> = shape.dil_pins[..ins + usize::from(shape.outputs)].to_vec();
        for n in 1..=shape.dil {
            if !used.contains(&n) {
                let (y, left) = at(n);
                let (a, z) = if left { ([0.25, y], [x0, y]) } else { ([w - 0.25, y], [x1, y]) };
                b.line(vec![a, z], Role::Outline, Width::Thin);
            }
            let (y, left) = at(n);
            let nx = if left { x0 + 0.55 } else { x1 - 0.55 };
            b.text(&n.to_string(), nx, y - 0.55, 0.38);
        }
        let room = fit_chars(w / 2.0 - 1.4, th);
        for (k, &n) in used.iter().enumerate() {
            let (y, left) = at(n);
            let out = k >= ins;
            match (left, out) {
                (true, false) => b.input(y, x0),
                (false, false) => b.input_right(y, x1),
                (true, true) => b.output_left(y, x0, u8::try_from(k - ins).unwrap_or(0)),
                (false, true) => b.output_at(y, x1, u8::try_from(k - ins).unwrap_or(0)),
            }
            let label = clip(&name_of(k), room);
            let lw = font::text_width(label.chars().count(), th);
            let cx = if left { x0 + 0.35 + lw / 2.0 } else { x1 - 0.35 - lw / 2.0 };
            b.text(&label, cx, y + 0.3, th);
        }
        // The name runs down the middle, between the two label columns.
        let nh = 0.55f32;
        let fit = ((h - 1.0) / (nh * 1.35)).floor().max(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let name = clip(&def.name, fit as usize);
        b.text_vertical(&name, w / 2.0, h / 2.0, nh);
    } else {
        b.body(rect(x0, 0.2, x1, h - 0.2));
        let room = fit_chars(w / 2.0 - 1.0, th);
        let mut lw_in = 0.0f32;
        let mut lw_out = 0.0f32;
        for k in 0..ins {
            let y = 2.0 * k as f32 + 1.0;
            b.input(y, x0);
            let label = clip(&name_of(k), room);
            let lw = font::text_width(label.chars().count(), th);
            lw_in = lw_in.max(lw);
            b.text(&label, x0 + 0.35 + lw / 2.0, y, th);
        }
        for k in 0..usize::from(shape.outputs) {
            let y = 2.0 * k as f32 + 1.0;
            b.output(y, x1);
            let label = clip(&name_of(ins + k), room);
            let lw = font::text_width(label.chars().count(), th);
            lw_out = lw_out.max(lw);
            b.text(&label, x1 - 0.35 - lw / 2.0, y, th);
        }
        // Name in the space between the two label columns.
        let (l, r) = (x0 + 0.7 + lw_in, x1 - 0.7 - lw_out);
        let n = def.name.chars().count().max(1) as f32;
        let nh = 0.65f32.min((r - l).max(0.5) / (0.9 * n)).max(0.3);
        let label = clip(&def.name, fit_chars((r - l).max(0.5), nh));
        b.text(&label, f32::midpoint(l, r), h / 2.0, nh);
    }
    b.finish()
}

impl Symbol {
    /// The symbol of `kind` in `style`.
    #[must_use]
    pub fn build(kind: SymbolKind, style: SymbolStyle) -> Self {
        match kind {
            SymbolKind::Gate(op, n) => gate(op, n, style),
            SymbolKind::Not => not_gate(style),
            SymbolKind::Switch(mode) => switch(mode),
            SymbolKind::Clock => clock(),
            SymbolKind::Led => led(),
            SymbolKind::FlipFlop(ty, trigger, pc) => flip_flop(ty, trigger, pc),
            SymbolKind::TriState => tri_state(style),
            SymbolKind::Memory(ty, a, d) => memory(ty, a, d),
            SymbolKind::SevenSegment => seven_segment(),
            SymbolKind::Numeric(bits, format, signed) => numeric_output(bits, format, signed),
            SymbolKind::Vcc => constant(true),
            SymbolKind::Ground => constant(false),
            SymbolKind::SequenceGenerator => sequence_generator(),
            SymbolKind::Macro => Self::macro_ic("IC", 4, 4),
            SymbolKind::Tag(dir, tri) => tag(dir, tri),
            SymbolKind::Junction => junction(),
            SymbolKind::Text => text_note(),
        }
    }

    /// A macro / IC drawn like a DIL package: `inputs` pins on the left,
    /// `outputs` on the right, a notch on top and `name` in the middle.
    #[must_use]
    pub fn macro_ic(name: &str, inputs: u8, outputs: u8) -> Self {
        let rows = inputs.max(outputs).max(2);
        let h = 2.0 * f32::from(rows);
        let mut b = B::new(6.0, h);
        let (x0, x1) = (0.8, 5.2);
        for i in 0..inputs {
            b.input(2.0 * f32::from(i) + 1.0, x0);
        }
        let mut body = vec![[x0, 0.2]];
        path::join(&mut body, &arc([3.0, 0.2], 0.45, PI, 0.0, 10));
        path::join(&mut body, &[[x1, 0.2], [x1, h - 0.2], [x0, h - 0.2]]);
        b.body(body);
        b.dot([x0 + 0.45, 0.75], 0.12, Role::Outline);
        let len = name.chars().count().max(1);
        #[allow(clippy::cast_precision_loss)]
        let th = (0.9f32).min(3.6 / (len as f32 * 0.9));
        b.text(name, 3.0, h / 2.0, th);
        for i in 0..outputs {
            b.output(2.0 * f32::from(i) + 1.0, x1);
        }
        b.finish()
    }

    /// Triangles of every shape, in painter's order (fill, then stroke).
    #[must_use]
    pub fn tessellate(&self) -> Vec<SymVertex> {
        let mut out = Vec::new();
        for s in &self.shapes {
            let from = out.len();
            let pin = (u32::from(s.pin) << 8) | (u32::from(s.extra) << 16);
            if let Some(role) = s.fill {
                if s.closed {
                    tess::fill(&s.points, tess::attr(role, Width::None, s.layer) | pin, &mut out);
                }
            }
            if let Some((role, width)) = s.stroke {
                let at = out.len();
                tess::stroke(&s.points, s.closed, tess::attr(role, width, s.layer) | pin, &mut out);
                if s.bake > 0.0 {
                    for v in &mut out[at..] {
                        v.pos = v.at(s.bake);
                    }
                }
            }
            if let Some(a) = s.anchor {
                let packed = tess::pack_anchor(a);
                for v in &mut out[from..] {
                    v.anchor = packed;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use bw_core::{Component, ComponentId, GridPos, Rotation};

    use super::*;

    fn components() -> Vec<ComponentKind> {
        let mut v: Vec<ComponentKind> = GateOp::ALL
            .iter()
            .flat_map(|&op| {
                (MIN_GATE_INPUTS..=bw_core::MAX_GATE_INPUTS).map(move |inputs| ComponentKind::Gate { op, inputs })
            })
            .collect();
        v.extend([
            ComponentKind::Not,
            ComponentKind::Clock { half_period: 3 },
            ComponentKind::Led,
            ComponentKind::TriState,
        ]);
        for mode in SwitchMode::ALL {
            for on in [false, true] {
                v.push(ComponentKind::Switch { on, mode });
            }
        }
        v.extend([
            ComponentKind::Const { high: true },
            ComponentKind::Const { high: false },
            ComponentKind::SeqGen { pattern: 0b1011, len: 4, half_period: 3 },
            ComponentKind::SeqGen { pattern: u64::MAX, len: 64, half_period: 1 },
            ComponentKind::SevenSeg { common_anode: false },
            ComponentKind::SevenSeg { common_anode: true },
        ]);
        for bits in MIN_NUMERIC_BITS..=MAX_NUMERIC_BITS {
            for format in NumFormat::ALL {
                for signed in [false, true] {
                    v.push(ComponentKind::Numeric { bits, format, signed });
                }
            }
        }
        // Text annotations of several sizes (their symbol is the note's).
        for n in notes() {
            v.push(n.kind());
        }
        for ty in FlipFlopType::ALL {
            for trigger in FfTrigger::ALL {
                for preset_clear in [false, true] {
                    let k = ComponentKind::FlipFlop { ty, trigger, preset_clear };
                    if k.is_valid() {
                        v.push(k);
                    }
                }
            }
        }
        for ty in [MemoryType::Ram, MemoryType::Rom] {
            for addr_bits in MIN_ADDR_BITS..=MAX_ADDR_BITS {
                for data_bits in DATA_WIDTHS {
                    v.push(ComponentKind::Memory { ty, addr_bits, data_bits });
                }
            }
        }
        v
    }

    fn notes() -> Vec<Note> {
        let mut v = vec![
            Note::default(),
            Note::new("a"),
            Note::new("Hello, world!\nline two: [x] = y + 1;\n\"quoted\" 50% @ $3 #4 *5 <6> ?!"),
            Note { text: "gjpqy Bold".into(), bold: true, ..Note::default() },
            Note { text: "Italic\nOver".into(), italic: true, overbar: true, height: 40, ..Note::default() },
            Note { text: "tiny".into(), height: bw_core::note::MIN_NOTE_HEIGHT, ..Note::default() },
            Note { text: "HUGE".into(), height: bw_core::note::MAX_NOTE_HEIGHT, overbar: true, ..Note::default() },
            Note::new(""),
            Note::new("çãõ áé ñ"),
        ];
        v.push(Note::new(&"x".repeat(bw_core::note::MAX_NOTE_BYTES)));
        v
    }

    /// The symbol a component instance is drawn with.
    fn symbol_of(kind: &ComponentKind, note: Option<&Note>, style: SymbolStyle) -> Symbol {
        match (kind, note) {
            (ComponentKind::Text { .. }, Some(n)) => note_symbol(n),
            _ => Symbol::build(SymbolKind::of(kind), style),
        }
    }

    /// Every pin anchor of every component's symbol (every variant: switch
    /// modes, VCC / ground, both 7-segment polarities, every numeric layout,
    /// text sizes), in both styles, sits exactly on the pin position
    /// `bw-core` uses for wires, a pin stub stroke starts there, and the
    /// symbol has the core's body size.
    #[test]
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    fn pin_anchors_match_core_pins() {
        let notes = notes();
        for kind in components() {
            let comp =
                Component { id: ComponentId(0), kind, pos: GridPos::new(0, 0), rotation: Rotation::R0, color: None };
            let note = notes.iter().find(|n| n.kind() == kind);
            for style in SymbolStyle::ALL {
                let sym = symbol_of(&kind, note, style);
                let (w, h) = kind.size();
                assert_eq!(sym.size, [w as f32, h as f32], "{kind:?} size");
                assert_eq!(sym.pins.len(), usize::from(kind.pin_count()), "{kind:?} pin count");
                for (i, pin) in sym.pins.iter().enumerate() {
                    let i = u8::try_from(i).unwrap();
                    let (lx, ly) = comp.pin_local_offset(i).unwrap();
                    assert_eq!(pin.at, [lx as f32, ly as f32], "{kind:?} {style:?} pin {i}");
                    assert_eq!(Some(pin.dir), comp.pin_dir(i));
                    let touches = sym.shapes.iter().any(|s| {
                        s.stroke.is_some_and(|(r, _)| matches!(r, Role::PinIn | Role::PinOut))
                            && (s.points.first() == Some(&pin.at) || s.points.last() == Some(&pin.at))
                    });
                    assert!(touches, "{kind:?} {style:?}: no pin stub at pin {i}");
                }
                // Each output stub is coloured by its own output's net.
                let outs: Vec<u8> = sym
                    .shapes
                    .iter()
                    .filter(|s| s.stroke.is_some_and(|(r, _)| r == Role::PinOut) && kind.output_count() > 0)
                    .filter(|s| s.layer == Layer::Always)
                    .map(|s| s.pin)
                    .collect();
                let expect: Vec<u8> = (0..kind.output_count()).collect();
                assert_eq!(outs, expect, "{kind:?} output stub indices");
            }
        }
    }

    /// Macro symbols (automatic box and DIL, every built-in part) put each
    /// pin stub exactly on the pin `bw-core` computes for the shape, and the
    /// GPU constant of the macro table matches the atlas layout.
    #[test]
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    fn macro_symbols_match_core_pins() {
        for e in bw_io::library::builtin() {
            let entry = e.file.table.get(e.file.root).unwrap();
            let shape = entry.shape;
            let kind = ComponentKind::Macro(shape);
            let comp =
                Component { id: ComponentId(0), kind, pos: GridPos::new(0, 0), rotation: Rotation::R0, color: None };
            let sym = macro_symbol(&entry.def, &shape);
            let (w, h) = kind.size();
            assert_eq!(sym.size, [w as f32, h as f32], "{}", e.key);
            assert_eq!(sym.pins.len(), usize::from(kind.pin_count()), "{}", e.key);
            for (i, p) in sym.pins.iter().enumerate() {
                let i = u8::try_from(i).unwrap();
                let (lx, ly) = comp.pin_local_offset(i).unwrap();
                assert_eq!(p.at, [lx as f32, ly as f32], "{} pin {i}", e.key);
                assert_eq!(Some(p.dir), comp.pin_dir(i));
            }
            let mesh = sym.tessellate();
            assert!(!mesh.is_empty() && mesh.len() < 1 << atlas::LOCAL_BITS, "{}: {} vertices", e.key, mesh.len());
            for v in &mesh {
                assert!(v.pos[0] >= -1e-3 && v.pos[0] <= w as f32 + 1e-3 && v.pos[1] >= -0.2, "{} {:?}", e.key, v.pos);
            }
        }
        let src = crate::shader_source();
        assert!(src.contains(&format!("const SYM_INFO_MACRO: u32 = {}u;", atlas::INFO_MACRO)), "shader constant");
        assert!(src.contains(&format!("const SYM_MACRO_CODE: u32 = {MACRO_CODE}u;")));
    }

    /// Annotation meshes: inside their box (descenders, italic slant and
    /// the overbar included), bold strokes wider than regular ones, and
    /// never beyond the vertex-index packing.
    #[test]
    #[allow(clippy::float_cmp, reason = "integer sizes converted to f32 on both sides")]
    fn note_symbols_fit_their_box() {
        for note in notes() {
            let sym = note_symbol(&note);
            let (w, h) = note.box_size();
            assert_eq!(sym.size, [f32::from(w), f32::from(h)]);
            let mesh = note_mesh(&note);
            assert_eq!(mesh.len() % 3, 0);
            assert!(mesh.len() <= MAX_NOTE_VERTICES);
            assert_eq!(mesh.is_empty(), note.text.trim().is_empty(), "{:?}", note.text);
            let th = f32::from(note.height) / 10.0;
            for v in &mesh {
                let p = v.at(Width::Thin.half_width(1e3));
                assert!(v.role() == Role::Ink && v.anchor == tess::NO_ANCHOR);
                let tol = 0.1 * th + 0.05;
                assert!(
                    p[0] >= -tol && p[1] >= -tol && p[0] <= sym.size[0] + tol && p[1] <= sym.size[1] + tol,
                    "{:?}: {p:?} outside {:?}",
                    note.text,
                    sym.size
                );
            }
        }
        let width = |bold: bool| {
            let m = note_mesh(&Note { text: "|".into(), bold, height: 40, ..Note::default() });
            m.iter().map(|v| v.at(0.0)[0]).fold(f32::MIN, f32::max)
                - m.iter().map(|v| v.at(0.0)[0]).fold(f32::MAX, f32::min)
        };
        assert!(width(true) > width(false) * 1.3, "bold is thicker");
        let plain = note_mesh(&Note::new("Ab"));
        let italic = note_mesh(&Note { italic: true, ..Note::new("Ab") });
        assert_eq!(plain.len(), italic.len());
        assert_ne!(plain, italic, "italic shears");
        assert!(note_mesh(&Note { overbar: true, ..Note::new("Ab") }).len() > plain.len(), "overbar");
    }

    /// Display meshes carry what the shader needs: 7-segment displays have
    /// segments a … g (display signals 0..7), numeric outputs seven segments
    /// per digit with their digit index; the GPU constants match.
    #[test]
    fn display_meshes_and_shader_constants() {
        let seven = Symbol::build(SymbolKind::SevenSegment, SymbolStyle::Ansi);
        let segs: Vec<u8> = seven.shapes.iter().filter(|s| s.fill == Some(Role::Segment)).map(|s| s.pin).collect();
        assert_eq!(segs, (0..SEGMENTS).collect::<Vec<_>>());
        for bits in MIN_NUMERIC_BITS..=MAX_NUMERIC_BITS {
            for format in NumFormat::ALL {
                for signed in [false, true] {
                    let sym = Symbol::build(SymbolKind::Numeric(bits, format, signed), SymbolStyle::Ansi);
                    let digits: Vec<(u8, u8)> =
                        sym.shapes.iter().filter(|s| s.fill == Some(Role::Digit)).map(|s| (s.extra, s.pin)).collect();
                    let n = numeric_digits(bits, format, signed);
                    assert_eq!(digits.len(), usize::from(n) * 7, "{bits} {format:?} {signed}");
                    assert!(digits.iter().all(|&(d, p)| d < n && p < 7));
                    let v = numeric_variant(bits, format, signed);
                    assert_eq!((v & 15) + 1, u32::from(bits));
                    assert!(v < VARIANT_SLOTS);
                }
            }
        }
        let src = crate::shader_source();
        for (name, value) in [
            ("SYM_BUCKETS", KIND_CODES),
            ("SYM_INFO_VCOUNT", KIND_CODES),
            ("SYM_INFO_TABLE", u32::try_from(atlas::INFO_TABLE).unwrap()),
            ("SYM_INFO_NOTE", u32::try_from(atlas::INFO_NOTE).unwrap()),
            ("SYM_TEXT_CODE", TEXT_CODE),
            ("CONST_CODE", CONST_CODE),
            ("SEQ_CODE", SEQ_CODE),
            ("SEVEN_SEG_CODE", SEVEN_SEG_CODE),
            ("NUMERIC_CODE", NUMERIC_CODE),
            ("TEXT_CODE", TEXT_CODE),
            ("MACRO_CODE", MACRO_CODE),
            ("JUNCTION_CODE", JUNCTION_CODE),
        ] {
            assert!(src.contains(&format!("const {name}: u32 = {value}u;")), "shader constant {name}");
        }
        let cull = crate::cull_shader_source();
        assert!(cull.contains(&format!("const NB: u32 = {KIND_CODES}u;")));
        assert!(cull.contains(&format!("const NCOL: u32 = {}u;", KIND_CODES + 1)));
        assert!(cull.contains(&format!("const INFO_VCOUNT: u32 = {KIND_CODES}u;")));
        assert!(cull.contains(&format!("const JUNCTION_BUCKET: u32 = {JUNCTION_CODE}u;")));
        assert!(cull.contains(&format!("array<DrawArgs, {}>", KIND_CODES + 2)));
        // The palette mirror: every entry, in index order.
        let body = &src[src.find("fn palette_color").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let nums: Vec<f32> = body
            .split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
            .filter(|t| t.contains('.') && t.chars().any(|ch| ch.is_ascii_digit()))
            .map(|t| t.parse().unwrap())
            .collect();
        let want: Vec<f32> = Color::ALL.iter().flat_map(|&c| palette_rgb(c)).collect();
        assert_eq!(nums, want, "palette_color mirrors palette_rgb");
    }

    #[test]
    fn every_symbol_has_geometry_inside_its_body() {
        for kind in SymbolKind::all() {
            for style in SymbolStyle::ALL {
                let sym = Symbol::build(kind, style);
                let mesh = sym.tessellate();
                assert!(!mesh.is_empty(), "{kind:?} {style:?}");
                assert_eq!(mesh.len() % 3, 0);
                for v in &mesh {
                    assert!(v.pos[0].is_finite() && v.pos[1].is_finite() && v.ext[0].is_finite());
                    // Centre lines stay inside the body (strokes may overhang
                    // by their half-width only).
                    let tol = 0.5;
                    {
                        assert!(
                            v.pos[0] >= -1e-4
                                && v.pos[1] >= -1e-4
                                && v.pos[0] <= sym.size[0] + 1e-4
                                && v.pos[1] <= sym.size[1] + tol,
                            "{kind:?} {style:?} vertex {:?} outside {:?}",
                            v.pos,
                            sym.size
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn styles_differ_only_where_expected() {
        for kind in SymbolKind::all() {
            let (a, i) = (Symbol::build(kind, SymbolStyle::Ansi), Symbol::build(kind, SymbolStyle::Iec));
            assert_eq!(a != i, kind.style_dependent(), "{kind:?}");
        }
    }

    #[test]
    fn gate_input_side_grows_with_input_count() {
        let ys = |n: u8| {
            let s = Symbol::build(SymbolKind::Gate(GateOp::And, n), SymbolStyle::Ansi);
            let outline: Vec<f32> = s
                .shapes
                .iter()
                .filter(|s| s.stroke.is_some_and(|(r, _)| r == Role::Outline))
                .flat_map(|s| s.points.iter().map(|p| p[1]))
                .collect();
            outline.iter().copied().fold(f32::MIN, f32::max) - outline.iter().copied().fold(f32::MAX, f32::min)
        };
        assert!(ys(8) > ys(4) && ys(4) > ys(2));
    }
}
