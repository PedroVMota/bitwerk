use serde::{Deserialize, Serialize};

use crate::geom::{GridPos, GridRect};

/// Minimum number of inputs of a multi-input gate.
pub const MIN_GATE_INPUTS: u8 = 2;
/// Maximum number of inputs of a multi-input gate.
pub const MAX_GATE_INPUTS: u8 = 8;
/// Maximum clock half-period, in simulation ticks.
pub const MAX_CLOCK_HALF_PERIOD: u32 = 1_000_000;
/// Address lines of a memory device (2^n words).
pub const MIN_ADDR_BITS: u8 = 4;
pub const MAX_ADDR_BITS: u8 = 14;
/// Data widths a memory device may have (bits per word).
pub const DATA_WIDTHS: [u8; 5] = [1, 2, 4, 8, 16];
/// Largest memory in words (`2^MAX_ADDR_BITS`).
pub const MAX_MEMORY_WORDS: usize = 1 << MAX_ADDR_BITS;

/// Longest pattern of a sequence generator (bits).
pub const MAX_SEQ_LEN: u8 = 64;
/// Input lines of a numeric output display.
pub const MIN_NUMERIC_BITS: u8 = 2;
pub const MAX_NUMERIC_BITS: u8 = 16;
/// Segments of a 7-segment display (a … g).
pub const SEGMENTS: u8 = 7;
/// Half-period (ticks) of a pulsing interactive input: the documented
/// default clock speed (1 Hz at the default 20 ticks/s), the same as a
/// newly placed clock.
pub const PULSE_HALF_PERIOD: u32 = 10;

/// Largest DIL package (pins).
pub const MAX_DIL_PINS: usize = 28;
/// DIL package sizes a macro template may use.
pub const DIL_SIZES: [u8; 7] = [8, 14, 16, 18, 20, 24, 28];
/// Maximum inputs (and, separately, outputs) of a macro.
pub const MAX_MACRO_SIDE_PINS: u8 = 32;
/// Maximum inputs, and separately outputs, of a macro (imported Digital
/// Works parts have wide buses: a 7 → 128 decoder, a 154-input interrupt
/// controller). Inputs plus outputs must fit in 255 pins, and only the first
/// [`MAX_TRISTATE_OUTPUTS`] outputs can be tri-state.
pub const MAX_MACRO_INPUTS: u8 = 254;
/// Outputs of a macro that may be tri-state (the mask is 64 bits).
pub const MAX_TRISTATE_OUTPUTS: u8 = 64;
/// Body width limits of a macro instance (grid units).
pub const MIN_MACRO_WIDTH: u8 = 4;
pub const MAX_MACRO_WIDTH: u8 = 32;

/// Stable identifier of a macro definition in a project's definition table
/// (see [`crate::MacroTable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MacroId(pub u32);

/// Direction of a macro tag (an interface pin of a macro definition), seen
/// from outside the macro: an `In` tag brings a signal into the body (it
/// drives the inner net), an `Out` tag takes one out (it reads the inner
/// net).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TagDir {
    In,
    Out,
}

/// The pin layout of a macro instance, derived from its definition (see
/// [`crate::MacroDef::shape`]). Kept in the component kind so pin geometry
/// stays a pure function of the kind; the definition table keeps every
/// instance's shape in sync when a definition changes.
///
/// On disk only the definition id is stored; the loader resolves the rest
/// from the definitions table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroShape {
    pub def: MacroId,
    pub inputs: u8,
    pub outputs: u8,
    /// Body width in grid units.
    pub width: u8,
    /// 0 = automatic box (inputs left, outputs right); otherwise the pin
    /// count of a DIL package.
    pub dil: u8,
    /// Bit `k` set: output `k` is tri-state (may share a bus).
    pub tristate: u64,
    /// DIL package pin number (1-based) of each instance pin (inputs, then
    /// outputs); unused for the automatic box.
    pub dil_pins: [u8; MAX_DIL_PINS],
}

impl MacroShape {
    /// A shape that only names its definition (as read from a file, before
    /// the loader resolves it); never valid on its own.
    #[must_use]
    pub const fn unresolved(def: MacroId) -> Self {
        Self { def, inputs: 0, outputs: 0, width: 0, dil: 0, tristate: 0, dil_pins: [0; MAX_DIL_PINS] }
    }

    /// Body height in grid units.
    #[must_use]
    pub const fn height(&self) -> i32 {
        if self.dil > 0 {
            self.dil as i32
        } else {
            let rows = if self.inputs > self.outputs { self.inputs } else { self.outputs };
            2 * if rows == 0 { 1 } else { rows as i32 }
        }
    }

    /// Internal consistency (not that it matches its definition).
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        let pins = self.inputs as usize + self.outputs as usize;
        if self.inputs > MAX_MACRO_INPUTS || self.outputs > MAX_MACRO_INPUTS || pins > 255 {
            return false;
        }
        if self.width < MIN_MACRO_WIDTH || self.width > MAX_MACRO_WIDTH || self.width % 2 != 0 {
            return false;
        }
        if self.outputs < 64 && self.tristate >> self.outputs != 0 {
            return false;
        }
        if self.dil == 0 {
            return true;
        }
        let mut ok_size = false;
        let mut i = 0;
        while i < DIL_SIZES.len() {
            if DIL_SIZES[i] == self.dil {
                ok_size = true;
            }
            i += 1;
        }
        if !ok_size || pins > self.dil as usize {
            return false;
        }
        // Every pin has a distinct package pin number in 1..=dil.
        let mut seen = 0u32;
        let mut k = 0;
        while k < pins {
            let n = self.dil_pins[k];
            if n == 0 || n > self.dil || seen & (1 << n) != 0 {
                return false;
            }
            seen |= 1 << n;
            k += 1;
        }
        true
    }
}

impl Serialize for MacroShape {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.def.serialize(s)
    }
}

impl<'de> Deserialize<'de> for MacroShape {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        MacroId::deserialize(d).map(Self::unresolved)
    }
}

/// Stable identifier of a component inside a [`crate::Circuit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ComponentId(pub u32);

/// Boolean operation of a combinational gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GateOp {
    And,
    Or,
    Nand,
    Nor,
    Xor,
    Xnor,
}

impl GateOp {
    pub const ALL: [Self; 6] = [Self::And, Self::Or, Self::Nand, Self::Nor, Self::Xor, Self::Xnor];

    /// Evaluates the gate over an iterator of (two-valued) input levels.
    #[must_use]
    pub fn eval(self, inputs: impl IntoIterator<Item = bool>) -> bool {
        let mut it = inputs.into_iter();
        match self {
            Self::And => it.all(|b| b),
            Self::Nand => !it.all(|b| b),
            Self::Or => it.any(|b| b),
            Self::Nor => !it.any(|b| b),
            Self::Xor => it.fold(false, |acc, b| acc ^ b),
            Self::Xnor => !it.fold(false, |acc, b| acc ^ b),
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
            Self::Nand => "NAND",
            Self::Nor => "NOR",
            Self::Xor => "XOR",
            Self::Xnor => "XNOR",
        }
    }
}

/// Logic function of a flip-flop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlipFlopType {
    /// Data: `Q ← D`.
    D,
    /// Set/reset; S = R = 1 is forbidden (the state becomes unknown, X).
    Rs,
    /// J/K: like RS, but J = K = 1 toggles.
    Jk,
}

impl FlipFlopType {
    pub const ALL: [Self; 3] = [Self::D, Self::Rs, Self::Jk];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::D => "D",
            Self::Rs => "RS",
            Self::Jk => "JK",
        }
    }

    /// Labels of the synchronous inputs (left side, top to bottom); the
    /// clock is always the middle one ([`Self::CLOCK_INPUT`]).
    #[must_use]
    pub const fn input_labels(self) -> &'static [&'static str] {
        match self {
            Self::D => &["D", "C"],
            Self::Rs => &["S", "C", "R"],
            Self::Jk => &["J", "C", "K"],
        }
    }

    /// Index of the clock among the synchronous inputs.
    pub const CLOCK_INPUT: u8 = 1;

    /// Number of synchronous inputs (data + clock).
    #[must_use]
    pub const fn sync_inputs(self) -> u8 {
        match self {
            Self::D => 2,
            Self::Rs | Self::Jk => 3,
        }
    }

    /// Trigger the original tool documents for this type (D: output on the
    /// rising edge; JK: master-slave, output on the falling edge; RS: the
    /// plain bistable, here a gated latch).
    #[must_use]
    pub const fn default_trigger(self) -> FfTrigger {
        match self {
            Self::D => FfTrigger::RisingEdge,
            Self::Rs => FfTrigger::Latch,
            Self::Jk => FfTrigger::MasterSlave,
        }
    }
}

/// When a flip-flop takes its inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FfTrigger {
    /// Level-sensitive bistable (gated latch): transparent while C = 1.
    Latch,
    /// Pulse-triggered master-slave: the master follows the inputs while
    /// C = 1 (for JK with the slave's feedback, so it can only change once
    /// per pulse), the output takes the master on the falling edge of C.
    MasterSlave,
    /// Edge-triggered on C 0 → 1.
    RisingEdge,
    /// Edge-triggered on C 1 → 0.
    FallingEdge,
}

impl FfTrigger {
    pub const ALL: [Self; 4] = [Self::Latch, Self::MasterSlave, Self::RisingEdge, Self::FallingEdge];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Latch => "Level (latch)",
            Self::MasterSlave => "Master-slave",
            Self::RisingEdge => "Rising edge",
            Self::FallingEdge => "Falling edge",
        }
    }

    /// Dense index (0..4), used to pack symbol variants.
    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Self::Latch => 0,
            Self::MasterSlave => 1,
            Self::RisingEdge => 2,
            Self::FallingEdge => 3,
        }
    }
}

/// Writable or read-only memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemoryType {
    Ram,
    Rom,
}

impl MemoryType {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ram => "RAM",
            Self::Rom => "ROM",
        }
    }
}

/// How an interactive input (switch) responds to the hand tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SwitchMode {
    /// Each click toggles the level (the documented default).
    #[default]
    Toggle,
    /// Push button: the level is the inverse of `on` while the mouse button
    /// is held (`on = false`: push-to-make, `on = true`: push-to-break).
    Momentary,
    /// Pulse input: produces a square wave with the default clock speed
    /// ([`PULSE_HALF_PERIOD`]); `on` is the level the cycle starts with.
    Pulse,
}

impl SwitchMode {
    pub const ALL: [Self; 3] = [Self::Toggle, Self::Momentary, Self::Pulse];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Toggle => "Toggle",
            Self::Momentary => "Push button",
            Self::Pulse => "Pulse",
        }
    }

    #[must_use]
    #[allow(clippy::trivially_copy_pass_by_ref, reason = "serde skip predicate signature")]
    pub const fn is_toggle(&self) -> bool {
        matches!(self, Self::Toggle)
    }
}

/// Display format of a numeric output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum NumFormat {
    /// One digit per input line (the documented default).
    #[default]
    Bin,
    Hex,
    Dec,
}

impl NumFormat {
    pub const ALL: [Self; 3] = [Self::Bin, Self::Hex, Self::Dec];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bin => "Binary",
            Self::Hex => "Hexadecimal",
            Self::Dec => "Decimal",
        }
    }

    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Self::Bin => 0,
            Self::Hex => 1,
            Self::Dec => 2,
        }
    }
}

/// Characters a numeric output shows for `bits` input lines in `format`
/// (the sign column included for two's complement decimals).
#[must_use]
pub const fn numeric_digits(bits: u8, format: NumFormat, signed: bool) -> u8 {
    let bits = if bits < MIN_NUMERIC_BITS {
        MIN_NUMERIC_BITS
    } else if bits > MAX_NUMERIC_BITS {
        MAX_NUMERIC_BITS
    } else {
        bits
    };
    match format {
        NumFormat::Bin => bits,
        NumFormat::Hex => bits.div_ceil(4),
        NumFormat::Dec => {
            // Decimal digits of the largest magnitude.
            let max: u32 = if signed { 1 << (bits - 1) } else { (1u32 << bits) - 1 };
            let mut n = 1;
            let mut v = max / 10;
            while v > 0 {
                n += 1;
                v /= 10;
            }
            if signed { n + 1 } else { n }
        }
    }
}

/// Display colour of an LED, clock, sequence generator, 7-segment display,
/// numeric output or text annotation (a small fixed palette, so the GPU
/// instance only carries an index). `None` on a component means the
/// kind's default colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Color {
    Red = 0,
    Orange = 1,
    Yellow = 2,
    Lime = 3,
    Green = 4,
    Cyan = 5,
    Blue = 6,
    Navy = 7,
    Purple = 8,
    Magenta = 9,
    White = 10,
    Grey = 11,
    Black = 12,
    Brown = 13,
    Olive = 14,
    Teal = 15,
}

impl Color {
    pub const ALL: [Self; 16] = [
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Lime,
        Self::Green,
        Self::Cyan,
        Self::Blue,
        Self::Navy,
        Self::Purple,
        Self::Magenta,
        Self::White,
        Self::Grey,
        Self::Black,
        Self::Brown,
        Self::Olive,
        Self::Teal,
    ];

    /// Palette index (0..16), as packed into GPU instances.
    #[must_use]
    pub const fn index(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_index(i: u8) -> Option<Self> {
        if (i as usize) < Self::ALL.len() { Some(Self::ALL[i as usize]) } else { None }
    }

    /// Nominal sRGB value (0..=255 per channel).
    #[must_use]
    pub const fn rgb(self) -> [u8; 3] {
        match self {
            Self::Red => [255, 0, 0],
            Self::Orange => [255, 140, 0],
            Self::Yellow => [255, 255, 0],
            Self::Lime => [0, 255, 0],
            Self::Green => [0, 128, 0],
            Self::Cyan => [0, 255, 255],
            Self::Blue => [0, 0, 255],
            Self::Navy => [0, 0, 128],
            Self::Purple => [128, 0, 128],
            Self::Magenta => [255, 0, 255],
            Self::White => [255, 255, 255],
            Self::Grey => [128, 128, 128],
            Self::Black => [0, 0, 0],
            Self::Brown => [128, 64, 0],
            Self::Olive => [128, 128, 64],
            Self::Teal => [0, 128, 128],
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Red => "Red",
            Self::Orange => "Orange",
            Self::Yellow => "Yellow",
            Self::Lime => "Lime",
            Self::Green => "Green",
            Self::Cyan => "Cyan",
            Self::Blue => "Blue",
            Self::Navy => "Navy",
            Self::Purple => "Purple",
            Self::Magenta => "Magenta",
            Self::White => "White",
            Self::Grey => "Grey",
            Self::Black => "Black",
            Self::Brown => "Brown",
            Self::Olive => "Olive",
            Self::Teal => "Teal",
        }
    }

    /// The palette entry closest to an sRGB colour, and whether it is exact.
    #[must_use]
    pub fn nearest(rgb: [u8; 3]) -> (Self, bool) {
        let d = |c: Self| -> u32 {
            let p = c.rgb();
            (0..3).map(|i| (i32::from(p[i]) - i32::from(rgb[i])).unsigned_abs().pow(2)).sum()
        };
        let best = Self::ALL.iter().copied().min_by_key(|&c| d(c)).unwrap_or(Self::Red);
        (best, d(best) == 0)
    }
}

/// The kind (and kind-specific parameters) of a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComponentKind {
    /// Multi-input gate with `inputs` in `MIN_GATE_INPUTS..=MAX_GATE_INPUTS`.
    Gate { op: GateOp, inputs: u8 },
    /// Inverter (one input, one output).
    Not,
    /// Interactive input (one output): a toggle switch, a push button or a
    /// pulse input (see [`SwitchMode`]).
    Switch {
        on: bool,
        #[serde(default, skip_serializing_if = "SwitchMode::is_toggle")]
        mode: SwitchMode,
    },
    /// Square-wave clock (one output); toggles every `half_period` ticks.
    Clock { half_period: u32 },
    /// Light-emitting diode (one input).
    Led,
    /// Tri-state buffer: inputs IN, EN; the output follows IN while EN = 1
    /// and floats (Z) while EN = 0. Its output may share a net with other
    /// tri-state outputs (a bus).
    TriState,
    /// Flip-flop with outputs Q and Q̄ and, optionally, asynchronous
    /// active-high preset and clear pins.
    FlipFlop { ty: FlipFlopType, trigger: FfTrigger, preset_clear: bool },
    /// RAM or ROM with `2^addr_bits` words of `data_bits` bits. Tri-state
    /// data outputs, active-low chip select and (RAM) R/W̅.
    Memory { ty: MemoryType, addr_bits: u8, data_bits: u8 },
    /// Interface pin of a macro definition (only meaningful inside a
    /// definition's body): an `In` tag has one output (it drives the inner
    /// net from the instance's input pin), an `Out` tag one input. A
    /// tri-state `Out` tag makes the instance's output tri-state.
    Tag { dir: TagDir, tristate: bool },
    /// An instance of a macro definition (a reference, not a copy).
    Macro(MacroShape),
    /// Fixed level source (one output): power supply (VCC, logic 1) or
    /// ground (logic 0).
    Const { high: bool },
    /// Sequence generator (one output): a cyclic pattern of `len` bits
    /// (bit `i` of `pattern` is step `i`), one step every `half_period`
    /// ticks, like a clock (whose pattern is `01`).
    SeqGen { pattern: u64, len: u8, half_period: u32 },
    /// 7-segment display: inputs a … g (left to right along the bottom);
    /// a segment lights at 1 (common cathode) or at 0 (common anode).
    SevenSeg { common_anode: bool },
    /// Numeric output: `bits` input lines (input `k` = bit `k`) shown in
    /// binary, hexadecimal or decimal (two's complement if `signed`).
    Numeric { bits: u8, format: NumFormat, signed: bool },
    /// Text annotation (no pins): a box of `w` × `h` grid units whose text
    /// and style live in the circuit's notes ([`crate::Note`]).
    Text { w: u8, h: u8 },
}

/// Where a pin sits on the body edge (unrotated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl ComponentKind {
    /// A D flip-flop with the documented defaults.
    #[must_use]
    pub const fn flip_flop(ty: FlipFlopType) -> Self {
        Self::FlipFlop { ty, trigger: ty.default_trigger(), preset_clear: !matches!(ty, FlipFlopType::Rs) }
    }

    /// A memory with the documented default size (8 address lines, 8-bit words).
    #[must_use]
    pub const fn memory(ty: MemoryType) -> Self {
        Self::Memory { ty, addr_bits: 8, data_bits: 8 }
    }

    #[must_use]
    pub const fn input_count(&self) -> u8 {
        match self {
            Self::Gate { inputs, .. } => *inputs,
            Self::Not | Self::Led | Self::Tag { dir: TagDir::Out, .. } => 1,
            Self::Switch { .. }
            | Self::Clock { .. }
            | Self::Tag { dir: TagDir::In, .. }
            | Self::Const { .. }
            | Self::SeqGen { .. }
            | Self::Text { .. } => 0,
            Self::SevenSeg { .. } => SEGMENTS,
            Self::Numeric { bits, .. } => *bits,
            Self::TriState => 2,
            Self::FlipFlop { ty, preset_clear, .. } => ty.sync_inputs() + if *preset_clear { 2 } else { 0 },
            Self::Memory { ty: MemoryType::Ram, addr_bits, data_bits } => *addr_bits + *data_bits + 2,
            Self::Memory { ty: MemoryType::Rom, addr_bits, .. } => *addr_bits + 1,
            Self::Macro(m) => m.inputs,
        }
    }

    #[must_use]
    pub const fn output_count(&self) -> u8 {
        match self {
            Self::Led
            | Self::Tag { dir: TagDir::Out, .. }
            | Self::SevenSeg { .. }
            | Self::Numeric { .. }
            | Self::Text { .. } => 0,
            Self::FlipFlop { .. } => 2,
            Self::Memory { data_bits, .. } => *data_bits,
            Self::Macro(m) => m.outputs,
            _ => 1,
        }
    }

    #[must_use]
    pub const fn pin_count(&self) -> u8 {
        self.input_count() + self.output_count()
    }

    /// Input levels a display shows (7-segment, numeric output): the
    /// simulator mirrors them as extra *display signals* after the outputs,
    /// so the renderer can colour each segment / digit from the per-pin
    /// signal slots it already has.
    #[must_use]
    pub const fn display_count(&self) -> u8 {
        match self {
            Self::SevenSeg { .. } => SEGMENTS,
            Self::Numeric { bits, .. } => *bits,
            _ => 0,
        }
    }

    /// Signal slots of a component: its outputs, then its display signals.
    #[must_use]
    pub const fn signal_count(&self) -> u8 {
        self.output_count() + self.display_count()
    }

    /// Is this a primitive that the simulator evaluates or shows (not a
    /// macro instance, tag or text annotation)?
    #[must_use]
    pub const fn is_primitive(&self) -> bool {
        !matches!(self, Self::Macro(_) | Self::Tag { .. } | Self::Text { .. })
    }

    /// Are all outputs tri-state outputs (which may share a net with other
    /// tri-state outputs)? See [`Self::output_tristate`] for macros, whose
    /// outputs differ.
    #[must_use]
    pub const fn tristate_outputs(&self) -> bool {
        matches!(self, Self::TriState | Self::Memory { .. })
    }

    /// Is output `k` (0-based among the outputs) a tri-state output?
    #[must_use]
    pub const fn output_tristate(&self, k: u8) -> bool {
        match self {
            Self::TriState | Self::Memory { .. } => true,
            Self::Macro(m) => k < 64 && m.tristate & (1 << k) != 0,
            _ => false,
        }
    }

    /// Tri-state outputs as a bit mask (bit `k` = output `k`).
    #[must_use]
    pub const fn tristate_mask(&self) -> u64 {
        match self {
            Self::TriState => 1,
            Self::Memory { data_bits, .. } => {
                if *data_bits >= 64 {
                    u64::MAX
                } else {
                    (1u64 << *data_bits) - 1
                }
            }
            Self::Macro(m) => m.tristate,
            _ => 0,
        }
    }

    /// Body size in grid units (width, height), unrotated.
    #[must_use]
    pub const fn size(&self) -> (i32, i32) {
        match self {
            Self::Gate { inputs, .. } => (4, 2 * (*inputs as i32)),
            Self::Not | Self::Tag { .. } | Self::SeqGen { .. } => (4, 2),
            Self::Switch { .. } | Self::Clock { .. } | Self::Led | Self::Const { .. } => (2, 2),
            Self::SevenSeg { .. } => (8, 12),
            Self::Numeric { bits, format, signed } => {
                let digits = numeric_digits(*bits, *format, *signed) as i32;
                // About 1.2 units per digit plus margins, rounded up to even.
                let w = (12 * digits + 9) / 10 + 3;
                let rows = if *bits < 2 { 2 } else { *bits as i32 };
                (w + w % 2, 2 * rows)
            }
            Self::Text { w, h } => (*w as i32, *h as i32),
            Self::TriState => (4, 3),
            Self::FlipFlop { ty, .. } => {
                let h = 2 * ty.sync_inputs() as i32;
                (8, if h < 4 { 4 } else { h })
            }
            Self::Memory { .. } => {
                let ins = self.input_count() as i32;
                let outs = self.output_count() as i32;
                (8, 2 * if ins > outs { ins } else { outs })
            }
            Self::Macro(m) => (m.width as i32, m.height()),
        }
    }

    /// Checks kind-specific parameter limits.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        match self {
            Self::Gate { inputs, .. } => *inputs >= MIN_GATE_INPUTS && *inputs <= MAX_GATE_INPUTS,
            Self::Clock { half_period } => *half_period >= 1 && *half_period <= MAX_CLOCK_HALF_PERIOD,
            // A level-sensitive JK with J = K = 1 oscillates while C = 1:
            // only the clocked variants exist.
            Self::FlipFlop { ty: FlipFlopType::Jk, trigger: FfTrigger::Latch, .. } => false,
            Self::Memory { addr_bits, data_bits, .. } => {
                *addr_bits >= MIN_ADDR_BITS && *addr_bits <= MAX_ADDR_BITS && data_width_index(*data_bits).is_some()
            }
            Self::Macro(m) => m.is_valid(),
            Self::SeqGen { pattern, len, half_period } => {
                *len >= 1
                    && *len <= MAX_SEQ_LEN
                    && (*len == MAX_SEQ_LEN || *pattern >> *len == 0)
                    && *half_period >= 1
                    && *half_period <= MAX_CLOCK_HALF_PERIOD
            }
            Self::Numeric { bits, .. } => *bits >= MIN_NUMERIC_BITS && *bits <= MAX_NUMERIC_BITS,
            Self::Text { w, h } => *w >= 1 && *h >= 1,
            Self::Not
            | Self::Switch { .. }
            | Self::Led
            | Self::TriState
            | Self::FlipFlop { .. }
            | Self::Tag { .. }
            | Self::Const { .. }
            | Self::SevenSeg { .. } => true,
        }
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Gate { op, .. } => op.name(),
            Self::Not => "NOT",
            Self::Switch { .. } => "Switch",
            Self::Clock { .. } => "Clock",
            Self::Led => "LED",
            Self::TriState => "Tri-state",
            Self::FlipFlop { ty: FlipFlopType::D, .. } => "D-FF",
            Self::FlipFlop { ty: FlipFlopType::Rs, .. } => "RS-FF",
            Self::FlipFlop { ty: FlipFlopType::Jk, .. } => "JK-FF",
            Self::Memory { ty, .. } => ty.name(),
            Self::Tag { dir: TagDir::In, .. } => "In tag",
            Self::Tag { dir: TagDir::Out, .. } => "Out tag",
            Self::Macro(_) => "Macro",
            Self::Const { high: true } => "VCC",
            Self::Const { high: false } => "Ground",
            Self::SeqGen { .. } => "Sequence",
            Self::SevenSeg { .. } => "7-segment",
            Self::Numeric { .. } => "Numeric",
            Self::Text { .. } => "Text",
        }
    }

    /// Words of a memory device (0 for other kinds).
    #[must_use]
    pub const fn memory_words(&self) -> usize {
        match self {
            Self::Memory { addr_bits, .. } => 1usize << *addr_bits,
            _ => 0,
        }
    }

    /// Side and position along that side of a pin (unrotated, grid units).
    ///
    /// Layout: inputs on the left at `y = 2i + 1`, outputs on the right
    /// (`y = h / 2` for single-output kinds), except the tri-state enable
    /// (top), the flip-flop preset (top) / clear (bottom) and the flip-flop
    /// outputs Q (`y = 1`) and Q̄ (`y = h - 1`).
    #[must_use]
    pub fn pin_offset(&self, pin: u8) -> Option<(i32, i32)> {
        let ins = self.input_count();
        if pin >= self.pin_count() {
            return None;
        }
        let (w, h) = self.size();
        let row = |i: u8| 2 * i32::from(i) + 1;
        Some(match *self {
            Self::TriState => match pin {
                0 => (0, 2),
                1 => (2, 0),
                _ => (w, 2),
            },
            Self::FlipFlop { ty, .. } => {
                let sync = ty.sync_inputs();
                if pin < sync {
                    (0, row(pin))
                } else if pin < ins {
                    // Preset on top, clear at the bottom (middle column).
                    if pin == sync { (w / 2, 0) } else { (w / 2, h) }
                } else if pin == ins {
                    (w, 1)
                } else {
                    (w, h - 1)
                }
            }
            Self::Macro(m) if m.dil > 0 => {
                let n = i32::from(m.dil_pins[usize::from(pin)]);
                let half = i32::from(m.dil) / 2;
                if n <= half { (0, 2 * n - 1) } else { (w, 2 * (i32::from(m.dil) - n) + 1) }
            }
            Self::Memory { .. } | Self::Macro(_) => {
                if pin < ins {
                    (0, row(pin))
                } else {
                    (w, row(pin - ins))
                }
            }
            // Segments a … g along the bottom, one unit apart.
            Self::SevenSeg { .. } => (1 + i32::from(pin), h),
            Self::Tag { dir: TagDir::In, .. } => (w, 1),
            Self::Tag { dir: TagDir::Out, .. } => (0, 1),
            _ => {
                if pin < ins {
                    (0, row(pin))
                } else {
                    (w, h / 2)
                }
            }
        })
    }

    /// Side of the body a pin leaves from (unrotated).
    #[must_use]
    pub fn pin_side(&self, pin: u8) -> Option<PinSide> {
        let (x, y) = self.pin_offset(pin)?;
        let (w, h) = self.size();
        Some(if x == 0 {
            PinSide::Left
        } else if x == w {
            PinSide::Right
        } else if y == 0 {
            PinSide::Top
        } else if y == h {
            PinSide::Bottom
        } else {
            PinSide::Left
        })
    }

    /// Human-readable pin name (tooltips, error messages).
    #[must_use]
    pub fn pin_name(&self, pin: u8) -> String {
        let ins = self.input_count();
        if pin >= self.pin_count() {
            return String::new();
        }
        match *self {
            Self::Gate { .. } | Self::Not | Self::Led => {
                if pin < ins {
                    format!("in{pin}")
                } else {
                    "out".into()
                }
            }
            Self::Switch { .. }
            | Self::Clock { .. }
            | Self::Tag { dir: TagDir::In, .. }
            | Self::Const { .. }
            | Self::SeqGen { .. } => "out".into(),
            Self::SevenSeg { .. } => ["a", "b", "c", "d", "e", "f", "g"][usize::from(pin.min(6))].into(),
            Self::Numeric { .. } => format!("D{pin}"),
            Self::Text { .. } => String::new(),
            Self::Tag { dir: TagDir::Out, .. } => "in".into(),
            Self::Macro(m) => {
                if pin < ins {
                    format!("in{pin}")
                } else if m.dil > 0 {
                    format!("out{} (pin {})", pin - ins, m.dil_pins[usize::from(pin)])
                } else {
                    format!("out{}", pin - ins)
                }
            }
            Self::TriState => ["IN", "EN", "OUT"][usize::from(pin.min(2))].into(),
            Self::FlipFlop { ty, .. } => {
                let sync = ty.sync_inputs();
                if pin < sync {
                    ty.input_labels()[usize::from(pin)].into()
                } else if pin < ins {
                    if pin == sync { "PRE".into() } else { "CLR".into() }
                } else if pin == ins {
                    "Q".into()
                } else {
                    "Q̄".into()
                }
            }
            Self::Memory { ty, addr_bits, data_bits } => {
                if pin < addr_bits {
                    format!("A{pin}")
                } else if pin >= ins {
                    format!("D{} (out)", pin - ins)
                } else if ty == MemoryType::Ram && pin < addr_bits + data_bits {
                    format!("D{} (in)", pin - addr_bits)
                } else if ty == MemoryType::Ram && pin == addr_bits + data_bits {
                    "R/W̄".into()
                } else {
                    "C̄S̄".into()
                }
            }
        }
    }
}

/// Index of a data width in [`DATA_WIDTHS`].
#[must_use]
pub const fn data_width_index(bits: u8) -> Option<u8> {
    let mut i = 0;
    while i < DATA_WIDTHS.len() {
        if DATA_WIDTHS[i] == bits {
            #[allow(clippy::cast_possible_truncation, reason = "DATA_WIDTHS has 5 entries")]
            return Some(i as u8);
        }
        i += 1;
    }
    None
}

/// Rotation of a component body, in 90° steps clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    /// Next rotation, 90° clockwise.
    #[must_use]
    pub const fn rotated_cw(self) -> Self {
        match self {
            Self::R0 => Self::R90,
            Self::R90 => Self::R180,
            Self::R180 => Self::R270,
            Self::R270 => Self::R0,
        }
    }
}

/// Direction of a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinDir {
    Input,
    Output,
}

/// Reference to one pin of one component. Pins are numbered inputs first,
/// then outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PinRef {
    pub component: ComponentId,
    pub pin: u8,
}

/// A component placed on the grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    pub id: ComponentId,
    pub kind: ComponentKind,
    /// Top-left corner of the (unrotated) body.
    pub pos: GridPos,
    #[serde(default)]
    pub rotation: Rotation,
    /// Display colour (LEDs, clocks, sequence generators, displays, text);
    /// `None` = the kind's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
}

impl Component {
    #[must_use]
    pub fn pin_dir(&self, pin: u8) -> Option<PinDir> {
        let ins = self.kind.input_count();
        if pin < ins {
            Some(PinDir::Input)
        } else if pin < self.kind.pin_count() {
            Some(PinDir::Output)
        } else {
            None
        }
    }

    /// Local (unrotated) offset of a pin from `pos`.
    #[must_use]
    pub fn pin_local_offset(&self, pin: u8) -> Option<(i32, i32)> {
        self.kind.pin_offset(pin)
    }

    /// Absolute grid position of a pin, taking rotation into account.
    #[must_use]
    pub fn pin_pos(&self, pin: u8) -> Option<GridPos> {
        let (lx, ly) = self.pin_local_offset(pin)?;
        let (w, h) = self.kind.size();
        let (rx, ry) = match self.rotation {
            Rotation::R0 => (lx, ly),
            Rotation::R90 => (h - ly, lx),
            Rotation::R180 => (w - lx, h - ly),
            Rotation::R270 => (ly, w - lx),
        };
        Some(self.pos.offset(rx, ry))
    }

    /// Bounding rectangle of the body on the grid.
    #[must_use]
    pub fn bounds(&self) -> GridRect {
        let (w, h) = self.kind.size();
        let (w, h) = match self.rotation {
            Rotation::R0 | Rotation::R180 => (w, h),
            Rotation::R90 | Rotation::R270 => (h, w),
        };
        GridRect { min: self.pos, max: self.pos.offset(w, h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_truth_tables() {
        let cases = [(false, false), (false, true), (true, false), (true, true)];
        let expect = |op: GateOp| -> [bool; 4] {
            match op {
                GateOp::And => [false, false, false, true],
                GateOp::Or => [false, true, true, true],
                GateOp::Nand => [true, true, true, false],
                GateOp::Nor => [true, false, false, false],
                GateOp::Xor => [false, true, true, false],
                GateOp::Xnor => [true, false, false, true],
            }
        };
        for op in GateOp::ALL {
            for (i, (a, b)) in cases.iter().enumerate() {
                assert_eq!(op.eval([*a, *b]), expect(op)[i], "{op:?} {a} {b}");
            }
        }
    }

    #[test]
    fn pins_and_bounds() {
        let c = Component {
            id: ComponentId(1),
            kind: ComponentKind::Gate { op: GateOp::And, inputs: 2 },
            pos: GridPos::new(10, 10),
            rotation: Rotation::R0,
            color: None,
        };
        assert_eq!(c.pin_pos(0), Some(GridPos::new(10, 11)));
        assert_eq!(c.pin_pos(1), Some(GridPos::new(10, 13)));
        assert_eq!(c.pin_pos(2), Some(GridPos::new(14, 12)));
        assert_eq!(c.pin_pos(3), None);
        assert_eq!(c.bounds().max, GridPos::new(14, 14));
    }

    fn all_new_kinds() -> Vec<ComponentKind> {
        let mut v = vec![ComponentKind::TriState];
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

    /// Every pin of the new kinds lies on the body edge, pins never overlap
    /// and inputs / outputs are on the documented sides.
    #[test]
    fn new_kind_pins_are_distinct_and_on_the_edge() {
        for kind in all_new_kinds() {
            let (w, h) = kind.size();
            let mut seen = std::collections::HashSet::new();
            for pin in 0..kind.pin_count() {
                let (x, y) = kind.pin_offset(pin).unwrap();
                assert!(x == 0 || x == w || y == 0 || y == h, "{kind:?} pin {pin} at ({x},{y}) inside the body");
                assert!((0..=w).contains(&x) && (0..=h).contains(&y), "{kind:?} pin {pin} outside");
                assert!(seen.insert((x, y)), "{kind:?}: two pins at ({x},{y})");
                assert!(!kind.pin_name(pin).is_empty());
            }
            assert!(kind.pin_offset(kind.pin_count()).is_none());
        }
    }

    #[test]
    fn flip_flop_and_memory_parameters() {
        let d = ComponentKind::flip_flop(FlipFlopType::D);
        assert_eq!((d.input_count(), d.output_count()), (4, 2));
        assert_eq!(d.pin_name(4), "Q");
        assert_eq!(d.pin_name(5), "Q̄");
        assert!(
            !ComponentKind::FlipFlop { ty: FlipFlopType::Jk, trigger: FfTrigger::Latch, preset_clear: false }
                .is_valid()
        );
        let ram = ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 4, data_bits: 4 };
        assert_eq!((ram.input_count(), ram.output_count(), ram.memory_words()), (10, 4, 16));
        assert_eq!(ram.pin_name(8), "R/W̄");
        assert_eq!(ram.pin_name(9), "C̄S̄");
        assert!(!ComponentKind::Memory { ty: MemoryType::Rom, addr_bits: 3, data_bits: 8 }.is_valid());
        assert!(!ComponentKind::Memory { ty: MemoryType::Rom, addr_bits: 15, data_bits: 8 }.is_valid());
        assert!(!ComponentKind::Memory { ty: MemoryType::Rom, addr_bits: 8, data_bits: 3 }.is_valid());
        assert!(ComponentKind::Memory { ty: MemoryType::Rom, addr_bits: 14, data_bits: 16 }.is_valid());
    }
}
