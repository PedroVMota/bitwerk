//! Writer of the Digital Works object stream: the exact inverse of
//! [`super::reader`].
//!
//! The writer serialises an object table ([`Graph`]) by running the *same*
//! per-class field programs as the reader ([`super::reader::program`]), so
//! the two cannot drift apart: a field the reader expects is a field the
//! writer emits, in the same order and with the same value encoding.
//!
//! Objects are emitted depth first from the root, as Digital Works does:
//! an object is written in full where it is first reached (object kind 0 =
//! new class, 1 = known class) and numbered *before* its fields, later
//! references are back references (kind 2), nil is kind 15. The table may
//! therefore be in any order and contain references in any direction; the
//! stream never has a forward reference. Integers use the smallest Delphi
//! `TValueType` form, strings `vaString` (≤ 255 bytes, else `vaLString`),
//! coordinates raw float32.
//!
//! The walk is iterative (explicit stack) and every count is checked
//! against the reader's limits, so whatever the writer produces, the reader
//! accepts.

use std::collections::HashMap;
use std::fmt;

use super::reader::{
    CP1252_HIGH, Cls, DWM_MAGIC, F, Graph, HeaderValue, MAX_CLASSES, MAX_LIST, MAX_MEMORY_WORDS, MAX_OBJECTS,
    MAX_STACK, MAX_STRING, Obj, Op, VERSIONS, Val, program,
};

/// Why a graph could not be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteError(pub String);

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn fail<T>(msg: impl Into<String>) -> Result<T, WriteError> {
    Err(WriteError(msg.into()))
}

/// Text in Windows-1252: characters outside the code page become `?`.
/// Returns the bytes and how many characters were replaced.
#[must_use]
pub fn to_cp1252(s: &str) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(s.len());
    let mut bad = 0;
    for ch in s.chars() {
        let b = match u32::from(ch) {
            c @ (0..0x80 | 0xA0..0x100) => u8::try_from(c).ok(),
            _ => CP1252_HIGH.iter().position(|&h| h == ch).and_then(|i| u8::try_from(0x80 + i).ok()),
        };
        if let Some(b) = b {
            out.push(b);
        } else {
            out.push(b'?');
            bad += 1;
        }
    }
    (out, bad)
}

/// Byte sink with Delphi `TWriter` value encodings.
struct Out {
    b: Vec<u8>,
}

impl Out {
    fn int(&mut self, v: i32) {
        if let Ok(x) = i8::try_from(v) {
            self.b.push(2);
            self.b.extend_from_slice(&x.to_le_bytes());
        } else if let Ok(x) = i16::try_from(v) {
            self.b.push(3);
            self.b.extend_from_slice(&x.to_le_bytes());
        } else {
            self.b.push(4);
            self.b.extend_from_slice(&v.to_le_bytes());
        }
    }

    fn bool(&mut self, v: bool) {
        self.b.push(if v { 9 } else { 8 });
    }

    fn string(&mut self, s: &str) -> Result<(), WriteError> {
        let (bytes, _) = to_cp1252(s);
        if bytes.len() > MAX_STRING {
            return fail("string too long");
        }
        if let Ok(n) = u8::try_from(bytes.len()) {
            self.b.push(6);
            self.b.push(n);
        } else {
            let n = i32::try_from(bytes.len()).map_err(|_| WriteError("string too long".into()))?;
            self.b.push(0x0C);
            self.b.extend_from_slice(&n.to_le_bytes());
        }
        self.b.extend_from_slice(&bytes);
        Ok(())
    }

    fn single(&mut self, v: f32) -> Result<(), WriteError> {
        if !v.is_finite() || v.abs() > 1.0e7 {
            return fail("coordinate out of range");
        }
        self.b.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
}

/// An object being written.
struct Frame {
    obj: u32,
    ops: std::rc::Rc<[Op]>,
    pc: usize,
    /// Next list item (`TSerializeList`).
    item: usize,
}

struct ObjWriter<'g> {
    g: &'g Graph,
    out: Out,
    version: u8,
    classes: HashMap<&'static str, i32>,
    programs: HashMap<Cls, std::rc::Rc<[Op]>>,
    /// Stream index of every table object already written.
    index: HashMap<u32, i32>,
    written: i32,
}

impl ObjWriter<'_> {
    #[allow(clippy::unused_self, reason = "reads like the other field accessors")]
    fn field<'a>(&self, o: &'a Obj, f: F) -> Result<&'a Val, WriteError> {
        match o {
            Obj::Rec(_, vals) => vals
                .iter()
                .find(|(k, _)| *k == f)
                .map(|(_, v)| v)
                .ok_or_else(|| WriteError(format!("{} without field {f:?}", o.cls().name()))),
            _ => fail(format!("{} has no fields", o.cls().name())),
        }
    }

    fn flag(&self, o: &Obj, f: F) -> Result<bool, WriteError> {
        match self.field(o, f)? {
            Val::Bool(b) => Ok(*b),
            _ => fail(format!("{} field {f:?} is not a boolean", o.cls().name())),
        }
    }

    /// Writes a reference: nil, a back reference, or the header of a new
    /// object (then returns its frame).
    fn reference(&mut self, r: Option<u32>) -> Result<Option<Frame>, WriteError> {
        let Some(r) = r else {
            self.out.int(15);
            return Ok(None);
        };
        if let Some(&i) = self.index.get(&r) {
            self.out.int(2);
            self.out.int(i);
            return Ok(None);
        }
        let o = self.g.get(r).ok_or_else(|| WriteError(format!("reference to missing object {r}")))?;
        let cls = match o {
            Obj::Pending(_) => return fail("unfinished object in the table"),
            _ => o.cls(),
        };
        let name = cls.name();
        if let Some(&ci) = self.classes.get(name) {
            self.out.int(1);
            self.out.int(ci);
        } else {
            if self.classes.len() >= MAX_CLASSES {
                return fail("too many classes");
            }
            let ci = i32::try_from(self.classes.len()).map_err(|_| WriteError("too many classes".into()))?;
            self.classes.insert(name, ci);
            self.out.int(0);
            self.out.string(name)?;
        }
        if usize::try_from(self.written).unwrap_or(usize::MAX) >= MAX_OBJECTS {
            return fail(format!("too many objects for a Digital Works file (limit {MAX_OBJECTS})"));
        }
        self.index.insert(r, self.written);
        self.written += 1;
        if cls == Cls::SerializeList {
            self.out.b.push(1);
        }
        let ops = std::rc::Rc::clone(self.programs.entry(cls).or_insert_with(|| program(cls).into()));
        Ok(Some(Frame { obj: r, ops, pc: 0, item: 0 }))
    }

    /// Writes one object and everything first reached from it.
    fn write(&mut self, root: Option<u32>) -> Result<(), WriteError> {
        let Some(first) = self.reference(root)? else { return Ok(()) };
        let mut stack = vec![first];
        while let Some(top) = stack.last_mut() {
            let g = self.g;
            let o = g.get(top.obj).ok_or_else(|| WriteError("missing object".into()))?;
            let mut next: Option<Option<u32>> = None;
            match o {
                Obj::List(items) => {
                    if top.item > MAX_LIST {
                        return fail("list too long");
                    }
                    if let Some(&it) = items.get(top.item) {
                        top.item += 1;
                        next = Some(it);
                    } else {
                        self.out.b.push(0);
                    }
                }
                Obj::Point(x, y) => {
                    self.out.single(*x)?;
                    self.out.single(*y)?;
                }
                Obj::Rec(..) => {
                    while top.pc < top.ops.len() {
                        let op = top.ops[top.pc];
                        top.pc += 1;
                        match op {
                            Op::Int(f) => match self.field(o, f)? {
                                Val::Int(v) => self.out.int(*v),
                                _ => return fail(format!("{} field {f:?} is not an integer", o.cls().name())),
                            },
                            Op::Bool(f) => {
                                let v = self.flag(o, f)?;
                                self.out.bool(v);
                            }
                            Op::Str(f) => match self.field(o, f)? {
                                Val::Str(s) => self.out.string(s)?,
                                _ => return fail(format!("{} field {f:?} is not a string", o.cls().name())),
                            },
                            Op::F32(f) => match self.field(o, f)? {
                                Val::F32(v) => self.out.single(*v)?,
                                _ => return fail(format!("{} field {f:?} is not a number", o.cls().name())),
                            },
                            Op::IfTrue(f, n) => {
                                if !self.flag(o, f)? {
                                    top.pc += usize::from(n);
                                }
                            }
                            Op::IfFalse(f, n) => {
                                if self.flag(o, f)? {
                                    top.pc += usize::from(n);
                                }
                            }
                            Op::IfV3(n) => {
                                if !matches!(self.version, 0x1E | 0x22) {
                                    top.pc += usize::from(n);
                                }
                            }
                            Op::Words(f) => match self.field(o, f)? {
                                Val::Words(w) => {
                                    if w.len() > MAX_MEMORY_WORDS {
                                        return fail("memory too large");
                                    }
                                    self.out.int(i32::try_from(w.len()).unwrap_or(i32::MAX));
                                    for &x in w {
                                        self.out.int(i32::from(x));
                                    }
                                }
                                _ => return fail(format!("{} field {f:?} is not a word list", o.cls().name())),
                            },
                            Op::Obj(f) => match self.field(o, f)? {
                                Val::Ref(r) => {
                                    next = Some(*r);
                                    break;
                                }
                                _ => return fail(format!("{} field {f:?} is not a reference", o.cls().name())),
                            },
                        }
                    }
                }
                Obj::Pending(_) => return fail("unfinished object in the table"),
            }
            match next {
                Some(r) => {
                    if let Some(child) = self.reference(r)? {
                        if stack.len() >= MAX_STACK {
                            return fail("objects nested too deeply");
                        }
                        stack.push(child);
                    }
                }
                None => {
                    stack.pop();
                }
            }
        }
        Ok(())
    }
}

/// Serialises a graph: magic, the header of its version, the root object
/// (a template: every root in turn).
///
/// # Errors
/// An unsupported version, a malformed table (missing fields, references
/// to missing objects) or limits exceeded.
pub fn write(g: &Graph) -> Result<Vec<u8>, WriteError> {
    let h = &g.header;
    if !VERSIONS.contains(&h.version) {
        return fail(format!("unsupported format version {}", h.version));
    }
    let lo = if h.template { super::reader::DWT_MAGIC } else { DWM_MAGIC };
    let magic = (u32::from(h.version) << 16) | lo;
    let mut out = Out { b: Vec::new() };
    out.int(i32::try_from(magic).map_err(|_| WriteError("bad magic".into()))?);
    if !h.template {
        match h.version {
            0x1E => {
                out.bool(h.v30_flag);
                out.string(&h.name)?;
                out.string(&h.datasheet)?;
                out.int(h.v30_int);
            }
            0x22 => {
                out.b.push(1);
                for (id, v) in &h.props {
                    match v {
                        HeaderValue::Str(s) => {
                            out.int(1);
                            out.int(*id);
                            out.string(s)?;
                        }
                        HeaderValue::Int(i) => {
                            out.int(2);
                            out.int(*id);
                            out.int(*i);
                        }
                        HeaderValue::Bool(b) => {
                            out.int(3);
                            out.int(*id);
                            out.bool(*b);
                        }
                    }
                }
                out.b.push(0);
            }
            _ => {}
        }
    }
    let mut w = ObjWriter {
        g,
        out,
        version: h.version,
        classes: HashMap::new(),
        programs: HashMap::new(),
        index: HashMap::new(),
        written: 0,
    };
    if g.roots.is_empty() {
        return fail("no root object");
    }
    for &r in &g.roots {
        w.write(r)?;
        if !h.template {
            break;
        }
    }
    if w.out.b.len() > super::reader::MAX_FILE {
        return fail("file too large");
    }
    Ok(w.out.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_use_the_smallest_form() {
        let mut o = Out { b: Vec::new() };
        for v in [0, -128, 127, 128, -129, 32767, 32768, -32769, i32::MIN] {
            o.int(v);
        }
        assert_eq!(
            o.b,
            [
                2, 0, 2, 0x80, 2, 0x7f, 3, 0x80, 0, 3, 0x7f, 0xff, 3, 0xff, 0x7f, 4, 0, 0x80, 0, 0, 4, 0xff, 0x7f,
                0xff, 0xff, 4, 0, 0, 0, 0x80
            ]
        );
    }

    #[test]
    fn cp1252_round_trip() {
        let (b, bad) = to_cp1252("Grüße € – ✓");
        assert_eq!(bad, 1);
        let back: String = b
            .iter()
            .map(|&x| if (0x80..0xA0).contains(&x) { CP1252_HIGH[usize::from(x - 0x80)] } else { char::from(x) })
            .collect();
        assert_eq!(back, "Grüße € – ?");
    }

    /// Every file of the corpus is rewritten byte for byte.
    #[test]
    fn corpus_rewrites_byte_identical() {
        let Some(dir) = std::env::var_os("BITWERK_DWM_CORPUS") else { return };
        let mut stack = vec![std::path::PathBuf::from(dir)];
        let (mut same, mut differ) = (0, 0);
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dwm") || x.eq_ignore_ascii_case("dwt"))
                {
                    let bytes = std::fs::read(&p).unwrap();
                    let g = super::super::reader::parse(&bytes).unwrap();
                    let out = write(&g).unwrap();
                    if out == bytes {
                        same += 1;
                    } else {
                        differ += 1;
                        let at = out.iter().zip(&bytes).position(|(a, b)| a != b).unwrap_or(out.len().min(bytes.len()));
                        eprintln!("DIFF {} at byte {at:#x} ({} vs {} bytes)", p.display(), out.len(), bytes.len());
                    }
                }
            }
        }
        eprintln!("byte-identical rewrites: {same}, different: {differ}");
        assert_eq!(differ, 0);
    }
}
