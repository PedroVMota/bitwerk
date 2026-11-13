//! CPU-side scene: slot-stable instance arrays + an R-tree for picking.
//!
//! Every component and every junction dot owns one *body slot* and every
//! wire segment one *wire slot*. Slots never move: an edit rewrites the
//! affected slots in place,
//! deleted slots are hidden and go to a free list, and new objects reuse free
//! slots first. The renderer mirrors the arrays in GPU buffers and only
//! re-uploads the slots listed in the dirty sets ([`Scene::take_dirty`]), so
//! an edit costs microseconds instead of a full rebuild.
//!
//! Net levels are addressed by *signal slot*: every output pin of a
//! component owns one, and a component's signal slots are contiguous (the
//! body's `net` field is the first; output `k` is `net + k`). A segment's
//! `net` (and a junction dot's, and an LED body's) is the signal slot of the
//! driver of its wire net — its smallest output pin, or [`NO_NET`] for an
//! undriven net (drawn as floating). Signal slots are as stable as body
//! slots (free lists per range length), so a component that does not change
//! never needs re-uploading when other parts of the circuit are edited. The
//! renderer maps the simulator's nets onto signal slots through the output
//! pins driving each net (several on a tri-state bus).
//!
//! A full [`Scene::build`] is only needed when a whole circuit is replaced
//! (file load, stress test); it runs on a worker thread.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use bw_core::{
    Applied, Circuit, Component, ComponentId, ComponentKind, GridPos, NodeId, PinDir, PinRef, Rotation, SegmentId,
};
use bytemuck::{Pod, Zeroable};
use glam::Vec2;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};

use crate::symbols::SymbolKind;

/// "No net" marker shared with the shaders.
pub const NO_NET: u32 = u32::MAX;
/// Body `kind` of a free (hidden) slot; the shaders collapse it.
pub const HIDDEN_KIND: u32 = u32::MAX;
/// Wire `flags` bit of a free (hidden) slot.
pub const WIRE_HIDDEN: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct BodyInstance {
    pub min: [f32; 2],
    pub size: [f32; 2],
    /// Packed: kind code (bits 0..8, see [`kind_code`]), mesh variant
    /// (8..16: gate input count, flip-flop trigger, memory size, tag
    /// direction), rotation (16..18) and, for macro instances, the
    /// definition id (18..32: its mesh in the per-definition table);
    /// [`HIDDEN_KIND`] for a free slot. See [`body_kind`].
    pub kind: u32,
    /// First signal slot of the body's outputs (output `k` is `net + k`),
    /// or for an LED the signal slot driving it, or [`NO_NET`].
    pub net: u32,
}

impl BodyInstance {
    pub const HIDDEN: Self = Self { min: [0.0; 2], size: [0.0; 2], kind: HIDDEN_KIND, net: NO_NET };

    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.kind == HIDDEN_KIND
    }

    fn rect(&self) -> Rectangle<[f32; 2]> {
        Rectangle::from_corners(self.min, [self.min[0] + self.size[0], self.min[1] + self.size[1]])
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct WireInstance {
    pub a: [f32; 2],
    pub b: [f32; 2],
    /// Signal slot of the driving output pin, or [`NO_NET`].
    pub net: u32,
    pub flags: u32,
}

impl WireInstance {
    pub const HIDDEN: Self = Self { a: [0.0; 2], b: [0.0; 2], net: NO_NET, flags: WIRE_HIDDEN };

    #[must_use]
    pub fn new(a: [f32; 2], b: [f32; 2], net: u32) -> Self {
        Self { a, b, net, flags: 0 }
    }

    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.flags & WIRE_HIDDEN != 0
    }

    fn rect(&self) -> Rectangle<[f32; 2]> {
        Rectangle::from_corners(self.a, self.b)
    }
}

/// What an R-tree entry refers to (a slot index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// A component body or a junction dot.
    Body(u32),
    /// A wire segment.
    Wire(u32),
}

/// What occupies a body slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyOwner {
    Component(ComponentId),
    /// The solder dot of a wire junction.
    Junction(NodeId),
}

/// Slots changed since the renderer last synchronised.
#[derive(Debug, Default)]
pub struct Dirty {
    pub bodies: Vec<u32>,
    pub wires: Vec<u32>,
}

type Entry = GeomWithData<Rectangle<[f32; 2]>, Item>;

/// Half the side of a junction dot's body (grid units).
pub const DOT_HALF: f32 = 0.25;
/// "No slot" in the dense id → slot tables.
const NO_SLOT: u32 = u32::MAX;

/// Shader kind code of a component (one GPU draw bucket per code).
#[must_use]
pub fn kind_code(kind: &ComponentKind) -> u32 {
    SymbolKind::of(kind).code().unwrap_or(0)
}

/// Packed `kind` field of a component's body instance: the shader picks the
/// symbol mesh (kind × variant × style) and the rotation from it.
#[must_use]
pub fn body_kind(c: &Component) -> u32 {
    let rot = match c.rotation {
        Rotation::R0 => 0,
        Rotation::R90 => 1,
        Rotation::R180 => 2,
        Rotation::R270 => 3,
    };
    let sym = SymbolKind::of(&c.kind);
    let def = match c.kind {
        ComponentKind::Macro(s) => s.def.0.min(MAX_MACRO_SLOT) << 18,
        _ => 0,
    };
    sym.code().unwrap_or(0) | ((sym.variant() & 0xff) << 8) | (rot << 16) | def
}

/// Largest macro definition id the packed body kind can hold (14 bits).
pub const MAX_MACRO_SLOT: u32 = (1 << 14) - 1;

/// Packed `kind` of a junction dot.
#[must_use]
pub fn junction_kind() -> u32 {
    SymbolKind::Junction.code().unwrap_or(0)
}

/// Signal slot range of a body: `start..start + len` (`len` = outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SigRange {
    start: u32,
    len: u8,
}

impl SigRange {
    const NONE: Self = Self { start: NO_NET, len: 0 };
}

/// Signal slot of output `k` of `c`'s range, or [`NO_NET`].
fn sig_at(r: SigRange, k: u8) -> u32 {
    if k < r.len { r.start + u32::from(k) } else { NO_NET }
}

/// Kind code of a packed body `kind`.
#[must_use]
pub const fn unpack_code(kind: u32) -> u32 {
    kind & 0xff
}

#[allow(clippy::cast_precision_loss)]
fn f(v: i32) -> f32 {
    v as f32
}

fn fp(p: GridPos) -> [f32; 2] {
    [f(p.x), f(p.y)]
}

fn slot_u32(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

fn dense_get(v: &[u32], i: u32) -> Option<u32> {
    v.get(i as usize).copied().filter(|&s| s != NO_SLOT)
}

fn dense_set(v: &mut Vec<u32>, i: u32, slot: u32) {
    let i = i as usize;
    if v.len() <= i {
        v.resize(i + 1, NO_SLOT);
    }
    v[i] = slot;
}

static NEXT_SCENE_ID: AtomicU64 = AtomicU64::new(1);

pub struct Scene {
    /// Unique per scene object; the renderer re-uploads when it changes.
    id: u64,
    /// Circuit revision the scene reflects.
    revision: u64,
    bodies: Vec<BodyInstance>,
    body_owner: Vec<Option<BodyOwner>>,
    body_free: Vec<u32>,
    body_slot: HashMap<ComponentId, u32>,
    /// Body slot of each junction dot, by node id.
    dot_slot: Vec<u32>,
    dots: usize,
    /// Signal slots of each body slot's outputs.
    body_sig: Vec<SigRange>,
    /// Free signal ranges, by length.
    sig_free: Vec<Vec<u32>>,
    /// Signal slots in use or free (high-water mark).
    sig_count: u32,
    wires: Vec<WireInstance>,
    wire_owner: Vec<Option<SegmentId>>,
    wire_free: Vec<u32>,
    /// Wire slot of each segment, by segment id.
    seg_slot: Vec<u32>,
    tree: RTree<Entry>,
    dirty: Dirty,
    /// Bumps whenever a signal range is assigned or freed (the net → signal
    /// slot mapping changes).
    layout: u64,
    /// World bounds of everything (`min`, `max`); grows with edits and is
    /// recomputed exactly by a full build.
    bounds: (Vec2, Vec2),
}

impl std::fmt::Debug for Scene {
    fn fmt(&self, fm: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fm.debug_struct("Scene")
            .field("id", &self.id)
            .field("revision", &self.revision)
            .field("body_slots", &self.bodies.len())
            .field("wire_slots", &self.wires.len())
            .finish_non_exhaustive()
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::empty(0)
    }
}

fn body_geom(c: &Component, kind: u32, net: u32) -> BodyInstance {
    let r = c.bounds();
    let min = fp(r.min);
    let max = fp(r.max);
    BodyInstance { min, size: [max[0] - min[0], max[1] - min[1]], kind, net }
}

fn dot_geom(pos: GridPos, net: u32) -> BodyInstance {
    let c = fp(pos);
    BodyInstance { min: [c[0] - DOT_HALF, c[1] - DOT_HALF], size: [2.0 * DOT_HALF; 2], kind: junction_kind(), net }
}

/// The driver that colours a net: its smallest output pin (deterministic,
/// so an incrementally patched scene equals a fresh build).
fn pick_driver(circuit: &Circuit, best: &mut Option<PinRef>, pin: Option<PinRef>) {
    let Some(p) = pin else { return };
    let is_out = circuit.component(p.component).and_then(|c| c.pin_dir(p.pin)) == Some(PinDir::Output);
    if is_out && best.is_none_or(|b| p < b) {
        *best = Some(p);
    }
}

impl Scene {
    /// Marks the scene as reflecting circuit `revision` (a scene that a
    /// change elsewhere in the project did not affect).
    pub fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }

    fn empty(revision: u64) -> Self {
        Self {
            id: NEXT_SCENE_ID.fetch_add(1, Ordering::Relaxed),
            revision,
            bodies: Vec::new(),
            body_owner: Vec::new(),
            body_free: Vec::new(),
            body_slot: HashMap::new(),
            dot_slot: Vec::new(),
            dots: 0,
            body_sig: Vec::new(),
            sig_free: Vec::new(),
            sig_count: 0,
            wires: Vec::new(),
            wire_owner: Vec::new(),
            wire_free: Vec::new(),
            seg_slot: Vec::new(),
            tree: RTree::new(),
            dirty: Dirty::default(),
            layout: 0,
            bounds: (Vec2::ZERO, Vec2::ZERO),
        }
    }

    /// Builds a scene from scratch (bulk-loaded R-tree, dense slots).
    #[must_use]
    pub fn build(circuit: &Circuit, revision: u64) -> Self {
        let mut s = Self::empty(revision);
        let nc = circuit.component_count();
        let ns = circuit.segment_count();
        s.bodies.reserve(nc);
        s.body_owner.reserve(nc);
        s.body_slot.reserve(nc);
        s.body_sig.reserve(nc);
        s.wires.reserve(ns);
        s.wire_owner.reserve(ns);
        s.seg_slot = vec![NO_SLOT; circuit.segment_id_bound()];
        s.dot_slot = vec![NO_SLOT; circuit.node_id_bound()];
        let mut entries: Vec<Entry> = Vec::with_capacity(nc + ns);
        let mut lo = Vec2::splat(f32::MAX);
        let mut hi = Vec2::splat(f32::MIN);

        for c in circuit.components() {
            let slot = slot_u32(s.bodies.len());
            let sig = s.alloc_sig(c.kind.output_count());
            s.body_sig.push(sig);
            let b = body_geom(c, body_kind(c), sig.start);
            entries.push(Entry::new(b.rect(), Item::Body(slot)));
            lo = lo.min(Vec2::from(b.min));
            hi = hi.max(Vec2::from(b.min) + Vec2::from(b.size));
            s.bodies.push(b);
            s.body_owner.push(Some(BodyOwner::Component(c.id)));
            s.body_slot.insert(c.id, slot);
        }

        // Colour of every net: label the connected components once.
        let mut net_of = vec![u32::MAX; circuit.node_id_bound()];
        let mut net_sig: Vec<u32> = Vec::new();
        let mut stack = Vec::new();
        for (start, _) in circuit.nodes() {
            if net_of[start.0 as usize] != u32::MAX {
                continue;
            }
            let label = slot_u32(net_sig.len());
            let mut best = None;
            net_of[start.0 as usize] = label;
            stack.push(start);
            while let Some(n) = stack.pop() {
                let Some(w) = circuit.node(n) else { continue };
                pick_driver(circuit, &mut best, w.pin);
                for &sg in w.segments() {
                    let Some(o) = circuit.segment(sg).map(|x| x.other(n)) else { continue };
                    if net_of[o.0 as usize] == u32::MAX {
                        net_of[o.0 as usize] = label;
                        stack.push(o);
                    }
                }
            }
            net_sig.push(best.map_or(NO_NET, |p| s.pin_sig(circuit, p)));
        }
        let net =
            |n: NodeId| net_of.get(n.0 as usize).and_then(|&l| net_sig.get(l as usize)).copied().unwrap_or(NO_NET);

        for (id, sg) in circuit.segments() {
            let (Some(a), Some(b)) = (circuit.node(sg.a), circuit.node(sg.b)) else { continue };
            let slot = slot_u32(s.wires.len());
            let inst = WireInstance::new(fp(a.pos), fp(b.pos), net(sg.a));
            entries.push(Entry::new(inst.rect(), Item::Wire(slot)));
            lo = lo.min(Vec2::from(inst.a).min(Vec2::from(inst.b)));
            hi = hi.max(Vec2::from(inst.a).max(Vec2::from(inst.b)));
            s.wires.push(inst);
            s.wire_owner.push(Some(id));
            s.seg_slot[id.0 as usize] = slot;
        }
        for (id, w) in circuit.nodes() {
            if w.is_junction() {
                let slot = slot_u32(s.bodies.len());
                let b = dot_geom(w.pos, net(id));
                entries.push(Entry::new(b.rect(), Item::Body(slot)));
                s.bodies.push(b);
                s.body_owner.push(Some(BodyOwner::Junction(id)));
                s.body_sig.push(SigRange::NONE);
                s.dot_slot[id.0 as usize] = slot;
                s.dots += 1;
            }
            // An LED shows the level of the net at its input.
            if let Some(p) = w.pin {
                if let Some(&slot) = s.body_slot.get(&p.component) {
                    if s.body_sig[slot as usize].len == 0 {
                        s.bodies[slot as usize].net = net(id);
                    }
                }
            }
        }
        if s.bodies.is_empty() && s.wires.is_empty() {
            lo = Vec2::ZERO;
            hi = Vec2::ZERO;
        }
        s.bounds = (lo, hi);
        s.tree = RTree::bulk_load(entries);
        s
    }

    // ---- accessors -------------------------------------------------------

    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Body slots (hidden ones included), indexed by slot.
    #[must_use]
    pub fn bodies(&self) -> &[BodyInstance] {
        &self.bodies
    }

    /// Wire segment slots (hidden ones included), indexed by slot.
    #[must_use]
    pub fn wires(&self) -> &[WireInstance] {
        &self.wires
    }

    /// Component occupying a body slot (junction dots are not components).
    #[must_use]
    pub fn body_id(&self, slot: u32) -> Option<ComponentId> {
        match self.body_owner.get(slot as usize).copied().flatten() {
            Some(BodyOwner::Component(id)) => Some(id),
            _ => None,
        }
    }

    /// What occupies a body slot.
    #[must_use]
    pub fn body_owner(&self, slot: u32) -> Option<BodyOwner> {
        self.body_owner.get(slot as usize).copied().flatten()
    }

    /// Body slot of a component.
    #[must_use]
    pub fn body_slot(&self, id: ComponentId) -> Option<u32> {
        self.body_slot.get(&id).copied()
    }

    /// Body slot of a junction's dot.
    #[must_use]
    pub fn dot_slot(&self, id: NodeId) -> Option<u32> {
        dense_get(&self.dot_slot, id.0)
    }

    /// Number of junction dots.
    #[must_use]
    pub fn dot_count(&self) -> usize {
        self.dots
    }

    /// Wire slot of a segment.
    #[must_use]
    pub fn segment_slot(&self, id: SegmentId) -> Option<u32> {
        dense_get(&self.seg_slot, id.0)
    }

    /// Segment drawn in a wire slot.
    #[must_use]
    pub fn segment_at(&self, slot: u32) -> Option<SegmentId> {
        self.wire_owner.get(slot as usize).copied().flatten()
    }

    /// Signal slot of output `k` (0-based among the outputs) of a component.
    #[must_use]
    pub fn signal_slot(&self, id: ComponentId, k: u8) -> Option<u32> {
        let slot = *self.body_slot.get(&id)?;
        let s = sig_at(self.body_sig[slot as usize], k);
        (s != NO_NET).then_some(s)
    }

    /// Number of signal slots (the size of the level array).
    #[must_use]
    pub fn signal_count(&self) -> u32 {
        self.sig_count
    }

    /// Changes whenever signal slots are assigned or freed.
    #[must_use]
    pub fn layout_version(&self) -> u64 {
        self.layout
    }

    #[must_use]
    pub fn bounds(&self) -> (Vec2, Vec2) {
        self.bounds
    }

    /// Live (visible) instances.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        (self.bodies.len() - self.body_free.len()) + (self.wires.len() - self.wire_free.len())
    }

    /// Total slots, free ones included.
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.bodies.len() + self.wires.len()
    }

    /// Fraction of slots that are free (a full rebuild compacts them).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn free_ratio(&self) -> f32 {
        let total = self.slot_count();
        if total == 0 {
            return 0.0;
        }
        (self.body_free.len() + self.wire_free.len()) as f32 / total as f32
    }

    /// Takes the slots changed since the last call.
    pub fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    // ---- queries ---------------------------------------------------------

    /// Visits every item intersecting the world rectangle.
    pub fn query(&self, min: Vec2, max: Vec2, mut visit: impl FnMut(Item)) {
        let env = AABB::from_corners([min.x, min.y], [max.x, max.y]);
        for e in self.tree.locate_in_envelope_intersecting(env) {
            visit(e.data);
        }
    }

    /// Topmost component body containing a world point.
    #[must_use]
    pub fn pick_body(&self, p: Vec2) -> Option<ComponentId> {
        let mut hit: Option<u32> = None;
        self.query(p, p, |it| {
            if let Item::Body(i) = it {
                if self.body_id(i).is_some() {
                    // Deterministic: highest slot wins (drawn last).
                    hit = Some(hit.map_or(i, |h| h.max(i)));
                }
            }
        });
        hit.and_then(|s| self.body_id(s))
    }

    /// Wire segment nearest to a world point, within `reach` world units
    /// (ties: the highest slot, drawn last).
    #[must_use]
    pub fn pick_wire(&self, p: Vec2, reach: f32) -> Option<SegmentId> {
        let mut best: Option<(f32, u32)> = None;
        self.query(p - Vec2::splat(reach), p + Vec2::splat(reach), |it| {
            let Item::Wire(slot) = it else { return };
            let inst = self.wires[slot as usize];
            let (from, to) = (Vec2::from(inst.a), Vec2::from(inst.b));
            let dir = to - from;
            let len2 = dir.length_squared();
            let t = if len2 > 0.0 { ((p - from).dot(dir) / len2).clamp(0.0, 1.0) } else { 0.0 };
            let dist = (from + dir * t - p).length();
            let better = best.is_none_or(|(bd, bs)| dist.total_cmp(&bd).then(bs.cmp(&slot)).is_lt());
            if dist <= reach && better {
                best = Some((dist, slot));
            }
        });
        best.and_then(|(_, slot)| self.segment_at(slot))
    }

    // ---- incremental updates -----------------------------------------------

    /// Brings the scene in line with `circuit` for what an edit changed.
    /// Cost is proportional to the touched objects (and, when connectivity
    /// changed, to the nets involved), not to the circuit size.
    pub fn apply(&mut self, circuit: &Circuit, applied: &Applied, revision: u64) {
        for &id in &applied.components {
            self.sync_component(circuit, id);
        }
        for &s in &applied.segments {
            self.sync_segment(circuit, s);
        }
        for &n in &applied.nodes {
            self.sync_dot(circuit, n);
        }
        if applied.connectivity {
            // Recolour every net the edit touched (each once).
            let mut done = HashSet::new();
            let pin_nodes = |id: ComponentId| {
                let pins = circuit.component(id).map_or(0, |c| c.kind.pin_count());
                (0..pins).filter_map(move |pin| circuit.pin_node(PinRef { component: id, pin }))
            };
            let seeds: Vec<NodeId> = applied
                .nodes
                .iter()
                .copied()
                .chain(applied.components.iter().flat_map(|&id| pin_nodes(id)))
                .chain(applied.pins.iter().filter_map(|&p| circuit.pin_node(p)))
                .collect();
            for n in seeds {
                self.recolor(circuit, n, &mut done);
            }
            // LEDs that lost their wire show nothing.
            for p in &applied.pins {
                if circuit.pin_node(*p).is_none() {
                    self.set_led_net(circuit, p.component, NO_NET);
                }
            }
        } else {
            self.settle_geometric(circuit, applied);
        }
        self.revision = revision;
    }

    /// After a geometry-only edit the nets are unchanged: a new or moved
    /// segment takes the colour of an untouched neighbour (a full net walk
    /// only if it has none).
    fn settle_geometric(&mut self, circuit: &Circuit, applied: &Applied) {
        let mut pending: Vec<SegmentId> =
            applied.segments.iter().copied().filter(|&s| circuit.segment(s).is_some()).collect();
        let mut resolved: HashSet<SegmentId> = HashSet::new();
        let touched: HashSet<SegmentId> = pending.iter().copied().collect();
        loop {
            let before = pending.len();
            pending.retain(|&s| {
                let Some(sg) = circuit.segment(s) else { return false };
                let net = [sg.a, sg.b].iter().find_map(|&n| {
                    circuit.node(n)?.segments().iter().find_map(|&t| {
                        if t == s || (touched.contains(&t) && !resolved.contains(&t)) {
                            return None;
                        }
                        self.segment_slot(t).map(|slot| self.wires[slot as usize].net)
                    })
                });
                match net {
                    Some(net) => {
                        self.set_wire_net(s, net);
                        resolved.insert(s);
                        false
                    }
                    None => true,
                }
            });
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        let mut done = HashSet::new();
        for s in pending {
            if let Some(sg) = circuit.segment(s) {
                self.recolor(circuit, sg.a, &mut done);
            }
        }
        for &n in &applied.nodes {
            let Some(slot) = self.dot_slot(n) else { continue };
            let net = circuit.node(n).and_then(|w| {
                w.segments().iter().find_map(|&t| self.segment_slot(t).map(|ws| self.wires[ws as usize].net))
            });
            if let Some(net) = net {
                self.set_body_net(slot, net);
            }
        }
    }

    /// Recolours the net containing `seed` (segments, dots, LEDs) with its
    /// driver's signal slot.
    fn recolor(&mut self, circuit: &Circuit, seed: NodeId, done: &mut HashSet<NodeId>) {
        if done.contains(&seed) || circuit.node(seed).is_none() {
            return;
        }
        let mut nodes = Vec::new();
        let mut segs = Vec::new();
        let mut best = None;
        circuit.visit_net(
            seed,
            |n, w| {
                nodes.push((n, w.pin));
                pick_driver(circuit, &mut best, w.pin);
            },
            |s| segs.push(s),
        );
        let net = best.map_or(NO_NET, |p| self.pin_sig(circuit, p));
        for s in segs {
            self.set_wire_net(s, net);
        }
        for (n, pin) in nodes {
            done.insert(n);
            if let Some(slot) = self.dot_slot(n) {
                self.set_body_net(slot, net);
            }
            if let Some(p) = pin {
                self.set_led_net(circuit, p.component, net);
            }
        }
    }

    /// Signal slot of an output pin.
    fn pin_sig(&self, circuit: &Circuit, p: PinRef) -> u32 {
        let Some(&slot) = self.body_slot.get(&p.component) else { return NO_NET };
        let ins = circuit.component(p.component).map_or(0, |c| c.kind.input_count());
        p.pin.checked_sub(ins).map_or(NO_NET, |k| sig_at(self.body_sig[slot as usize], k))
    }

    fn set_led_net(&mut self, circuit: &Circuit, id: ComponentId, net: u32) {
        let is_led = circuit.component(id).is_some_and(|c| c.kind.output_count() == 0);
        if let (true, Some(&slot)) = (is_led, self.body_slot.get(&id)) {
            self.set_body_net(slot, net);
        }
    }

    fn set_body_net(&mut self, slot: u32, net: u32) {
        let b = &mut self.bodies[slot as usize];
        if b.net != net {
            // Geometry unchanged: no R-tree work.
            b.net = net;
            self.dirty.bodies.push(slot);
        }
    }

    fn set_wire_net(&mut self, s: SegmentId, net: u32) {
        let Some(slot) = self.segment_slot(s) else { return };
        let w = &mut self.wires[slot as usize];
        if w.net != net {
            w.net = net;
            self.dirty.wires.push(slot);
        }
    }

    fn extend_bounds(&mut self, lo: Vec2, hi: Vec2) {
        if self.instance_count() <= 1 && self.bounds == (Vec2::ZERO, Vec2::ZERO) {
            self.bounds = (lo, hi);
        } else {
            self.bounds = (self.bounds.0.min(lo), self.bounds.1.max(hi));
        }
    }

    /// Allocates `len` contiguous signal slots (reusing a free range of
    /// that length first).
    fn alloc_sig(&mut self, len: u8) -> SigRange {
        if len == 0 {
            return SigRange::NONE;
        }
        self.layout += 1;
        let l = usize::from(len);
        if let Some(start) = self.sig_free.get_mut(l).and_then(Vec::pop) {
            return SigRange { start, len };
        }
        let start = self.sig_count;
        self.sig_count = self.sig_count.saturating_add(u32::from(len));
        SigRange { start, len }
    }

    fn free_sig(&mut self, r: SigRange) {
        if r.len == 0 {
            return;
        }
        self.layout += 1;
        let l = usize::from(r.len);
        if self.sig_free.len() <= l {
            self.sig_free.resize_with(l + 1, Vec::new);
        }
        self.sig_free[l].push(r.start);
    }

    fn set_body(&mut self, slot: u32, b: BodyInstance) {
        let old = self.bodies[slot as usize];
        if !old.is_hidden() {
            self.tree.remove(&Entry::new(old.rect(), Item::Body(slot)));
        }
        if !b.is_hidden() {
            self.tree.insert(Entry::new(b.rect(), Item::Body(slot)));
            let lo = Vec2::from(b.min);
            self.extend_bounds(lo, lo + Vec2::from(b.size));
        }
        self.bodies[slot as usize] = b;
        self.dirty.bodies.push(slot);
    }

    fn set_wire(&mut self, slot: u32, w: WireInstance) {
        let old = self.wires[slot as usize];
        if !old.is_hidden() {
            self.tree.remove(&Entry::new(old.rect(), Item::Wire(slot)));
        }
        if !w.is_hidden() {
            self.tree.insert(Entry::new(w.rect(), Item::Wire(slot)));
            let (a, b) = (Vec2::from(w.a), Vec2::from(w.b));
            self.extend_bounds(a.min(b), a.max(b));
        }
        self.wires[slot as usize] = w;
        self.dirty.wires.push(slot);
    }

    fn alloc_body(&mut self, owner: BodyOwner) -> u32 {
        let slot = if let Some(s) = self.body_free.pop() {
            s
        } else {
            self.bodies.push(BodyInstance::HIDDEN);
            self.body_owner.push(None);
            self.body_sig.push(SigRange::NONE);
            slot_u32(self.bodies.len() - 1)
        };
        self.body_owner[slot as usize] = Some(owner);
        slot
    }

    fn free_body(&mut self, slot: u32) {
        self.set_body(slot, BodyInstance::HIDDEN);
        self.body_owner[slot as usize] = None;
        let sig = std::mem::replace(&mut self.body_sig[slot as usize], SigRange::NONE);
        self.free_sig(sig);
        self.body_free.push(slot);
    }

    fn sync_component(&mut self, circuit: &Circuit, id: ComponentId) {
        if let Some(c) = circuit.component(id) {
            let (slot, fresh) = if let Some(&s) = self.body_slot.get(&id) {
                (s, false)
            } else {
                self.layout += 1;
                let s = self.alloc_body(BodyOwner::Component(id));
                self.body_slot.insert(id, s);
                (s, true)
            };
            // (Re)assign the signal range if the output count changed.
            let outs = c.kind.output_count();
            let old = self.body_sig[slot as usize];
            if old.len != outs {
                self.free_sig(old);
                self.body_sig[slot as usize] = self.alloc_sig(outs);
            }
            // An LED keeps the net it shows (recoloured with its net).
            let net = if outs > 0 {
                self.body_sig[slot as usize].start
            } else if fresh {
                NO_NET
            } else {
                self.bodies[slot as usize].net
            };
            let b = body_geom(c, body_kind(c), net);
            self.set_body(slot, b);
        } else if let Some(slot) = self.body_slot.remove(&id) {
            self.layout += 1;
            self.free_body(slot);
        }
    }

    fn sync_segment(&mut self, circuit: &Circuit, id: SegmentId) {
        let current = circuit.segment_ends(id);
        match (self.segment_slot(id), current) {
            (Some(slot), None) => {
                self.set_wire(slot, WireInstance::HIDDEN);
                self.wire_owner[slot as usize] = None;
                self.wire_free.push(slot);
                dense_set(&mut self.seg_slot, id.0, NO_SLOT);
            }
            (slot, Some((a, b))) => {
                let (slot, net) = if let Some(s) = slot {
                    (s, self.wires[s as usize].net)
                } else {
                    let s = if let Some(s) = self.wire_free.pop() {
                        s
                    } else {
                        self.wires.push(WireInstance::HIDDEN);
                        self.wire_owner.push(None);
                        slot_u32(self.wires.len() - 1)
                    };
                    self.wire_owner[s as usize] = Some(id);
                    dense_set(&mut self.seg_slot, id.0, s);
                    (s, NO_NET)
                };
                let inst = WireInstance::new(fp(a), fp(b), net);
                if self.wires[slot as usize] != inst {
                    self.set_wire(slot, inst);
                }
            }
            (None, None) => {}
        }
    }

    fn sync_dot(&mut self, circuit: &Circuit, id: NodeId) {
        let want = circuit.node(id).filter(|w| w.is_junction()).map(|w| w.pos);
        match (self.dot_slot(id), want) {
            (Some(slot), None) => {
                self.free_body(slot);
                dense_set(&mut self.dot_slot, id.0, NO_SLOT);
                self.dots -= 1;
            }
            (slot, Some(pos)) => {
                let (slot, net) = if let Some(s) = slot {
                    (s, self.bodies[s as usize].net)
                } else {
                    let s = self.alloc_body(BodyOwner::Junction(id));
                    dense_set(&mut self.dot_slot, id.0, s);
                    self.dots += 1;
                    (s, NO_NET)
                };
                let b = dot_geom(pos, net);
                if self.bodies[slot as usize] != b {
                    self.set_body(slot, b);
                }
            }
            (None, None) => {}
        }
    }

    /// Checks every internal invariant against `circuit` (tests / debug).
    ///
    /// # Errors
    /// Describes the first inconsistency found.
    #[doc(hidden)]
    #[allow(clippy::float_cmp, reason = "both sides come from the same integer→f32 conversion")]
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self, circuit: &Circuit) -> Result<(), String> {
        let fresh = Self::build(circuit, self.revision);
        // Same live objects with identical geometry (slots may differ).
        if self.body_slot.len() != circuit.component_count() {
            return Err(format!("{} bodies for {} components", self.body_slot.len(), circuit.component_count()));
        }
        // Signal slot → (component, output) for both scenes.
        let owners = |scene: &Self| {
            let mut m = HashMap::new();
            for (slot, r) in scene.body_sig.iter().enumerate() {
                if let Some(BodyOwner::Component(id)) = scene.body_owner[slot] {
                    for k in 0..r.len {
                        m.insert(r.start + u32::from(k), (id, k));
                    }
                }
            }
            m
        };
        let (mine_owner, fresh_owner) = (owners(self), owners(&fresh));
        let net_comp = |scene: &Self, net: u32| {
            if net == NO_NET {
                None
            } else if std::ptr::eq(scene, self) {
                mine_owner.get(&net).copied()
            } else {
                fresh_owner.get(&net).copied()
            }
        };
        // Live signal ranges never overlap and stay below the high-water mark.
        let mut used = HashSet::new();
        for (slot, r) in self.body_sig.iter().enumerate() {
            if !matches!(self.body_owner[slot], Some(BodyOwner::Component(_))) && r.len != 0 {
                return Err(format!("body slot {slot} owns signals but is no component"));
            }
            for k in 0..u32::from(r.len) {
                if r.start + k >= self.sig_count || !used.insert(r.start + k) {
                    return Err(format!("signal slot {} reused or out of range", r.start + k));
                }
            }
        }
        for c in circuit.components() {
            let mine = self.body_slot(c.id).ok_or(format!("{:?} has no slot", c.id))?;
            let theirs = fresh.body_slot(c.id).ok_or("fresh build lacks component")?;
            let (a, b) = (self.bodies[mine as usize], fresh.bodies[theirs as usize]);
            if a.min != b.min || a.size != b.size || a.kind != b.kind {
                return Err(format!("body of {:?} differs: {a:?} vs {b:?}", c.id));
            }
            if net_comp(self, a.net) != net_comp(&fresh, b.net) {
                return Err(format!("net of {:?} differs", c.id));
            }
            if self.body_owner[mine as usize] != Some(BodyOwner::Component(c.id)) {
                return Err(format!("slot {mine} owner mismatch"));
            }
        }
        // Junction dots.
        if self.dots != fresh.dots {
            return Err(format!("{} dots vs {}", self.dots, fresh.dots));
        }
        for (id, w) in circuit.nodes() {
            match (self.dot_slot(id), fresh.dot_slot(id)) {
                (None, None) => {}
                (Some(m), Some(t)) => {
                    let (a, b) = (self.bodies[m as usize], fresh.bodies[t as usize]);
                    if a.min != b.min || a.size != b.size || a.kind != b.kind {
                        return Err(format!("dot of {id:?} differs: {a:?} vs {b:?}"));
                    }
                    if net_comp(self, a.net) != net_comp(&fresh, b.net) {
                        return Err(format!("net of dot {id:?} differs"));
                    }
                    if self.body_owner[m as usize] != Some(BodyOwner::Junction(id)) {
                        return Err(format!("dot slot {m} owner mismatch"));
                    }
                }
                (m, t) => return Err(format!("dot of {id:?} ({w:?}): {m:?} vs fresh {t:?}")),
            }
        }
        // Segments.
        let live_wires = self.wires.len() - self.wire_free.len();
        if live_wires != circuit.segment_count() {
            return Err(format!("{live_wires} wire slots for {} segments", circuit.segment_count()));
        }
        for (id, _) in circuit.segments() {
            let m = self.segment_slot(id).ok_or(format!("{id:?} has no slot"))?;
            let t = fresh.segment_slot(id).ok_or("fresh build lacks segment")?;
            let (a, b) = (self.wires[m as usize], fresh.wires[t as usize]);
            if a.a != b.a || a.b != b.b || net_comp(self, a.net) != net_comp(&fresh, b.net) || a.flags != 0 {
                return Err(format!("segment {id:?} differs: {a:?} vs {b:?}"));
            }
            if self.wire_owner[m as usize] != Some(id) {
                return Err(format!("wire slot {m} owner mismatch"));
            }
        }
        // Free slots are hidden and unowned.
        for &s in &self.body_free {
            if !self.bodies[s as usize].is_hidden() || self.body_owner[s as usize].is_some() {
                return Err(format!("free body slot {s} still live"));
            }
        }
        for &s in &self.wire_free {
            if !self.wires[s as usize].is_hidden() || self.wire_owner[s as usize].is_some() {
                return Err(format!("free wire slot {s} still live"));
            }
        }
        // R-tree holds exactly the live slots, with their current rectangles.
        if self.tree.size() != self.instance_count() {
            return Err(format!("R-tree has {} entries for {} live slots", self.tree.size(), self.instance_count()));
        }
        for e in &self.tree {
            let ok = match e.data {
                Item::Body(s) => self.bodies.get(s as usize).is_some_and(|b| !b.is_hidden() && b.rect() == *e.geom()),
                Item::Wire(s) => self.wires.get(s as usize).is_some_and(|w| !w.is_hidden() && w.rect() == *e.geom()),
            };
            if !ok {
                return Err(format!("stale R-tree entry {:?}", e.data));
            }
        }
        Ok(())
    }
}
