//! Streaming reader of the Digital Works object stream (see
//! `docs/dwm/DWM-FORMAT.md`).
//!
//! * Fields are positional: every class has a fixed *program* of value
//!   reads ([`Op`]), built from its parent chain. An interpreter runs the
//!   programs with an explicit stack of frames, so a hostile file nested
//!   arbitrarily deep can never overflow the native stack.
//! * Objects are appended to the object table **before** their fields are
//!   read (as the original writer numbers them), so back references may
//!   point to ancestors still being read, never forward.
//! * Everything is bounded: file size, object count, stack depth, class
//!   table, string and list lengths, memory words (also against the bytes
//!   left, before allocating).
//!
//! The reader only checks the stream's syntax. References are typed and
//! checked later (`Graph` accessors return `None` for a reference to an
//! object of the wrong class, and the converter reports it).

#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "stream parser and geometry code: short coordinate names, one linear pass per class"
)]

use std::fmt;

/// Hard limits (see the format notes; the largest real file has 408 k
/// objects and 88 levels of nesting).
pub const MAX_FILE: usize = 64 * 1024 * 1024;
pub const MAX_OBJECTS: usize = 2_000_000;
pub const MAX_STACK: usize = 200_000;
pub const MAX_STRING: usize = 1 << 20;
pub const MAX_LIST: usize = 1_000_000;
pub const MAX_MEMORY_WORDS: usize = 1 << 20;
pub const MAX_CLASSES: usize = 256;
pub const MAX_HEADER_PROPS: usize = 1000;
pub const MAX_TEMPLATE_ROOTS: usize = 16;

/// Magic low word of a circuit / macro (`.dwm`) and of a template (`.dwt`).
pub const DWM_MAGIC: u32 = 0xC4DD;
pub const DWT_MAGIC: u32 = 0xED19;
/// Format versions Digital Works 3 reads.
pub const VERSIONS: [u8; 3] = [0x14, 0x1E, 0x22];

/// A syntax error with the byte offset where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    pub offset: usize,
    pub msg: String,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {:#x})", self.msg, self.offset)
    }
}

/// Every class of the stream. Classes without a `Read` override of their
/// own read like their parent; the leaf names still matter for the
/// converter (the gate type and input count live in the class name only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cls {
    SerializeList,
    FloatPoint,
    GraphicList,
    Segment,
    Polyline,
    Polygon,
    GraphicBox,
    MessageBox,
    GraphicText,
    Pin,
    MacroPin,
    Wire,
    Solder,
    /// A gate: operation index (0 And, 1 Nand, 2 Or, 3 Nor, 4 Xor, 5 Xnor,
    /// 6 Not) and input count (0 for the abstract classes).
    Gate(u8, u8),
    TriState,
    DFlipFlop,
    JkFlipFlop,
    RsFlipFlop,
    /// Abstract device classes (`TDevice`, `TFlipFlop`, …): read as a plain
    /// device.
    Device,
    TextDevice,
    HistoryDevice,
    InputDevice,
    Supply,
    Ground,
    InteractiveInput,
    SeqInput,
    Clock,
    BitGenerator,
    InteractiveSwitch,
    OutputDevice,
    Led,
    LedSegment,
    Numeric,
    Memory,
    Tag,
    Macro,
}

impl Cls {
    /// Class of a name (case-sensitive, as written by Digital Works).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let gate = |op: u8, rest: &str| -> Option<Self> {
            match rest {
                "" => Some(Self::Gate(op, 0)),
                _ => None,
            }
        };
        Some(match name {
            "TSerializeList" => Self::SerializeList,
            "TFloatPoint" => Self::FloatPoint,
            "TGraphicList" | "TGraphicObject" | "TSerialize" => Self::GraphicList,
            "TSegment" => Self::Segment,
            "TPolyline" => Self::Polyline,
            "TPolygon" => Self::Polygon,
            "TGraphicBox" => Self::GraphicBox,
            "TMessageBox" => Self::MessageBox,
            "TGraphicText" => Self::GraphicText,
            "TPin" => Self::Pin,
            "TMacroPin" => Self::MacroPin,
            "TWire" => Self::Wire,
            "TSolder" => Self::Solder,
            "TTriState" => Self::TriState,
            "TDFlipFlop" => Self::DFlipFlop,
            "TJKFlipFlop" => Self::JkFlipFlop,
            "TRSFlipFlop" => Self::RsFlipFlop,
            "TDevice" | "TFlipFlop" | "TClockedFlipFlop" | "TGateDevice" => Self::Device,
            "TTextDevice" => Self::TextDevice,
            "THistoryDevice" => Self::HistoryDevice,
            "TInputDevice" => Self::InputDevice,
            "TSupply" => Self::Supply,
            "TGround" => Self::Ground,
            "TInteractiveInput" => Self::InteractiveInput,
            "TSeqInputDevice" => Self::SeqInput,
            "TClockDevice" => Self::Clock,
            "TBitGenerator" => Self::BitGenerator,
            "TInteractiveSwitch" => Self::InteractiveSwitch,
            "TOutputDevice" => Self::OutputDevice,
            "TLEDDevice" => Self::Led,
            "TLEDSegmentDevice" => Self::LedSegment,
            "TNumericDevice" => Self::Numeric,
            "TMemoryDevice" => Self::Memory,
            "TTagDevice" => Self::Tag,
            "TMacro" => Self::Macro,
            "TNotGate" => Self::Gate(6, 1),
            _ => {
                // T<n><Op>Gate and the abstract T<Op>Gate.
                let body = name.strip_prefix('T')?.strip_suffix("Gate")?;
                let (n, op) = match body.as_bytes().first()? {
                    d @ b'2'..=b'4' => (d - b'0', &body[1..]),
                    _ => (0, body),
                };
                let op = match op {
                    "And" => 0,
                    "Nand" => 1,
                    "Or" => 2,
                    "Nor" => 3,
                    "Xor" => 4,
                    "Xnor" => 5,
                    _ => return None,
                };
                // Digital Works has 2–4 input AND/NAND/OR/NOR and 2-input
                // XOR/XNOR classes only.
                if n > 2 && op >= 4 {
                    return None;
                }
                return if n == 0 { gate(op, "") } else { Some(Self::Gate(op, n)) };
            }
        })
    }

    /// Is this a device (an element of a macro's device lists)?
    #[must_use]
    pub const fn is_device(self) -> bool {
        matches!(
            self,
            Self::Gate(..)
                | Self::TriState
                | Self::DFlipFlop
                | Self::JkFlipFlop
                | Self::RsFlipFlop
                | Self::Device
                | Self::TextDevice
                | Self::HistoryDevice
                | Self::InputDevice
                | Self::Supply
                | Self::Ground
                | Self::InteractiveInput
                | Self::SeqInput
                | Self::Clock
                | Self::BitGenerator
                | Self::InteractiveSwitch
                | Self::OutputDevice
                | Self::Led
                | Self::LedSegment
                | Self::Numeric
                | Self::Memory
                | Self::Tag
                | Self::Macro
        )
    }

    /// A drawing primitive (polyline, polygon, text)?
    #[must_use]
    pub const fn is_graphic(self) -> bool {
        matches!(
            self,
            Self::Segment | Self::Polyline | Self::Polygon | Self::GraphicBox | Self::MessageBox | Self::GraphicText
        )
    }

    #[must_use]
    pub const fn is_pin(self) -> bool {
        matches!(self, Self::Pin | Self::MacroPin)
    }

    /// The Digital Works class name written for this class (the inverse of
    /// [`Self::from_name`] for every concrete class; abstract classes get
    /// the name of their family's base class).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SerializeList => "TSerializeList",
            Self::FloatPoint => "TFloatPoint",
            Self::GraphicList => "TGraphicList",
            Self::Segment => "TSegment",
            Self::Polyline => "TPolyline",
            Self::Polygon => "TPolygon",
            Self::GraphicBox => "TGraphicBox",
            Self::MessageBox => "TMessageBox",
            Self::GraphicText => "TGraphicText",
            Self::Pin => "TPin",
            Self::MacroPin => "TMacroPin",
            Self::Wire => "TWire",
            Self::Solder => "TSolder",
            Self::Gate(op, n) => gate_name(op, n),
            Self::TriState => "TTriState",
            Self::DFlipFlop => "TDFlipFlop",
            Self::JkFlipFlop => "TJKFlipFlop",
            Self::RsFlipFlop => "TRSFlipFlop",
            Self::Device => "TDevice",
            Self::TextDevice => "TTextDevice",
            Self::HistoryDevice => "THistoryDevice",
            Self::InputDevice => "TInputDevice",
            Self::Supply => "TSupply",
            Self::Ground => "TGround",
            Self::InteractiveInput => "TInteractiveInput",
            Self::SeqInput => "TSeqInputDevice",
            Self::Clock => "TClockDevice",
            Self::BitGenerator => "TBitGenerator",
            Self::InteractiveSwitch => "TInteractiveSwitch",
            Self::OutputDevice => "TOutputDevice",
            Self::Led => "TLEDDevice",
            Self::LedSegment => "TLEDSegmentDevice",
            Self::Numeric => "TNumericDevice",
            Self::Memory => "TMemoryDevice",
            Self::Tag => "TTagDevice",
            Self::Macro => "TMacro",
        }
    }
}

/// Class name of a gate (operation index as in [`Cls::Gate`]).
const fn gate_name(op: u8, n: u8) -> &'static str {
    const NAMES: [[&str; 5]; 6] = [
        ["TAndGate", "TAndGate", "T2AndGate", "T3AndGate", "T4AndGate"],
        ["TNandGate", "TNandGate", "T2NandGate", "T3NandGate", "T4NandGate"],
        ["TOrGate", "TOrGate", "T2OrGate", "T3OrGate", "T4OrGate"],
        ["TNorGate", "TNorGate", "T2NorGate", "T3NorGate", "T4NorGate"],
        ["TXorGate", "TXorGate", "T2XorGate", "TXorGate", "TXorGate"],
        ["TXnorGate", "TXnorGate", "T2XnorGate", "TXnorGate", "TXnorGate"],
    ];
    if op >= 6 {
        return "TNotGate";
    }
    let n = if n > 4 { 0 } else { n };
    NAMES[op as usize][n as usize]
}

/// Field names (what a value means; see the class layouts in the notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum F {
    PenColor,
    PenWidth,
    Points,
    Brush,
    FontSize,
    FontColor,
    Italic,
    FontWeight,
    Angle,
    Text,
    Flag94,
    HasOverbar,
    Overbar,
    PinKind,
    Wire,
    Partner,
    Unk3c,
    End1,
    End2,
    X,
    Y,
    Wire1,
    Wire2,
    Wire3,
    Children,
    HasGraphics,
    Interactive,
    Flag1c,
    Pins,
    HasLabel,
    TextPos,
    NameLabel,
    LogicHistory,
    Pulse,
    State,
    Sequence,
    Color,
    SwitchType,
    Flag3c,
    CommonCathode,
    InputLines,
    Unk58,
    Unk5c,
    UnkColor60,
    Flag64,
    DataBits,
    AddressBits,
    IsRam,
    Unused,
    Words,
    Direction,
    MacroPin,
    Datasheet,
    Unk10,
    Embedded38,
    Embedded3a,
    Devices,
    Tags,
    Inputs,
    Outputs,
    Solders,
    Wires,
}

/// One value read for a field.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Int(i32),
    Bool(bool),
    Str(String),
    F32(f32),
    /// Object reference (`None` = nil).
    Ref(Option<u32>),
    Words(Vec<u16>),
}

/// One object of the table.
#[derive(Debug, Clone, PartialEq)]
pub enum Obj {
    /// A `TFloatPoint` (kept apart: there are millions).
    Point(f32, f32),
    /// A `TSerializeList`: its items (`None` = nil item).
    List(Vec<Option<u32>>),
    /// Anything else: its class and the fields read, in order.
    Rec(Cls, Vec<(F, Val)>),
    /// Placeholder while the object's fields are being read.
    Pending(Cls),
}

impl Obj {
    #[must_use]
    pub fn cls(&self) -> Cls {
        match self {
            Self::Point(..) => Cls::FloatPoint,
            Self::List(_) => Cls::SerializeList,
            Self::Rec(c, _) | Self::Pending(c) => *c,
        }
    }

    fn get(&self, f: F) -> Option<&Val> {
        match self {
            Self::Rec(_, v) => v.iter().find(|(k, _)| *k == f).map(|(_, v)| v),
            _ => None,
        }
    }

    #[must_use]
    pub fn int(&self, f: F) -> Option<i32> {
        match self.get(f)? {
            Val::Int(i) => Some(*i),
            _ => None,
        }
    }

    #[must_use]
    pub fn bool(&self, f: F) -> Option<bool> {
        match self.get(f)? {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }

    #[must_use]
    pub fn f32(&self, f: F) -> Option<f32> {
        match self.get(f)? {
            Val::F32(v) => Some(*v),
            _ => None,
        }
    }

    #[must_use]
    pub fn str(&self, f: F) -> Option<&str> {
        match self.get(f)? {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A reference field (`None` if absent or nil).
    #[must_use]
    pub fn r(&self, f: F) -> Option<u32> {
        match self.get(f)? {
            Val::Ref(r) => *r,
            _ => None,
        }
    }

    #[must_use]
    pub fn words(&self, f: F) -> Option<&[u16]> {
        match self.get(f)? {
            Val::Words(w) => Some(w),
            _ => None,
        }
    }

    /// Every object this one references (fields and list items).
    pub fn refs(&self) -> impl Iterator<Item = (Option<F>, u32)> + '_ {
        let (list, rec): (&[Option<u32>], &[(F, Val)]) = match self {
            Self::List(v) => (v, &[]),
            Self::Rec(_, v) => (&[], v),
            _ => (&[], &[]),
        };
        list.iter().filter_map(|r| r.map(|r| (None, r))).chain(rec.iter().filter_map(|(f, v)| match v {
            Val::Ref(Some(r)) => Some((Some(*f), *r)),
            _ => None,
        }))
    }
}

/// One read instruction of a class program (shared with the writer, so
/// both sides follow the same field layout).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Op {
    Int(F),
    Bool(F),
    Str(F),
    F32(F),
    Obj(F),
    /// Skip the next `n` ops unless the boolean field was true.
    IfTrue(F, u8),
    /// Skip the next `n` ops unless the boolean field was false.
    IfFalse(F, u8),
    /// Skip the next `n` ops unless the format version is 30 or 34.
    IfV3(u8),
    /// `TMemoryDevice` contents: a count, then that many integers.
    Words(F),
}

const SEGMENT: &[Op] = &[Op::Int(F::PenColor), Op::Int(F::PenWidth), Op::Obj(F::Points)];
const DEVICE: &[Op] = &[
    Op::Bool(F::HasGraphics),
    Op::IfTrue(F::HasGraphics, 1),
    Op::Obj(F::Children),
    Op::Bool(F::Interactive),
    Op::Bool(F::Flag1c),
    Op::Obj(F::Pins),
    Op::IfFalse(F::HasGraphics, 2),
    Op::F32(F::X),
    Op::F32(F::Y),
];
const TEXT_DEVICE: &[Op] =
    &[Op::Bool(F::HasLabel), Op::IfTrue(F::HasLabel, 2), Op::Int(F::TextPos), Op::Obj(F::NameLabel)];

/// The read (and write) program of a class (parents first).
pub(crate) fn program(c: Cls) -> Vec<Op> {
    let mut p: Vec<Op> = Vec::new();
    let polygon = |p: &mut Vec<Op>| {
        p.extend_from_slice(SEGMENT);
        p.push(Op::Int(F::Brush));
    };
    let history = |p: &mut Vec<Op>| {
        p.extend_from_slice(DEVICE);
        p.extend_from_slice(TEXT_DEVICE);
        p.push(Op::Bool(F::LogicHistory));
    };
    match c {
        Cls::SerializeList | Cls::FloatPoint => {}
        Cls::GraphicList => p.push(Op::Obj(F::Children)),
        Cls::Segment | Cls::Polyline => p.extend_from_slice(SEGMENT),
        Cls::Polygon | Cls::GraphicBox | Cls::MessageBox => polygon(&mut p),
        Cls::GraphicText => {
            polygon(&mut p);
            p.extend_from_slice(&[
                Op::Int(F::FontSize),
                Op::Int(F::FontColor),
                Op::Bool(F::Italic),
                Op::Int(F::FontWeight),
                Op::F32(F::Angle),
                Op::Str(F::Text),
                Op::Bool(F::Flag94),
                Op::Bool(F::HasOverbar),
                Op::IfTrue(F::HasOverbar, 1),
                Op::Obj(F::Overbar),
            ]);
        }
        Cls::Pin | Cls::MacroPin => {
            polygon(&mut p);
            p.extend_from_slice(&[Op::Int(F::PinKind), Op::Obj(F::Wire)]);
            if c == Cls::MacroPin {
                p.push(Op::Obj(F::Partner));
            }
        }
        Cls::Wire => {
            p.extend_from_slice(SEGMENT);
            p.extend_from_slice(&[Op::Int(F::Unk3c), Op::Obj(F::End1), Op::Obj(F::End2)]);
        }
        Cls::Solder => p.extend_from_slice(&[
            Op::F32(F::X),
            Op::F32(F::Y),
            Op::Obj(F::Wire1),
            Op::Obj(F::Wire2),
            Op::Obj(F::Wire3),
        ]),
        Cls::Gate(..) | Cls::TriState | Cls::DFlipFlop | Cls::JkFlipFlop | Cls::RsFlipFlop | Cls::Device => {
            p.extend_from_slice(DEVICE);
        }
        Cls::TextDevice => {
            p.extend_from_slice(DEVICE);
            p.extend_from_slice(TEXT_DEVICE);
        }
        Cls::HistoryDevice | Cls::InputDevice | Cls::Supply | Cls::Ground => history(&mut p),
        Cls::InteractiveInput => {
            history(&mut p);
            p.extend_from_slice(&[Op::Bool(F::Pulse), Op::Int(F::State)]);
        }
        Cls::SeqInput | Cls::Clock | Cls::BitGenerator => {
            history(&mut p);
            p.extend_from_slice(&[Op::Str(F::Sequence), Op::Int(F::Color)]);
        }
        Cls::InteractiveSwitch => {
            p.extend_from_slice(DEVICE);
            p.extend_from_slice(TEXT_DEVICE);
            p.extend_from_slice(&[Op::Int(F::SwitchType), Op::Bool(F::Flag3c)]);
        }
        Cls::OutputDevice | Cls::Led => {
            history(&mut p);
            p.push(Op::Int(F::Color));
        }
        Cls::LedSegment => {
            history(&mut p);
            p.extend_from_slice(&[Op::Int(F::Color), Op::IfV3(1), Op::Bool(F::CommonCathode)]);
        }
        Cls::Numeric => {
            history(&mut p);
            p.extend_from_slice(&[
                Op::Int(F::Color),
                Op::Int(F::InputLines),
                Op::Int(F::Unk58),
                Op::Int(F::Unk5c),
                Op::Int(F::UnkColor60),
                Op::Bool(F::Flag64),
            ]);
        }
        Cls::Memory => {
            p.extend_from_slice(DEVICE);
            p.extend_from_slice(&[
                Op::Int(F::DataBits),
                Op::Int(F::AddressBits),
                Op::Bool(F::IsRam),
                Op::Bool(F::Unused),
                Op::Words(F::Words),
            ]);
        }
        Cls::Tag => {
            p.extend_from_slice(DEVICE);
            p.extend_from_slice(&[Op::Int(F::TextPos), Op::Int(F::Direction), Op::Obj(F::MacroPin)]);
        }
        Cls::Macro => {
            p.extend_from_slice(DEVICE);
            p.extend_from_slice(&[
                Op::Str(F::Datasheet),
                Op::Int(F::Unk10),
                Op::Bool(F::Embedded38),
                Op::Bool(F::Embedded3a),
                Op::Obj(F::Devices),
                Op::Obj(F::Tags),
                Op::Obj(F::Inputs),
                Op::Obj(F::Outputs),
                Op::Obj(F::Solders),
                Op::Obj(F::Wires),
            ]);
        }
    }
    p
}

/// Header of a `.dwm` file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Header {
    /// Format version byte (0x14, 0x1E or 0x22).
    pub version: u8,
    /// A template (`.dwt`) rather than a circuit.
    pub template: bool,
    /// Macro name / description (v30 info string or v34 property 100).
    pub name: String,
    /// Datasheet path (v34 property 101).
    pub datasheet: String,
    /// Other header properties (id, value as text), kept for the report.
    pub other: Vec<(i32, String)>,
    /// Every version 34 header property in file order, with its kind (the
    /// writer reproduces them exactly).
    pub props: Vec<(i32, HeaderValue)>,
    /// Version 30 header: the leading boolean and the trailing integer.
    pub v30_flag: bool,
    pub v30_int: i32,
}

/// Value of a version 34 header property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Str(String),
    Int(i32),
    Bool(bool),
}

/// A parsed file: header, object table and the root object(s).
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub header: Header,
    pub objects: Vec<Obj>,
    /// Root objects: one for a `.dwm`, a sequence for a `.dwt`.
    pub roots: Vec<Option<u32>>,
    /// Deepest object nesting seen.
    pub max_depth: usize,
}

impl Graph {
    #[must_use]
    pub fn get(&self, r: u32) -> Option<&Obj> {
        self.objects.get(r as usize)
    }

    /// The object if it has class `c` (type-checked reference).
    #[must_use]
    pub fn of(&self, r: Option<u32>, ok: impl Fn(Cls) -> bool) -> Option<&Obj> {
        let o = self.get(r?)?;
        ok(o.cls()).then_some(o)
    }

    /// Items of a list reference: a `TSerializeList` directly or a
    /// `TGraphicList` (through its children). Nil items are skipped.
    #[must_use]
    pub fn items(&self, r: Option<u32>) -> Vec<u32> {
        let Some(o) = r.and_then(|r| self.get(r)) else { return Vec::new() };
        let list = match o {
            Obj::List(v) => v,
            Obj::Rec(Cls::GraphicList, _) => match o.r(F::Children).and_then(|c| self.get(c)) {
                Some(Obj::List(v)) => v,
                _ => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        list.iter().filter_map(|x| *x).collect()
    }

    /// The point of a `TFloatPoint` reference.
    #[must_use]
    pub fn point(&self, r: u32) -> Option<(f32, f32)> {
        match self.get(r)? {
            Obj::Point(x, y) => Some((*x, *y)),
            _ => None,
        }
    }

    /// The points of a points list (non-points skipped).
    #[must_use]
    pub fn points(&self, r: Option<u32>) -> Vec<(f32, f32)> {
        self.items(r).into_iter().filter_map(|i| self.point(i)).collect()
    }
}

/// Byte cursor with Delphi `TReader` value decoding.
struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl Cursor<'_> {
    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ReadError> {
        Err(ReadError { offset: self.p, msg: msg.into() })
    }

    fn take(&mut self, n: usize) -> Result<&[u8], ReadError> {
        if self.b.len() - self.p < n {
            return self.err("unexpected end of file");
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.take(1)?[0])
    }

    fn peek(&self) -> Result<u8, ReadError> {
        self.b.get(self.p).copied().ok_or(ReadError { offset: self.p, msg: "unexpected end of file".into() })
    }

    fn int(&mut self) -> Result<i32, ReadError> {
        let at = self.p;
        match self.u8()? {
            2 => Ok(i32::from(i8::from_le_bytes([self.u8()?]))),
            3 => {
                let s = self.take(2)?;
                Ok(i32::from(i16::from_le_bytes([s[0], s[1]])))
            }
            4 => {
                let s = self.take(4)?;
                Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            }
            t => Err(ReadError { offset: at, msg: format!("expected an integer, found value tag {t:#04x}") }),
        }
    }

    fn bool(&mut self) -> Result<bool, ReadError> {
        let at = self.p;
        match self.u8()? {
            8 => Ok(false),
            9 => Ok(true),
            t => Err(ReadError { offset: at, msg: format!("expected a boolean, found value tag {t:#04x}") }),
        }
    }

    fn string(&mut self) -> Result<String, ReadError> {
        let at = self.p;
        let n = match self.u8()? {
            6 => usize::from(self.u8()?),
            0x0C => {
                let s = self.take(4)?;
                let n = i32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                match usize::try_from(n) {
                    Ok(n) if n <= MAX_STRING => n,
                    _ => return self.err(format!("bad string length {n}")),
                }
            }
            t => return Err(ReadError { offset: at, msg: format!("expected a string, found value tag {t:#04x}") }),
        };
        // Windows-1252 text; bytes 0x80–0x9F are mapped through the code
        // page table, the rest are Latin-1.
        Ok(self.take(n)?.iter().map(|&b| cp1252(b)).collect())
    }

    fn single(&mut self) -> Result<f32, ReadError> {
        let s = self.take(4)?;
        let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        if !v.is_finite() || v.abs() > 1.0e7 {
            return Err(ReadError { offset: self.p - 4, msg: "coordinate is not a finite number in range".into() });
        }
        Ok(v)
    }

    fn list_begin(&mut self) -> Result<(), ReadError> {
        let at = self.p;
        match self.u8()? {
            1 => Ok(()),
            t => Err(ReadError { offset: at, msg: format!("expected a list, found value tag {t:#04x}") }),
        }
    }
}

/// Windows-1252 characters of the bytes 0x80–0x9F (the rest are Latin-1;
/// undefined bytes map to the C1 control of the same value).
pub(crate) const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’',
    '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// Windows-1252 byte to char.
fn cp1252(b: u8) -> char {
    if (0x80..0xA0).contains(&b) { CP1252_HIGH[usize::from(b - 0x80)] } else { char::from(b) }
}

/// An object being read.
struct Frame {
    obj: u32,
    cls: Cls,
    ops: std::rc::Rc<[Op]>,
    pc: usize,
    vals: Vec<(F, Val)>,
    /// Field waiting for the nested object being read.
    waiting: Option<F>,
    /// List items read so far (`TSerializeList`).
    items: Vec<Option<u32>>,
}

/// What reading an object header produced.
enum Start {
    Ref(Option<u32>),
    New(Frame),
}

struct ObjReader<'a, 'b> {
    c: &'b mut Cursor<'a>,
    version: u8,
    classes: Vec<Cls>,
    programs: std::collections::HashMap<Cls, std::rc::Rc<[Op]>>,
    objects: Vec<Obj>,
    max_depth: usize,
}

impl ObjReader<'_, '_> {
    fn start(&mut self) -> Result<Start, ReadError> {
        let at = self.c.p;
        let cls = match self.c.int()? {
            15 => return Ok(Start::Ref(None)),
            2 => {
                let i = self.c.int()?;
                return match usize::try_from(i) {
                    Ok(i) if i < self.objects.len() => Ok(Start::Ref(Some(u32::try_from(i).unwrap_or(u32::MAX)))),
                    _ => Err(ReadError { offset: at, msg: format!("reference to unknown object {i}") }),
                };
            }
            1 => {
                let i = self.c.int()?;
                match usize::try_from(i).ok().and_then(|i| self.classes.get(i)) {
                    Some(&c) => c,
                    None => return Err(ReadError { offset: at, msg: format!("unknown class index {i}") }),
                }
            }
            0 => {
                let name = self.c.string()?;
                let Some(c) = Cls::from_name(&name) else {
                    return Err(ReadError { offset: at, msg: format!("unknown class {:?}", clip(&name, 64)) });
                };
                if self.classes.len() >= MAX_CLASSES {
                    return Err(ReadError { offset: at, msg: "too many classes".into() });
                }
                self.classes.push(c);
                c
            }
            k => return Err(ReadError { offset: at, msg: format!("bad object kind {k}") }),
        };
        if self.objects.len() >= MAX_OBJECTS {
            return Err(ReadError { offset: at, msg: format!("too many objects (limit {MAX_OBJECTS})") });
        }
        let obj = u32::try_from(self.objects.len()).unwrap_or(u32::MAX);
        // Numbered before its fields are read.
        self.objects.push(Obj::Pending(cls));
        let ops = std::rc::Rc::clone(self.programs.entry(cls).or_insert_with(|| program(cls).into()));
        if cls == Cls::SerializeList {
            self.c.list_begin()?;
        }
        Ok(Start::New(Frame { obj, cls, ops, pc: 0, vals: Vec::new(), waiting: None, items: Vec::new() }))
    }

    /// Reads one object (and everything nested in it) with an explicit
    /// stack.
    #[allow(clippy::single_match_else, reason = "the two cases of a frame step read best as a match")]
    fn read(&mut self) -> Result<Option<u32>, ReadError> {
        let mut stack: Vec<Frame> = match self.start()? {
            Start::Ref(r) => return Ok(r),
            Start::New(f) => vec![f],
        };
        let mut result: Option<Option<u32>> = None;
        while let Some(top) = stack.last_mut() {
            if let Some(r) = result.take() {
                if let Some(f) = top.waiting.take() {
                    top.vals.push((f, Val::Ref(r)));
                } else {
                    if top.items.len() >= MAX_LIST {
                        return self.c.err("list too long");
                    }
                    top.items.push(r);
                }
            }
            // Run the frame until it needs a nested object or ends.
            let mut need: Option<Option<F>> = None;
            if top.cls == Cls::SerializeList {
                if self.c.peek()? == 0 {
                    self.c.p += 1;
                } else {
                    need = Some(None);
                }
            } else if top.cls == Cls::FloatPoint {
                let x = self.c.single()?;
                let y = self.c.single()?;
                top.vals.push((F::X, Val::F32(x)));
                top.vals.push((F::Y, Val::F32(y)));
            } else {
                while top.pc < top.ops.len() {
                    let op = top.ops[top.pc];
                    top.pc += 1;
                    let flag = |vals: &[(F, Val)], f: F| vals.iter().any(|(k, v)| *k == f && *v == Val::Bool(true));
                    match op {
                        Op::Int(f) => {
                            let v = self.c.int()?;
                            top.vals.push((f, Val::Int(v)));
                        }
                        Op::Bool(f) => {
                            let v = self.c.bool()?;
                            top.vals.push((f, Val::Bool(v)));
                        }
                        Op::Str(f) => {
                            let v = self.c.string()?;
                            top.vals.push((f, Val::Str(v)));
                        }
                        Op::F32(f) => {
                            let v = self.c.single()?;
                            top.vals.push((f, Val::F32(v)));
                        }
                        Op::IfTrue(f, n) => {
                            if !flag(&top.vals, f) {
                                top.pc += usize::from(n);
                            }
                        }
                        Op::IfFalse(f, n) => {
                            if flag(&top.vals, f) {
                                top.pc += usize::from(n);
                            }
                        }
                        Op::IfV3(n) => {
                            if !matches!(self.version, 0x1E | 0x22) {
                                top.pc += usize::from(n);
                            }
                        }
                        Op::Words(f) => {
                            let n = self.c.int()?;
                            let n = match usize::try_from(n) {
                                // Each word takes at least two bytes.
                                Ok(n) if n <= MAX_MEMORY_WORDS && n.saturating_mul(2) <= self.c.b.len() - self.c.p => n,
                                _ => return self.c.err(format!("bad memory word count {n}")),
                            };
                            let mut w = Vec::with_capacity(n);
                            for _ in 0..n {
                                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                                w.push(self.c.int()? as u16);
                            }
                            top.vals.push((f, Val::Words(w)));
                        }
                        Op::Obj(f) => {
                            need = Some(Some(f));
                            break;
                        }
                    }
                }
            }
            match need {
                Some(field) => match self.start()? {
                    Start::Ref(r) => {
                        let Some(top) = stack.last_mut() else { break };
                        match field {
                            Some(f) => top.vals.push((f, Val::Ref(r))),
                            None => {
                                if top.items.len() >= MAX_LIST {
                                    return self.c.err("list too long");
                                }
                                top.items.push(r);
                            }
                        }
                    }
                    Start::New(child) => {
                        if let Some(top) = stack.last_mut() {
                            top.waiting = field;
                        }
                        if stack.len() >= MAX_STACK {
                            return self.c.err("objects nested too deeply");
                        }
                        stack.push(child);
                        self.max_depth = self.max_depth.max(stack.len());
                    }
                },
                None => {
                    // Frame finished.
                    let Some(done) = stack.pop() else { break };
                    let obj = match done.cls {
                        Cls::SerializeList => Obj::List(done.items),
                        Cls::FloatPoint => {
                            let g = |f| done.vals.iter().find(|(k, _)| *k == f);
                            match (g(F::X), g(F::Y)) {
                                (Some((_, Val::F32(x))), Some((_, Val::F32(y)))) => Obj::Point(*x, *y),
                                _ => Obj::Point(0.0, 0.0),
                            }
                        }
                        c => Obj::Rec(c, done.vals),
                    };
                    self.objects[done.obj as usize] = obj;
                    if stack.is_empty() {
                        return Ok(Some(done.obj));
                    }
                    result = Some(Some(done.obj));
                }
            }
        }
        Ok(None)
    }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Parses a `.dwm` or `.dwt` file (syntax only).
///
/// # Errors
/// Bad magic or version, malformed stream, limits exceeded, trailing bytes.
pub fn parse(bytes: &[u8]) -> Result<Graph, ReadError> {
    if bytes.len() > MAX_FILE {
        return Err(ReadError { offset: 0, msg: format!("file too large (limit {MAX_FILE} bytes)") });
    }
    let mut c = Cursor { b: bytes, p: 0 };
    let magic = c.int().map_err(|_| ReadError { offset: 0, msg: "not a Digital Works file".into() })?;
    #[allow(clippy::cast_sign_loss)]
    let magic = magic as u32;
    let lo = magic & 0xFFFF;
    #[allow(clippy::cast_possible_truncation)]
    let version = (magic >> 16) as u8;
    if magic >> 24 != 0 || (lo != DWM_MAGIC && lo != DWT_MAGIC) {
        return Err(ReadError { offset: 0, msg: "not a Digital Works file".into() });
    }
    let template = lo == DWT_MAGIC;
    if !VERSIONS.contains(&version) {
        return Err(ReadError {
            offset: 0,
            msg: format!("unsupported Digital Works format version {version} (supported: 20, 30, 34)"),
        });
    }
    let mut header = Header { version, template, ..Header::default() };
    if !template {
        match version {
            0x1E => {
                header.v30_flag = c.bool()?;
                header.name = c.string()?;
                let s2 = c.string()?;
                let i = c.int()?;
                header.v30_int = i;
                // The second string is the datasheet path (as property 101
                // of version 34).
                header.datasheet = s2;
                if i != 0 {
                    header.other.push((-3, i.to_string()));
                }
            }
            0x22 => {
                c.list_begin()?;
                let mut n = 0;
                while c.peek()? != 0 {
                    n += 1;
                    if n > MAX_HEADER_PROPS {
                        return c.err("too many header properties");
                    }
                    let kind = c.int()?;
                    let id = c.int()?;
                    let raw = match kind {
                        1 => HeaderValue::Str(c.string()?),
                        2 => HeaderValue::Int(c.int()?),
                        3 => HeaderValue::Bool(c.bool()?),
                        k => return c.err(format!("bad header property kind {k}")),
                    };
                    let value = match &raw {
                        HeaderValue::Str(s) => s.clone(),
                        HeaderValue::Int(i) => i.to_string(),
                        HeaderValue::Bool(b) => b.to_string(),
                    };
                    header.props.push((id, raw));
                    match id {
                        100 => header.name = value,
                        101 => header.datasheet = value,
                        _ => header.other.push((id, value)),
                    }
                }
                c.p += 1;
            }
            _ => {}
        }
    }
    let mut r = ObjReader {
        c: &mut c,
        version,
        classes: Vec::new(),
        programs: std::collections::HashMap::new(),
        objects: Vec::new(),
        max_depth: 1,
    };
    let mut roots = vec![r.read()?];
    if template {
        // A template is a sequence of top-level objects sharing the tables.
        while r.c.p < bytes.len() {
            if roots.len() >= MAX_TEMPLATE_ROOTS {
                return r.c.err("too many template objects");
            }
            roots.push(r.read()?);
        }
    }
    let (objects, max_depth) = (r.objects, r.max_depth);
    if c.p != bytes.len() {
        return c.err(format!("{} trailing bytes", bytes.len() - c.p));
    }
    Ok(Graph { header, objects, roots, max_depth })
}

#[cfg(test)]
mod tests {
    #[test]
    fn corpus_parses() {
        let Some(dir) = std::env::var_os("BITWERK_DWM_CORPUS") else { return };
        let mut stack = vec![std::path::PathBuf::from(dir)];
        let (mut ok, mut bad) = (0, 0);
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dwm") || x.eq_ignore_ascii_case("dwt"))
                {
                    match super::parse(&std::fs::read(&p).unwrap()) {
                        Ok(_) => ok += 1,
                        Err(e) => {
                            bad += 1;
                            eprintln!("FAIL {}: {e}", p.display());
                        }
                    }
                }
            }
        }
        eprintln!("ok {ok} bad {bad}");
        assert_eq!(bad, 0);
    }
}
