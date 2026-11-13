//! Conversion of a parsed Digital Works object graph into the bitwerk model.
//!
//! Every macro level (the root `TMacro` and each embedded copy) becomes one
//! circuit: devices become components placed so that their pins land as
//! close as possible to the Digital Works pin positions (coordinates ÷ 5,
//! rotation derived from the pre-rotated pin geometry), wires become
//! orthogonal segment paths (diagonals get a corner), solders become
//! junctions, and short *stubs* bridge the remaining gap between a bitwerk
//! pin and the Digital Works pin position. Embedded macro copies are
//! deduplicated by a canonical content hash of their implementation and
//! become shared macro definitions.
//!
//! The graph is hostile input: every reference is type-checked through the
//! accessors (a reference to an object of the wrong class is treated as nil
//! and reported), macro nesting is bounded and cycle-checked, and the final
//! circuits go through the same validation as a project file.

#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "stream parser and geometry code: short coordinate names, one linear pass per class"
)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bw_core::{
    Circuit, CircuitRaw, Color, Component, ComponentId, ComponentKind, FfTrigger, FlipFlopType, GateOp, GridPos,
    MAX_SEQ_LEN, MacroDef, MacroId, MacroPin, MacroTable, MemoryRecord, MemoryType, Note, NoteRecord, NumFormat,
    Package, PinDir, PinRef, Rotation, SwitchMode, TagDir, data_width_index, memenc,
};
use bw_core::{MAX_ADDR_BITS, MAX_MACRO_DEPTH, MAX_NUMERIC_BITS, MIN_ADDR_BITS, MIN_NUMERIC_BITS, NodeAt};
use bw_core::{NodeId, NodeRecord, SegmentRecord};

use super::reader::{Cls, F, Graph, Obj, Val};
use super::{ImportNote, Severity};

/// Pixels per grid unit (the default Digital Works grid).
const PX: f32 = 5.0;
/// Default clock half period: 1 Hz at the default 20 ticks per second.
pub const CLOCK_HALF_PERIOD: u32 = 10;

/// Conversion error (the whole import fails).
#[derive(Debug, Clone)]
pub struct ConvError(pub String);

type R<T> = Result<T, ConvError>;

fn fail<T>(msg: impl Into<String>) -> R<T> {
    Err(ConvError(msg.into()))
}

/// Report accumulator.
#[derive(Debug, Default)]
pub struct Notes {
    notes: BTreeMap<(Severity, String), usize>,
    pub counts: BTreeMap<&'static str, usize>,
}

impl Notes {
    pub fn add(&mut self, sev: Severity, msg: impl Into<String>) {
        *self.notes.entry((sev, msg.into())).or_default() += 1;
    }

    pub(crate) fn count(&mut self, what: &'static str, n: usize) {
        if n > 0 {
            *self.counts.entry(what).or_default() += n;
        }
    }

    #[must_use]
    pub fn into_list(self) -> Vec<ImportNote> {
        let mut v: Vec<ImportNote> = self
            .notes
            .into_iter()
            .map(|((severity, message), count)| ImportNote { severity, message, count })
            .collect();
        v.sort_by(|a, b| (b.severity, &a.message).cmp(&(a.severity, &b.message)));
        v
    }
}

/// Display order of the counts.
pub const COUNT_ORDER: &[&str] = &[
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
    "wires",
    "wire segments",
    "junctions",
    "pin stubs",
];

fn grid(v: f32) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let r = (v / PX).round() as i32;
    r
}

fn gp((x, y): (f32, f32)) -> GridPos {
    GridPos::new(grid(x), grid(y))
}

/// Bounding box of points (min, max).
fn bbox(pts: &[(f32, f32)]) -> Option<((f32, f32), (f32, f32))> {
    let mut it = pts.iter();
    let &first = it.next()?;
    let mut lo = first;
    let mut hi = first;
    for &(x, y) in it {
        lo = (lo.0.min(x), lo.1.min(y));
        hi = (hi.0.max(x), hi.1.max(y));
    }
    Some((lo, hi))
}

/// Digital Works `TColor` (0x00BBGGRR) to the palette.
fn tcolor(c: i32) -> (Color, bool) {
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let c = c as u32;
    #[allow(clippy::cast_possible_truncation)]
    let rgb = [(c & 0xff) as u8, ((c >> 8) & 0xff) as u8, ((c >> 16) & 0xff) as u8];
    Color::nearest(rgb)
}

// ---------------------------------------------------------------- hashing

struct Fnv(u64);

impl Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Canonical content hashes of embedded macros (the implementation only:
/// the outer pins and the icon drawing, which are in the parent's
/// coordinates, are left out; references are renumbered in visit order).
struct Hashes<'g> {
    g: &'g Graph,
    memo: HashMap<u32, u64>,
    busy: HashSet<u32>,
}

impl Hashes<'_> {
    fn macro_hash(&mut self, m: u32, depth: u32) -> R<u64> {
        if let Some(&h) = self.memo.get(&m) {
            return Ok(h);
        }
        if depth > MAX_MACRO_DEPTH {
            return fail(format!("macros nested deeper than {MAX_MACRO_DEPTH} levels"));
        }
        if !self.busy.insert(m) {
            return fail("a macro contains itself");
        }
        let g = self.g;
        let Some(root) = g.get(m) else { return fail("bad macro reference") };
        let ext: HashMap<u32, usize> = g.items(root.r(F::Pins)).into_iter().enumerate().map(|(k, p)| (p, k)).collect();
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        let mut order: HashMap<u32, u32> = HashMap::new();
        let mut stack: Vec<u32> = Vec::new();
        order.insert(m, 0);
        // The root's own fields minus pins and icon.
        let visit_refs =
            |o: &Obj, skip_root: bool, order: &mut HashMap<u32, u32>, stack: &mut Vec<u32>, h: &mut Fnv| {
                for (f, r) in o.refs() {
                    if skip_root && matches!(f, Some(F::Pins | F::Children)) {
                        continue;
                    }
                    f.hash(h);
                    if let Some(&k) = ext.get(&r) {
                        (1u8, k).hash(h);
                        continue;
                    }
                    let next = u32::try_from(order.len()).unwrap_or(u32::MAX);
                    let id = *order.entry(r).or_insert_with(|| {
                        stack.push(r);
                        next
                    });
                    (2u8, id).hash(h);
                }
            };
        hash_scalars(root, &mut h);
        visit_refs(root, true, &mut order, &mut stack, &mut h);
        let mut steps = 0usize;
        while let Some(i) = stack.pop() {
            steps += 1;
            if steps > super::reader::MAX_OBJECTS {
                return fail("macro too large");
            }
            let Some(o) = g.get(i) else { continue };
            if o.cls() == Cls::Macro {
                // A nested macro counts by its own hash, plus its outer
                // pins and icon (which live at this level).
                let inner = self.macro_hash(i, depth + 1)?;
                (0xEEu8, inner).hash(&mut h);
                for (f, r) in o.refs() {
                    if matches!(f, Some(F::Pins | F::Children)) {
                        f.hash(&mut h);
                        let next = u32::try_from(order.len()).unwrap_or(u32::MAX);
                        let id = *order.entry(r).or_insert_with(|| {
                            stack.push(r);
                            next
                        });
                        (2u8, id).hash(&mut h);
                    }
                }
                continue;
            }
            hash_scalars(o, &mut h);
            visit_refs(o, false, &mut order, &mut stack, &mut h);
        }
        self.busy.remove(&m);
        let v = h.finish();
        self.memo.insert(m, v);
        Ok(v)
    }
}

fn hash_scalars(o: &Obj, h: &mut Fnv) {
    o.cls().hash(h);
    match o {
        Obj::Point(x, y) => (x.to_bits(), y.to_bits()).hash(h),
        Obj::List(v) => v.len().hash(h),
        Obj::Rec(_, vals) => {
            for (f, v) in vals {
                match v {
                    Val::Ref(_) => {}
                    Val::Int(i) => (f, 0u8, i).hash(h),
                    Val::Bool(b) => (f, 1u8, b).hash(h),
                    Val::Str(s) => (f, 2u8, s).hash(h),
                    Val::F32(x) => (f, 3u8, x.to_bits()).hash(h),
                    Val::Words(w) => (f, 4u8, w).hash(h),
                }
            }
        }
        Obj::Pending(_) => {}
    }
}

// ----------------------------------------------------------- wiring build

#[derive(Debug, Clone, Copy)]
struct NodeB {
    pos: GridPos,
    pin: Option<PinRef>,
}

/// Incremental wire-graph builder: nodes merged by union-find when two
/// consecutive points coincide, corners added to diagonal steps.
#[derive(Default)]
struct Wiring {
    nodes: Vec<NodeB>,
    parent: Vec<u32>,
    segs: Vec<(u32, u32)>,
    pin_node: HashMap<PinRef, u32>,
    diagonals: usize,
}

impl Wiring {
    fn node(&mut self, pos: GridPos, pin: Option<PinRef>) -> u32 {
        if let Some(p) = pin {
            if let Some(&n) = self.pin_node.get(&p) {
                return n;
            }
        }
        let id = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(NodeB { pos, pin });
        self.parent.push(id);
        if let Some(p) = pin {
            self.pin_node.insert(p, id);
        }
        id
    }

    fn find(&mut self, mut a: u32) -> u32 {
        while self.parent[a as usize] != a {
            let p = self.parent[a as usize];
            self.parent[a as usize] = self.parent[p as usize];
            a = p;
        }
        a
    }

    /// Joins two nodes with an orthogonal path (horizontal first).
    fn link(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (pa, pb) = (self.nodes[ra as usize].pos, self.nodes[rb as usize].pos);
        if pa == pb {
            let (ia, ib) = (self.nodes[ra as usize].pin.is_some(), self.nodes[rb as usize].pin.is_some());
            if ia && ib {
                // Two pins on the same spot: a zero-length segment.
                self.segs.push((ra, rb));
            } else if ia {
                self.parent[rb as usize] = ra;
            } else {
                self.parent[ra as usize] = rb;
            }
            return;
        }
        if pa.x == pb.x || pa.y == pb.y {
            self.segs.push((ra, rb));
        } else {
            self.diagonals += 1;
            let c = self.node(GridPos::new(pb.x, pa.y), None);
            self.segs.push((ra, c));
            self.segs.push((c, rb));
        }
    }

    /// Joins `a` to `b` through `via` (a polyline).
    fn path(&mut self, pts: &[GridPos]) -> Vec<u32> {
        pts.iter().map(|&p| self.node(p, None)).collect()
    }

    /// Final node / segment records: merged nodes resolved, duplicates
    /// dropped, dangling free ends pruned.
    fn records(mut self) -> (Vec<NodeRecord>, Vec<SegmentRecord>) {
        let n = self.nodes.len();
        let mut segs: Vec<(u32, u32)> = Vec::with_capacity(self.segs.len());
        let mut seen = HashSet::new();
        for k in 0..self.segs.len() {
            let (a, b) = self.segs[k];
            let (a, b) = (self.find(a), self.find(b));
            if a == b || !seen.insert((a.min(b), a.max(b))) {
                continue;
            }
            segs.push((a, b));
        }
        let mut alive = vec![true; segs.len()];
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, &(a, b)) in segs.iter().enumerate() {
            adj[a as usize].push(i);
            adj[b as usize].push(i);
        }
        let mut degree: Vec<usize> = adj.iter().map(Vec::len).collect();
        let mut work: Vec<u32> = (0..n).filter_map(|i| u32::try_from(i).ok()).collect();
        while let Some(v) = work.pop() {
            let vi = v as usize;
            if self.nodes[vi].pin.is_some() || degree[vi] != 1 {
                continue;
            }
            // A dangling free end: remove its segment.
            if let Some(&s) = adj[vi].iter().find(|&&s| alive[s]) {
                alive[s] = false;
                let (a, b) = segs[s];
                let o = if a == v { b } else { a };
                degree[vi] -= 1;
                degree[o as usize] -= 1;
                work.push(o);
            }
        }
        let mut dense = vec![u32::MAX; n];
        let mut nodes = Vec::new();
        for (i, nb) in self.nodes.iter().enumerate() {
            if self.parent[i] as usize != i || degree[i] == 0 {
                continue;
            }
            if nb.pin.is_none() && degree[i] < 2 {
                continue;
            }
            dense[i] = u32::try_from(nodes.len()).unwrap_or(u32::MAX);
            nodes.push(NodeRecord { id: NodeId(dense[i]), at: nb.pin.map_or(NodeAt::Free(nb.pos), NodeAt::Pin) });
        }
        let records = segs
            .iter()
            .zip(&alive)
            .filter(|&(&(a, b), &ok)| ok && dense[a as usize] != u32::MAX && dense[b as usize] != u32::MAX)
            .map(|(&(a, b), _)| SegmentRecord { a: NodeId(dense[a as usize]), b: NodeId(dense[b as usize]) })
            .collect();
        (nodes, records)
    }
}

// --------------------------------------------------------------- levels

/// A converted macro definition and how its instance pins map onto the
/// Digital Works outer pins.
#[derive(Debug, Clone)]
struct DefInfo {
    id: MacroId,
    /// For each instance pin (bitwerk order), the index of the Digital
    /// Works outer pin.
    dw_pin: Vec<Option<usize>>,
}

/// A tag of a level: the interface pin it makes.
struct TagInfo {
    id: ComponentId,
    /// Outer pin index (DIL number − 1), if the tag is linked.
    outer: Option<usize>,
    name: String,
}

struct Level {
    raw: CircuitRaw,
    tags: Vec<TagInfo>,
}

/// The whole conversion of one file.
pub struct Converter<'g> {
    g: &'g Graph,
    hashes: Hashes<'g>,
    pub notes: Notes,
    pub table: MacroTable,
    defs: HashMap<u64, DefInfo>,
    names: HashSet<String>,
    pub copies: usize,
}

/// A device placed at one level.
struct Placed {
    id: ComponentId,
    comp: Component,
    /// (our pin, Digital Works pin object).
    map: Vec<(u8, u32)>,
}

/// Placement: the rotation and position that bring the pins closest to
/// the Digital Works pin positions, preferring the body on the same side
/// as the original drawing.
fn place(
    kind: ComponentKind,
    pins: &[(u8, GridPos)],
    centre: (f32, f32),
    rotations: &[Rotation],
) -> (GridPos, Rotation) {
    let mut best: Option<(f32, GridPos, Rotation)> = None;
    for &rot in rotations {
        let c0 = Component { id: ComponentId(0), kind, pos: GridPos::new(0, 0), rotation: rot, color: None };
        let rel: Vec<(GridPos, GridPos)> =
            pins.iter().filter_map(|&(p, target)| c0.pin_pos(p).map(|r| (r, target))).collect();
        let pos = if rel.is_empty() {
            let b = c0.bounds();
            #[allow(clippy::cast_possible_truncation)]
            GridPos::new(
                (centre.0 - f32::from(i16::try_from(b.max.x).unwrap_or(0)) / 2.0).round() as i32,
                (centre.1 - f32::from(i16::try_from(b.max.y).unwrap_or(0)) / 2.0).round() as i32,
            )
        } else {
            let mut dx: Vec<i32> = rel.iter().map(|(r, t)| t.x - r.x).collect();
            let mut dy: Vec<i32> = rel.iter().map(|(r, t)| t.y - r.y).collect();
            dx.sort_unstable();
            dy.sort_unstable();
            GridPos::new(dx[dx.len() / 2], dy[dy.len() / 2])
        };
        #[allow(clippy::cast_precision_loss)]
        let cost = rel.iter().map(|(r, t)| ((r.x + pos.x - t.x).abs() + (r.y + pos.y - t.y).abs()) as f32).sum::<f32>();
        let b = Component { pos, ..c0 }.bounds();
        #[allow(clippy::cast_precision_loss)]
        let bc = ((b.min.x + b.max.x) as f32 / 2.0, (b.min.y + b.max.y) as f32 / 2.0);
        let tie = ((bc.0 - centre.0).abs() + (bc.1 - centre.1).abs()) * 0.01;
        let total = cost * 10.0 + tie;
        if best.is_none_or(|(c, ..)| total < c - 1e-4) {
            best = Some((total, pos, rot));
        }
    }
    best.map_or((GridPos::new(0, 0), Rotation::R0), |(_, p, r)| (p, r))
}

const ALL_ROT: [Rotation; 4] = [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270];

impl<'g> Converter<'g> {
    #[must_use]
    pub fn new(g: &'g Graph) -> Self {
        Self {
            g,
            hashes: Hashes { g, memo: HashMap::new(), busy: HashSet::new() },
            notes: Notes::default(),
            table: MacroTable::default(),
            defs: HashMap::new(),
            names: HashSet::new(),
            copies: 0,
        }
    }

    fn obj(&self, r: u32) -> Option<&'g Obj> {
        self.g.get(r)
    }

    /// A typed reference: `None` (and a warning) if it points to an object
    /// of another class.
    fn typed(&mut self, r: Option<u32>, ok: impl Fn(Cls) -> bool, what: &str) -> Option<&'g Obj> {
        let r = r?;
        let o = self.g.get(r)?;
        if ok(o.cls()) {
            Some(o)
        } else {
            self.notes.add(Severity::Warning, format!("stale reference ({what} pointing to a {:?}) ignored", o.cls()));
            None
        }
    }

    /// Centre of a pin (its 8 × 8 outline), in pixels.
    fn pin_centre(&self, p: u32) -> Option<(f32, f32)> {
        let o = self.obj(p)?;
        let (lo, hi) = bbox(&self.g.points(o.r(F::Points)))?;
        Some((f32::midpoint(lo.0, hi.0), f32::midpoint(lo.1, hi.1)))
    }

    /// Pins of a device (type-checked).
    fn pins_of(&mut self, d: &Obj) -> Vec<u32> {
        let items = self.g.items(d.r(F::Pins));
        let mut out = Vec::with_capacity(items.len());
        for p in items {
            if self.typed(Some(p), Cls::is_pin, "device pin").is_some() {
                out.push(p);
            }
        }
        out
    }

    /// Centre of a device in pixels: its stored position, or the middle of
    /// its drawing and pins.
    fn device_centre(&self, d: &Obj, pins: &[u32]) -> (f32, f32) {
        if let (Some(x), Some(y)) = (d.f32(F::X), d.f32(F::Y)) {
            return (x, y);
        }
        let mut pts: Vec<(f32, f32)> = pins.iter().filter_map(|&p| self.pin_centre(p)).collect();
        for c in self.g.items(d.r(F::Children)) {
            if let Some(o) = self.obj(c) {
                pts.extend(self.g.points(o.r(F::Points)));
            }
        }
        bbox(&pts).map_or((0.0, 0.0), |(lo, hi)| (f32::midpoint(lo.0, hi.0), f32::midpoint(lo.1, hi.1)))
    }

    /// The macro definition of an embedded copy (converted once per
    /// distinct implementation).
    fn definition(&mut self, m: u32, depth: u32) -> R<DefInfo> {
        let h = self.hashes.macro_hash(m, depth)?;
        self.copies += 1;
        if let Some(d) = self.defs.get(&h) {
            return Ok(d.clone());
        }
        if depth > MAX_MACRO_DEPTH {
            return fail(format!("macros nested deeper than {MAX_MACRO_DEPTH} levels"));
        }
        let Some(mo) = self.obj(m) else { return fail("bad macro reference") };
        let level = self.level(m, depth)?;
        let name = self.unique_name(&self.macro_name(mo));
        let outer = self.g.items(mo.r(F::Pins));
        let outer_pos: Vec<Option<(f32, f32)>> = outer.iter().map(|&p| self.pin_centre(p)).collect();
        let package = dil_package(&outer_pos);
        let mut datasheet = String::from("Imported from Digital Works.");
        if let Some(ds) = mo.str(F::Datasheet).filter(|s| !s.is_empty()) {
            let _ = write!(datasheet, "\nOriginal datasheet file: {ds}");
        }
        self.notes.add(Severity::Approximation, "macro icon drawings replaced by a box or DIL template");
        let (def, outer) = make_def(&name, level, package, datasheet, &self.table)?;
        let id = self.table.free_id().ok_or_else(|| ConvError("too many macro definitions".into()))?;
        let def = Arc::new(def);
        let dw_pin = def_pin_map(&def, &outer);
        self.table.add(id, Arc::clone(&def)).map_err(|e| ConvError(format!("macro {:?}: {e}", def.name)))?;
        let info = DefInfo { id, dw_pin };
        self.defs.insert(h, info.clone());
        Ok(info)
    }

    fn unique_name(&mut self, base: &str) -> String {
        let base: String = base.chars().filter(|c| !c.is_control()).take(56).collect();
        let base = if base.trim().is_empty() { "Macro".to_string() } else { base.trim().to_string() };
        let mut name = base.clone();
        let mut k = 2;
        while !self.names.insert(name.clone()) {
            name = format!("{base} ({k})");
            k += 1;
        }
        name
    }

    /// A name for an embedded macro: the most descriptive text of its icon
    /// (e.g. the part number), else its datasheet file name.
    fn macro_name(&self, m: &Obj) -> String {
        let mut best: Option<&str> = None;
        for c in self.g.items(m.r(F::Children)) {
            if let Some(t) = self.obj(c).and_then(|o| o.str(F::Text)) {
                let t = t.trim();
                if t.chars().any(char::is_alphabetic) && best.is_none_or(|b| t.len() > b.len()) {
                    best = Some(t);
                }
            }
        }
        if let Some(b) = best {
            return b.to_string();
        }
        if let Some(ds) = m.str(F::Datasheet).filter(|s| !s.is_empty()) {
            let stem = ds.rsplit(['\\', '/']).next().unwrap_or(ds);
            let stem = stem.rsplit_once('.').map_or(stem, |(a, _)| a);
            if !stem.is_empty() {
                return stem.to_uppercase();
            }
        }
        "Macro".into()
    }

    /// Converts one macro level (`m`) into a circuit.
    #[allow(clippy::too_many_lines)]
    fn level(&mut self, m: u32, depth: u32) -> R<Level> {
        let Some(mo) = self.obj(m) else { return fail("bad macro reference") };
        let mut comps: Vec<Component> = Vec::new();
        let mut placed: Vec<Placed> = Vec::new();
        let mut notes: Vec<NoteRecord> = Vec::new();
        let mut memories: Vec<MemoryRecord> = Vec::new();
        let mut tags: Vec<TagInfo> = Vec::new();
        let mut next_id = 0u32;
        let mut new_id = || {
            let id = ComponentId(next_id);
            next_id += 1;
            id
        };
        // Extra wiring: (our pin, our pin) joins (RS clocks tied high,
        // interactive-switch enables).
        let mut extra_links: Vec<(PinRef, PinRef)> = Vec::new();
        let mut switches: Vec<u32> = Vec::new();

        let mut devices: Vec<u32> = Vec::new();
        for f in [F::Devices, F::Inputs, F::Outputs] {
            devices.extend(self.g.items(mo.r(f)));
        }
        // Macros too wide for a bitwerk macro (more than 255 pins): their
        // contents are inlined into this level.
        let mut inlined: Vec<(u32, Level, GridPos)> = Vec::new();
        for d in devices {
            let Some(o) = self.obj(d) else { continue };
            let cls = o.cls();
            if cls == Cls::Macro && self.too_wide(o) {
                let pins = self.pins_of(o);
                let mut pts: Vec<(f32, f32)> = pins.iter().filter_map(|&p| self.pin_centre(p)).collect();
                pts.push(self.device_centre(o, &pins));
                let at = bbox(&pts).map_or(GridPos::new(0, 0), |(lo, _)| gp(lo));
                if depth + 1 > MAX_MACRO_DEPTH {
                    return fail(format!("macros nested deeper than {MAX_MACRO_DEPTH} levels"));
                }
                // Cycle check through the content hash.
                self.hashes.macro_hash(d, depth + 1)?;
                let inner = self.level(d, depth + 1)?;
                self.notes.add(
                    Severity::Approximation,
                    format!(
                        "macro {:?} has more pins than a bitwerk macro can have (255): its contents were inlined",
                        self.macro_name(o)
                    ),
                );
                inlined.push((d, inner, at));
                continue;
            }
            if cls == Cls::GraphicText {
                if let Some(n) = self.text_note(o) {
                    let id = new_id();
                    let (pos, rot, color) = n.1;
                    comps.push(Component { id, kind: n.0.kind(), pos, rotation: rot, color });
                    notes.push(NoteRecord { id, note: n.0 });
                    self.notes.count("text annotations", 1);
                }
                continue;
            }
            if cls.is_graphic() {
                self.notes.add(Severity::Unsupported, "free drawing (polygon / polyline) left out");
                continue;
            }
            if !cls.is_device() {
                self.notes.add(Severity::Warning, format!("unexpected {cls:?} in a device list ignored"));
                continue;
            }
            if cls == Cls::InteractiveSwitch {
                switches.push(d);
                continue;
            }
            // Device name label → a text annotation next to it.
            if o.bool(F::HasLabel) == Some(true) {
                if let Some(lo) = self.typed(o.r(F::NameLabel), |c| c == Cls::GraphicText, "name label") {
                    if let Some(n) = self.text_note(lo) {
                        let id = new_id();
                        let (pos, rot, color) = n.1;
                        comps.push(Component { id, kind: n.0.kind(), pos, rotation: rot, color });
                        notes.push(NoteRecord { id, note: n.0 });
                        self.notes.count("text annotations", 1);
                    }
                }
            }
            if o.bool(F::LogicHistory) == Some(true) {
                self.notes.add(Severity::Unsupported, "Logic History membership (no Logic History window yet)");
            }
            let pins = self.pins_of(o);
            let centre = self.device_centre(o, &pins);
            let rotations: &[Rotation] = if o.bool(F::HasGraphics) == Some(true) { &ALL_ROT } else { &[Rotation::R0] };
            let Some(conv) = self.device(d, cls, o, &pins, depth)? else { continue };
            let Conv { kind, color, map, commutative, rs_clock } = conv;
            // Positions of the Digital Works pins.
            let target = |k: usize| pins.get(k).and_then(|&p| self.pin_centre(p)).map(gp);
            let mut map2: Vec<(u8, usize)> = map;
            if let Some((first, n)) = commutative {
                // Interchangeable inputs: pair them by position along the
                // input edge for the best rotation.
                let (pos, rot) = {
                    let pts: Vec<(u8, GridPos)> =
                        map2.iter().filter_map(|&(our, k)| target(k).map(|t| (our, t))).collect();
                    place(kind, &pts, (centre.0 / PX, centre.1 / PX), rotations)
                };
                let c = Component { id: ComponentId(0), kind, pos, rotation: rot, color: None };
                let mut ours: Vec<(u8, GridPos)> =
                    (first..first + n).filter_map(|p| c.pin_pos(p).map(|q| (p, q))).collect();
                let mut theirs: Vec<(usize, GridPos)> = map2
                    .iter()
                    .filter(|&&(our, _)| our >= first && our < first + n)
                    .filter_map(|&(_, k)| target(k).map(|t| (k, t)))
                    .collect();
                ours.sort_by_key(|&(_, q)| (q.x, q.y));
                theirs.sort_by_key(|&(_, q)| (q.x, q.y));
                if ours.len() == theirs.len() {
                    map2.retain(|&(our, _)| our < first || our >= first + n);
                    map2.extend(ours.iter().zip(&theirs).map(|(&(o, _), &(k, _))| (o, k)));
                }
            }
            let pts: Vec<(u8, GridPos)> = map2.iter().filter_map(|&(our, k)| target(k).map(|t| (our, t))).collect();
            let (pos, rotation) = place(kind, &pts, (centre.0 / PX, centre.1 / PX), rotations);
            let id = new_id();
            let comp = Component { id, kind, pos, rotation, color };
            if let ComponentKind::Memory { data_bits, .. } = kind {
                if let Some(words) = o.words(F::Words) {
                    let mask = memenc::width_mask(data_bits);
                    let mut w: Vec<u16> = words.iter().map(|&x| x & mask).collect();
                    let n = kind.memory_words();
                    if w.len() != n {
                        self.notes.add(Severity::Warning, "memory contents of the wrong length padded / cut");
                    }
                    w.resize(n, 0);
                    memories.push(MemoryRecord { id, words: memenc::encode(&w) });
                }
            }
            if let ComponentKind::Tag { .. } = kind {
                let (outer, name) = self.tag_link(o, mo);
                tags.push(TagInfo { id, outer, name });
            }
            if rs_clock {
                // Digital Works' RS flip-flop has no clock: tie ours high.
                let vcc = new_id();
                let c = comp.pin_pos(1).unwrap_or(pos);
                let k = Component {
                    id: vcc,
                    kind: ComponentKind::Const { high: true },
                    pos: GridPos::new(c.x - 4, c.y - 1),
                    rotation: Rotation::R0,
                    color: None,
                };
                comps.push(k);
                extra_links.push((PinRef { component: vcc, pin: 0 }, PinRef { component: id, pin: 1 }));
                self.notes
                    .add(Severity::Approximation, "RS flip-flop (no clock) imported as an RS latch with C tied to VCC");
            }
            let map: Vec<(u8, u32)> = map2.iter().filter_map(|&(our, k)| pins.get(k).map(|&p| (our, p))).collect();
            placed.push(Placed { id, comp, map });
        }
        // Tags.
        for t in self.g.items(mo.r(F::Tags)) {
            let Some(o) = self.typed(Some(t), |c| c == Cls::Tag, "macro tag") else { continue };
            let pins = self.pins_of(o);
            let dir = o.int(F::Direction).unwrap_or(0);
            let kind = match dir {
                1 => ComponentKind::Tag { dir: TagDir::In, tristate: false },
                2 => ComponentKind::Tag { dir: TagDir::Out, tristate: false },
                3 => {
                    self.notes.add(
                        Severity::Approximation,
                        "bidirectional / tri-state macro tag imported as a tri-state output",
                    );
                    ComponentKind::Tag { dir: TagDir::Out, tristate: true }
                }
                _ => {
                    self.notes.add(Severity::Approximation, "macro tag without a direction imported as an input");
                    ComponentKind::Tag { dir: TagDir::In, tristate: false }
                }
            };
            let centre = self.device_centre(o, &pins);
            let pts: Vec<(u8, GridPos)> =
                pins.first().and_then(|&p| self.pin_centre(p)).map(|c| vec![(0u8, gp(c))]).unwrap_or_default();
            let (pos, rotation) = place(kind, &pts, (centre.0 / PX, centre.1 / PX), &ALL_ROT);
            let id = new_id();
            let comp = Component { id, kind, pos, rotation, color: None };
            let (outer, name) = self.tag_link(o, mo);
            tags.push(TagInfo { id, outer, name });
            self.notes.count("macro tags", 1);
            let map = pins.first().map(|&p| vec![(0u8, p)]).unwrap_or_default();
            placed.push(Placed { id, comp, map });
        }

        // Wiring.
        let mut w = Wiring::default();
        let mut anchor: HashMap<u32, u32> = HashMap::new();
        let mut stubs = 0usize;
        for p in &placed {
            for &(our, dwp) in &p.map {
                let Some(target) = self.pin_centre(dwp).map(gp) else { continue };
                let pr = PinRef { component: p.id, pin: our };
                // Only pins that are wired need a node.
                let wired = self.obj(dwp).and_then(|o| o.r(F::Wire)).is_some();
                if !wired {
                    continue;
                }
                let Some(at) = p.comp.pin_pos(our) else { continue };
                let pn = w.node(at, Some(pr));
                let an = *anchor.entry(dwp).or_insert_with(|| w.node(target, None));
                if at == target {
                    w.link(pn, an);
                } else {
                    stubs += 1;
                    let out = outward(&p.comp, our);
                    let q = GridPos::new(at.x + out.0, at.y + out.1);
                    let qn = w.node(q, None);
                    w.link(pn, qn);
                    w.link(qn, an);
                }
            }
        }
        for (a, b) in &extra_links {
            let (Some(ca), Some(cb)) = (
                comps
                    .iter()
                    .find(|c| c.id == a.component)
                    .or_else(|| placed.iter().find(|p| p.id == a.component).map(|p| &p.comp)),
                comps
                    .iter()
                    .find(|c| c.id == b.component)
                    .or_else(|| placed.iter().find(|p| p.id == b.component).map(|p| &p.comp)),
            ) else {
                continue;
            };
            let (Some(pa), Some(pb)) = (ca.pin_pos(a.pin), cb.pin_pos(b.pin)) else { continue };
            let na = w.node(pa, Some(*a));
            let nb = w.node(pb, Some(*b));
            w.link(na, nb);
        }
        for (d, inner, at) in inlined {
            self.inline(d, inner, at, &mut w, &mut anchor, &mut comps, &mut notes, &mut memories, &mut new_id);
        }
        let mut solder_node: HashMap<u32, u32> = HashMap::new();
        for s in self.g.items(mo.r(F::Solders)) {
            let Some(o) = self.typed(Some(s), |c| c == Cls::Solder, "solder") else { continue };
            let pos = gp((o.f32(F::X).unwrap_or(0.0), o.f32(F::Y).unwrap_or(0.0)));
            let n = w.node(pos, None);
            solder_node.insert(s, n);
            self.notes.count("junctions", 1);
        }
        let mut wires = 0usize;
        let mut odd_ends = 0usize;
        for wr in self.g.items(mo.r(F::Wires)) {
            let Some(o) = self.typed(Some(wr), |c| c == Cls::Wire, "wire") else { continue };
            wires += 1;
            if o.int(F::PenColor).is_some_and(|c| c != 0) {
                self.notes.add(Severity::Warning, "wire colour (cosmetic) ignored");
            }
            let pts: Vec<GridPos> = self.g.points(o.r(F::Points)).into_iter().map(gp).collect();
            let mut end = |f: F, fallback: Option<GridPos>, w: &mut Wiring, this: &mut Self| -> Option<u32> {
                let r = o.r(f)?;
                let e = this.obj(r)?;
                match e.cls() {
                    Cls::Solder => solder_node.get(&r).copied(),
                    c if c.is_pin() => {
                        if let Some(&a) = anchor.get(&r) {
                            return Some(a);
                        }
                        // A pin of something not imported (or a pin not of
                        // this level): a loose end.
                        odd_ends += 1;
                        let at = this.pin_centre(r).map(gp).or(fallback)?;
                        Some(w.node(at, None))
                    }
                    other => {
                        this.notes.add(
                            Severity::Warning,
                            format!("stale reference (wire end pointing to a {other:?}) ignored"),
                        );
                        None
                    }
                }
            };
            let e1 = end(F::End1, pts.first().copied(), &mut w, self);
            let e2 = end(F::End2, pts.last().copied(), &mut w, self);
            let mut chain: Vec<u32> = Vec::with_capacity(pts.len() + 2);
            chain.extend(e1);
            chain.extend(w.path(&pts));
            chain.extend(e2);
            for k in 1..chain.len() {
                w.link(chain[k - 1], chain[k]);
            }
        }
        if odd_ends > 0 {
            self.notes.add(
                Severity::Warning,
                format!(
                    "{odd_ends} wire end(s) on a pin with no bitwerk counterpart (e.g. an IC power pin) left loose"
                ),
            );
        }
        self.notes.count("wires", wires);
        self.notes.count("pin stubs", stubs);

        // Interactive (two-terminal) switches: a tri-state buffer enabled by
        // a toggle switch, from the driven side to the other.
        for s in switches {
            let Some(o) = self.obj(s) else { continue };
            let pins = self.pins_of(o);
            if pins.len() != 2 {
                self.notes.add(Severity::Warning, "interactive switch without two terminals left out");
                continue;
            }
            for &p in &pins {
                if let std::collections::hash_map::Entry::Vacant(e) = anchor.entry(p) {
                    let at = self.pin_centre(p).map(gp).unwrap_or_default();
                    e.insert(w.node(at, None));
                }
            }
            let driven = |w: &mut Wiring, k: usize, placed: &[Placed]| -> bool {
                let Some(&a) = anchor.get(&pins[k]) else { return false };
                let ra = w.find(a);
                placed.iter().any(|p| {
                    p.map.iter().any(|&(our, _)| {
                        p.comp.pin_dir(our) == Some(PinDir::Output)
                            && w.pin_node.get(&PinRef { component: p.id, pin: our }).copied().is_some_and(|n| {
                                let rn = w.find(n);
                                connected(w, rn, ra)
                            })
                    })
                })
            };
            let (src, dst) = if !driven(&mut w, 0, &placed) && driven(&mut w, 1, &placed) { (1, 0) } else { (0, 1) };
            let centre = self.device_centre(o, &pins);
            let kind = ComponentKind::TriState;
            let pts: Vec<(u8, GridPos)> = [(0u8, src), (2u8, dst)]
                .iter()
                .filter_map(|&(our, k)| self.pin_centre(pins[k]).map(|c| (our, gp(c))))
                .collect();
            let (pos, rotation) = place(kind, &pts, (centre.0 / PX, centre.1 / PX), &ALL_ROT);
            let tid = new_id();
            let tri = Component { id: tid, kind, pos, rotation, color: None };
            let sid = new_id();
            let en = tri.pin_pos(1).unwrap_or(pos);
            let on = o.bool(F::Flag3c) == Some(true);
            let sw = Component {
                id: sid,
                kind: ComponentKind::Switch { on, mode: SwitchMode::Toggle },
                pos: GridPos::new(en.x - 5, en.y - 3),
                rotation: Rotation::R0,
                color: None,
            };
            for (our, k) in [(0u8, src), (2u8, dst)] {
                let Some(at) = tri.pin_pos(our) else { continue };
                let pn = w.node(at, Some(PinRef { component: tid, pin: our }));
                let an = anchor[&pins[k]];
                w.link(pn, an);
            }
            let swn = w.node(sw.pin_pos(0).unwrap_or(pos), Some(PinRef { component: sid, pin: 0 }));
            let enn = w.node(en, Some(PinRef { component: tid, pin: 1 }));
            w.link(swn, enn);
            comps.push(tri);
            comps.push(sw);
            self.notes.add(
                Severity::Approximation,
                "two-terminal interactive switch imported as a one-way tri-state buffer enabled by a switch",
            );
        }

        comps.extend(placed.into_iter().map(|p| p.comp));
        comps.sort_by_key(|c| c.id);
        self.fix_drivers(&comps, &mut w);
        let (nodes, segments) = w.records();
        self.notes.count("wire segments", segments.len());
        Ok(Level {
            raw: CircuitRaw { components: comps, nodes, segments, memories, notes, ..CircuitRaw::default() },
            tags,
        })
    }

    /// Does an embedded macro have more pins than a bitwerk macro can?
    fn too_wide(&self, m: &Obj) -> bool {
        let tags = self.g.items(m.r(F::Tags));
        let ins =
            tags.iter().filter(|&&t| self.obj(t).and_then(|o| o.int(F::Direction)).is_none_or(|d| d <= 1)).count();
        let outs = tags.len() - ins;
        tags.len() > 255
            || ins > usize::from(bw_core::MAX_MACRO_INPUTS)
            || outs > usize::from(bw_core::MAX_MACRO_INPUTS)
    }

    /// Merges a converted macro level into this one: components, wires,
    /// memories and notes get new ids and move to `at`; each tag becomes a
    /// plain wire node joined to the macro's outer pin.
    #[allow(clippy::too_many_arguments)]
    fn inline(
        &mut self,
        m: u32,
        inner: Level,
        at: GridPos,
        w: &mut Wiring,
        anchor: &mut HashMap<u32, u32>,
        comps: &mut Vec<Component>,
        notes: &mut Vec<NoteRecord>,
        memories: &mut Vec<MemoryRecord>,
        new_id: &mut impl FnMut() -> ComponentId,
    ) {
        let Some(mo) = self.obj(m) else { return };
        let outer = self.g.items(mo.r(F::Pins));
        let raw = inner.raw;
        let lo = raw
            .components
            .iter()
            .map(|c| c.bounds().min)
            .fold(GridPos::new(i32::MAX, i32::MAX), |a, b| GridPos::new(a.x.min(b.x), a.y.min(b.y)));
        let (dx, dy) = if lo.x == i32::MAX { (0, 0) } else { (at.x - lo.x, at.y - lo.y) };
        let shift = |p: GridPos| GridPos::new(p.x + dx, p.y + dy);
        let tag_outer: HashMap<ComponentId, Option<usize>> = inner.tags.iter().map(|t| (t.id, t.outer)).collect();
        let mut ids: HashMap<ComponentId, ComponentId> = HashMap::new();
        for c in &raw.components {
            if tag_outer.contains_key(&c.id) {
                continue;
            }
            let id = new_id();
            ids.insert(c.id, id);
            comps.push(Component { id, pos: shift(c.pos), ..c.clone() });
        }
        for n in raw.notes {
            if let Some(&id) = ids.get(&n.id) {
                notes.push(NoteRecord { id, note: n.note });
            }
        }
        for mrec in raw.memories {
            if let Some(&id) = ids.get(&mrec.id) {
                memories.push(MemoryRecord { id, words: mrec.words });
            }
        }
        let tag_pos: HashMap<ComponentId, GridPos> = raw
            .components
            .iter()
            .filter(|c| tag_outer.contains_key(&c.id))
            .filter_map(|c| c.pin_pos(0).map(|p| (c.id, shift(p))))
            .collect();
        let mut node_of: HashMap<NodeId, u32> = HashMap::new();
        for n in &raw.nodes {
            let id = match n.at {
                NodeAt::Free(p) => w.node(shift(p), None),
                NodeAt::Pin(p) => {
                    if let Some(&t) = tag_pos.get(&p.component) {
                        // The tag's pin: a free node joined to the outer pin.
                        let node = w.node(t, None);
                        if let Some(Some(k)) = tag_outer.get(&p.component) {
                            if let Some(&op) = outer.get(*k) {
                                if let Some(c) = self.pin_centre(op).map(gp) {
                                    let a = *anchor.entry(op).or_insert_with(|| w.node(c, None));
                                    w.link(node, a);
                                }
                            }
                        }
                        node
                    } else if let Some(&nid) = ids.get(&p.component) {
                        let pr = PinRef { component: nid, pin: p.pin };
                        let pos = comps.iter().rev().find(|c| c.id == nid).and_then(|c| c.pin_pos(p.pin));
                        w.node(pos.unwrap_or(at), Some(pr))
                    } else {
                        continue;
                    }
                }
            };
            node_of.insert(n.id, id);
        }
        for sgm in &raw.segments {
            if let (Some(&a), Some(&b)) = (node_of.get(&sgm.a), node_of.get(&sgm.b)) {
                w.link(a, b);
            }
        }
    }

    /// Nets with more than one non-tri-state driver (allowed in Digital
    /// Works, not here): every extra driver is disconnected (its pin node
    /// becomes a plain junction) and reported.
    fn fix_drivers(&mut self, comps: &[Component], w: &mut Wiring) {
        let by_id: HashMap<ComponentId, &Component> = comps.iter().map(|c| (c.id, c)).collect();
        // Union-find over segments.
        let segs = w.segs.clone();
        let mut uf: Vec<u32> = (0..u32::try_from(w.nodes.len()).unwrap_or(0)).collect();
        for (a, b) in segs {
            let (a, b) = (w.find(a), w.find(b));
            let (ra, rb) = (root(&mut uf, a), root(&mut uf, b));
            if ra != rb {
                uf[ra as usize] = rb;
            }
        }
        let mut nets: BTreeMap<u32, Vec<(PinRef, bool, u32)>> = BTreeMap::new();
        let pins: Vec<(PinRef, u32)> = w.pin_node.iter().map(|(&p, &n)| (p, n)).collect();
        for (p, n) in pins {
            let Some(c) = by_id.get(&p.component) else { continue };
            if c.pin_dir(p.pin) != Some(PinDir::Output) {
                continue;
            }
            let k = p.pin - c.kind.input_count();
            let tri = c.kind.output_tristate(k);
            let rn = w.find(n);
            let r = root(&mut uf, rn);
            nets.entry(r).or_default().push((p, tri, n));
        }
        let mut cut = 0usize;
        for (_, mut drivers) in nets {
            drivers.sort_by_key(|d| d.0);
            let plain = drivers.iter().filter(|d| !d.1).count();
            if plain == 0 || drivers.len() < 2 {
                continue;
            }
            // Keep the first plain driver; detach the others.
            let keep = drivers.iter().position(|d| !d.1).unwrap_or(0);
            for (i, &(p, _, n)) in drivers.iter().enumerate() {
                if i == keep {
                    continue;
                }
                let r = w.find(n);
                if w.nodes[r as usize].pin == Some(p) {
                    w.nodes[r as usize].pin = None;
                }
                w.pin_node.remove(&p);
                cut += 1;
            }
        }
        if cut > 0 {
            self.notes.add(
                Severity::Warning,
                format!("{cut} output(s) sharing a net with another output disconnected (only tri-state outputs may share a net)"),
            );
        }
    }

    /// The outer pin a tag is linked to (tag → inner macro pin → partner)
    /// and a pin name.
    fn tag_link(&mut self, t: &Obj, m: &Obj) -> (Option<usize>, String) {
        let inner = self.typed(t.r(F::MacroPin), |c| c == Cls::MacroPin, "tag pin");
        let partner = inner.and_then(|i| i.r(F::Partner));
        let partner = self.typed(partner, |c| c == Cls::MacroPin, "macro pin partner").and(partner);
        let outer = partner.and_then(|p| self.g.items(m.r(F::Pins)).iter().position(|&q| q == p));
        let label = self
            .g
            .items(t.r(F::Children))
            .iter()
            .filter_map(|&c| self.obj(c).and_then(|o| o.str(F::Text)))
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string);
        let name = match (outer, label) {
            (_, Some(l)) if l.chars().any(char::is_alphabetic) => l,
            (Some(k), _) => format!("P{}", k + 1),
            (None, Some(l)) => format!("T{l}"),
            (None, None) => "T".into(),
        };
        (outer, name)
    }

    /// A text annotation for a `TGraphicText`: note, position, rotation,
    /// colour.
    fn text_note(&mut self, o: &Obj) -> Option<(Note, Placement)> {
        let text = Note::sanitize_text(o.str(F::Text)?);
        if text.trim().is_empty() {
            return None;
        }
        let size = o.int(F::FontSize).unwrap_or(8).clamp(4, 72);
        // Points to cap height in tenths of a grid unit: 96 dpi, cap height
        // ≈ 0.7 em, 5 px per unit.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let height = ((size as f32 * 96.0 / 72.0 * 0.7 / PX * 10.0).round() as u8).clamp(4, 120);
        let note = Note {
            text,
            height,
            bold: o.int(F::FontWeight).unwrap_or(400) >= 600,
            italic: o.bool(F::Italic) == Some(true),
            overbar: o.bool(F::HasOverbar) == Some(true),
        };
        let pts = self.g.points(o.r(F::Points));
        let (lo, _) = bbox(&pts)?;
        let angle = o.f32(F::Angle).unwrap_or(0.0).rem_euclid(360.0);
        #[allow(clippy::cast_possible_truncation)]
        let rot = match ((angle / 90.0).round() as i32).rem_euclid(4) {
            1 => Rotation::R270,
            2 => Rotation::R180,
            3 => Rotation::R90,
            _ => Rotation::R0,
        };
        if !(angle % 90.0).abs().lt(&0.5) && !((angle % 90.0) - 90.0).abs().lt(&0.5) {
            self.notes.add(Severity::Approximation, "text at an angle that is not a multiple of 90° rounded");
        }
        let fc = o.int(F::FontColor).unwrap_or(0);
        let color = if fc == 0 {
            None
        } else {
            let (c, exact) = tcolor(fc);
            if !exact {
                self.notes.add(Severity::Approximation, "colour mapped to the nearest palette entry");
            }
            Some(c)
        };
        Some((note, (gp(lo), rot, color)))
    }

    /// The bitwerk kind of a device and how its pins map.
    #[allow(clippy::too_many_lines)]
    fn device(&mut self, idx: u32, cls: Cls, o: &Obj, pins: &[u32], depth: u32) -> R<Option<Conv>> {
        let n_pins = pins.len();
        let conv = |kind: ComponentKind, map: Vec<(u8, usize)>| Conv {
            kind,
            color: None,
            map,
            commutative: None,
            rs_clock: false,
        };
        let color_of = |this: &mut Self, default: Color| -> Option<Color> {
            let c = o.int(F::Color)?;
            let (col, exact) = tcolor(c);
            if !exact {
                this.notes.add(Severity::Approximation, "colour mapped to the nearest palette entry");
            }
            (col != default).then_some(col)
        };
        let expect = |this: &mut Self, n: usize, what: &str| {
            if n_pins != n {
                this.notes
                    .add(Severity::Warning, format!("{what} with {n_pins} pins instead of {n}: pins matched by order"));
            }
        };
        Ok(Some(match cls {
            Cls::Gate(6, _) => {
                expect(self, 2, "NOT gate");
                self.notes.count("NOT gates", 1);
                conv(ComponentKind::Not, vec![(0, 1), (1, 0)])
            }
            Cls::Gate(op, n) if n >= 2 => {
                let op = [GateOp::And, GateOp::Nand, GateOp::Or, GateOp::Nor, GateOp::Xor, GateOp::Xnor]
                    [usize::from(op.min(5))];
                expect(self, usize::from(n) + 1, "gate");
                self.notes.count("gates", 1);
                let mut map = vec![(n, 0usize)];
                map.extend((0..n).map(|i| (i, usize::from(i) + 1)));
                Conv { commutative: Some((0, n)), ..conv(ComponentKind::Gate { op, inputs: n }, map) }
            }
            Cls::TriState => {
                expect(self, 3, "tri-state buffer");
                self.notes.count("tri-state buffers", 1);
                conv(ComponentKind::TriState, vec![(0, 0), (1, 2), (2, 1)])
            }
            Cls::DFlipFlop => {
                expect(self, 6, "D flip-flop");
                self.notes.count("flip-flops", 1);
                let k =
                    ComponentKind::FlipFlop { ty: FlipFlopType::D, trigger: FfTrigger::RisingEdge, preset_clear: true };
                // DW [Q, /Q, D, CLK, CLR, PRE] → ours [D, C, PRE, CLR, Q, Q̄].
                conv(k, vec![(0, 2), (1, 3), (2, 5), (3, 4), (4, 0), (5, 1)])
            }
            Cls::JkFlipFlop => {
                expect(self, 7, "JK flip-flop");
                self.notes.count("flip-flops", 1);
                let k = ComponentKind::FlipFlop {
                    ty: FlipFlopType::Jk,
                    trigger: FfTrigger::MasterSlave,
                    preset_clear: true,
                };
                // DW [Q, /Q, J, CLK, CLR, PRE, K] → ours [J, C, K, PRE, CLR, Q, Q̄].
                conv(k, vec![(0, 2), (1, 3), (2, 6), (3, 5), (4, 4), (5, 0), (6, 1)])
            }
            Cls::RsFlipFlop => {
                expect(self, 4, "RS flip-flop");
                self.notes.count("flip-flops", 1);
                let k =
                    ComponentKind::FlipFlop { ty: FlipFlopType::Rs, trigger: FfTrigger::Latch, preset_clear: false };
                // DW [Q, /Q, S, R] → ours [S, C, R, Q, Q̄] (C tied high).
                Conv { rs_clock: true, ..conv(k, vec![(0, 2), (2, 3), (3, 0), (4, 1)]) }
            }
            Cls::Memory => {
                let data = o.int(F::DataBits).unwrap_or(0);
                let addr = o.int(F::AddressBits).unwrap_or(0);
                let (Ok(d), Ok(a)) = (u8::try_from(data), u8::try_from(addr)) else {
                    self.notes
                        .add(Severity::Unsupported, format!("memory with {addr} address / {data} data lines left out"));
                    return Ok(None);
                };
                if data_width_index(d).is_none() || !(MIN_ADDR_BITS..=MAX_ADDR_BITS).contains(&a) {
                    self.notes.add(Severity::Unsupported, format!("memory with {a} address / {d} data lines left out"));
                    return Ok(None);
                }
                let ram = o.bool(F::IsRam) == Some(true);
                let ty = if ram { MemoryType::Ram } else { MemoryType::Rom };
                let kind = ComponentKind::Memory { ty, addr_bits: a, data_bits: d };
                expect(self, usize::from(a) + usize::from(d) + 1 + usize::from(ram), "memory");
                self.notes.count("memories", 1);
                let (na, nd) = (usize::from(a), usize::from(d));
                let mut map = Vec::new();
                for i in 0..a {
                    map.push((i, na - 1 - usize::from(i)));
                }
                let ins = kind.input_count();
                for k in 0..d {
                    let dw = na + nd - 1 - usize::from(k);
                    if ram {
                        map.push((a + k, dw));
                    }
                    map.push((ins + k, dw));
                }
                if ram {
                    map.push((a + d, na + nd + 1)); // R/W
                    map.push((a + d + 1, na + nd)); // CS
                } else {
                    map.push((a, na + nd));
                }
                conv(kind, map)
            }
            Cls::Supply | Cls::Ground => {
                self.notes.count("power / ground", 1);
                conv(ComponentKind::Const { high: cls == Cls::Supply }, vec![(0, 0)])
            }
            Cls::InteractiveInput => {
                self.notes.count("switches", 1);
                let on = o.int(F::State).unwrap_or(0) != 0;
                let mode = if o.bool(F::Pulse) == Some(true) { SwitchMode::Pulse } else { SwitchMode::Toggle };
                conv(ComponentKind::Switch { on, mode }, vec![(0, 0)])
            }
            Cls::Clock | Cls::BitGenerator | Cls::SeqInput => {
                let seq = o.str(F::Sequence).unwrap_or("");
                let mut bits: Vec<bool> = Vec::new();
                let mut odd = false;
                for ch in seq.chars() {
                    match ch {
                        '0' => bits.push(false),
                        '1' => bits.push(true),
                        _ => odd = true,
                    }
                }
                if odd {
                    self.notes.add(Severity::Warning, "characters other than 0/1 in a sequence ignored");
                }
                if bits.len() > usize::from(MAX_SEQ_LEN) {
                    self.notes.add(Severity::Warning, "sequence longer than 64 bits cut");
                    bits.truncate(usize::from(MAX_SEQ_LEN));
                }
                if bits.is_empty() {
                    bits.push(false);
                }
                self.notes.add(
                    Severity::Approximation,
                    format!("clock speed is not stored in .dwm files: clocks and sequences step every {CLOCK_HALF_PERIOD} ticks (1 Hz at the default 20 ticks/s)"),
                );
                let is_clock = cls == Cls::Clock && bits == [false, true];
                let (kind, default) = if is_clock {
                    self.notes.count("clocks", 1);
                    (ComponentKind::Clock { half_period: CLOCK_HALF_PERIOD }, Color::Lime)
                } else {
                    if cls == Cls::Clock {
                        self.notes.add(
                            Severity::Approximation,
                            "clock with a pattern other than 01 imported as a sequence generator",
                        );
                    }
                    self.notes.count("sequence generators", 1);
                    let pattern = bits.iter().enumerate().fold(0u64, |acc, (i, &b)| acc | (u64::from(b) << i));
                    let len = u8::try_from(bits.len()).unwrap_or(MAX_SEQ_LEN);
                    (ComponentKind::SeqGen { pattern, len, half_period: CLOCK_HALF_PERIOD }, Color::Red)
                };
                let color = color_of(self, default);
                Conv { color, ..conv(kind, vec![(0, 0)]) }
            }
            Cls::Led | Cls::OutputDevice => {
                self.notes.count("LEDs", 1);
                let color = color_of(self, Color::Red);
                Conv { color, ..conv(ComponentKind::Led, vec![(0, 0)]) }
            }
            Cls::LedSegment => {
                expect(self, 7, "7-segment display");
                self.notes.count("7-segment displays", 1);
                let common_anode = o.bool(F::CommonCathode) == Some(false);
                let color = color_of(self, Color::Red);
                Conv {
                    color,
                    ..conv(ComponentKind::SevenSeg { common_anode }, (0..7u8).map(|i| (i, usize::from(i))).collect())
                }
            }
            Cls::Numeric => {
                let lines = o.int(F::InputLines).unwrap_or(8);
                let bits =
                    u8::try_from(lines.clamp(i32::from(MIN_NUMERIC_BITS), i32::from(MAX_NUMERIC_BITS))).unwrap_or(8);
                if i32::from(bits) != lines {
                    self.notes
                        .add(Severity::Warning, format!("numeric output with {lines} input lines clamped to {bits}"));
                }
                // The format is not a field we can read reliably: take it
                // from the digits of the stored drawing.
                let digits = self
                    .g
                    .items(o.r(F::Children))
                    .iter()
                    .filter(|&&c| self.obj(c).is_some_and(|x| x.cls() == Cls::GraphicText))
                    .count();
                let signed_flag = o.bool(F::Flag64) == Some(true);
                let format = match o.int(F::Unk5c) {
                    Some(1) => NumFormat::Hex,
                    Some(2) => NumFormat::Dec,
                    _ if digits == usize::from(bits) => NumFormat::Bin,
                    _ if digits == usize::from(bits.div_ceil(4)) => NumFormat::Hex,
                    _ if digits > 0 => NumFormat::Dec,
                    _ => NumFormat::Bin,
                };
                let signed = signed_flag && format == NumFormat::Dec;
                if signed_flag && format != NumFormat::Dec {
                    self.notes.add(
                        Severity::Warning,
                        "numeric output flag (probably two's complement) ignored outside decimal",
                    );
                }
                self.notes.count("numeric outputs", 1);
                let color = color_of(self, Color::White);
                let kind = ComponentKind::Numeric { bits, format, signed };
                Conv { color, ..conv(kind, (0..bits).map(|i| (i, usize::from(i))).collect()) }
            }
            Cls::Tag => {
                // Handled with the tag list; a tag in a device list is odd.
                self.notes.add(Severity::Warning, "macro tag in a device list ignored");
                return Ok(None);
            }
            Cls::Macro => {
                let info = self.definition(idx, depth + 1)?;
                let Some(e) = self.table.get(info.id) else { return Ok(None) };
                self.notes.count("macro instances", 1);
                let map = info
                    .dw_pin
                    .iter()
                    .enumerate()
                    .filter_map(|(i, k)| k.map(|k| (u8::try_from(i).unwrap_or(u8::MAX), k)))
                    .collect();
                conv(ComponentKind::Macro(e.shape), map)
            }
            other => {
                self.notes.add(Severity::Unsupported, format!("device {other:?} left out"));
                return Ok(None);
            }
        }))
    }

    /// Converts the root macro of the file into a root circuit.
    ///
    /// # Errors
    /// Malformed graph, limits, or a circuit that fails validation.
    pub fn root_circuit(&mut self) -> R<Circuit> {
        let root = self.root()?;
        let level = self.level(root, 1)?;
        let mut c = Circuit::from_raw(level.raw, &self.table, false).map_err(|e| ConvError(format!("circuit: {e}")))?;
        c.set_eval_mode(bw_core::EvalMode::Sequential);
        Ok(c)
    }

    /// Converts the root macro of the file into a macro definition (its
    /// tags are the interface); returns its id.
    ///
    /// # Errors
    /// As [`Self::root_circuit`]; a file without tags is not a macro.
    pub fn root_macro(&mut self, name: &str, datasheet: &str) -> R<MacroId> {
        let root = self.root()?;
        let level = self.level(root, 1)?;
        if level.tags.is_empty() {
            return fail("not a macro: the circuit has no macro tags");
        }
        let Some(mo) = self.obj(root) else { return fail("bad root") };
        let outer = self.g.items(mo.r(F::Pins));
        let outer_pos: Vec<Option<(f32, f32)>> = outer.iter().map(|&p| self.pin_centre(p)).collect();
        let package = dil_package(&outer_pos);
        let name = self.unique_name(name);
        let mut datasheet = datasheet.to_string();
        if let Some(ds) = mo.str(F::Datasheet).map(str::trim).filter(|s| !s.is_empty()) {
            if !datasheet.contains(ds) {
                let _ = write!(datasheet, "\nOriginal datasheet file: {ds}");
            }
        }
        let (def, _) = make_def(&name, level, package, datasheet, &self.table)?;
        let id = self.table.free_id().ok_or_else(|| ConvError("too many macro definitions".into()))?;
        self.table.add(id, Arc::new(def)).map_err(|e| ConvError(format!("macro: {e}")))?;
        Ok(id)
    }

    fn root(&self) -> R<u32> {
        let r = self.g.roots.first().copied().flatten();
        match r.and_then(|r| self.g.get(r).map(|o| (r, o.cls()))) {
            Some((r, Cls::Macro)) => Ok(r),
            _ => fail("the file does not contain a circuit"),
        }
    }
}

/// Root of `a` in a plain union-find array.
fn root(uf: &mut [u32], mut a: u32) -> u32 {
    while uf[a as usize] != a {
        uf[a as usize] = uf[uf[a as usize] as usize];
        a = uf[a as usize];
    }
    a
}

/// Are two union-find roots in the same net (through segments)?
fn connected(w: &mut Wiring, a: u32, b: u32) -> bool {
    if a == b {
        return true;
    }
    let mut adj: HashMap<u32, Vec<u32>> = HashMap::new();
    let segs = w.segs.clone();
    for (x, y) in segs {
        let (x, y) = (w.find(x), w.find(y));
        adj.entry(x).or_default().push(y);
        adj.entry(y).or_default().push(x);
    }
    let mut seen = HashSet::from([a]);
    let mut stack = vec![a];
    while let Some(v) = stack.pop() {
        if v == b {
            return true;
        }
        for &u in adj.get(&v).map_or(&[][..], Vec::as_slice) {
            if seen.insert(u) {
                stack.push(u);
            }
        }
    }
    false
}

/// Direction a pin leaves its (rotated) body.
fn outward(c: &Component, pin: u8) -> (i32, i32) {
    let side = c.kind.pin_side(pin);
    let d = match side {
        Some(bw_core::PinSide::Left) | None => (-1, 0),
        Some(bw_core::PinSide::Right) => (1, 0),
        Some(bw_core::PinSide::Top) => (0, -1),
        Some(bw_core::PinSide::Bottom) => (0, 1),
    };
    match c.rotation {
        Rotation::R0 => d,
        Rotation::R90 => (-d.1, d.0),
        Rotation::R180 => (-d.0, -d.1),
        Rotation::R270 => (d.1, -d.0),
    }
}

/// Where a text annotation goes: position, rotation, colour.
type Placement = (GridPos, Rotation, Option<Color>);

/// How one device converts.
struct Conv {
    kind: ComponentKind,
    color: Option<Color>,
    /// (our pin, index into the Digital Works pin list).
    map: Vec<(u8, usize)>,
    /// Interchangeable inputs: (first pin, count).
    commutative: Option<(u8, u8)>,
    /// RS flip-flop: tie the clock high.
    rs_clock: bool,
}

/// A DIL package if the outer pins are laid out like one (pins 1…n/2 down
/// one side, n/2+1…n up the other, at a constant pitch), any rotation.
fn dil_package(pins: &[Option<(f32, f32)>]) -> Package {
    let n = pins.len();
    let Ok(n8) = u8::try_from(n) else { return Package::Auto };
    if !bw_core::DIL_SIZES.contains(&n8) {
        return Package::Auto;
    }
    let Some(p): Option<Vec<(f32, f32)>> = pins.iter().copied().collect() else { return Package::Auto };
    let u = (p[1].0 - p[0].0, p[1].1 - p[0].1);
    let v = (p[n - 1].0 - p[0].0, p[n - 1].1 - p[0].1);
    let len = |a: (f32, f32)| (a.0 * a.0 + a.1 * a.1).sqrt();
    if len(u) < 1.0 || len(v) < 1.0 || (u.0 * v.0 + u.1 * v.1).abs() > 0.1 * len(u) * len(v) {
        return Package::Auto;
    }
    let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 1.5 && (a.1 - b.1).abs() < 1.5;
    #[allow(clippy::cast_precision_loss)]
    for j in 0..n / 2 {
        let jf = j as f32;
        let left = (p[0].0 + jf * u.0, p[0].1 + jf * u.1);
        let right = (left.0 + v.0, left.1 + v.1);
        if !near(p[j], left) || !near(p[n - 1 - j], right) {
            return Package::Auto;
        }
    }
    Package::Dil(n8)
}

/// [`dil_package`] for template pin positions (grid units).
#[must_use]
pub fn dil_package_of(pins: &[(f32, f32)]) -> Package {
    let px: Vec<Option<(f32, f32)>> = pins.iter().map(|&(x, y)| Some((x * PX, y * PX))).collect();
    dil_package(&px)
}

/// Builds a definition from a converted level: interface pins in outer pin
/// order (inputs then outputs follow from the tags), unique names.
fn make_def(
    name: &str,
    level: Level,
    package: Package,
    datasheet: String,
    table: &MacroTable,
) -> R<(MacroDef, HashMap<ComponentId, usize>)> {
    let mut tags = level.tags;
    tags.sort_by_key(|t| (t.outer.unwrap_or(usize::MAX), t.id));
    let mut used = HashSet::new();
    let mut pins = Vec::with_capacity(tags.len());
    for t in &tags {
        let mut base: String = t.name.chars().filter(|c| !c.is_control()).collect();
        while base.len() > bw_core::MAX_PIN_NAME {
            base.pop();
        }
        if base.is_empty() {
            base = "P".into();
        }
        let mut nm = base.clone();
        let mut k = 2;
        while !used.insert(nm.clone()) {
            let suffix = format!("{k}");
            let mut b = base.clone();
            while b.len() + suffix.len() > bw_core::MAX_PIN_NAME {
                b.pop();
            }
            nm = format!("{b}{suffix}");
            k += 1;
        }
        let dil = match package {
            Package::Dil(n) => t.outer.and_then(|k| u8::try_from(k + 1).ok()).filter(|&d| d <= n).unwrap_or(0),
            Package::Auto => 0,
        };
        pins.push(MacroPin { tag: t.id, name: nm, dil });
    }
    let outer: HashMap<ComponentId, usize> = tags.iter().filter_map(|t| t.outer.map(|k| (t.id, k))).collect();
    let body = Circuit::from_raw(level.raw, table, true).map_err(|e| ConvError(format!("macro {name:?}: {e}")))?;
    let mut def = MacroDef::new(name);
    def.body = body;
    def.pins = pins;
    def.package = package;
    def.datasheet = datasheet;
    let pin_count = def.pins.len();
    if let Err(e) = def.validate() {
        if matches!(package, Package::Dil(_)) {
            // Fall back to a box.
            def.package = Package::Auto;
            for p in &mut def.pins {
                p.dil = 0;
            }
        }
        let iface = def.interface();
        let ins = iface.iter().filter(|p| p.dir == TagDir::In).count();
        def.validate().map_err(|_| {
            ConvError(format!("macro {name:?} ({pin_count} pins, {ins} in / {} out): {e}", iface.len() - ins))
        })?;
    }
    Ok((def, outer))
}

/// For each instance pin (inputs then outputs), the outer Digital Works pin.
fn def_pin_map(def: &MacroDef, outer: &HashMap<ComponentId, usize>) -> Vec<Option<usize>> {
    def.interface().iter().map(|ip| outer.get(&ip.tag).copied()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dil_detection() {
        // A 14-pin DIL, 10 px pitch, 40 px between the rows.
        let mut p = Vec::new();
        for j in 0..7u8 {
            p.push(Some((0.0, 10.0 * f32::from(j))));
        }
        for j in 0..7u8 {
            p.push(Some((40.0, 60.0 - 10.0 * f32::from(j))));
        }
        assert_eq!(dil_package(&p), Package::Dil(14));
        p.swap(0, 1);
        assert_eq!(dil_package(&p), Package::Auto);
        assert_eq!(dil_package(&p[..5]), Package::Auto);
    }

    #[test]
    fn colours() {
        assert_eq!(tcolor(255), (Color::Red, true));
        assert_eq!(tcolor(0x0000_FF00), (Color::Lime, true));
        assert_eq!(tcolor(0x0000_FFFF), (Color::Yellow, true));
        assert_eq!(tcolor(0x0000_8000), (Color::Green, true));
        assert_eq!(tcolor(0x00FF_0000).0, Color::Blue);
    }
}
