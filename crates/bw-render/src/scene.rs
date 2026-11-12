//! CPU-side scene: slot-stable instance arrays + an R-tree for picking.
//!
//! Every component owns one *body slot* and every wire segment one *wire
//! slot*. Slots never move: an edit rewrites the affected slots in place,
//! deleted slots are hidden and go to a free list, and new objects reuse free
//! slots first. The renderer mirrors the arrays in GPU buffers and only
//! re-uploads the slots listed in the dirty sets ([`Scene::take_dirty`]), so
//! an edit costs microseconds instead of a full rebuild.
//!
//! Net levels are addressed by body slot as well: the `net` field of an
//! instance is the body slot of the component driving it, so the slot of a
//! component that does not change never needs re-uploading when other parts
//! of the circuit are edited.
//!
//! A full [`Scene::build`] is only needed when a whole circuit is replaced
//! (file load, stress test); it runs on a worker thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bw_core::{Circuit, Component, ComponentId, ComponentKind, GateOp, Rotation, WireId};
use bytemuck::{Pod, Zeroable};
use glam::Vec2;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};

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
    /// Packed: kind code (bits 0..8, see [`kind_code`]), input count (8..16)
    /// and rotation (16..18); [`HIDDEN_KIND`] for a free slot. See
    /// [`body_kind`].
    pub kind: u32,
    /// Body slot whose level colours this body (own slot, or the driver of
    /// an LED), or [`NO_NET`].
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
    /// Body slot of the driving component, or [`NO_NET`].
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
    Body(u32),
    Wire(u32),
}

/// A model object whose render data must be refreshed after an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Added, moved, rotated, re-kinded or removed component (its wires are
    /// refreshed too).
    Component(ComponentId),
    /// Added or removed wire.
    Wire(WireId),
}

/// Slots changed since the renderer last synchronised.
#[derive(Debug, Default)]
pub struct Dirty {
    pub bodies: Vec<u32>,
    pub wires: Vec<u32>,
}

type Entry = GeomWithData<Rectangle<[f32; 2]>, Item>;

#[derive(Debug, Clone)]
struct WireRec {
    from: ComponentId,
    to: ComponentId,
    segs: Vec<u32>,
}

/// Shader kind code of a component.
#[must_use]
pub fn kind_code(kind: &ComponentKind) -> u32 {
    match kind {
        ComponentKind::Gate { op, .. } => match op {
            GateOp::And => 0,
            GateOp::Or => 1,
            GateOp::Nand => 2,
            GateOp::Nor => 3,
            GateOp::Xor => 4,
            GateOp::Xnor => 5,
        },
        ComponentKind::Not => 6,
        ComponentKind::Switch { .. } => 7,
        ComponentKind::Clock { .. } => 8,
        ComponentKind::Led => 9,
    }
}

/// Packed `kind` field of a component's body instance: the shader picks the
/// symbol mesh (kind × input count × style) and the rotation from it.
#[must_use]
pub fn body_kind(c: &Component) -> u32 {
    let rot = match c.rotation {
        Rotation::R0 => 0,
        Rotation::R90 => 1,
        Rotation::R180 => 2,
        Rotation::R270 => 3,
    };
    kind_code(&c.kind) | (u32::from(c.kind.input_count()) << 8) | (rot << 16)
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

fn slot_u32(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

static NEXT_SCENE_ID: AtomicU64 = AtomicU64::new(1);

pub struct Scene {
    /// Unique per scene object; the renderer re-uploads when it changes.
    id: u64,
    /// Circuit revision the scene reflects.
    revision: u64,
    bodies: Vec<BodyInstance>,
    body_ids: Vec<Option<ComponentId>>,
    body_free: Vec<u32>,
    body_slot: HashMap<ComponentId, u32>,
    wires: Vec<WireInstance>,
    wire_owner: Vec<Option<WireId>>,
    wire_free: Vec<u32>,
    wire_rec: HashMap<WireId, WireRec>,
    /// Wires attached to each component (either end).
    adj: HashMap<ComponentId, Vec<WireId>>,
    tree: RTree<Entry>,
    dirty: Dirty,
    /// Bumps whenever a body slot is assigned or freed (net mapping changes).
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

/// Wire segments of a circuit wire, or `None` if an end is missing.
fn wire_segments(circuit: &Circuit, w: &bw_core::Wire) -> Option<Vec<([f32; 2], [f32; 2])>> {
    let src = circuit.component(w.from.component)?;
    let dst = circuit.component(w.to.component)?;
    let a = src.pin_pos(w.from.pin)?;
    let b = dst.pin_pos(w.to.pin)?;
    let mut out = Vec::with_capacity(w.via.len() + 1);
    let mut prev = a;
    for &p in w.via.iter().chain(std::iter::once(&b)) {
        out.push(([f(prev.x), f(prev.y)], [f(p.x), f(p.y)]));
        prev = p;
    }
    Some(out)
}

fn body_geom(c: &Component, kind: u32, net: u32) -> BodyInstance {
    let r = c.bounds();
    let min = [f(r.min.x), f(r.min.y)];
    let max = [f(r.max.x), f(r.max.y)];
    BodyInstance { min, size: [max[0] - min[0], max[1] - min[1]], kind, net }
}

impl Scene {
    fn empty(revision: u64) -> Self {
        Self {
            id: NEXT_SCENE_ID.fetch_add(1, Ordering::Relaxed),
            revision,
            bodies: Vec::new(),
            body_ids: Vec::new(),
            body_free: Vec::new(),
            body_slot: HashMap::new(),
            wires: Vec::new(),
            wire_owner: Vec::new(),
            wire_free: Vec::new(),
            wire_rec: HashMap::new(),
            adj: HashMap::new(),
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
        let nw = circuit.wire_count();
        s.bodies.reserve(nc);
        s.body_ids.reserve(nc);
        s.body_slot.reserve(nc);
        s.wires.reserve(nw * 2);
        s.wire_owner.reserve(nw * 2);
        s.wire_rec.reserve(nw);
        s.adj.reserve(nc);
        let mut entries: Vec<Entry> = Vec::with_capacity(nc + nw * 2);
        let mut lo = Vec2::splat(f32::MAX);
        let mut hi = Vec2::splat(f32::MIN);

        for c in circuit.components() {
            let slot = slot_u32(s.bodies.len());
            let net = if c.kind.output_count() > 0 { slot } else { NO_NET };
            let b = body_geom(c, body_kind(c), net);
            entries.push(Entry::new(b.rect(), Item::Body(slot)));
            lo = lo.min(Vec2::from(b.min));
            hi = hi.max(Vec2::from(b.min) + Vec2::from(b.size));
            s.bodies.push(b);
            s.body_ids.push(Some(c.id));
            s.body_slot.insert(c.id, slot);
        }
        for w in circuit.wires() {
            let Some(segs) = wire_segments(circuit, w) else { continue };
            let net = s.body_slot.get(&w.from.component).copied().unwrap_or(NO_NET);
            let mut rec = WireRec { from: w.from.component, to: w.to.component, segs: Vec::with_capacity(segs.len()) };
            for (a, b) in segs {
                let slot = slot_u32(s.wires.len());
                let inst = WireInstance::new(a, b, net);
                entries.push(Entry::new(inst.rect(), Item::Wire(slot)));
                lo = lo.min(Vec2::from(a).min(Vec2::from(b)));
                hi = hi.max(Vec2::from(a).max(Vec2::from(b)));
                s.wires.push(inst);
                s.wire_owner.push(Some(w.id));
                rec.segs.push(slot);
            }
            s.adj.entry(w.from.component).or_default().push(w.id);
            if w.to.component != w.from.component {
                s.adj.entry(w.to.component).or_default().push(w.id);
            }
            // An LED shows the level of the net driving its input.
            if let Some(&led) = s.body_slot.get(&w.to.component) {
                if unpack_code(s.bodies[led as usize].kind) == kind_code(&ComponentKind::Led) {
                    s.bodies[led as usize].net = net;
                }
            }
            s.wire_rec.insert(w.id, rec);
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

    /// Component occupying a body slot.
    #[must_use]
    pub fn body_id(&self, slot: u32) -> Option<ComponentId> {
        self.body_ids.get(slot as usize).copied().flatten()
    }

    /// Body slot of a component.
    #[must_use]
    pub fn body_slot(&self, id: ComponentId) -> Option<u32> {
        self.body_slot.get(&id).copied()
    }

    /// Wire slots of a wire's segments.
    #[must_use]
    pub fn wire_slots(&self, id: WireId) -> Option<&[u32]> {
        self.wire_rec.get(&id).map(|r| r.segs.as_slice())
    }

    /// Changes whenever body slots are assigned or freed.
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

    /// Topmost body containing a world point.
    #[must_use]
    pub fn pick_body(&self, p: Vec2) -> Option<ComponentId> {
        let mut hit: Option<u32> = None;
        self.query(p, p, |it| {
            if let Item::Body(i) = it {
                // Deterministic: highest slot wins (drawn last).
                hit = Some(hit.map_or(i, |h| h.max(i)));
            }
        });
        hit.and_then(|s| self.body_id(s))
    }

    // ---- incremental updates -----------------------------------------------

    /// Brings the scene in line with `circuit` for the given changes. Cost is
    /// proportional to the touched objects (and their wires), not to the
    /// circuit size.
    pub fn apply(&mut self, circuit: &Circuit, changes: &[Change], revision: u64) {
        for ch in changes {
            match *ch {
                Change::Component(id) => self.sync_component(circuit, id),
                Change::Wire(id) => self.sync_wire(circuit, id),
            }
        }
        self.revision = revision;
    }

    fn extend_bounds(&mut self, lo: Vec2, hi: Vec2) {
        if self.instance_count() <= 1 && self.bounds == (Vec2::ZERO, Vec2::ZERO) {
            self.bounds = (lo, hi);
        } else {
            self.bounds = (self.bounds.0.min(lo), self.bounds.1.max(hi));
        }
    }

    /// Net (driver body slot) shown by a component's body.
    fn body_net(&self, circuit: &Circuit, c: &Component, slot: u32) -> u32 {
        if c.kind.output_count() > 0 {
            return slot;
        }
        // LED (no output): the component driving its input, per our wires.
        self.adj
            .get(&c.id)
            .into_iter()
            .flatten()
            .filter_map(|w| self.wire_rec.get(w))
            .find(|r| r.to == c.id && circuit.component(r.from).is_some())
            .and_then(|r| self.body_slot.get(&r.from).copied())
            .unwrap_or(NO_NET)
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

    fn alloc_body(&mut self, id: ComponentId) -> u32 {
        self.layout += 1;
        let slot = if let Some(s) = self.body_free.pop() {
            s
        } else {
            self.bodies.push(BodyInstance::HIDDEN);
            self.body_ids.push(None);
            slot_u32(self.bodies.len() - 1)
        };
        self.body_ids[slot as usize] = Some(id);
        self.body_slot.insert(id, slot);
        slot
    }

    fn alloc_wire(&mut self, id: WireId) -> u32 {
        let slot = if let Some(s) = self.wire_free.pop() {
            s
        } else {
            self.wires.push(WireInstance::HIDDEN);
            self.wire_owner.push(None);
            slot_u32(self.wires.len() - 1)
        };
        self.wire_owner[slot as usize] = Some(id);
        slot
    }

    fn free_wire_slot(&mut self, slot: u32) {
        self.set_wire(slot, WireInstance::HIDDEN);
        self.wire_owner[slot as usize] = None;
        self.wire_free.push(slot);
    }

    fn sync_component(&mut self, circuit: &Circuit, id: ComponentId) {
        if let Some(c) = circuit.component(id) {
            let slot = match self.body_slot.get(&id) {
                Some(&s) => s,
                None => self.alloc_body(id),
            };
            let b = body_geom(c, body_kind(c), self.body_net(circuit, c, slot));
            self.set_body(slot, b);
            // Pin positions may have moved: refresh every attached wire.
            let attached = self.adj.get(&id).cloned().unwrap_or_default();
            for w in attached {
                self.sync_wire(circuit, w);
            }
        } else {
            if let Some(slot) = self.body_slot.remove(&id) {
                self.layout += 1;
                self.set_body(slot, BodyInstance::HIDDEN);
                self.body_ids[slot as usize] = None;
                self.body_free.push(slot);
            }
            // The circuit removed the attached wires together with the
            // component; drop their segments (and fix LEDs they drove).
            let attached = self.adj.remove(&id).unwrap_or_default();
            for w in attached {
                self.sync_wire(circuit, w);
            }
        }
    }

    fn unlink(&mut self, comp: ComponentId, wire: WireId) {
        if let Some(list) = self.adj.get_mut(&comp) {
            list.retain(|&w| w != wire);
            if list.is_empty() {
                self.adj.remove(&comp);
            }
        }
    }

    fn sync_wire(&mut self, circuit: &Circuit, id: WireId) {
        let current = circuit.wire(id).and_then(|w| wire_segments(circuit, w).map(|segs| (w, segs)));
        let old = self.wire_rec.remove(&id);
        let mut led_targets: Vec<ComponentId> = Vec::new();
        match (old, current) {
            (Some(rec), None) => {
                for &s in &rec.segs {
                    self.free_wire_slot(s);
                }
                self.unlink(rec.from, id);
                self.unlink(rec.to, id);
                led_targets.push(rec.to);
            }
            (old, Some((w, segs))) => {
                let net = self.body_slot.get(&w.from.component).copied().unwrap_or(NO_NET);
                let mut slots = old.as_ref().map(|r| r.segs.clone()).unwrap_or_default();
                while slots.len() > segs.len() {
                    if let Some(s) = slots.pop() {
                        self.free_wire_slot(s);
                    }
                }
                while slots.len() < segs.len() {
                    slots.push(self.alloc_wire(id));
                }
                for (&slot, &(a, b)) in slots.iter().zip(&segs) {
                    let inst = WireInstance::new(a, b, net);
                    if self.wires[slot as usize] != inst {
                        self.set_wire(slot, inst);
                    }
                }
                if old.is_none() {
                    self.adj.entry(w.from.component).or_default().push(id);
                    if w.to.component != w.from.component {
                        self.adj.entry(w.to.component).or_default().push(id);
                    }
                    led_targets.push(w.to.component);
                }
                self.wire_rec.insert(id, WireRec { from: w.from.component, to: w.to.component, segs: slots });
            }
            (None, None) => {}
        }
        // A wire into an LED changes the net the LED body shows.
        for t in led_targets {
            self.refresh_body_net(circuit, t);
        }
    }

    fn refresh_body_net(&mut self, circuit: &Circuit, id: ComponentId) {
        let (Some(c), Some(&slot)) = (circuit.component(id), self.body_slot.get(&id)) else { return };
        let net = self.body_net(circuit, c, slot);
        let mut b = self.bodies[slot as usize];
        if b.net != net {
            b.net = net;
            // Geometry unchanged: no R-tree work.
            self.bodies[slot as usize] = b;
            self.dirty.bodies.push(slot);
        }
    }

    /// Checks every internal invariant against `circuit` (tests / debug).
    ///
    /// # Errors
    /// Describes the first inconsistency found.
    #[doc(hidden)]
    #[allow(clippy::float_cmp, reason = "both sides come from the same integer→f32 conversion")]
    pub fn validate(&self, circuit: &Circuit) -> Result<(), String> {
        let fresh = Self::build(circuit, self.revision);
        // Same live objects with identical geometry (slots may differ).
        if self.body_slot.len() != circuit.component_count() {
            return Err(format!("{} bodies for {} components", self.body_slot.len(), circuit.component_count()));
        }
        let net_comp = |scene: &Self, net: u32| if net == NO_NET { None } else { scene.body_id(net) };
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
            if self.body_ids[mine as usize] != Some(c.id) {
                return Err(format!("slot {mine} owner mismatch"));
            }
        }
        if self.wire_rec.len() != fresh.wire_rec.len() {
            return Err(format!("{} wires vs {}", self.wire_rec.len(), fresh.wire_rec.len()));
        }
        for (id, rec) in &fresh.wire_rec {
            let mine = self.wire_rec.get(id).ok_or(format!("{id:?} missing"))?;
            if mine.segs.len() != rec.segs.len() {
                return Err(format!("{id:?} segment count differs"));
            }
            for (&m, &t) in mine.segs.iter().zip(&rec.segs) {
                let (a, b) = (self.wires[m as usize], fresh.wires[t as usize]);
                if a.a != b.a || a.b != b.b || net_comp(self, a.net) != net_comp(&fresh, b.net) || a.flags != 0 {
                    return Err(format!("segment of {id:?} differs: {a:?} vs {b:?}"));
                }
                if self.wire_owner[m as usize] != Some(*id) {
                    return Err(format!("wire slot {m} owner mismatch"));
                }
            }
        }
        // Free slots are hidden and unowned.
        for &s in &self.body_free {
            if !self.bodies[s as usize].is_hidden() || self.body_ids[s as usize].is_some() {
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
