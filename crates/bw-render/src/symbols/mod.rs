//! Schematic symbols: original vector drawings of every component, in two
//! styles (ANSI/IEEE distinctive shapes and IEC rectangular), compiled into
//! the binary.
//!
//! * [`Symbol::build`] describes a symbol as outline paths plus fills in its
//!   own unrotated local frame (grid units, origin at the body's top-left,
//!   y down), together with its pin anchors. Pins follow `bw-core`'s pin
//!   geometry exactly (inputs at `(0, 2i + 1)`, the output at `(w, h / 2)`),
//!   so wires end precisely on the symbol's pin stubs.
//! * [`tess`] turns the paths into triangles once (fills by ear clipping,
//!   strokes as mitred strips with a unit *extrude* vector per vertex).
//! * [`atlas()`] holds every kind × input count × style in one shared,
//!   static vertex array with a sub-range per symbol. The GPU renderer
//!   uploads it once; the app's palette paints the very same vertices with
//!   egui; the app icon rasterizes them on the CPU ([`raster`]).
//!
//! State never changes a mesh: each vertex has a colour *role* and a *layer*
//! (always / only when high / only when not high) that the shader resolves
//! against the per-slot net level bits (LED lit, switch lever, clock trace,
//! output pin colour).

#![allow(clippy::many_single_char_names, reason = "geometry code: points, radii, curve parameters")]

pub mod atlas;
pub mod font;
pub mod path;
pub mod raster;
pub mod tess;

use std::f32::consts::PI;

use bw_core::{ComponentKind, GateOp, MAX_GATE_INPUTS, MIN_GATE_INPUTS, PinDir};

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

    /// Visible at `level` (0 low, 1 high, 2 unknown)?
    #[must_use]
    pub const fn shows(self, level: u8) -> bool {
        match self {
            Self::Always => true,
            Self::High => level == 1,
            Self::NotHigh => level != 1,
        }
    }
}

/// Every symbol the app knows: the components of `bw-core` plus placeholders
/// for components that are not simulated yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Gate(GateOp, u8),
    Not,
    Switch,
    Clock,
    Led,
    // ---- not simulated yet (symbol only) ----
    DFlipFlop,
    RsFlipFlop,
    JkFlipFlop,
    TriState,
    Ram,
    Rom,
    SevenSegment,
    NumericOutput,
    Vcc,
    Ground,
    SequenceGenerator,
    InteractiveInput,
    /// Generic macro / IC (DIL-style box); [`Symbol::macro_ic`] builds named
    /// variants with any pin count.
    Macro,
    Junction,
    Text,
}

/// Number of GPU kind codes (see [`crate::kind_code`]).
pub const KIND_CODES: u32 = 10;
/// Input-count slots per kind in the GPU mesh table (`0..=MAX_GATE_INPUTS`).
pub const INPUT_SLOTS: u32 = MAX_GATE_INPUTS as u32 + 1;

impl SymbolKind {
    /// Symbols of components that are not simulated yet.
    pub const FUTURE: [Self; 15] = [
        Self::DFlipFlop,
        Self::RsFlipFlop,
        Self::JkFlipFlop,
        Self::TriState,
        Self::Ram,
        Self::Rom,
        Self::SevenSegment,
        Self::NumericOutput,
        Self::Vcc,
        Self::Ground,
        Self::SequenceGenerator,
        Self::InteractiveInput,
        Self::Macro,
        Self::Junction,
        Self::Text,
    ];

    #[must_use]
    pub fn of(kind: &ComponentKind) -> Self {
        match *kind {
            ComponentKind::Gate { op, inputs } => Self::Gate(op, inputs.clamp(MIN_GATE_INPUTS, MAX_GATE_INPUTS)),
            ComponentKind::Not => Self::Not,
            ComponentKind::Switch { .. } => Self::Switch,
            ComponentKind::Clock { .. } => Self::Clock,
            ComponentKind::Led => Self::Led,
        }
    }

    /// Every symbol variant: gates for each input count, the other current
    /// components, then the future ones.
    #[must_use]
    pub fn all() -> Vec<Self> {
        let mut v = Vec::new();
        for op in GateOp::ALL {
            for n in MIN_GATE_INPUTS..=MAX_GATE_INPUTS {
                v.push(Self::Gate(op, n));
            }
        }
        v.extend([Self::Not, Self::Switch, Self::Clock, Self::Led]);
        v.extend(Self::FUTURE);
        v
    }

    #[must_use]
    pub fn is_future(self) -> bool {
        self.code().is_none()
    }

    /// GPU kind code (current components only); matches [`crate::kind_code`].
    #[must_use]
    pub const fn code(self) -> Option<u32> {
        Some(match self {
            Self::Gate(op, _) => match op {
                GateOp::And => 0,
                GateOp::Or => 1,
                GateOp::Nand => 2,
                GateOp::Nor => 3,
                GateOp::Xor => 4,
                GateOp::Xnor => 5,
            },
            Self::Not => 6,
            Self::Switch => 7,
            Self::Clock => 8,
            Self::Led => 9,
            _ => return None,
        })
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
            Self::Switch => "Switch",
            Self::Clock => "Clock",
            Self::Led => "LED",
            Self::DFlipFlop => "D flip-flop",
            Self::RsFlipFlop => "RS flip-flop",
            Self::JkFlipFlop => "JK flip-flop",
            Self::TriState => "Tri-state buffer",
            Self::Ram => "RAM",
            Self::Rom => "ROM",
            Self::SevenSegment => "7-segment display",
            Self::NumericOutput => "Numeric output",
            Self::Vcc => "Power (VCC)",
            Self::Ground => "Ground",
            Self::SequenceGenerator => "Sequence generator",
            Self::InteractiveInput => "Interactive input",
            Self::Macro => "Macro / IC",
            Self::Junction => "Wire junction",
            Self::Text => "Text",
        }
    }

    /// Accent colour (linear-ish RGB, 0..1). For current components it
    /// mirrors `kind_accent` in the shaders.
    #[must_use]
    pub const fn accent(self) -> [f32; 3] {
        match self {
            Self::Gate(GateOp::And | GateOp::Nand, _) => [0.35, 0.62, 0.95],
            Self::Gate(GateOp::Or | GateOp::Nor, _) => [0.95, 0.62, 0.30],
            Self::Gate(GateOp::Xor | GateOp::Xnor, _) => [0.70, 0.45, 0.95],
            Self::Not | Self::TriState => [0.85, 0.85, 0.40],
            Self::Switch | Self::InteractiveInput => [0.40, 0.85, 0.55],
            Self::Clock | Self::SequenceGenerator => [0.40, 0.85, 0.85],
            Self::Led | Self::SevenSegment | Self::NumericOutput => [0.95, 0.35, 0.35],
            Self::DFlipFlop | Self::RsFlipFlop | Self::JkFlipFlop => [0.95, 0.55, 0.78],
            Self::Ram | Self::Rom => [0.55, 0.78, 0.95],
            Self::Vcc => [0.95, 0.45, 0.35],
            Self::Ground => [0.60, 0.66, 0.75],
            Self::Macro => [0.78, 0.78, 0.84],
            Self::Junction => [0.45, 0.50, 0.58],
            Self::Text => [0.80, 0.82, 0.86],
        }
    }
}

/// Wire / pin colour of a net level (0 low, 1 high, else unknown). Mirrors
/// `level_color` in `common.wgsl`.
#[must_use]
pub const fn level_color(level: u8) -> [f32; 3] {
    match level {
        1 => [0.30, 0.95, 0.40],
        0 => [0.12, 0.42, 0.18],
        _ => [0.45, 0.50, 0.58],
    }
}

/// Colour of a role for a symbol with `accent` at `level`. Mirrors
/// `sym_color` in `symbol.wgsl`.
#[must_use]
pub fn shade(role: Role, accent: [f32; 3], level: u8) -> [f32; 3] {
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t);
    let outline = mix(accent, [0.92; 3], 0.45);
    match role {
        Role::Outline | Role::Text | Role::PinIn => outline,
        Role::Fill => {
            if level == 1 {
                [0.19, 0.25, 0.21]
            } else {
                [0.16, 0.17, 0.20]
            }
        }
        Role::State => match level {
            1 => accent,
            0 => accent.map(|c| c * 0.28),
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
        self.shapes.push(Shape { points, closed, fill, stroke, layer: Layer::Always, anchor: None });
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

    /// Output pin at the right edge, stub from `x_start`.
    fn output(&mut self, y: f32, x_start: f32) {
        let w = self.size[0];
        self.line(vec![[x_start, y], [w, y]], Role::PinOut, Width::Pin);
        self.outputs.push(SymPin { at: [w, y], dir: PinDir::Output });
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
    let n = n.clamp(MIN_GATE_INPUTS, MAX_GATE_INPUTS);
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

/// SPST switch: the lever is drawn closed (net colour) when on and open
/// otherwise; both variants live in the mesh, the net bit picks one.
fn switch() -> Symbol {
    let mut b = B::new(2.0, 2.0);
    b.body(rect(0.15, 0.15, 1.65, 1.85));
    b.output(1.0, 1.35);
    b.line(vec![[0.45, 1.0], [1.35, 1.0]], Role::PinOut, Width::Outline).layer = Layer::High;
    b.line(vec![[0.45, 1.0], [1.22, 0.42]], Role::Outline, Width::Outline).layer = Layer::NotHigh;
    b.dot([0.45, 1.0], 0.13, Role::Outline);
    b.dot([1.35, 1.0], 0.13, Role::Outline);
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

/// Edge-triggered flip-flop box with the dynamic (clock) triangle.
fn flip_flop(inputs: &[&str], clock_at: usize) -> Symbol {
    #[allow(clippy::cast_precision_loss)]
    let h = 2.0 * inputs.len() as f32;
    let mut b = B::new(6.0, h.max(4.0));
    let h = b.size[1];
    let (x0, x1) = (0.6, 5.4);
    for i in 0..inputs.len() {
        #[allow(clippy::cast_precision_loss)]
        b.input(2.0 * i as f32 + 1.0, x0);
    }
    b.body(rect(x0, 0.2, x1, h - 0.2));
    for (i, l) in inputs.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let y = 2.0 * i as f32 + 1.0;
        if i == clock_at {
            b.line(vec![[x0, y - 0.4], [x0 + 0.5, y], [x0 + 0.0, y + 0.4]], Role::Outline, Width::Outline);
            b.text(l, x0 + 1.1, y, 0.6);
        } else {
            b.text(l, x0 + 0.65, y, 0.7);
        }
    }
    b.text("Q", x1 - 0.6, 1.0, 0.7);
    b.text_bar("Q", x1 - 0.6, h - 1.0, 0.7);
    b.output(1.0, x1);
    b.output(h - 1.0, x1);
    b.finish()
}

fn tri_state(style: SymbolStyle) -> Symbol {
    let mut b = B::new(4.0, 3.0);
    let y = 2.0;
    match style {
        SymbolStyle::Ansi => {
            b.input(y, 0.6);
            let (x0, x1, half) = (0.6, 2.9, 0.85);
            // Enable stub ends on the triangle's upper edge.
            let ex = 1.8;
            let ey = (y - half) + (ex - x0) / (x1 - x0) * half;
            b.input_top(ex, ey);
            b.body(vec![[x0, y - half], [x1, y], [x0, y + half]]);
            b.output(y, x1);
        }
        SymbolStyle::Iec => {
            b.input(y, 0.6);
            b.input_top(1.8, 1.1);
            b.body(rect(0.6, 1.1, 2.9, 2.9));
            b.text("EN", 1.8, 1.45, 0.3);
            b.text("1", 1.25, 2.2, 0.65);
            // Three-state output qualifier (inverted triangle).
            b.loop_line(vec![[2.2, 2.0], [2.65, 2.0], [2.425, 2.4]], Role::Text, Width::Thin);
            b.output(y, 2.9);
        }
    }
    b.finish()
}

fn memory(title: &str, inputs: &[&str], outputs: &[&str]) -> Symbol {
    #[allow(clippy::cast_precision_loss)]
    let h = 2.0 * inputs.len().max(outputs.len()).max(3) as f32;
    let mut b = B::new(8.0, h);
    let (x0, x1) = (0.6, 7.4);
    #[allow(clippy::cast_precision_loss)]
    let pin_y = |i: usize| 2.0 * i as f32 + 1.0;
    for i in 0..inputs.len() {
        b.input(pin_y(i), x0);
    }
    b.body(rect(x0, 0.2, x1, h - 0.2));
    // Title band.
    b.line(vec![[2.6, 0.2], [2.6, h - 0.2]], Role::Outline, Width::Thin);
    b.line(vec![[5.4, 0.2], [5.4, h - 0.2]], Role::Outline, Width::Thin);
    b.text(title, 4.0, h / 2.0, 0.9);
    for (i, l) in inputs.iter().enumerate() {
        b.text(l, x0 + 0.9, pin_y(i), 0.55);
    }
    for (i, l) in outputs.iter().enumerate() {
        b.text(l, x1 - 0.9, pin_y(i), 0.55);
        b.output(pin_y(i), x1);
    }
    b.finish()
}

fn seven_segment() -> Symbol {
    let mut b = B::new(6.0, 16.0);
    for i in 0..8u8 {
        b.input(2.0 * f32::from(i) + 1.0, 0.6);
    }
    b.body(rect(0.6, 0.2, 5.4, 15.8));
    // Slanted digit, segments a–g plus the decimal point.
    let sk = |x: f32, y: f32| [x + (12.0 - y) * 0.08, y];
    let (l, r, t, m, bo) = (1.9, 4.1, 4.4, 8.0, 11.6);
    let segs: [(P, P); 7] = [
        (sk(l + 0.25, t), sk(r - 0.25, t)),
        (sk(r, t + 0.25), sk(r, m - 0.25)),
        (sk(r, m + 0.25), sk(r, bo - 0.25)),
        (sk(l + 0.25, bo), sk(r - 0.25, bo)),
        (sk(l, m + 0.25), sk(l, bo - 0.25)),
        (sk(l, t + 0.25), sk(l, m - 0.25)),
        (sk(l + 0.25, m), sk(r - 0.25, m)),
    ];
    for (a, z) in segs {
        b.line(vec![a, z], Role::State, Width::Pin);
    }
    b.dot(sk(r + 0.6, bo), 0.2, Role::State);
    for (i, l) in ["A", "B", "C", "D", "E", "F", "G", "P"].iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        b.text(l, 1.05, 2.0 * i as f32 + 1.0, 0.4);
    }
    b.finish()
}

fn numeric_output() -> Symbol {
    let mut b = B::new(6.0, 8.0);
    for i in 0..4u8 {
        b.input(2.0 * f32::from(i) + 1.0, 0.6);
    }
    b.body(rect(0.6, 0.2, 5.4, 7.8));
    b.push(rect(1.7, 1.2, 4.6, 6.8), true, Some(Role::State), Some((Role::Outline, Width::Thin)));
    b.text("0", 3.15, 4.0, 3.2);
    b.finish()
}

fn vcc() -> Symbol {
    let mut b = B::new(2.0, 2.0);
    b.output(1.0, 1.0);
    b.line(vec![[1.0, 1.0], [1.0, 0.45]], Role::Outline, Width::Outline);
    b.line(vec![[0.35, 0.45], [1.65, 0.45]], Role::Outline, Width::Outline);
    b.finish()
}

fn ground() -> Symbol {
    let mut b = B::new(2.0, 2.0);
    b.output(1.0, 1.0);
    b.line(vec![[1.0, 1.0], [1.0, 1.2]], Role::Outline, Width::Outline);
    for (y, half) in [(1.2, 0.65), (1.48, 0.42), (1.76, 0.18)] {
        b.line(vec![[1.0 - half, y], [1.0 + half, y]], Role::Outline, Width::Outline);
    }
    b.finish()
}

fn sequence_generator() -> Symbol {
    let mut b = B::new(6.0, 4.0);
    b.body(rect(0.2, 0.2, 5.0, 3.8));
    b.text("SEQ", 2.6, 1.0, 0.7);
    let (lo, hi) = (3.1, 2.1);
    b.line(
        vec![
            [0.7, lo],
            [1.2, lo],
            [1.2, hi],
            [2.2, hi],
            [2.2, lo],
            [2.7, lo],
            [2.7, hi],
            [3.2, hi],
            [3.2, lo],
            [4.4, lo],
            [4.4, hi],
            [4.6, hi],
        ],
        Role::State,
        Width::Outline,
    );
    b.output(2.0, 5.0);
    b.finish()
}

fn interactive_input() -> Symbol {
    let mut b = B::new(4.0, 2.0);
    b.push(
        path::round_rect(0.2, 0.2, 2.9, 1.8, 0.35, 4),
        true,
        Some(Role::Fill),
        Some((Role::Outline, Width::Outline)),
    );
    b.text("IN", 1.25, 1.0, 0.65);
    b.loop_line(vec![[2.2, 0.75], [2.6, 1.0], [2.2, 1.25]], Role::State, Width::Thin);
    b.output(1.0, 2.9);
    b.finish()
}

fn junction() -> Symbol {
    let mut b = B::new(0.0, 0.0);
    b.dot([0.0, 0.0], 0.22, Role::PinOut);
    b.outputs.push(SymPin { at: [0.0, 0.0], dir: PinDir::Output });
    b.finish()
}

fn text_note() -> Symbol {
    let mut b = B::new(4.0, 2.0);
    let r = rect(0.1, 0.2, 3.9, 1.8);
    // Dashed frame: annotation, not a component body.
    for k in 0..4 {
        let (a, z) = (r[k], r[(k + 1) % 4]);
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
    b.text("TEXT", 2.0, 1.0, 0.7);
    b.finish()
}

impl Symbol {
    /// The symbol of `kind` in `style`.
    #[must_use]
    pub fn build(kind: SymbolKind, style: SymbolStyle) -> Self {
        match kind {
            SymbolKind::Gate(op, n) => gate(op, n, style),
            SymbolKind::Not => not_gate(style),
            SymbolKind::Switch => switch(),
            SymbolKind::Clock => clock(),
            SymbolKind::Led => led(),
            SymbolKind::DFlipFlop => flip_flop(&["D", "C"], 1),
            SymbolKind::RsFlipFlop => flip_flop(&["S", "C", "R"], 1),
            SymbolKind::JkFlipFlop => flip_flop(&["J", "C", "K"], 1),
            SymbolKind::TriState => tri_state(style),
            SymbolKind::Ram => memory("RAM", &["A", "DI", "WE", "CS"], &["DO"]),
            SymbolKind::Rom => memory("ROM", &["A", "CS"], &["D"]),
            SymbolKind::SevenSegment => seven_segment(),
            SymbolKind::NumericOutput => numeric_output(),
            SymbolKind::Vcc => vcc(),
            SymbolKind::Ground => ground(),
            SymbolKind::SequenceGenerator => sequence_generator(),
            SymbolKind::InteractiveInput => interactive_input(),
            SymbolKind::Macro => Self::macro_ic("IC", 4, 4),
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
            if let Some(role) = s.fill {
                if s.closed {
                    tess::fill(&s.points, tess::attr(role, Width::None, s.layer), &mut out);
                }
            }
            if let Some((role, width)) = s.stroke {
                tess::stroke(&s.points, s.closed, tess::attr(role, width, s.layer), &mut out);
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
            .flat_map(|&op| (MIN_GATE_INPUTS..=MAX_GATE_INPUTS).map(move |inputs| ComponentKind::Gate { op, inputs }))
            .collect();
        v.extend([
            ComponentKind::Not,
            ComponentKind::Switch { on: false },
            ComponentKind::Switch { on: true },
            ComponentKind::Clock { half_period: 3 },
            ComponentKind::Led,
        ]);
        v
    }

    /// Every pin anchor of every current component's symbol, in both styles,
    /// sits exactly on the pin position `bw-core` uses for wires, and a pin
    /// stub stroke starts there.
    #[test]
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    fn pin_anchors_match_core_pins() {
        for kind in components() {
            let comp = Component { id: ComponentId(0), kind, pos: GridPos::new(0, 0), rotation: Rotation::R0 };
            for style in SymbolStyle::ALL {
                let sym = Symbol::build(SymbolKind::of(&kind), style);
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
            }
        }
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
                    if kind != SymbolKind::Junction {
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
