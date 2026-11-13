//! Export of bitwerk circuits and macros as Digital Works 3 (`.dwm`,
//! format version 34) object graphs; [`super::writer`] turns the graph
//! into bytes.
//!
//! The mapping is the inverse of [`super::convert`]:
//!
//! * coordinates are grid units × 5 (the default Digital Works grid), as
//!   float32 pixels;
//! * Digital Works stores no rotation. Devices it draws with a stored
//!   drawing in real files (gates, NOT, tri-state, memories, numeric
//!   outputs, tags, macros) get a pre-rotated drawing of our symbol and pin
//!   outlines exactly on our pins. Devices it always draws itself
//!   (flip-flops, switches, clocks, sequence generators, power / ground,
//!   LEDs, 7-segment displays) keep Digital Works' own unrotated symbol
//!   (`x`, `y` only); their pins land on ours where possible, otherwise a
//!   short wire (*pin stub*) joins them;
//! * the wire graph becomes wires (polylines between pins and solders);
//!   a junction with `n` connections becomes a chain of `n − 2` solders
//!   (three wires each, as in every real file), a pin with several wires a
//!   zero-length wire to such a chain;
//! * macro instances become embedded copies of the definition (a
//!   `TMacro` with its outer `TMacroPin`s, each paired with the inner pin
//!   of a `TTagDevice`), in DIL pin order for DIL packages;
//! * what Digital Works cannot represent is approximated (wide gates as
//!   trees of 2–4-input gates, other flip-flop triggers with helper gates,
//!   …) or dropped, and every case is counted in the report.

#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::single_match_else,
    reason = "geometry code: short coordinate names, grid units (small integers) converted to float pixels; \
              one mapping per case reads best as a match"
)]

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use bw_core::{
    Circuit, Color, Component, ComponentId, ComponentKind, EvalMode, FfTrigger, FlipFlopType, GateOp, MacroId,
    MacroShape, MacroTable, MemoryType, Note, PinRef, Rotation, SwitchMode, TagDir,
};

use super::Severity;
use super::convert::{CLOCK_HALF_PERIOD, Notes};
use super::reader::{Cls, F, Graph, Header, HeaderValue, MAX_OBJECTS, Obj, Val};

/// Format version written (Digital Works 3.x).
pub const EXPORT_VERSION: u8 = 0x22;
/// Pixels per grid unit.
const PX: f32 = 5.0;
const WHITE: i32 = 0x00FF_FFFF;
/// Brush of a tag's connection pin in real files.
const TAG_PIN_BRUSH: i32 = 0x00EE_FAFF;
/// Colour of the suppressed digits of a numeric output in real files.
const DIM: i32 = 0x0080_8080;

/// Display order of the export counts.
pub const EXPORT_COUNT_ORDER: &[&str] = &[
    "gates",
    "NOT gates",
    "tri-state buffers",
    "flip-flops",
    "memories",
    "switches",
    "clocks",
    "sequence generators",
    "power / ground",
    "LEDs",
    "7-segment displays",
    "numeric outputs",
    "text annotations",
    "macro tags",
    "macro instances",
    "helper gates",
    "wires",
    "junctions",
    "pin stubs",
];

/// Export failure (nothing is written).
#[derive(Debug, Clone)]
pub struct ExportError(pub String);

type R<T> = Result<T, ExportError>;

fn fail<T>(msg: impl Into<String>) -> R<T> {
    Err(ExportError(msg.into()))
}

fn rf(o: u32) -> Val {
    Val::Ref(Some(o))
}

const NIL: Val = Val::Ref(None);

/// Palette colour as a Digital Works `TColor` (0x00BBGGRR).
fn tcolor(c: Color) -> i32 {
    let [r, g, b] = c.rgb();
    i32::from(r) | (i32::from(g) << 8) | (i32::from(b) << 16)
}

type P = (f32, f32);

// ------------------------------------------------------------ graph build

/// Object table under construction.
#[derive(Default)]
struct Gb {
    objs: Vec<Obj>,
}

/// A text to draw: box corners (pixels), style.
struct Txt<'a> {
    text: &'a str,
    lo: P,
    hi: P,
    size: i32,
    color: i32,
    bold: bool,
    italic: bool,
    angle: f32,
    overbar: bool,
}

impl Gb {
    fn push(&mut self, o: Obj) -> u32 {
        let i = u32::try_from(self.objs.len()).unwrap_or(u32::MAX);
        self.objs.push(o);
        i
    }

    fn rec(&mut self, c: Cls, v: Vec<(F, Val)>) -> u32 {
        self.push(Obj::Rec(c, v))
    }

    fn list(&mut self, items: &[u32]) -> u32 {
        self.push(Obj::List(items.iter().map(|&i| Some(i)).collect()))
    }

    fn glist(&mut self, items: &[u32]) -> u32 {
        let l = self.list(items);
        self.rec(Cls::GraphicList, vec![(F::Children, rf(l))])
    }

    fn points(&mut self, pts: &[P]) -> u32 {
        let ids: Vec<u32> = pts.iter().map(|&(x, y)| self.push(Obj::Point(x, y))).collect();
        self.list(&ids)
    }

    fn set(&mut self, o: u32, f: F, v: Val) {
        if let Some(Obj::Rec(_, vals)) = self.objs.get_mut(o as usize) {
            if let Some(slot) = vals.iter_mut().find(|(k, _)| *k == f) {
                slot.1 = v;
            } else {
                vals.push((f, v));
            }
        }
    }

    fn polygon(&mut self, pts: &[P], brush: i32) -> u32 {
        let p = self.points(pts);
        self.rec(
            Cls::Polygon,
            vec![
                (F::PenColor, Val::Int(0)),
                (F::PenWidth, Val::Int(1)),
                (F::Points, rf(p)),
                (F::Brush, Val::Int(brush)),
            ],
        )
    }

    fn polyline(&mut self, pts: &[P]) -> u32 {
        let p = self.points(pts);
        self.rec(Cls::Polyline, vec![(F::PenColor, Val::Int(0)), (F::PenWidth, Val::Int(1)), (F::Points, rf(p))])
    }

    fn text(&mut self, t: &Txt<'_>) -> u32 {
        let (x0, y0, x1, y1) = (t.lo.0, t.lo.1, t.hi.0, t.hi.1);
        let p = self.points(&[(x0, y0), (x0, y1), (x1, y1), (x1, y0)]);
        let mut v = vec![
            (F::PenColor, Val::Int(0)),
            (F::PenWidth, Val::Int(1)),
            (F::Points, rf(p)),
            (F::Brush, Val::Int(WHITE)),
            (F::FontSize, Val::Int(t.size)),
            (F::FontColor, Val::Int(t.color)),
            (F::Italic, Val::Bool(t.italic)),
            (F::FontWeight, Val::Int(if t.bold { 700 } else { 400 })),
            (F::Angle, Val::F32(t.angle)),
            (F::Text, Val::Str(t.text.to_string())),
            (F::Flag94, Val::Bool(true)),
            (F::HasOverbar, Val::Bool(t.overbar)),
        ];
        if t.overbar {
            let ob = self.polyline(&[(x0, y0), (x1, y0)]);
            v.push((F::Overbar, rf(ob)));
        }
        self.rec(Cls::GraphicText, v)
    }

    /// A pin outline (`half` px around `at`), unwired.
    fn pin(&mut self, cls: Cls, at: P, half: f32, kind: i32, brush: i32) -> u32 {
        let (x, y) = at;
        let p = self.points(&[(x + half, y + half), (x + half, y - half), (x - half, y - half), (x - half, y + half)]);
        let mut v = vec![
            (F::PenColor, Val::Int(0)),
            (F::PenWidth, Val::Int(1)),
            (F::Points, rf(p)),
            (F::Brush, Val::Int(brush)),
            (F::PinKind, Val::Int(kind)),
            (F::Wire, NIL),
        ];
        if cls == Cls::MacroPin {
            v.push((F::Partner, NIL));
        }
        self.rec(cls, v)
    }
}

/// The record of a device: the common `TDevice` fields, then `extra`.
fn device_rec(
    gb: &mut Gb,
    cls: Cls,
    children: Option<u32>,
    flag1c: bool,
    pins: &[u32],
    centre: P,
    extra: Vec<(F, Val)>,
) -> u32 {
    let pl = gb.glist(pins);
    let mut v = vec![(F::HasGraphics, Val::Bool(children.is_some()))];
    if let Some(ch) = children {
        v.push((F::Children, rf(ch)));
    }
    v.push((F::Interactive, Val::Bool(cls == Cls::InteractiveInput)));
    v.push((F::Flag1c, Val::Bool(flag1c)));
    v.push((F::Pins, rf(pl)));
    if children.is_none() {
        v.push((F::X, Val::F32(centre.0)));
        v.push((F::Y, Val::F32(centre.1)));
    }
    v.extend(extra);
    gb.rec(cls, v)
}

// ---------------------------------------------------------------- drawing

/// A drawing primitive in a device's local frame (grid units).
enum Gx {
    Poly(Vec<P>, i32),
    Line(Vec<P>),
    /// Text centred at a point: text, size (pt), colour.
    Text(P, String, i32, i32),
}

fn ellipse(c: P, rx: f32, ry: f32, a0: f32, a1: f32, n: usize) -> Vec<P> {
    (0..=n)
        .map(|k| {
            let a = a0 + (a1 - a0) * k as f32 / n as f32;
            (c.0 + rx * a.cos(), c.1 + ry * a.sin())
        })
        .collect()
}

/// A gate symbol in a `w` × `h` box (operation as in [`Cls::Gate`]: 0 And,
/// 1 Nand, 2 Or, 3 Nor, 4 Xor, 5 Xnor, 6 Not).
fn gate_gx(op: u8, w: f32, h: f32) -> Vec<Gx> {
    use std::f32::consts::{FRAC_PI_2, PI};
    let bubble = matches!(op, 1 | 3 | 5 | 6);
    let r = (h / 4.0).min(0.35);
    let right = if bubble { w - 2.0 * r } else { w };
    let mut v = Vec::new();
    match op {
        0 | 1 => {
            let mut p = vec![(0.0, h), (0.0, 0.0)];
            p.extend(ellipse((right / 2.0, h / 2.0), right / 2.0, h / 2.0, -FRAC_PI_2, FRAC_PI_2, 12));
            v.push(Gx::Poly(p, WHITE));
        }
        2..=5 => {
            let x0 = if op >= 4 { 0.5 } else { 0.0 };
            let bulge = (right - x0) * 0.2;
            let mut p: Vec<P> =
                (0..=8).map(|k| (x0 + bulge * (PI * k as f32 / 8.0).sin(), h * k as f32 / 8.0)).collect();
            p.reverse();
            // Upper and lower front curves to the tip.
            for k in 1..=8 {
                let t = k as f32 / 8.0;
                p.push((x0 + (right - x0) * t, (h / 2.0) * t * t));
            }
            for k in 1..8 {
                let t = 1.0 - k as f32 / 8.0;
                p.push((x0 + (right - x0) * t, h - (h / 2.0) * t * t));
            }
            v.push(Gx::Poly(p, WHITE));
            if op >= 4 {
                v.push(Gx::Line((0..=8).map(|k| (bulge * (PI * k as f32 / 8.0).sin(), h * k as f32 / 8.0)).collect()));
            }
        }
        _ => v.push(Gx::Poly(vec![(0.0, 0.0), (right, h / 2.0), (0.0, h)], WHITE)),
    }
    if bubble {
        v.push(Gx::Poly(ellipse((w - r, h / 2.0), r, r, 0.0, 2.0 * PI, 12), WHITE));
    }
    v
}

/// A labelled box.
fn box_gx(w: f32, h: f32, label: &str) -> Vec<Gx> {
    let mut v = vec![Gx::Poly(vec![(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)], WHITE)];
    if !label.is_empty() {
        v.push(Gx::Text((w / 2.0, h / 2.0), label.to_string(), 7, 0));
    }
    v
}

/// Local frame of a component → pixels (the same transform as
/// [`Component::pin_pos`]).
fn xf(c: &Component, (lx, ly): P) -> P {
    let (w, h) = c.kind.size();
    let (w, h) = (w as f32, h as f32);
    let (rx, ry) = match c.rotation {
        Rotation::R0 => (lx, ly),
        Rotation::R90 => (h - ly, lx),
        Rotation::R180 => (w - lx, h - ly),
        Rotation::R270 => (ly, w - lx),
    };
    ((c.pos.x as f32 + rx) * PX, (c.pos.y as f32 + ry) * PX)
}

fn px(p: bw_core::GridPos) -> P {
    (p.x as f32 * PX, p.y as f32 * PX)
}

/// Pixel position of one of our pins.
fn pin_px(c: &Component, pin: u8) -> P {
    c.pin_pos(pin).map_or_else(|| xf(c, (0.0, 0.0)), px)
}

/// Text box (pixels) of a short label centred at `at`.
fn text_box(at: P, text: &str, size: i32) -> (P, P) {
    let s = size as f32;
    let w = (text.chars().count().max(1) as f32 * s * 0.6).max(4.0);
    let h = s * 1.7;
    ((at.0 - w / 2.0, at.1 - h / 2.0), (at.0 + w / 2.0, at.1 + h / 2.0))
}

/// Emits a drawing through a frame transform; returns the objects.
fn emit(gb: &mut Gb, gx: &[Gx], map: &dyn Fn(P) -> P) -> Vec<u32> {
    gx.iter()
        .map(|g| match g {
            Gx::Poly(p, brush) => {
                let q: Vec<P> = p.iter().map(|&a| map(a)).collect();
                gb.polygon(&q, *brush)
            }
            Gx::Line(p) => {
                let q: Vec<P> = p.iter().map(|&a| map(a)).collect();
                gb.polyline(&q)
            }
            Gx::Text(at, t, size, color) => {
                let (lo, hi) = text_box(map(*at), t, *size);
                gb.text(&Txt {
                    text: t,
                    lo,
                    hi,
                    size: *size,
                    color: *color,
                    bold: false,
                    italic: false,
                    angle: 0.0,
                    overbar: false,
                })
            }
        })
        .collect()
}

// ------------------------------------------------------------ wire graph

struct Vx {
    pos: P,
    /// The pin object of a terminal.
    pin: Option<u32>,
}

struct Ed {
    a: u32,
    b: u32,
    via: Vec<P>,
    live: bool,
}

/// The wiring of one level: terminals (Digital Works pins), free points
/// and polylines between them.
#[derive(Default)]
struct Net {
    v: Vec<Vx>,
    e: Vec<Ed>,
}

impl Net {
    fn vertex(&mut self, pos: P, pin: Option<u32>) -> u32 {
        let i = u32::try_from(self.v.len()).unwrap_or(u32::MAX);
        self.v.push(Vx { pos, pin });
        i
    }

    /// An orthogonal edge (an L corner, horizontal first, if needed).
    fn edge(&mut self, a: u32, b: u32) {
        let (pa, pb) = (self.v[a as usize].pos, self.v[b as usize].pos);
        let via =
            if (pa.0 - pb.0).abs() > 1e-3 && (pa.1 - pb.1).abs() > 1e-3 { vec![(pb.0, pa.1)] } else { Vec::new() };
        self.e.push(Ed { a, b, via, live: true });
    }

    /// Turns the graph into `TWire` / `TSolder` objects; returns (solders,
    /// wires).
    fn finish(mut self, gb: &mut Gb) -> (Vec<u32>, Vec<u32>) {
        let n = self.v.len();
        let is_pin = |v: &[Vx], x: usize| v[x].pin.is_some();
        let mut paths: Vec<(usize, usize, Vec<P>)>;
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
            for (i, e) in self.e.iter_mut().enumerate() {
                if !e.live {
                    continue;
                }
                if e.a == e.b {
                    e.live = false;
                    continue;
                }
                adj[e.a as usize].push(i);
                adj[e.b as usize].push(i);
            }
            let mut deg: Vec<usize> = adj.iter().map(Vec::len).collect();
            // Dangling free ends carry nothing: prune them.
            let mut work: Vec<usize> = (0..n).filter(|&x| !is_pin(&self.v, x) && deg[x] == 1).collect();
            while let Some(x) = work.pop() {
                if is_pin(&self.v, x) || deg[x] != 1 {
                    continue;
                }
                if let Some(&ei) = adj[x].iter().find(|&&ei| self.e[ei].live) {
                    self.e[ei].live = false;
                    let o = if self.e[ei].a as usize == x { self.e[ei].b } else { self.e[ei].a } as usize;
                    deg[x] -= 1;
                    deg[o] -= 1;
                    if !is_pin(&self.v, o) && deg[o] == 1 {
                        work.push(o);
                    }
                }
            }
            let anchor = |x: usize| deg[x] > 0 && (self.v[x].pin.is_some() || deg[x] != 2);
            let mut used = vec![false; self.e.len()];
            paths = Vec::new();
            let mut loops: Vec<usize> = Vec::new();
            for a in 0..n {
                if !anchor(a) {
                    continue;
                }
                for &first in &adj[a] {
                    if !self.e[first].live || used[first] {
                        continue;
                    }
                    let mut pts = vec![self.v[a].pos];
                    let mut edges = Vec::new();
                    let (mut cur, mut ei) = (a, first);
                    let end = loop {
                        used[ei] = true;
                        edges.push(ei);
                        let ed = &self.e[ei];
                        let nx = if ed.a as usize == cur { ed.b as usize } else { ed.a as usize };
                        if ed.a as usize == cur {
                            pts.extend(ed.via.iter().copied());
                        } else {
                            pts.extend(ed.via.iter().rev().copied());
                        }
                        pts.push(self.v[nx].pos);
                        if anchor(nx) {
                            break nx;
                        }
                        match adj[nx].iter().copied().find(|&k| self.e[k].live && !used[k]) {
                            Some(k) => {
                                cur = nx;
                                ei = k;
                            }
                            None => break nx,
                        }
                    };
                    if end == a {
                        loops.extend(edges);
                    } else {
                        // Canonical direction (from the lower end point),
                        // so equal wiring always gives equal wires.
                        let key = |p: P| (p.0, p.1);
                        let (f, l) = (key(pts[0]), key(pts[pts.len() - 1]));
                        if f.0.total_cmp(&l.0).then(f.1.total_cmp(&l.1)).is_gt() {
                            pts.reverse();
                            paths.push((end, a, pts));
                        } else {
                            paths.push((a, end, pts));
                        }
                    }
                }
            }
            // A path from a junction back to itself is redundant: drop it
            // and look again (degrees changed).
            if loops.is_empty() || rounds > 64 {
                break;
            }
            for ei in loops {
                self.e[ei].live = false;
            }
        }

        // Canonical order (geometry first), so the same wiring always gives
        // the same file whatever order its nodes were created in.
        let cmp_pts = |a: &[P], b: &[P]| {
            a.iter()
                .zip(b)
                .map(|(p, q)| p.0.total_cmp(&q.0).then(p.1.total_cmp(&q.1)))
                .find(|o| o.is_ne())
                .unwrap_or_else(|| a.len().cmp(&b.len()))
        };
        paths.sort_by(|x, y| cmp_pts(&x.2, &y.2));
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&x, &y| {
            let (p, q) = (self.v[x].pos, self.v[y].pos);
            p.0.total_cmp(&q.0).then(p.1.total_cmp(&q.1)).then(self.v[x].pin.cmp(&self.v[y].pin))
        });
        let mut ends = vec![0usize; n];
        for (a, b, _) in &paths {
            ends[*a] += 1;
            ends[*b] += 1;
        }
        let mut solders: Vec<u32> = Vec::new();
        let mut wires: Vec<u32> = Vec::new();
        let mut solder_wires: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut slots: Vec<Vec<u32>> = vec![Vec::new(); n];
        let attach = |gb: &mut Gb, sw: &mut HashMap<u32, Vec<u32>>, end: Option<u32>, w: u32| {
            if let Some(o) = end {
                if let Some(l) = sw.get_mut(&o) {
                    l.push(w);
                } else {
                    gb.set(o, F::Wire, rf(w));
                }
            }
        };
        let wire = |gb: &mut Gb, e1: Option<u32>, e2: Option<u32>, pts: &[P]| -> u32 {
            let mut q = simplify(pts);
            if q.len() < 2 {
                let p = q.first().copied().unwrap_or((0.0, 0.0));
                q = vec![p, p];
            }
            let pl = gb.points(&q);
            gb.rec(
                Cls::Wire,
                vec![
                    (F::PenColor, Val::Int(0)),
                    (F::PenWidth, Val::Int(1)),
                    (F::Points, rf(pl)),
                    (F::Unk3c, Val::Int(0)),
                    (F::End1, e1.map_or(NIL, rf)),
                    (F::End2, e2.map_or(NIL, rf)),
                ],
            )
        };
        // A chain of m − 2 solders for m connections: the slot list.
        let chain = |gb: &mut Gb,
                     sw: &mut HashMap<u32, Vec<u32>>,
                     solders: &mut Vec<u32>,
                     wires: &mut Vec<u32>,
                     pos: P,
                     m: usize|
         -> Vec<u32> {
            let k = m - 2;
            let ss: Vec<u32> = (0..k)
                .map(|_| {
                    let s = gb.rec(
                        Cls::Solder,
                        vec![
                            (F::X, Val::F32(pos.0)),
                            (F::Y, Val::F32(pos.1)),
                            (F::Wire1, NIL),
                            (F::Wire2, NIL),
                            (F::Wire3, NIL),
                        ],
                    );
                    sw.insert(s, Vec::new());
                    solders.push(s);
                    s
                })
                .collect();
            for i in 1..k {
                let w = wire(gb, Some(ss[i - 1]), Some(ss[i]), &[pos, pos]);
                wires.push(w);
                if let Some(l) = sw.get_mut(&ss[i - 1]) {
                    l.push(w);
                }
                if let Some(l) = sw.get_mut(&ss[i]) {
                    l.push(w);
                }
            }
            if k == 1 {
                return vec![ss[0]; 3];
            }
            let mut out = vec![ss[0], ss[0]];
            out.extend(ss[1..k - 1].iter().copied());
            out.push(ss[k - 1]);
            out.push(ss[k - 1]);
            out
        };
        for &x in &order {
            let m = ends[x];
            if m == 0 {
                continue;
            }
            let pos = self.v[x].pos;
            if let Some(pin) = self.v[x].pin {
                if m == 1 {
                    slots[x] = vec![pin];
                } else {
                    let mut s = chain(gb, &mut solder_wires, &mut solders, &mut wires, pos, m + 1);
                    let first = s.remove(0);
                    let w = wire(gb, Some(pin), Some(first), &[pos, pos]);
                    wires.push(w);
                    attach(gb, &mut solder_wires, Some(pin), w);
                    attach(gb, &mut solder_wires, Some(first), w);
                    slots[x] = s;
                }
            } else if m >= 3 {
                slots[x] = chain(gb, &mut solder_wires, &mut solders, &mut wires, pos, m);
            }
        }
        for x in &mut slots {
            x.reverse();
        }
        for (a, b, pts) in &paths {
            let e1 = slots[*a].pop();
            let e2 = slots[*b].pop();
            let w = wire(gb, e1, e2, pts);
            wires.push(w);
            attach(gb, &mut solder_wires, e1, w);
            attach(gb, &mut solder_wires, e2, w);
        }
        for &s in &solders {
            let l = solder_wires.get(&s).cloned().unwrap_or_default();
            for (k, f) in [F::Wire1, F::Wire2, F::Wire3].into_iter().enumerate() {
                gb.set(s, f, l.get(k).map_or(NIL, |&w| rf(w)));
            }
        }
        (solders, wires)
    }
}

/// A wire's polyline in normal form: repeated points and the loops
/// between two visits of the same point dropped (a stub that runs out and
/// back), and collinear middle points removed. Keeps both ends.
fn simplify(pts: &[P]) -> Vec<P> {
    let same = |a: P, b: P| (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3;
    let mut q: Vec<P> = Vec::with_capacity(pts.len());
    for &p in pts {
        if let Some(i) = q.iter().position(|&x| same(x, p)) {
            q.truncate(i + 1);
        } else {
            q.push(p);
        }
    }
    let mut k = 1;
    while k + 1 < q.len() {
        let (a, b, c) = (q[k - 1], q[k], q[k + 1]);
        let vertical = (a.0 - b.0).abs() < 1e-3 && (b.0 - c.0).abs() < 1e-3;
        let horizontal = (a.1 - b.1).abs() < 1e-3 && (b.1 - c.1).abs() < 1e-3;
        if vertical || horizontal {
            q.remove(k);
        } else {
            k += 1;
        }
    }
    q
}

// ----------------------------------------------------------------- levels

/// A signal feeding a helper: one of our pins or a net vertex.
#[derive(Clone, Copy)]
enum Sig {
    Pin(PinRef),
    V(u32),
}

/// Which list of the macro a device goes to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum List {
    Devices,
    Inputs,
    Outputs,
}

/// One macro level being exported.
#[derive(Default)]
struct Level {
    net: Net,
    /// Digital Works terminals (net vertices) of each of our pins.
    pin_terms: HashMap<PinRef, Vec<u32>>,
    devices: Vec<u32>,
    inputs: Vec<u32>,
    outputs: Vec<u32>,
    tags: Vec<u32>,
    /// Inner `TMacroPin` of each tag.
    tag_pins: HashMap<ComponentId, u32>,
}

impl Level {
    fn put(&mut self, list: List, o: u32) {
        match list {
            List::Devices => self.devices.push(o),
            List::Inputs => self.inputs.push(o),
            List::Outputs => self.outputs.push(o),
        }
    }

    /// A Digital Works pin as a terminal of the net, optionally standing
    /// for one of our pins.
    fn term(&mut self, gb: &mut Gb, cls: Cls, at: P, half: f32, kind: i32, ours: Option<PinRef>) -> u32 {
        let brush = if cls == Cls::Pin && kind == 0 { TAG_PIN_BRUSH } else { WHITE };
        let o = gb.pin(cls, at, half, kind, brush);
        let v = self.net.vertex(at, Some(o));
        if let Some(p) = ours {
            self.pin_terms.entry(p).or_default().push(v);
        }
        o
    }

    /// The net vertex of the last terminal created.
    fn last_vertex(&self) -> u32 {
        u32::try_from(self.net.v.len().saturating_sub(1)).unwrap_or(0)
    }
}

/// The exported implementation of one definition, copied into every
/// instance.
struct Template {
    objs: Vec<Obj>,
    /// devices, tags, inputs, outputs, solders, wires.
    lists: [u32; 6],
    tag_pins: HashMap<ComponentId, u32>,
}

/// The whole export of one file.
pub struct Exporter<'t> {
    table: &'t MacroTable,
    pub notes: Notes,
    templates: HashMap<MacroId, Rc<Template>>,
    busy: HashSet<MacroId>,
    pub copies: usize,
    noted_defs: HashSet<MacroId>,
}

/// Instance pin index of each outer pin slot, in Digital Works order (DIL
/// pin number − 1, or interface order).
fn outer_order(shape: &MacroShape) -> Vec<Option<u8>> {
    let pins = shape.inputs.saturating_add(shape.outputs);
    if shape.dil > 0 {
        (1..=shape.dil)
            .map(|num| (0..pins).find(|&i| shape.dil_pins.get(usize::from(i)).copied() == Some(num)))
            .collect()
    } else {
        (0..pins).map(Some).collect()
    }
}

/// Unrotated offset of DIL pin `num` of a DIL instance.
fn dil_offset(shape: &MacroShape, num: u8) -> P {
    let n = i32::from(num);
    let half = i32::from(shape.dil) / 2;
    let (w, _) = ComponentKind::Macro(*shape).size();
    let (x, y) = if n <= half { (0, 2 * n - 1) } else { (w, 2 * (i32::from(shape.dil) - n) + 1) };
    (x as f32, y as f32)
}

/// Datasheet path for Digital Works from our datasheet text.
fn dw_datasheet(text: &str) -> (String, bool) {
    let mut path = String::new();
    let mut other = false;
    for line in text.lines() {
        let l = line.trim();
        if let Some(p) = l.strip_prefix("Original datasheet file:") {
            path = p.trim().to_string();
        } else if !l.is_empty() && l != "Imported from Digital Works." {
            other = true;
        }
    }
    (path, other)
}

/// Font size (pt) of a note height (tenths of a grid unit); the inverse of
/// the importer's conversion.
fn font_size(height: u8) -> i32 {
    let s = (f32::from(height) / 10.0 * PX / (96.0 / 72.0 * 0.7)).round();
    #[allow(clippy::cast_possible_truncation)]
    let s = s as i32;
    s.clamp(4, 72)
}

impl<'t> Exporter<'t> {
    #[must_use]
    pub fn new(table: &'t MacroTable) -> Self {
        Self {
            table,
            notes: Notes::default(),
            templates: HashMap::new(),
            busy: HashSet::new(),
            copies: 0,
            noted_defs: HashSet::new(),
        }
    }

    /// Distinct definitions exported.
    #[must_use]
    pub fn definitions(&self) -> usize {
        self.templates.len()
    }

    fn approx(&mut self, msg: impl Into<String>) {
        self.notes.add(Severity::Approximation, msg);
    }

    /// Exports one circuit level into `gb`; `numbers` gives each tag's
    /// pin number (outer pin index + 1).
    fn level(
        &mut self,
        gb: &mut Gb,
        c: &Circuit,
        numbers: &HashMap<ComponentId, usize>,
        depth: u32,
    ) -> R<([u32; 6], HashMap<ComponentId, u32>)> {
        let mut lv = Level::default();
        let mut links: Vec<(u32, u32)> = Vec::new();
        // A VCC that only ties an RS latch's clock high (what the importer
        // makes of Digital Works' clockless RS bistable) is implied by the
        // bistable: leave it out, so import and export round-trip.
        let mut implied: HashSet<ComponentId> = HashSet::new();
        for comp in c.components() {
            if let ComponentKind::FlipFlop { ty: FlipFlopType::Rs, trigger: FfTrigger::Latch, .. } = comp.kind {
                let clk = PinRef { component: comp.id, pin: FlipFlopType::CLOCK_INPUT };
                let net = c.net_pins(clk);
                if let [a, b] = net.as_slice() {
                    let other = if a.component == comp.id { b } else { a };
                    if c.component(other.component).is_some_and(|k| k.kind == ComponentKind::Const { high: true }) {
                        implied.insert(other.component);
                    }
                }
            }
        }
        for comp in c.components() {
            if implied.contains(&comp.id) {
                continue;
            }
            self.component(gb, &mut lv, &mut links, c, comp, numbers, depth)?;
        }
        // Our wiring: one vertex per node, pin nodes joined to their
        // terminals (directly when they coincide, else by a stub).
        let mut node_v: HashMap<bw_core::NodeId, u32> = HashMap::new();
        let mut stubs = 0usize;
        for (id, node) in c.nodes() {
            let at = px(node.pos);
            let v = match node.pin {
                Some(p) => {
                    let terms = lv.pin_terms.get(&p).cloned().unwrap_or_default();
                    match terms.as_slice() {
                        [t] if lv.net.v[*t as usize].pos == at => *t,
                        _ => {
                            let v = lv.net.vertex(at, None);
                            for t in terms {
                                if lv.net.v[t as usize].pos != at {
                                    stubs += 1;
                                }
                                lv.net.edge(v, t);
                            }
                            v
                        }
                    }
                }
                None => lv.net.vertex(at, None),
            };
            node_v.insert(id, v);
        }
        for (_, s) in c.segments() {
            if let (Some(&a), Some(&b)) = (node_v.get(&s.a), node_v.get(&s.b)) {
                lv.net.edge(a, b);
            }
        }
        for (a, b) in links {
            lv.net.edge(a, b);
        }
        self.notes.count("pin stubs", stubs);
        let Level { net, devices, inputs, outputs, tags, tag_pins, .. } = lv;
        let (solders, wires) = net.finish(gb);
        self.notes.count("wires", wires.len());
        self.notes.count("junctions", solders.len());
        let lists = [
            gb.glist(&devices),
            gb.glist(&tags),
            gb.glist(&inputs),
            gb.glist(&outputs),
            gb.glist(&solders),
            gb.glist(&wires),
        ];
        Ok((lists, tag_pins))
    }

    /// A device Digital Works draws itself: its standard pin offsets
    /// (pixels from the centre); the centre is chosen so that most pins
    /// coincide with ours. `pins`: (our pin, offset, kind, outline half
    /// size).
    #[allow(clippy::too_many_arguments)]
    fn standard(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        c: Option<&Component>,
        cls: Cls,
        pins: &[(Option<PinRef>, P, i32, f32)],
        fallback: P,
        extra: Vec<(F, Val)>,
        list: List,
        label: Option<&str>,
    ) -> (u32, Vec<u32>) {
        let comp_of = |p: PinRef| c.filter(|c| c.id == p.component);
        let ours: Vec<(P, P)> = pins
            .iter()
            .filter_map(|&(o, off, ..)| o.and_then(|p| comp_of(p).map(|c| (pin_px(c, p.pin), off))))
            .collect();
        let mut best: Option<(usize, f32, P)> = None;
        for &(at, off) in &ours {
            let centre = (at.0 - off.0, at.1 - off.1);
            let mut exact = 0;
            let mut dist = 0.0;
            for &(a, o) in &ours {
                let d = (a.0 - centre.0 - o.0).abs() + (a.1 - centre.1 - o.1).abs();
                if d < 1e-3 {
                    exact += 1;
                }
                dist += d;
            }
            if best.is_none_or(|(e, d, _)| exact > e || (exact == e && dist < d - 1e-3)) {
                best = Some((exact, dist, centre));
            }
        }
        let mut centre = best.map_or(fallback, |b| b.2);
        // Unrotated and fully mapped: the centre the importer would place
        // this very component from (the median of the pin offsets), so a
        // re-export of an imported circuit gives the same file.
        if let Some(c) = c.filter(|c| c.rotation == Rotation::R0) {
            let rel: Option<Vec<(P, P)>> = pins
                .iter()
                .map(|&(o, off, ..)| {
                    let p = o.filter(|p| p.component == c.id)?;
                    let q = c.pin_pos(p.pin)?;
                    Some(((q.x - c.pos.x) as f32, (q.y - c.pos.y) as f32)).map(|r| (r, off))
                })
                .collect();
            if let Some(rel) = rel.filter(|r| !r.is_empty()) {
                let median = |f: &dyn Fn(&(P, P)) -> f32| {
                    let mut v: Vec<f32> = rel.iter().map(f).collect();
                    v.sort_by(f32::total_cmp);
                    v[v.len() / 2]
                };
                let mx = median(&|(r, o)| o.0 / PX - r.0);
                let my = median(&|(r, o)| o.1 / PX - r.1);
                centre = ((c.pos.x as f32 - mx) * PX, (c.pos.y as f32 - my) * PX);
            }
        }
        if c.is_some_and(|c| c.rotation != Rotation::R0) {
            self.approx(format!("{} drawn unrotated (Digital Works cannot rotate it)", cls.name()));
        }
        let mut objs = Vec::new();
        let mut verts = Vec::new();
        for &(o, off, kind, half) in pins {
            let at = (centre.0 + off.0, centre.1 + off.1);
            objs.push(lv.term(gb, Cls::Pin, at, half, kind, o));
            verts.push(lv.last_vertex());
        }
        let mut extra = extra;
        if let Some(l) = label {
            // As in real files: a name label left of the device.
            let (lo, hi) = text_box((centre.0 - 20.0 - l.len() as f32 * 2.0, centre.1), l, 7);
            let t = gb.text(&Txt {
                text: l,
                lo,
                hi,
                size: 7,
                color: 0,
                bold: false,
                italic: false,
                angle: 0.0,
                overbar: false,
            });
            extra.splice(0..1, [(F::HasLabel, Val::Bool(true)), (F::TextPos, Val::Int(0)), (F::NameLabel, rf(t))]);
        }
        let d = device_rec(gb, cls, None, false, &objs, centre, extra);
        lv.put(list, d);
        (d, verts)
    }

    /// A device with a stored drawing and pins exactly on ours. `pins`:
    /// (position, kind, our pin).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::unused_self, reason = "kept a method like the other device builders")]
    fn custom(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        cls: Cls,
        drawing: &[u32],
        pins: &[(P, i32, Option<PinRef>)],
        extra: Vec<(F, Val)>,
        list: List,
    ) -> (u32, Vec<u32>) {
        let ch = gb.list(drawing);
        let mut objs = Vec::new();
        let mut verts = Vec::new();
        for &(at, kind, o) in pins {
            objs.push(lv.term(gb, Cls::Pin, at, 4.0, kind, o));
            verts.push(lv.last_vertex());
        }
        let d = device_rec(gb, cls, Some(ch), false, &objs, (0.0, 0.0), extra);
        lv.put(list, d);
        (d, verts)
    }

    /// A helper gate (part of an approximation): drawn as a small gate in
    /// the box spanned by its pins. Returns (output vertex, input vertices).
    fn helper(&mut self, gb: &mut Gb, lv: &mut Level, op: u8, ins: &[P], out: P) -> (u32, Vec<u32>) {
        let n = u8::try_from(ins.len()).unwrap_or(2);
        let cls = if op == 6 { Cls::Gate(6, 1) } else { Cls::Gate(op, n) };
        let x0 = ins.iter().map(|p| p.0).fold(f32::MAX, f32::min);
        let y0 = ins.iter().map(|p| p.1).chain([out.1]).fold(f32::MAX, f32::min) - 2.0;
        let y1 = ins.iter().map(|p| p.1).chain([out.1]).fold(f32::MIN, f32::max) + 2.0;
        let (w, h) = (((out.0 - x0) / PX).max(0.4), ((y1 - y0) / PX).max(0.4));
        let gx = gate_gx(op, w, h);
        let drawing = emit(gb, &gx, &|(lx, ly)| (x0 + lx * PX, y0 + ly * PX));
        let mut pins: Vec<(P, i32, Option<PinRef>)> = vec![(out, 2, None)];
        pins.extend(ins.iter().map(|&p| (p, 1, None)));
        let (_, v) = self.custom(gb, lv, cls, &drawing, &pins, Vec::new(), List::Devices);
        self.notes.count("helper gates", 1);
        (v[0], v[1..].to_vec())
    }

    #[allow(clippy::too_many_arguments)]
    fn component(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        links: &mut Vec<(u32, u32)>,
        circ: &Circuit,
        c: &Component,
        numbers: &HashMap<ComponentId, usize>,
        depth: u32,
    ) -> R<()> {
        let pr = |pin: u8| Some(PinRef { component: c.id, pin });
        let (w, h) = c.kind.size();
        let (wf, hf) = (w as f32, h as f32);
        let map = |p: P| xf(c, p);
        let colour = |default: Color| tcolor(c.color.unwrap_or(default));
        let history = vec![(F::HasLabel, Val::Bool(false)), (F::LogicHistory, Val::Bool(false))];
        match c.kind {
            ComponentKind::Gate { op, inputs } => {
                let k = match op {
                    GateOp::And => 0,
                    GateOp::Nand => 1,
                    GateOp::Or => 2,
                    GateOp::Nor => 3,
                    GateOp::Xor => 4,
                    GateOp::Xnor => 5,
                };
                self.notes.count("gates", 1);
                let max = if k >= 4 { 2 } else { 4 };
                if inputs <= max {
                    let drawing = emit(gb, &gate_gx(k, wf, hf), &map);
                    let mut pins = vec![(pin_px(c, inputs), 2, pr(inputs))];
                    pins.extend((0..inputs).map(|i| (pin_px(c, i), 1, pr(i))));
                    self.custom(gb, lv, Cls::Gate(k, inputs), &drawing, &pins, Vec::new(), List::Devices);
                } else {
                    self.wide_gate(gb, lv, links, c, k, inputs);
                }
            }
            ComponentKind::Not => {
                self.notes.count("NOT gates", 1);
                let drawing = emit(gb, &gate_gx(6, wf, hf), &map);
                let pins = [(pin_px(c, 1), 2, pr(1)), (pin_px(c, 0), 1, pr(0))];
                self.custom(gb, lv, Cls::Gate(6, 1), &drawing, &pins, Vec::new(), List::Devices);
            }
            ComponentKind::TriState => {
                self.notes.count("tri-state buffers", 1);
                let gx = vec![
                    Gx::Poly(vec![(0.5, 0.75), (3.5, 2.0), (0.5, 3.25)], WHITE),
                    Gx::Line(vec![(0.0, 2.0), (0.5, 2.0)]),
                    Gx::Line(vec![(3.5, 2.0), (4.0, 2.0)]),
                    Gx::Line(vec![(2.0, 0.0), (2.0, 1.25)]),
                ];
                let drawing = emit(gb, &gx, &map);
                let pins = [(pin_px(c, 0), 1, pr(0)), (pin_px(c, 2), 3, pr(2)), (pin_px(c, 1), 1, pr(1))];
                self.custom(gb, lv, Cls::TriState, &drawing, &pins, Vec::new(), List::Devices);
            }
            ComponentKind::Switch { on, mode } => {
                self.notes.count("switches", 1);
                if mode == SwitchMode::Momentary {
                    self.approx("push button exported as a toggle switch (Digital Works has no push button)");
                }
                let mut extra = history;
                extra.push((F::Pulse, Val::Bool(mode == SwitchMode::Pulse)));
                extra.push((F::State, Val::Int(if on { 65535 } else { 0 })));
                self.standard(
                    gb,
                    lv,
                    Some(c),
                    Cls::InteractiveInput,
                    &[(pr(0), (0.0, 0.0), 2, 4.0)],
                    pin_px(c, 0),
                    extra,
                    List::Inputs,
                    None,
                );
            }
            ComponentKind::Clock { half_period } => {
                self.notes.count("clocks", 1);
                if half_period != CLOCK_HALF_PERIOD {
                    self.approx("clock speed is not stored in .dwm files (Digital Works sets it globally)");
                }
                let mut extra = history;
                extra.push((F::Sequence, Val::Str("01".into())));
                extra.push((F::Color, Val::Int(colour(Color::Lime))));
                self.standard(
                    gb,
                    lv,
                    Some(c),
                    Cls::Clock,
                    &[(pr(0), (15.0, 0.0), 2, 4.0)],
                    pin_px(c, 0),
                    extra,
                    List::Inputs,
                    None,
                );
            }
            ComponentKind::SeqGen { pattern, len, half_period } => {
                self.notes.count("sequence generators", 1);
                if half_period != CLOCK_HALF_PERIOD {
                    self.approx("clock speed is not stored in .dwm files (Digital Works sets it globally)");
                }
                let seq: String = (0..len).map(|i| if pattern >> i & 1 == 1 { '1' } else { '0' }).collect();
                let mut extra = history;
                extra.push((F::Sequence, Val::Str(seq)));
                extra.push((F::Color, Val::Int(colour(Color::Red))));
                self.standard(
                    gb,
                    lv,
                    Some(c),
                    Cls::BitGenerator,
                    &[(pr(0), (15.0, 0.0), 2, 4.0)],
                    pin_px(c, 0),
                    extra,
                    List::Inputs,
                    None,
                );
            }
            ComponentKind::Const { high } => {
                self.notes.count("power / ground", 1);
                let (cls, off) = if high { (Cls::Supply, (10.0, 0.0)) } else { (Cls::Ground, (0.0, -10.0)) };
                self.standard(gb, lv, Some(c), cls, &[(pr(0), off, 2, 4.0)], pin_px(c, 0), history, List::Inputs, None);
            }
            ComponentKind::Led => {
                self.notes.count("LEDs", 1);
                let mut extra = history;
                extra.push((F::Color, Val::Int(colour(Color::Red))));
                self.standard(
                    gb,
                    lv,
                    Some(c),
                    Cls::Led,
                    &[(pr(0), (0.0, 0.0), 1, 4.0)],
                    pin_px(c, 0),
                    extra,
                    List::Outputs,
                    None,
                );
            }
            ComponentKind::SevenSeg { common_anode } => {
                self.notes.count("7-segment displays", 1);
                let mut extra = history;
                extra.push((F::Color, Val::Int(colour(Color::Red))));
                extra.push((F::CommonCathode, Val::Bool(!common_anode)));
                let pins: Vec<(Option<PinRef>, P, i32, f32)> =
                    (0..7u8).map(|i| (pr(i), (-15.0 + 5.0 * f32::from(i), 50.0), 1, 2.0)).collect();
                self.standard(
                    gb,
                    lv,
                    Some(c),
                    Cls::LedSegment,
                    &pins,
                    xf(c, (wf / 2.0, hf / 2.0)),
                    extra,
                    List::Outputs,
                    None,
                );
            }
            ComponentKind::Numeric { bits, signed, .. } => {
                self.notes.count("numeric outputs", 1);
                self.approx(
                    "numeric output display format is not stored (Digital Works shows one digit per input line)",
                );
                let n = usize::from(bits);
                let col = colour(Color::White);
                // As in real files: frame, one digit per line (least
                // significant first), leads, pins.
                let fw = wf - 1.0;
                let cell = fw / n as f32;
                let mut drawing =
                    vec![gb.polygon(
                        &[map((1.0, 0.0)), map((wf, 0.0)), map((wf, hf)), map((1.0, hf)), map((1.0, 0.0))],
                        0,
                    )];
                for k in 0..n {
                    let cx = 1.0 + fw - cell * (k as f32 + 0.5);
                    let at = map((cx, hf / 2.0));
                    let half_w = (cell * PX / 2.0 - 1.0).max(1.0);
                    drawing.push(gb.text(&Txt {
                        text: "0",
                        lo: (at.0 - half_w, at.1 - 8.0),
                        hi: (at.0 + half_w, at.1 + 8.0),
                        size: 10,
                        color: if k == 0 { col } else { DIM },
                        bold: false,
                        italic: false,
                        angle: 0.0,
                        overbar: false,
                    }));
                }
                for k in 0..bits {
                    let y = 2.0 * f32::from(k) + 1.0;
                    drawing.push(gb.polyline(&[map((0.0, y)), map((1.0, y))]));
                }
                let pins: Vec<(P, i32, Option<PinRef>)> = (0..bits).map(|k| (pin_px(c, k), 1, pr(k))).collect();
                let mut extra = history;
                extra.extend([
                    (F::Color, Val::Int(col)),
                    (F::InputLines, Val::Int(i32::from(bits))),
                    (F::Unk58, Val::Int(18)),
                    (F::Unk5c, Val::Int(0)),
                    (F::UnkColor60, Val::Int(DIM)),
                    (F::Flag64, Val::Bool(signed)),
                ]);
                self.custom(gb, lv, Cls::Numeric, &drawing, &pins, extra, List::Outputs);
            }
            ComponentKind::FlipFlop { ty, trigger, preset_clear } => {
                self.notes.count("flip-flops", 1);
                self.flip_flop(gb, lv, links, circ, c, ty, trigger, preset_clear);
            }
            ComponentKind::Memory { ty, addr_bits, data_bits } => {
                self.notes.count("memories", 1);
                let ram = ty == MemoryType::Ram;
                let drawing = emit(gb, &box_gx(wf, hf, ty.name()), &map);
                let ins = c.kind.input_count();
                let mut pins: Vec<(P, i32, Option<PinRef>)> = Vec::new();
                for i in (0..addr_bits).rev() {
                    pins.push((pin_px(c, i), 1, pr(i)));
                }
                let mut din: Vec<(u8, usize)> = Vec::new();
                for k in (0..data_bits).rev() {
                    pins.push((pin_px(c, ins + k), 3, pr(ins + k)));
                    if ram {
                        din.push((addr_bits + k, pins.len() - 1));
                    }
                }
                let cs = if ram { addr_bits + data_bits + 1 } else { addr_bits };
                pins.push((pin_px(c, cs), 1, pr(cs)));
                if ram {
                    pins.push((pin_px(c, addr_bits + data_bits), 1, pr(addr_bits + data_bits)));
                }
                let words = circ.memory(c.id).map(|m| m.as_ref().clone()).unwrap_or_default();
                let extra = vec![
                    (F::DataBits, Val::Int(i32::from(data_bits))),
                    (F::AddressBits, Val::Int(i32::from(addr_bits))),
                    (F::IsRam, Val::Bool(ram)),
                    (F::Unused, Val::Bool(false)),
                    (F::Words, Val::Words(words)),
                ];
                let (_, verts) = self.custom(gb, lv, Cls::Memory, &drawing, &pins, extra, List::Devices);
                // RAM data inputs share the bidirectional data pins. A data
                // input on its own net is driven onto the pin through a
                // tri-state buffer enabled while writing (R/W̄ = 0 and
                // C̄S̄ = 0), so the separate input and output nets stay
                // separate.
                let mut enable: Option<u32> = None;
                for (our, k) in din {
                    let p = PinRef { component: c.id, pin: our };
                    if circ.pin_node(p).is_none() {
                        continue;
                    }
                    let out = PinRef { component: c.id, pin: ins + (our - addr_bits) };
                    if circ.pin_node(out).is_none() || circ.net_pins(out).contains(&p) {
                        lv.pin_terms.entry(p).or_default().push(verts[k]);
                        continue;
                    }
                    let en = match enable {
                        Some(e) => e,
                        None => {
                            let rw = pin_px(c, addr_bits + data_bits);
                            let cs = pin_px(c, addr_bits + data_bits + 1);
                            let (o, iv) = self.helper(
                                gb,
                                lv,
                                3,
                                &[(rw.0 + 10.0, rw.1), (cs.0 + 10.0, cs.1)],
                                (rw.0 + 20.0, f32::midpoint(rw.1, cs.1)),
                            );
                            lv.pin_terms
                                .entry(PinRef { component: c.id, pin: addr_bits + data_bits })
                                .or_default()
                                .push(iv[0]);
                            lv.pin_terms
                                .entry(PinRef { component: c.id, pin: addr_bits + data_bits + 1 })
                                .or_default()
                                .push(iv[1]);
                            self.approx("RAM with separate data input and output nets: inputs driven onto Digital Works' bidirectional data pins by tri-state buffers enabled while writing");
                            enable = Some(o);
                            o
                        }
                    };
                    let at = pin_px(c, our);
                    let drawing =
                        emit(gb, &[Gx::Poly(vec![(0.0, -1.0), (2.0, 0.0), (0.0, 1.0)], WHITE)], &|(lx, ly)| {
                            (at.0 + 5.0 + lx * PX, at.1 + ly * PX)
                        });
                    let tpins = [
                        ((at.0 + 5.0, at.1), 1, Some(p)),
                        ((at.0 + 15.0, at.1), 3, None),
                        ((at.0 + 10.0, at.1 - 5.0), 1, None),
                    ];
                    let (_, tv) = self.custom(gb, lv, Cls::TriState, &drawing, &tpins, Vec::new(), List::Devices);
                    self.notes.count("helper gates", 1);
                    links.push((tv[1], verts[k]));
                    links.push((en, tv[2]));
                }
            }
            ComponentKind::Tag { dir, tristate } => {
                self.notes.count("macro tags", 1);
                let num = numbers.get(&c.id).map(ToString::to_string).unwrap_or_default();
                let at = pin_px(c, 0);
                let centre = xf(c, (wf / 2.0, hf / 2.0));
                let (lo, hi) = text_box(centre, &num, 7);
                let t = gb.text(&Txt {
                    text: &num,
                    lo,
                    hi,
                    size: 7,
                    color: 0,
                    bold: false,
                    italic: false,
                    angle: 0.0,
                    overbar: false,
                });
                let ch = gb.list(&[t]);
                let pin = lv.term(gb, Cls::Pin, at, 4.0, 0, pr(0));
                let inner = gb.pin(Cls::MacroPin, (0.0, 0.0), 4.0, 0, WHITE);
                let direction = match (dir, tristate) {
                    (TagDir::In, _) => 1,
                    (TagDir::Out, false) => 2,
                    (TagDir::Out, true) => 3,
                };
                let extra = vec![
                    (F::TextPos, Val::Int(i32::from(dir == TagDir::Out))),
                    (F::Direction, Val::Int(direction)),
                    (F::MacroPin, rf(inner)),
                ];
                let d = device_rec(gb, Cls::Tag, Some(ch), true, &[pin], (0.0, 0.0), extra);
                lv.tags.push(d);
                lv.tag_pins.insert(c.id, inner);
            }
            ComponentKind::Macro(shape) => {
                self.notes.count("macro instances", 1);
                let d = self.instance(gb, lv, c, &shape, depth)?;
                lv.devices.push(d);
            }
            ComponentKind::Text { .. } => {
                let note = circ.note(c.id).cloned().unwrap_or_default();
                self.text_note(gb, lv, c, &note);
            }
        }
        Ok(())
    }

    /// A text annotation: one `TGraphicText` per line.
    fn text_note(&mut self, gb: &mut Gb, lv: &mut Level, c: &Component, note: &Note) {
        self.notes.count("text annotations", 1);
        let size = font_size(note.height);
        let color = c.color.map_or(0, tcolor);
        let angle = match c.rotation {
            Rotation::R0 => 0.0,
            Rotation::R90 => 270.0,
            Rotation::R180 => 180.0,
            Rotation::R270 => 90.0,
        };
        let lines: Vec<&str> = note.lines().collect();
        if lines.len() > 1 {
            self.approx("multi-line text split into one text per line");
        }
        if super::writer::to_cp1252(&note.text).1 > 0 {
            self.notes.add(Severity::Warning, "characters outside Windows-1252 replaced by '?'");
        }
        let b = c.bounds();
        let (lo, hi) = (px(b.min), px(b.max));
        let n = lines.len().max(1) as f32;
        for (i, line) in lines.iter().enumerate() {
            // Split the box along the text's own vertical axis.
            let (l, h) = match c.rotation {
                Rotation::R0 => {
                    let d = (hi.1 - lo.1) / n;
                    ((lo.0, lo.1 + d * i as f32), (hi.0, lo.1 + d * (i as f32 + 1.0)))
                }
                Rotation::R180 => {
                    let d = (hi.1 - lo.1) / n;
                    ((lo.0, hi.1 - d * (i as f32 + 1.0)), (hi.0, hi.1 - d * i as f32))
                }
                Rotation::R90 => {
                    let d = (hi.0 - lo.0) / n;
                    ((hi.0 - d * (i as f32 + 1.0), lo.1), (hi.0 - d * i as f32, hi.1))
                }
                Rotation::R270 => {
                    let d = (hi.0 - lo.0) / n;
                    ((lo.0 + d * i as f32, lo.1), (lo.0 + d * (i as f32 + 1.0), hi.1))
                }
            };
            let t = gb.text(&Txt {
                text: line,
                lo: l,
                hi: h,
                size,
                color,
                bold: note.bold,
                italic: note.italic,
                angle,
                overbar: note.overbar,
            });
            lv.devices.push(t);
        }
    }

    /// A gate with more inputs than Digital Works has: a tree (AND / OR
    /// family) or chain (XOR family) of smaller gates in the same box.
    fn wide_gate(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        links: &mut Vec<(u32, u32)>,
        c: &Component,
        op: u8,
        inputs: u8,
    ) {
        self.approx(if op >= 4 {
            "XOR / XNOR gate with more than 2 inputs exported as a chain of 2-input gates"
        } else {
            "gate with more than 4 inputs exported as a tree of gates"
        });
        let pr = |pin: u8| PinRef { component: c.id, pin };
        let map = |p: P| xf(c, p);
        let row = |i: u8| 2.0 * f32::from(i) + 1.0;
        let out_pin = pr(inputs);
        let link_pin = |lv: &mut Level, p: PinRef, v: u32| lv.pin_terms.entry(p).or_default().push(v);
        if op >= 4 {
            // in0 ^ in1 ^ … ; the last gate is XOR / XNOR.
            let stages = inputs - 1;
            let mut prev: Option<(u32, P)> = None;
            for s in 0..stages {
                let x = 0.5 + 3.0 * f32::from(s) / f32::from(stages);
                let x1 = 0.5 + 3.0 * f32::from(s + 1) / f32::from(stages);
                let ya = if s == 0 { row(0) } else { row(s) };
                let yb = row(s + 1);
                let last = s + 1 == stages;
                let out = if last { map((4.0, f32::from(inputs))) } else { map((x1 - 0.2, f32::midpoint(ya, yb))) };
                let ins = [map((x, ya)), map((x, yb))];
                let (o, iv) = self.helper(gb, lv, if last { op } else { 4 }, &ins, out);
                match prev {
                    None => link_pin(lv, pr(0), iv[0]),
                    Some((pv, _)) => links.push((pv, iv[0])),
                }
                link_pin(lv, pr(s + 1), iv[1]);
                if last {
                    link_pin(lv, out_pin, o);
                }
                prev = Some((o, out));
            }
        } else {
            let groups = usize::from(inputs).div_ceil(4);
            let base = if op <= 1 { 0 } else { 2 };
            let mut outs = Vec::new();
            let mut start = 0u8;
            for g in 0..groups {
                let left = usize::from(inputs - start);
                let size = u8::try_from(left.div_ceil(groups - g)).unwrap_or(4);
                let ins: Vec<P> = (start..start + size).map(|i| map((0.0, row(i)))).collect();
                let ym = f32::midpoint(row(start), row(start + size - 1));
                let (o, iv) = self.helper(gb, lv, base, &ins, map((2.0, ym)));
                for (k, v) in iv.into_iter().enumerate() {
                    link_pin(lv, pr(start + u8::try_from(k).unwrap_or(0)), v);
                }
                outs.push((o, ym));
                start += size;
            }
            let ins: Vec<P> = outs.iter().map(|&(_, y)| map((2.5, y))).collect();
            let (o, iv) = self.helper(gb, lv, op, &ins, map((4.0, f32::from(inputs))));
            for ((po, _), v) in outs.iter().zip(iv) {
                links.push((*po, v));
            }
            link_pin(lv, out_pin, o);
        }
    }

    /// Connects a signal to a helper's input vertex.
    fn feed(lv: &mut Level, links: &mut Vec<(u32, u32)>, sig: Sig, input: u32) {
        match sig {
            Sig::Pin(p) => lv.pin_terms.entry(p).or_default().push(input),
            Sig::V(v) => links.push((v, input)),
        }
    }

    /// A helper gate fed by `ins`, laid out at the cursor (which moves
    /// down); returns its output.
    fn gate_of(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        links: &mut Vec<(u32, u32)>,
        op: u8,
        ins: &[Sig],
        cur: &mut P,
    ) -> Sig {
        let pts: Vec<P> = (0..ins.len()).map(|k| (cur.0, cur.1 + 10.0 * k as f32)).collect();
        let out = (cur.0 + 15.0, cur.1 + 5.0 * (ins.len() as f32 - 1.0));
        let (o, iv) = self.helper(gb, lv, op, &pts, out);
        for (&sig, v) in ins.iter().zip(iv) {
            Self::feed(lv, links, sig, v);
        }
        cur.1 += 10.0 * ins.len() as f32 + 10.0;
        Sig::V(o)
    }

    /// Set / reset signals of a gated RS latch: S·C, R·C (no gating if
    /// `clk` is `None`), with active-high preset / clear taking priority:
    /// S' = (S·C + PRE)·C̄L̄R̄, R' = (R·C + CLR)·P̄R̄Ē.
    #[allow(clippy::too_many_arguments)]
    fn latch_inputs(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        links: &mut Vec<(u32, u32)>,
        s: Sig,
        r: Sig,
        clk: Option<Sig>,
        async_pins: Option<(Sig, Sig)>,
        cur: &mut P,
    ) -> (Sig, Sig) {
        let (mut s, mut r) = (s, r);
        if let Some(c) = clk {
            s = self.gate_of(gb, lv, links, 0, &[s, c], cur);
            r = self.gate_of(gb, lv, links, 0, &[r, c], cur);
        }
        if let Some((pre, clr)) = async_pins {
            let npre = self.gate_of(gb, lv, links, 6, &[pre], cur);
            let nclr = self.gate_of(gb, lv, links, 6, &[clr], cur);
            let so = self.gate_of(gb, lv, links, 2, &[s, pre], cur);
            let ro = self.gate_of(gb, lv, links, 2, &[r, clr], cur);
            s = self.gate_of(gb, lv, links, 0, &[so, nclr], cur);
            r = self.gate_of(gb, lv, links, 0, &[ro, npre], cur);
        }
        (s, r)
    }

    /// Flip-flops: Digital Works has a rising-edge D, a master-slave JK and
    /// a clockless RS bistable, all with fixed pins and active-high
    /// preset / clear; the other variants are built from them and helper
    /// gates with the same behaviour.
    #[allow(clippy::too_many_arguments)]
    fn flip_flop(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        links: &mut Vec<(u32, u32)>,
        circ: &Circuit,
        c: &Component,
        ty: FlipFlopType,
        trigger: FfTrigger,
        preset_clear: bool,
    ) {
        let pr = |pin: u8| PinRef { component: c.id, pin };
        let sync = ty.sync_inputs();
        let ins = c.kind.input_count();
        let (q, qn) = (pr(ins), pr(ins + 1));
        let pre = preset_clear.then(|| pr(sync));
        let clr = preset_clear.then(|| pr(sync + 1));
        let clk = pr(FlipFlopType::CLOCK_INPUT);
        let wired = |p: PinRef| circ.pin_node(p).is_some();
        let async_used = pre.is_some_and(wired) || clr.is_some_and(wired);
        let async_sigs = match (pre, clr) {
            (Some(p), Some(c)) if async_used => Some((Sig::Pin(p), Sig::Pin(c))),
            _ => None,
        };
        let (cw, ch) = c.kind.size();
        let centre = xf(c, (cw as f32 / 2.0, ch as f32 / 2.0));
        let first = pin_px(c, 0);
        let mut cur = (first.0.min(centre.0) - 120.0, first.1 - 20.0);
        // Digital Works' D flip-flop; returns its D and CLK input vertices.
        let d_ff = |this: &mut Self, gb: &mut Gb, lv: &mut Level, data: Option<PinRef>, clk_pin: bool| -> (u32, u32) {
            let pins = [
                (Some(q), (30.0, -10.0), 2, 4.0),
                (Some(qn), (30.0, 10.0), 2, 4.0),
                (data, (-30.0, -10.0), 1, 4.0),
                (clk_pin.then_some(clk), (-30.0, 0.0), 1, 4.0),
                (clr, (-20.0, 20.0), 1, 4.0),
                (pre, (-10.0, 20.0), 1, 4.0),
            ];
            let (_, v) = this.standard(gb, lv, Some(c), Cls::DFlipFlop, &pins, centre, Vec::new(), List::Devices, None);
            (v[2], v[3])
        };
        match (ty, trigger) {
            (FlipFlopType::D, FfTrigger::RisingEdge) => {
                d_ff(self, gb, lv, Some(pr(0)), true);
            }
            (FlipFlopType::D, FfTrigger::FallingEdge | FfTrigger::MasterSlave) => {
                self.approx("falling-edge / master-slave D flip-flop exported as a D flip-flop with an inverted clock");
                let nc = self.gate_of(gb, lv, links, 6, &[Sig::Pin(clk)], &mut cur);
                let (_, k) = d_ff(self, gb, lv, Some(pr(0)), false);
                Self::feed(lv, links, nc, k);
            }
            (FlipFlopType::Jk, FfTrigger::MasterSlave) => {
                let pins = [
                    (Some(q), (30.0, -10.0), 2, 4.0),
                    (Some(qn), (30.0, 10.0), 2, 4.0),
                    (Some(pr(0)), (-30.0, -10.0), 1, 4.0),
                    (Some(clk), (-30.0, 0.0), 1, 4.0),
                    (clr, (-20.0, 20.0), 1, 4.0),
                    (pre, (-10.0, 20.0), 1, 4.0),
                    (Some(pr(2)), (-30.0, 10.0), 1, 4.0),
                ];
                self.standard(gb, lv, Some(c), Cls::JkFlipFlop, &pins, centre, Vec::new(), List::Devices, None);
            }
            (_, FfTrigger::RisingEdge | FfTrigger::FallingEdge) => {
                // Edge-triggered JK / RS: a D flip-flop with the next
                // state computed by gates, D = J·Q̄ + K̄·Q or S + R̄·Q.
                self.approx("edge-triggered JK / RS flip-flop exported as a D flip-flop with next-state gates");
                let nc = (trigger == FfTrigger::FallingEdge)
                    .then(|| self.gate_of(gb, lv, links, 6, &[Sig::Pin(clk)], &mut cur));
                let qpos = lv.net.v.len();
                let (d, k) = d_ff(self, gb, lv, None, nc.is_none());
                if let Some(nc) = nc {
                    Self::feed(lv, links, nc, k);
                }
                // The D flip-flop's Q and Q̄ terminals are the first two
                // vertices it created.
                let vq = Sig::V(u32::try_from(qpos).unwrap_or(0));
                let vqn = Sig::V(u32::try_from(qpos + 1).unwrap_or(0));
                let nk = self.gate_of(gb, lv, links, 6, &[Sig::Pin(pr(2))], &mut cur);
                let hold = self.gate_of(gb, lv, links, 0, &[nk, vq], &mut cur);
                let set = if ty == FlipFlopType::Jk {
                    self.gate_of(gb, lv, links, 0, &[Sig::Pin(pr(0)), vqn], &mut cur)
                } else {
                    Sig::Pin(pr(0))
                };
                let next = self.gate_of(gb, lv, links, 2, &[set, hold], &mut cur);
                Self::feed(lv, links, next, d);
            }
            (FlipFlopType::Rs, FfTrigger::MasterSlave) => {
                // Master: a gated RS latch (with preset / clear); slave: a
                // D flip-flop on the falling edge.
                self.approx("master-slave RS flip-flop exported as an RS latch and a D flip-flop");
                let (s, r) = self.latch_inputs(
                    gb,
                    lv,
                    links,
                    Sig::Pin(pr(0)),
                    Sig::Pin(pr(2)),
                    Some(Sig::Pin(clk)),
                    async_sigs,
                    &mut cur,
                );
                let m = self.bistable(gb, lv, links, s, r, None, (cur.0 + 40.0, cur.1));
                let nc = self.gate_of(gb, lv, links, 6, &[Sig::Pin(clk)], &mut cur);
                let (d, k) = d_ff(self, gb, lv, None, false);
                Self::feed(lv, links, m, d);
                Self::feed(lv, links, nc, k);
            }
            _ => {
                // Level-sensitive D / RS latch: Digital Works' RS bistable
                // with gated inputs (no gating if the clock is tied high).
                let tied_high = circ.drivers_of(clk).iter().any(|d| {
                    circ.component(d.component).is_some_and(|k| k.kind == ComponentKind::Const { high: true })
                });
                let (s0, r0) = if ty == FlipFlopType::D {
                    self.approx("D latch exported as an RS bistable with gates (S = D·C, R = D̄·C)");
                    let nd = self.gate_of(gb, lv, links, 6, &[Sig::Pin(pr(0))], &mut cur);
                    (Sig::Pin(pr(0)), nd)
                } else {
                    (Sig::Pin(pr(0)), Sig::Pin(pr(2)))
                };
                let gate_clk = (ty == FlipFlopType::D || !tied_high).then_some(Sig::Pin(clk));
                if async_sigs.is_some() {
                    self.approx("preset / clear of a latch exported as gates in front of S and R");
                }
                let (s, r) = self.latch_inputs(gb, lv, links, s0, r0, gate_clk, async_sigs, &mut cur);
                self.bistable(gb, lv, links, s, r, Some(c), centre);
            }
        }
    }

    /// Digital Works' RS bistable fed by `s`, `r`; returns its Q.
    #[allow(clippy::too_many_arguments)]
    fn bistable(
        &mut self,
        gb: &mut Gb,
        lv: &mut Level,
        links: &mut Vec<(u32, u32)>,
        s: Sig,
        r: Sig,
        c: Option<&Component>,
        at: P,
    ) -> Sig {
        let pr = |pin: u8| c.map(|c| PinRef { component: c.id, pin });
        let ins = c.map_or(0, |c| c.kind.input_count());
        let (q, qn) = (pr(ins), pr(ins + 1));
        let pins = [
            (q, (30.0, -10.0), 2, 4.0),
            (qn, (30.0, 10.0), 2, 4.0),
            (None, (-30.0, -10.0), 1, 4.0),
            (None, (-30.0, 10.0), 1, 4.0),
        ];
        let (_, v) = self.standard(gb, lv, c, Cls::RsFlipFlop, &pins, at, Vec::new(), List::Devices, None);
        Self::feed(lv, links, s, v[2]);
        Self::feed(lv, links, r, v[3]);
        Sig::V(v[0])
    }

    /// The exported body of a definition (cached; copied per instance).
    fn template(&mut self, id: MacroId, depth: u32) -> R<Rc<Template>> {
        if let Some(t) = self.templates.get(&id) {
            return Ok(Rc::clone(t));
        }
        if depth > bw_core::MAX_MACRO_DEPTH {
            return fail(format!("macros nested deeper than {} levels", bw_core::MAX_MACRO_DEPTH));
        }
        if !self.busy.insert(id) {
            return fail("a macro contains itself");
        }
        let table = self.table;
        let e = table.get(id).ok_or_else(|| ExportError(format!("unknown macro definition {}", id.0)))?;
        let def = Arc::clone(&e.def);
        let iface = def.interface();
        let order = outer_order(&e.shape);
        let mut numbers = HashMap::new();
        for (slot, i) in order.iter().enumerate() {
            if let Some(ip) = i.and_then(|i| iface.get(usize::from(i))) {
                numbers.insert(ip.tag, slot + 1);
            }
        }
        if self.noted_defs.insert(id) {
            // Pin names other than the numbered default ("P3") are lost.
            if def.pins.iter().any(|p| numbers.get(&p.tag).is_none_or(|n| p.name != format!("P{n}"))) {
                self.approx("macro pin names not stored (Digital Works tags show the pin number)");
            }
            if dw_datasheet(&def.datasheet).1 {
                self.notes.add(
                    Severity::Unsupported,
                    "macro datasheet text dropped (Digital Works stores only a datasheet file path)",
                );
            }
            if def.doc_link.is_some() {
                self.notes.add(Severity::Unsupported, "macro documentation link dropped");
            }
        }
        let mut gb = Gb::default();
        let (lists, tag_pins) = self.level(&mut gb, &def.body, &numbers, depth)?;
        self.busy.remove(&id);
        let t = Rc::new(Template { objs: gb.objs, lists, tag_pins });
        self.templates.insert(id, Rc::clone(&t));
        Ok(t)
    }

    /// Appends a copy of a template; returns the index offset.
    fn copy(gb: &mut Gb, t: &Template) -> R<u32> {
        if gb.objs.len().saturating_add(t.objs.len()) > MAX_OBJECTS {
            return fail(format!(
                "the circuit is too large for a Digital Works file: macros are copied into every instance and the copies exceed {MAX_OBJECTS} objects"
            ));
        }
        let off = u32::try_from(gb.objs.len()).map_err(|_| ExportError("too many objects".into()))?;
        let sh = |r: &Option<u32>| r.map(|r| r + off);
        for o in &t.objs {
            gb.objs.push(match o {
                Obj::List(v) => Obj::List(v.iter().map(sh).collect()),
                Obj::Rec(c, vals) => Obj::Rec(
                    *c,
                    vals.iter()
                        .map(|(f, v)| (*f, if let Val::Ref(r) = v { Val::Ref(sh(r)) } else { v.clone() }))
                        .collect(),
                ),
                other => other.clone(),
            });
        }
        Ok(off)
    }

    /// The outer pins of an instance (or of a file's template) and the
    /// embedded copy's lists; returns (outer pins, lists).
    fn macro_parts(
        &mut self,
        gb: &mut Gb,
        mut lv: Option<&mut Level>,
        c: &Component,
        shape: &MacroShape,
        depth: u32,
    ) -> R<(Vec<u32>, [u32; 6], String)> {
        let t = self.template(shape.def, depth + 1)?;
        let off = Self::copy(gb, &t)?;
        self.copies += 1;
        let table = self.table;
        let e = table.get(shape.def).ok_or_else(|| ExportError("unknown macro definition".into()))?;
        let iface = e.def.interface();
        let mut outer = Vec::new();
        for (slot, i) in outer_order(shape).into_iter().enumerate() {
            let (at, kind, inner) = match i {
                Some(i) => {
                    let ip = iface.get(usize::from(i));
                    let kind = match ip {
                        Some(p) if p.dir == TagDir::In => 1,
                        Some(p) if p.tristate => 3,
                        _ => 2,
                    };
                    let inner = ip.and_then(|p| t.tag_pins.get(&p.tag)).map(|&x| x + off);
                    (pin_px(c, i), kind, inner)
                }
                None => {
                    let num = u8::try_from(slot + 1).unwrap_or(1);
                    (xf(c, dil_offset(shape, num)), 0, None)
                }
            };
            let o = match (lv.as_deref_mut(), i) {
                (Some(lv), Some(i)) => {
                    lv.term(gb, Cls::MacroPin, at, 4.0, kind, Some(PinRef { component: c.id, pin: i }))
                }
                _ => gb.pin(Cls::MacroPin, at, 4.0, kind, WHITE),
            };
            if let Some(inner) = inner {
                gb.set(o, F::Partner, rf(inner));
                gb.set(inner, F::Partner, rf(o));
            }
            outer.push(o);
        }
        let lists = t.lists.map(|l| l + off);
        Ok((outer, lists, dw_datasheet(&e.def.datasheet).0))
    }

    /// The icon of a macro: its box and name.
    fn icon(gb: &mut Gb, c: &Component, name: &str) -> u32 {
        let b = c.bounds();
        let (lo, hi) = (px(b.min), px(b.max));
        let poly = gb.polygon(&[lo, (hi.0, lo.1), hi, (lo.0, hi.1)], WHITE);
        let (tl, th) = text_box((f32::midpoint(lo.0, hi.0), f32::midpoint(lo.1, hi.1)), name, 7);
        let t = gb.text(&Txt {
            text: name,
            lo: tl,
            hi: th,
            size: 7,
            color: 0,
            bold: false,
            italic: false,
            angle: 0.0,
            overbar: false,
        });
        gb.list(&[poly, t])
    }

    fn macro_rec(gb: &mut Gb, icon: u32, outer: &[u32], datasheet: String, embedded: bool, lists: [u32; 6]) -> u32 {
        let pins = gb.glist(outer);
        gb.rec(
            Cls::Macro,
            vec![
                (F::HasGraphics, Val::Bool(true)),
                (F::Children, rf(icon)),
                (F::Interactive, Val::Bool(false)),
                (F::Flag1c, Val::Bool(true)),
                (F::Pins, rf(pins)),
                (F::Datasheet, Val::Str(datasheet)),
                (F::Unk10, Val::Int(0)),
                (F::Embedded38, Val::Bool(embedded)),
                (F::Embedded3a, Val::Bool(embedded)),
                (F::Devices, rf(lists[0])),
                (F::Tags, rf(lists[1])),
                (F::Inputs, rf(lists[2])),
                (F::Outputs, rf(lists[3])),
                (F::Solders, rf(lists[4])),
                (F::Wires, rf(lists[5])),
            ],
        )
    }

    fn instance(&mut self, gb: &mut Gb, lv: &mut Level, c: &Component, shape: &MacroShape, depth: u32) -> R<u32> {
        let (outer, lists, ds) = self.macro_parts(gb, Some(lv), c, shape, depth)?;
        let name = self.table.get(shape.def).map(|e| e.def.name.clone()).unwrap_or_default();
        let icon = Self::icon(gb, c, &name);
        Ok(Self::macro_rec(gb, icon, &outer, ds, true, lists))
    }

    /// The root object of a project file.
    fn project_root(&mut self, gb: &mut Gb, c: &Circuit, name: &str) -> R<u32> {
        if c.eval_mode() == EvalMode::UnitDelay {
            self.approx("unit-delay timing not kept (Digital Works propagates immediately, like the Sequential mode; it has no unknown X level)");
        }
        // Tags in the top-level circuit (e.g. an imported Digital Works
        // macro opened as a circuit) make the file a macro again: inputs
        // first, then outputs, in id order, on a box template.
        let mut tags: Vec<(bool, ComponentId, bool)> = c
            .components()
            .filter_map(|k| match k.kind {
                ComponentKind::Tag { dir, tristate } => Some((dir == TagDir::Out, k.id, tristate)),
                _ => None,
            })
            .collect();
        tags.sort_unstable();
        let ins = tags.iter().filter(|t| !t.0).count();
        let outs = tags.len() - ins;
        let shape = (ins <= usize::from(bw_core::MAX_MACRO_INPUTS)
            && outs <= usize::from(bw_core::MAX_MACRO_INPUTS)
            && tags.len() <= 255)
            .then(|| {
                let mut s = MacroShape::unresolved(MacroId(0));
                s.inputs = u8::try_from(ins).unwrap_or(0);
                s.outputs = u8::try_from(outs).unwrap_or(0);
                s.width = 8;
                for (k, t) in tags[ins..].iter().enumerate() {
                    if t.2 && k < 64 {
                        s.tristate |= 1 << k;
                    }
                }
                s
            });
        if shape.is_none() && !tags.is_empty() {
            self.notes.add(Severity::Warning, "macro tags in the top-level circuit exported unlinked (too many)");
        }
        let numbers: HashMap<ComponentId, usize> =
            if shape.is_some() { tags.iter().enumerate().map(|(k, t)| (t.1, k + 1)).collect() } else { HashMap::new() };
        let (lists, tag_pins) = self.level(gb, c, &numbers, 1)?;
        let (icon, outer) = match shape.filter(|_| !tags.is_empty()) {
            Some(shape) => {
                let (w, h) = ComponentKind::Macro(shape).size();
                let t = Component {
                    id: ComponentId(0),
                    kind: ComponentKind::Macro(shape),
                    pos: bw_core::GridPos::new(-w / 2, -h / 2),
                    rotation: Rotation::R0,
                    color: None,
                };
                let mut outer = Vec::new();
                for (i, tag) in tags.iter().enumerate() {
                    let kind = if !tag.0 {
                        1
                    } else if tag.2 {
                        3
                    } else {
                        2
                    };
                    let o = gb.pin(Cls::MacroPin, pin_px(&t, u8::try_from(i).unwrap_or(0)), 4.0, kind, WHITE);
                    if let Some(&inner) = tag_pins.get(&tag.1) {
                        gb.set(o, F::Partner, rf(inner));
                        gb.set(inner, F::Partner, rf(o));
                    }
                    outer.push(o);
                }
                (Self::icon(gb, &t, name), outer)
            }
            None => {
                let poly = gb.polygon(&[(-35.0, -10.0), (35.0, -10.0), (35.0, 10.0), (-35.0, 10.0)], WHITE);
                let (lo, hi) = text_box((0.0, 0.0), name, 7);
                let t = gb.text(&Txt {
                    text: name,
                    lo,
                    hi,
                    size: 7,
                    color: 0,
                    bold: false,
                    italic: false,
                    angle: 0.0,
                    overbar: false,
                });
                (gb.list(&[poly, t]), Vec::new())
            }
        };
        Ok(Self::macro_rec(gb, icon, &outer, String::new(), false, lists))
    }

    /// The root object of a macro file: the definition with its template
    /// pins centred on the origin.
    fn macro_root(&mut self, gb: &mut Gb, id: MacroId) -> R<(u32, String)> {
        let e = self.table.get(id).ok_or_else(|| ExportError(format!("unknown macro definition {}", id.0)))?;
        let shape = e.shape;
        let name = e.def.name.clone();
        let (w, h) = ComponentKind::Macro(shape).size();
        let c = Component {
            id: ComponentId(0),
            kind: ComponentKind::Macro(shape),
            pos: bw_core::GridPos::new(-w / 2, -h / 2),
            rotation: Rotation::R0,
            color: None,
        };
        let (outer, lists, ds) = self.macro_parts(gb, None, &c, &shape, 0)?;
        self.copies = self.copies.saturating_sub(1);
        let icon = Self::icon(gb, &c, &name);
        Ok((Self::macro_rec(gb, icon, &outer, ds.clone(), false, lists), ds))
    }
}

/// The header of an exported file (as Digital Works 3 writes it).
fn header(name: &str, datasheet: &str) -> Header {
    Header {
        version: EXPORT_VERSION,
        template: false,
        name: name.to_string(),
        datasheet: datasheet.to_string(),
        props: vec![
            (100, HeaderValue::Str(name.to_string())),
            (101, HeaderValue::Str(datasheet.to_string())),
            (102, HeaderValue::Str("0".into())),
            (202, HeaderValue::Int(0)),
            (200, HeaderValue::Int(1)),
        ],
        ..Header::default()
    }
}

/// Builds the object graph of a project.
///
/// # Errors
/// Limits (macro copies too large), malformed macro tables.
pub fn project_graph<'c>(c: &'c Circuit, name: &str) -> R<(Graph, Exporter<'c>)> {
    let mut ex = Exporter::new(c.macros());
    let mut gb = Gb::default();
    let root = ex.project_root(&mut gb, c, name)?;
    Ok((Graph { header: header(name, ""), objects: gb.objs, roots: vec![Some(root)], max_depth: 0 }, ex))
}

/// Builds the object graph of a macro file (the definition `id` with its
/// tags as the interface).
///
/// # Errors
/// As [`project_graph`].
pub fn macro_graph(table: &MacroTable, id: MacroId) -> R<(Graph, Exporter<'_>)> {
    let mut ex = Exporter::new(table);
    let mut gb = Gb::default();
    let (root, ds) = ex.macro_root(&mut gb, id)?;
    let name = table.get(id).map(|e| e.def.name.clone()).unwrap_or_default();
    Ok((Graph { header: header(&name, &ds), objects: gb.objs, roots: vec![Some(root)], max_depth: 0 }, ex))
}

/// Counts of the export, in display order.
#[must_use]
pub fn ordered_counts(notes: &Notes) -> Vec<(String, usize)> {
    let mut v = Vec::new();
    for what in EXPORT_COUNT_ORDER {
        if let Some(&n) = notes.counts.get(what) {
            v.push(((*what).to_string(), n));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_inverts_the_importer() {
        for size in 6..=26 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let height = (size as f32 * 96.0 / 72.0 * 0.7 / PX * 10.0).round() as u8;
            assert_eq!(font_size(height), size);
        }
    }

    #[test]
    fn solder_chains_have_three_wires() {
        let mut gb = Gb::default();
        let mut net = Net::default();
        let c = net.vertex((0.0, 0.0), None);
        let mut pins = Vec::new();
        for k in 0..5 {
            let p = gb.pin(Cls::Pin, (10.0 * k as f32 + 10.0, 0.0), 4.0, 1, WHITE);
            let v = net.vertex((10.0 * k as f32 + 10.0, 20.0), Some(p));
            net.edge(c, v);
            pins.push(p);
        }
        let (solders, wires) = net.finish(&mut gb);
        assert_eq!(solders.len(), 3);
        assert_eq!(wires.len(), 7);
        for s in solders {
            let Obj::Rec(_, v) = &gb.objs[s as usize] else { panic!() };
            assert_eq!(v.iter().filter(|(_, x)| matches!(x, Val::Ref(Some(_)))).count(), 3);
        }
    }
}
