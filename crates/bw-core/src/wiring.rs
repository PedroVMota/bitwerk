//! The wire graph: nodes joined by orthogonal segments.
//!
//! A net's wiring is a graph whose **nodes** are component pins, junctions
//! (solder dots) and bend vertices, and whose **segments** are horizontal or
//! vertical lines between two nodes. Only segment *ends* connect: two wires
//! that merely cross or overlap are not joined. The electrical net is the
//! connected component of the graph; a junction is a free node where three
//! or more segments meet (drawn as a dot automatically).
//!
//! Invariants (kept by every edit and checked when loading):
//! * every segment joins two different nodes and is horizontal or vertical
//!   (zero length only between two pins that ended up on the same point);
//! * a free node (junction / bend) has at least two segments; a pin node has
//!   at least one; a pin has at most one node, which sits on the pin;
//! * per net, at most one non-tri-state driver, or only tri-state drivers.
//!
//! After every edit the touched part of the graph is normalised: dangling
//! free ends are trimmed, a bend whose two segments are collinear is merged
//! away, zero-length segments collapse into one node and duplicate segments
//! between the same two nodes are dropped. All of this works on the
//! adjacency of the touched nodes (and, for connectivity checks, on the nets
//! involved), never on the whole circuit.
//!
//! Node and segment ids index slabs and are reused (LIFO free lists). The
//! UI and the simulator replay the same edits, so both copies allocate the
//! same ids.

#![allow(clippy::many_single_char_names, reason = "graph code: nodes, segments, points")]

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::circuit::{Circuit, CircuitError};
use crate::component::{PinDir, PinRef};
use crate::geom::GridPos;

/// Stable identifier of a wire graph node (pin, junction or bend).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u32);

/// Stable identifier of a wire segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SegmentId(pub u32);

/// Adjacency list of a node: inline up to four segments (the common case,
/// no allocation), a vector beyond.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Adj {
    Small { len: u8, ids: [SegmentId; 4] },
    Big(Vec<SegmentId>),
}

impl Default for Adj {
    fn default() -> Self {
        Self::Small { len: 0, ids: [SegmentId(0); 4] }
    }
}

impl Adj {
    fn as_slice(&self) -> &[SegmentId] {
        match self {
            Self::Small { len, ids } => &ids[..usize::from(*len)],
            Self::Big(v) => v,
        }
    }

    fn push(&mut self, s: SegmentId) {
        match self {
            Self::Small { len, ids } if usize::from(*len) < ids.len() => {
                ids[usize::from(*len)] = s;
                *len += 1;
            }
            Self::Small { ids, .. } => {
                let mut v = ids.to_vec();
                v.push(s);
                *self = Self::Big(v);
            }
            Self::Big(v) => v.push(s),
        }
    }

    fn remove(&mut self, s: SegmentId) {
        match self {
            Self::Small { len, ids } => {
                let n = usize::from(*len);
                if let Some(i) = ids[..n].iter().position(|&x| x == s) {
                    ids.copy_within(i + 1..n, i);
                    *len -= 1;
                }
            }
            Self::Big(v) => {
                if let Some(i) = v.iter().position(|&x| x == s) {
                    v.remove(i);
                }
            }
        }
    }
}

/// A node of the wire graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireNode {
    pub pos: GridPos,
    /// The pin this node sits on, or `None` for a junction / bend.
    pub pin: Option<PinRef>,
    adj: Adj,
}

impl WireNode {
    /// Segments ending at this node.
    #[must_use]
    pub fn segments(&self) -> &[SegmentId] {
        self.adj.as_slice()
    }

    #[must_use]
    pub fn degree(&self) -> usize {
        self.segments().len()
    }

    /// Junction / bend (not a pin)?
    #[must_use]
    pub fn is_free(&self) -> bool {
        self.pin.is_none()
    }

    /// Is a solder dot drawn here? Where three or more wires meet: a free
    /// node with three or more segments, or a pin with two or more (the pin
    /// stub counts as the third).
    #[must_use]
    pub fn is_junction(&self) -> bool {
        if self.is_free() { self.degree() >= 3 } else { self.degree() >= 2 }
    }
}

/// A straight, horizontal or vertical wire segment between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub a: NodeId,
    pub b: NodeId,
}

impl Segment {
    /// The end that is not `n`.
    #[must_use]
    pub fn other(&self, n: NodeId) -> NodeId {
        if self.a == n { self.b } else { self.a }
    }
}

/// One end of a wire being drawn ([`crate::Edit::AddWire`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireEnd {
    /// A component pin (its node is created on first use).
    Pin(PinRef),
    /// An existing node: pin, junction or bend (a bend becomes a junction).
    Node(NodeId),
    /// A point on an existing segment: the segment is split there and the
    /// new node becomes a junction.
    OnSegment(SegmentId, GridPos),
}

/// Pins of every net, as a CSR: net `i` has `pins[start[i]..start[i + 1]]`.
/// Only nets with at least one pin are listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Nets {
    pub start: Vec<u32>,
    pub pins: Vec<PinRef>,
}

impl Nets {
    #[must_use]
    pub fn len(&self) -> usize {
        self.start.len().saturating_sub(1)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pins of net `i`.
    #[must_use]
    pub fn net(&self, i: usize) -> &[PinRef] {
        &self.pins[self.start[i] as usize..self.start[i + 1] as usize]
    }
}

/// On-disk position of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeAt {
    /// On a component pin (the position follows the pin).
    Pin(PinRef),
    /// A junction or bend at a grid point.
    Free(GridPos),
}

/// On-disk node record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRecord {
    pub id: NodeId,
    pub at: NodeAt,
}

/// On-disk segment record (node ids of its two ends).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentRecord {
    pub a: NodeId,
    pub b: NodeId,
}

const fn orthogonal(p: GridPos, q: GridPos) -> bool {
    p.x == q.x || p.y == q.y
}

/// `p` lies on the closed segment `a`–`b` (which is axis-aligned).
fn on_segment(p: GridPos, a: GridPos, b: GridPos) -> bool {
    if a.x == b.x {
        p.x == a.x && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
    } else if a.y == b.y {
        p.y == a.y && p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x)
    } else {
        false
    }
}

fn collinear(p: GridPos, n: GridPos, q: GridPos) -> bool {
    (p.x == n.x && n.x == q.x) || (p.y == n.y && n.y == q.y)
}

/// Orthogonal route through `points`: consecutive duplicates are dropped and
/// an L corner (horizontal first) is inserted between two points that are
/// not on a common horizontal or vertical line.
#[must_use]
pub fn route(points: &[GridPos]) -> Vec<GridPos> {
    let mut out: Vec<GridPos> = Vec::with_capacity(points.len() * 2);
    for &p in points {
        if let Some(&last) = out.last() {
            if last == p {
                continue;
            }
            if !orthogonal(last, p) {
                out.push(GridPos::new(p.x, last.y));
            }
        }
        out.push(p);
    }
    // Drop corners that ended up collinear with their neighbours.
    let mut i = 1;
    while i + 1 < out.len() {
        if collinear(out[i - 1], out[i], out[i + 1]) && between(out[i - 1], out[i], out[i + 1]) {
            out.remove(i);
        } else {
            i += 1;
        }
    }
    out
}

/// `n` lies between `p` and `q` on their common line.
fn between(p: GridPos, n: GridPos, q: GridPos) -> bool {
    on_segment(n, p, q)
}

fn u32_of(n: usize) -> Result<u32, CircuitError> {
    u32::try_from(n).ok().filter(|&v| v < u32::MAX).ok_or(CircuitError::IdSpaceExhausted)
}

/// Changes recorded while an edit runs (becomes [`crate::Applied`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WireLog {
    pub segments: Vec<SegmentId>,
    pub nodes: Vec<NodeId>,
    pub pins: Vec<PinRef>,
}

/// A resolved wire end, before anything is mutated.
#[derive(Debug, Clone, Copy)]
enum End {
    /// A pin without a node yet.
    NewPin(PinRef, GridPos),
    Node(NodeId),
    Split(SegmentId, GridPos),
}

impl Circuit {
    // ---- read access -------------------------------------------------

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&WireNode> {
        self.nodes.get(id.0 as usize).and_then(Option::as_ref)
    }

    #[must_use]
    pub fn segment(&self, id: SegmentId) -> Option<&Segment> {
        self.segs.get(id.0 as usize).and_then(Option::as_ref)
    }

    /// Node ids in ascending order with their nodes.
    pub fn nodes(&self) -> impl Iterator<Item = (NodeId, &WireNode)> + Clone {
        self.nodes.iter().enumerate().filter_map(|(i, n)| Some((NodeId(u32::try_from(i).ok()?), n.as_ref()?)))
    }

    /// Segment ids in ascending order with their segments.
    pub fn segments(&self) -> impl Iterator<Item = (SegmentId, &Segment)> + Clone {
        self.segs.iter().enumerate().filter_map(|(i, s)| Some((SegmentId(u32::try_from(i).ok()?), s.as_ref()?)))
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.node_live
    }

    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.seg_live
    }

    /// Number of solder dots (see [`WireNode::is_junction`]); O(nodes).
    #[must_use]
    pub fn junction_count(&self) -> usize {
        self.nodes().filter(|(_, n)| n.is_junction()).count()
    }

    /// Upper bound (exclusive) of node ids, for dense side tables.
    #[must_use]
    pub fn node_id_bound(&self) -> usize {
        self.nodes.len()
    }

    /// Upper bound (exclusive) of segment ids.
    #[must_use]
    pub fn segment_id_bound(&self) -> usize {
        self.segs.len()
    }

    /// The node sitting on a pin, if the pin is wired.
    #[must_use]
    pub fn pin_node(&self, pin: PinRef) -> Option<NodeId> {
        self.pin_node.get(&pin).copied()
    }

    /// End points of a segment.
    #[must_use]
    pub fn segment_ends(&self, id: SegmentId) -> Option<(GridPos, GridPos)> {
        let s = self.segment(id)?;
        Some((self.node(s.a)?.pos, self.node(s.b)?.pos))
    }

    /// Visits the net (connected component) containing `start`: every node
    /// and every segment exactly once. Cost: the size of that net.
    pub fn visit_net(&self, start: NodeId, mut node: impl FnMut(NodeId, &WireNode), mut seg: impl FnMut(SegmentId)) {
        if self.node(start).is_none() {
            return;
        }
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut stack = vec![start];
        seen.insert(start);
        while let Some(n) = stack.pop() {
            let Some(w) = self.node(n) else { continue };
            node(n, w);
            for &s in w.segments() {
                let Some(sg) = self.segment(s) else { continue };
                if sg.a == n {
                    seg(s);
                }
                let o = sg.other(n);
                if seen.insert(o) {
                    stack.push(o);
                }
            }
        }
    }

    /// Output pins of the net containing `start` (sorted).
    #[must_use]
    pub fn net_drivers(&self, start: NodeId) -> Vec<PinRef> {
        let mut d = Vec::new();
        self.visit_net(start, |_, n| d.extend(n.pin.filter(|&p| self.is_output(p))), |_| {});
        d.sort_unstable();
        d
    }

    /// Every pin on the same net as `pin` (just `pin` if it is not wired).
    #[must_use]
    pub fn net_pins(&self, pin: PinRef) -> Vec<PinRef> {
        let Some(n) = self.pin_node(pin) else { return vec![pin] };
        let mut v = Vec::new();
        self.visit_net(n, |_, w| v.extend(w.pin), |_| {});
        v.sort_unstable();
        v
    }

    /// Output pins on the same net as `pin` (sorted).
    #[must_use]
    pub fn drivers_of(&self, pin: PinRef) -> Vec<PinRef> {
        match self.pin_node(pin) {
            Some(n) => self.net_drivers(n),
            None if self.is_output(pin) => vec![pin],
            None => Vec::new(),
        }
    }

    /// The pins of every net (connected component with at least one pin).
    /// O(nodes + segments); used by the simulator's compilation.
    #[must_use]
    pub fn nets(&self) -> Nets {
        let mut nets = Nets { start: vec![0], pins: Vec::with_capacity(self.pin_node.len()) };
        let mut seen = vec![false; self.nodes.len()];
        let mut stack = Vec::new();
        for (i, slot) in self.nodes.iter().enumerate() {
            if slot.is_none() || seen[i] {
                continue;
            }
            let before = nets.pins.len();
            seen[i] = true;
            stack.push(i);
            while let Some(n) = stack.pop() {
                let Some(w) = &self.nodes[n] else { continue };
                if let Some(p) = w.pin {
                    nets.pins.push(p);
                }
                for &s in w.segments() {
                    let Some(sg) = self.segment(s) else { continue };
                    let o = sg.other(NodeId(u32::try_from(n).unwrap_or(u32::MAX))).0 as usize;
                    if o < seen.len() && !seen[o] {
                        seen[o] = true;
                        stack.push(o);
                    }
                }
            }
            if nets.pins.len() > before {
                nets.start.push(u32::try_from(nets.pins.len()).unwrap_or(u32::MAX));
            }
        }
        nets
    }

    fn is_output(&self, p: PinRef) -> bool {
        self.components.get(&p.component).and_then(|c| c.pin_dir(p.pin)) == Some(PinDir::Output)
    }

    fn is_tristate(&self, p: PinRef) -> bool {
        self.components
            .get(&p.component)
            .is_some_and(|c| c.kind.output_tristate(p.pin.saturating_sub(c.kind.input_count())))
    }

    /// Checks the driver rule for a set of output pins on one net.
    pub(crate) fn check_drivers(&self, drivers: &[PinRef]) -> Result<(), CircuitError> {
        if drivers.len() < 2 {
            return Ok(());
        }
        if let Some(&plain) = drivers.iter().find(|&&p| !self.is_tristate(p)) {
            let other = drivers.iter().copied().find(|&p| p != plain).unwrap_or(plain);
            let (a, b) = if plain < other { (plain, other) } else { (other, plain) };
            return Err(CircuitError::TwoDrivers(a, b));
        }
        Ok(())
    }

    // ---- low-level mutation (logged) -----------------------------------

    fn node_mut(&mut self, id: NodeId) -> Option<&mut WireNode> {
        self.nodes.get_mut(id.0 as usize).and_then(Option::as_mut)
    }

    fn pos(&self, id: NodeId) -> GridPos {
        self.node(id).map_or_else(GridPos::default, |n| n.pos)
    }

    fn adj(&self, id: NodeId) -> Vec<SegmentId> {
        self.node(id).map(|n| n.segments().to_vec()).unwrap_or_default()
    }

    /// Room for `nodes` more nodes and `segs` more segments?
    fn ensure_room(&self, nodes: usize, segs: usize) -> Result<(), CircuitError> {
        let room = |len: usize, free: usize, add: usize| add <= free || u32_of(len + add - free).is_ok();
        if room(self.nodes.len(), self.node_free.len(), nodes) && room(self.segs.len(), self.seg_free.len(), segs) {
            Ok(())
        } else {
            Err(CircuitError::IdSpaceExhausted)
        }
    }

    fn alloc_node(&mut self, pos: GridPos, pin: Option<PinRef>) -> NodeId {
        let node = WireNode { pos, pin, adj: Adj::default() };
        let id = if let Some(i) = self.node_free.pop() {
            self.nodes[i as usize] = Some(node);
            NodeId(i)
        } else {
            self.nodes.push(Some(node));
            NodeId(u32::try_from(self.nodes.len() - 1).unwrap_or(u32::MAX))
        };
        self.node_live += 1;
        if let Some(p) = pin {
            self.pin_node.insert(p, id);
            self.log.pins.push(p);
        }
        self.log.nodes.push(id);
        id
    }

    fn free_node(&mut self, id: NodeId) {
        let Some(n) = self.nodes.get_mut(id.0 as usize).and_then(Option::take) else { return };
        if let Some(p) = n.pin {
            self.pin_node.remove(&p);
            self.log.pins.push(p);
        }
        self.node_free.push(id.0);
        self.node_live -= 1;
        self.log.nodes.push(id);
    }

    fn alloc_seg(&mut self, a: NodeId, b: NodeId) -> SegmentId {
        let seg = Segment { a, b };
        let id = if let Some(i) = self.seg_free.pop() {
            self.segs[i as usize] = Some(seg);
            SegmentId(i)
        } else {
            self.segs.push(Some(seg));
            SegmentId(u32::try_from(self.segs.len() - 1).unwrap_or(u32::MAX))
        };
        self.seg_live += 1;
        for n in [a, b] {
            if let Some(w) = self.node_mut(n) {
                w.adj.push(id);
            }
            self.log.nodes.push(n);
        }
        self.log.segments.push(id);
        id
    }

    fn free_seg(&mut self, id: SegmentId) {
        let Some(s) = self.segs.get_mut(id.0 as usize).and_then(Option::take) else { return };
        for n in [s.a, s.b] {
            if let Some(w) = self.node_mut(n) {
                w.adj.remove(id);
            }
            self.log.nodes.push(n);
        }
        self.seg_free.push(id.0);
        self.seg_live -= 1;
        self.log.segments.push(id);
    }

    /// Moves the `from` end of segment `s` to node `to`.
    fn retarget(&mut self, s: SegmentId, from: NodeId, to: NodeId) {
        let Some(seg) = self.segs.get_mut(s.0 as usize).and_then(Option::as_mut) else { return };
        if seg.a == from {
            seg.a = to;
        } else if seg.b == from {
            seg.b = to;
        } else {
            return;
        }
        if let Some(w) = self.node_mut(from) {
            w.adj.remove(s);
        }
        if let Some(w) = self.node_mut(to) {
            w.adj.push(s);
        }
        self.log.nodes.push(from);
        self.log.nodes.push(to);
        self.log.segments.push(s);
    }

    fn set_pos(&mut self, n: NodeId, pos: GridPos) {
        let segs = match self.node_mut(n) {
            Some(w) if w.pos != pos => {
                w.pos = pos;
                w.segments().to_vec()
            }
            _ => return,
        };
        self.log.nodes.push(n);
        self.log.segments.extend(segs);
    }

    /// Segment between `a` and `b`, other than `except`.
    fn seg_between(&self, a: NodeId, b: NodeId, except: &[SegmentId]) -> Option<SegmentId> {
        self.node(a)?
            .segments()
            .iter()
            .copied()
            .find(|&s| !except.contains(&s) && self.segment(s).is_some_and(|sg| sg.other(a) == b))
    }

    /// Splits segment `s` at `p` (strictly inside it): returns the new node
    /// and the new segment (the `p`–`b` half; `s` keeps the `a` half).
    fn split(&mut self, s: SegmentId, p: GridPos) -> (NodeId, SegmentId) {
        let b = self.segment(s).map_or(NodeId(0), |sg| sg.b);
        let j = self.alloc_node(p, None);
        self.retarget(s, b, j);
        let t = self.alloc_seg(j, b);
        (j, t)
    }

    // ---- normalisation ---------------------------------------------------

    /// Merges free node `u` into node `v` (they are on the same point).
    fn merge_into(&mut self, u: NodeId, v: NodeId, work: &mut Vec<NodeId>) {
        for s in self.adj(u) {
            let Some(w) = self.segment(s).map(|sg| sg.other(u)) else { continue };
            if w == v || self.seg_between(v, w, &[s]).is_some() {
                self.free_seg(s);
            } else {
                self.retarget(s, u, v);
            }
            work.push(w);
        }
        self.free_node(u);
        work.push(v);
    }

    /// Restores the invariants around the given nodes (see the module docs).
    pub(crate) fn cleanup(&mut self, mut work: Vec<NodeId>) {
        while let Some(n) = work.pop() {
            let Some(node) = self.node(n) else { continue };
            let pos = node.pos;
            let free = node.is_free();
            let adj = node.segments().to_vec();
            // Zero-length segments: one point, one node (pins never merge).
            let mut merged = false;
            for &s in &adj {
                let Some(m) = self.segment(s).map(|sg| sg.other(n)) else { continue };
                if self.pos(m) != pos {
                    continue;
                }
                if free {
                    self.merge_into(n, m, &mut work);
                    merged = true;
                    break;
                }
                if self.node(m).is_some_and(WireNode::is_free) {
                    self.merge_into(m, n, &mut work);
                    merged = true;
                    break;
                }
            }
            if merged {
                continue;
            }
            // Duplicate segments to the same neighbour.
            let mut dup = None;
            for (i, &s) in adj.iter().enumerate() {
                let Some(m) = self.segment(s).map(|sg| sg.other(n)) else { continue };
                if adj[..i].iter().any(|&t| self.segment(t).is_some_and(|sg| sg.other(n) == m)) {
                    dup = Some((s, m));
                    break;
                }
            }
            if let Some((s, m)) = dup {
                self.free_seg(s);
                work.push(m);
                work.push(n);
                continue;
            }
            match (free, adj.len()) {
                (_, 0) => self.free_node(n),
                (true, 1) => {
                    let m = self.segment(adj[0]).map(|sg| sg.other(n));
                    self.free_seg(adj[0]);
                    self.free_node(n);
                    work.extend(m);
                }
                (true, 2) => {
                    let (s1, s2) = (adj[0], adj[1]);
                    let (Some(p), Some(q)) =
                        (self.segment(s1).map(|sg| sg.other(n)), self.segment(s2).map(|sg| sg.other(n)))
                    else {
                        continue;
                    };
                    if collinear(self.pos(p), pos, self.pos(q)) {
                        // A straight bend: one segment p–q (unless one exists).
                        if self.seg_between(p, q, &[s1, s2]).is_some() {
                            self.free_seg(s1);
                            self.free_seg(s2);
                        } else {
                            self.free_seg(s2);
                            self.retarget(s1, n, q);
                        }
                        self.free_node(n);
                        work.push(p);
                        work.push(q);
                    }
                }
                _ => {}
            }
        }
    }

    // ---- edits -------------------------------------------------------------

    fn resolve_end(&self, e: WireEnd) -> Result<End, CircuitError> {
        match e {
            WireEnd::Pin(p) => {
                let pos = self
                    .components
                    .get(&p.component)
                    .ok_or(CircuitError::NoSuchComponent(p.component))?
                    .pin_pos(p.pin)
                    .ok_or(CircuitError::NoSuchPin(p))?;
                Ok(self.pin_node(p).map_or(End::NewPin(p, pos), End::Node))
            }
            WireEnd::Node(n) => {
                self.node(n).ok_or(CircuitError::NoSuchNode(n))?;
                Ok(End::Node(n))
            }
            WireEnd::OnSegment(s, p) => {
                let sg = *self.segment(s).ok_or(CircuitError::NoSuchSegment(s))?;
                let (a, b) = (self.pos(sg.a), self.pos(sg.b));
                if !on_segment(p, a, b) {
                    return Err(CircuitError::NotOnSegment(p));
                }
                Ok(if p == a {
                    End::Node(sg.a)
                } else if p == b {
                    End::Node(sg.b)
                } else {
                    End::Split(s, p)
                })
            }
        }
    }

    fn end_pos(&self, e: End) -> GridPos {
        match e {
            End::NewPin(_, p) | End::Split(_, p) => p,
            End::Node(n) => self.pos(n),
        }
    }

    /// A node of the net an end belongs to, or the lone pin.
    fn end_seed(&self, e: End) -> Result<NodeId, PinRef> {
        match e {
            End::NewPin(p, _) => Err(p),
            End::Node(n) => Ok(n),
            End::Split(s, _) => Ok(self.segment(s).map_or(NodeId(0), |sg| sg.a)),
        }
    }

    /// Adds a wire along `via` between two ends (see [`crate::Edit::AddWire`]).
    pub(crate) fn add_wire(&mut self, from: WireEnd, to: WireEnd, via: &[GridPos]) -> Result<(), CircuitError> {
        if via.len() > crate::MAX_WIRE_VERTICES {
            return Err(CircuitError::TooManyVertices);
        }
        let (ea, eb) = (self.resolve_end(from)?, self.resolve_end(to)?);
        let same = match (ea, eb) {
            (End::Node(a), End::Node(b)) => a == b,
            (End::NewPin(a, _), End::NewPin(b, _)) => a == b,
            (End::Split(s, p), End::Split(t, q)) => s == t && p == q,
            _ => false,
        };
        if same {
            return Err(CircuitError::SameNode);
        }
        // Connectivity: merging two nets must keep the driver rule.
        let mut drivers = Vec::new();
        let mut joined = false;
        match self.end_seed(ea) {
            Ok(n) => {
                let target = self.end_seed(eb).ok();
                self.visit_net(
                    n,
                    |id, w| {
                        joined |= Some(id) == target;
                        drivers.extend(w.pin.filter(|&p| self.is_output(p)));
                    },
                    |_| {},
                );
            }
            Err(p) => drivers.extend(Some(p).filter(|&p| self.is_output(p))),
        }
        if !joined {
            match self.end_seed(eb) {
                Ok(n) => drivers.extend(self.net_drivers(n)),
                Err(p) => drivers.extend(Some(p).filter(|&p| self.is_output(p))),
            }
            drivers.sort_unstable();
            drivers.dedup();
            self.check_drivers(&drivers)?;
        }
        let mut pts = Vec::with_capacity(via.len() + 2);
        pts.push(self.end_pos(ea));
        pts.extend_from_slice(via);
        pts.push(self.end_pos(eb));
        let mut path = route(&pts);
        if path.len() == 1 {
            path.push(path[0]);
        }
        self.ensure_room(path.len() + 2, path.len() + 2)?;

        // Mutation (cannot fail from here on).
        let mut split_of: Option<(SegmentId, SegmentId)> = None;
        let mut materialize = |c: &mut Self, e: End| -> NodeId {
            match e {
                End::Node(n) => n,
                End::NewPin(p, pos) => c.alloc_node(pos, Some(p)),
                End::Split(s, p) => {
                    // The other end may have split this segment already.
                    let seg = match split_of {
                        Some((orig, new)) if orig == s => {
                            let (a, b) = c.segment_ends(s).unwrap_or_default();
                            if on_segment(p, a, b) { s } else { new }
                        }
                        _ => s,
                    };
                    let (a, b) = c.segment_ends(seg).unwrap_or_default();
                    if p == a {
                        return c.segment(seg).map_or(NodeId(0), |sg| sg.a);
                    }
                    if p == b {
                        return c.segment(seg).map_or(NodeId(0), |sg| sg.b);
                    }
                    let (j, t) = c.split(seg, p);
                    split_of = Some((seg, t));
                    j
                }
            }
        };
        let na = materialize(self, ea);
        let nb = materialize(self, eb);
        let mut prev = na;
        let mut touched = vec![na, nb];
        let last = path.len() - 1;
        for (i, &p) in path.iter().enumerate().skip(1) {
            let n = if i == last { nb } else { self.alloc_node(p, None) };
            self.alloc_seg(prev, n);
            touched.push(n);
            prev = n;
        }
        self.cleanup(touched);
        Ok(())
    }

    /// Removes a segment; dangling free ends are trimmed and straight bends
    /// merged.
    pub(crate) fn remove_segment(&mut self, s: SegmentId) -> Result<(), CircuitError> {
        let sg = *self.segment(s).ok_or(CircuitError::NoSuchSegment(s))?;
        self.free_seg(s);
        self.cleanup(vec![sg.a, sg.b]);
        Ok(())
    }

    /// Moves nodes, keeping every attached segment orthogonal by
    /// rubber-banding: a segment that would become diagonal either slides
    /// the bend at its far end (when that bend's other segment runs the
    /// other way) or gets a new bend (an L).
    pub(crate) fn move_nodes(&mut self, moves: &[(NodeId, GridPos)]) {
        let mut old: HashMap<NodeId, GridPos> = HashMap::with_capacity(moves.len());
        for &(n, p) in moves {
            if let Some(w) = self.node(n) {
                old.entry(n).or_insert(w.pos);
                self.set_pos(n, p);
            }
        }
        let mut work = Vec::new();
        for &(n, _) in moves {
            for s in self.adj(n) {
                let Some(m) = self.segment(s).map(|sg| sg.other(n)) else { continue };
                let (pn, pm) = (self.pos(n), self.pos(m));
                work.push(n);
                work.push(m);
                if orthogonal(pn, pm) {
                    continue;
                }
                let on = old.get(&n).copied().unwrap_or(pn);
                let om = old.get(&m).copied().unwrap_or(pm);
                // Direction the segment had: horizontal unless clearly vertical.
                let horizontal = !(on.x == om.x && on.y != om.y);
                if !old.contains_key(&m) && self.can_slide(m, s, horizontal) {
                    let q = if horizontal { GridPos::new(pm.x, pn.y) } else { GridPos::new(pn.x, pm.y) };
                    self.set_pos(m, q);
                    work.extend(self.adj(m).iter().filter_map(|&t| self.segment(t).map(|sg| sg.other(m))));
                } else {
                    let corner = if horizontal { GridPos::new(pm.x, pn.y) } else { GridPos::new(pn.x, pm.y) };
                    let b = self.alloc_node(corner, None);
                    self.retarget(s, m, b);
                    self.alloc_seg(b, m);
                    work.push(b);
                }
            }
        }
        self.cleanup(work);
    }

    /// Can bend `m` slide perpendicular to segment `s` (which runs
    /// `horizontal`ly)? Only a free node with exactly one other segment,
    /// running the other way.
    fn can_slide(&self, m: NodeId, s: SegmentId, horizontal: bool) -> bool {
        let Some(w) = self.node(m) else { return false };
        if !w.is_free() || w.degree() != 2 {
            return false;
        }
        let Some(&t) = w.segments().iter().find(|&&t| t != s) else { return false };
        let Some(q) = self.segment(t).map(|sg| self.pos(sg.other(m))) else { return false };
        if horizontal { q.x == w.pos.x && q.y != w.pos.y } else { q.y == w.pos.y && q.x != w.pos.x }
    }

    /// Room needed to rubber-band the given nodes.
    fn move_room(&self, nodes: impl Iterator<Item = NodeId>) -> Result<(), CircuitError> {
        let deg: usize = nodes.filter_map(|n| self.node(n)).map(WireNode::degree).sum();
        self.ensure_room(deg, deg)
    }

    /// Moves a junction or bend.
    pub(crate) fn move_node(&mut self, n: NodeId, to: GridPos) -> Result<(), CircuitError> {
        let w = self.node(n).ok_or(CircuitError::NoSuchNode(n))?;
        if !w.is_free() {
            return Err(CircuitError::NotFreeNode(n));
        }
        if w.pos == to {
            return Ok(());
        }
        self.move_room(std::iter::once(n))?;
        self.move_nodes(&[(n, to)]);
        Ok(())
    }

    /// Drags a segment perpendicular to itself by `offset` grid units
    /// (down / right is positive). Ends that cannot follow (pins, junctions,
    /// bends whose other segments are not perpendicular) stay and get a new
    /// connecting segment.
    pub(crate) fn drag_segment(&mut self, s: SegmentId, offset: i32) -> Result<(), CircuitError> {
        let sg = *self.segment(s).ok_or(CircuitError::NoSuchSegment(s))?;
        let (pa, pb) = (self.pos(sg.a), self.pos(sg.b));
        if pa == pb {
            return Err(CircuitError::ZeroLengthSegment(s));
        }
        if offset == 0 {
            return Ok(());
        }
        let horizontal = pa.y == pb.y;
        let shift = |p: GridPos| if horizontal { p.offset(0, offset) } else { p.offset(offset, 0) };
        self.move_room([sg.a, sg.b].into_iter())?;
        self.ensure_room(2, 2)?;
        let mut moves = Vec::new();
        let mut work = vec![sg.a, sg.b];
        for e in [sg.a, sg.b] {
            let p = self.pos(e);
            let follows = self.node(e).is_some_and(|w| {
                w.is_free()
                    && w.segments().iter().filter(|&&t| t != s).all(|&t| {
                        self.segment(t).map(|x| self.pos(x.other(e))).is_some_and(|q| {
                            if horizontal { q.x == p.x && q.y != p.y } else { q.y == p.y && q.x != p.x }
                        })
                    })
            });
            if follows {
                moves.push((e, shift(p)));
            } else {
                let e2 = self.alloc_node(shift(p), None);
                self.retarget(s, e, e2);
                self.alloc_seg(e, e2);
                work.push(e2);
            }
        }
        self.move_nodes(&moves);
        self.cleanup(work);
        Ok(())
    }

    /// Moves the nodes of a component's pins to the pins' current positions
    /// (after a move, rotation or kind change), rubber-banding the wires.
    pub(crate) fn follow_pins(&mut self, id: crate::ComponentId) {
        let Some(c) = self.components.get(&id) else { return };
        let mut moves = Vec::new();
        for pin in 0..c.kind.pin_count() {
            let pr = PinRef { component: id, pin };
            if let (Some(n), Some(pos)) = (self.pin_node.get(&pr).copied(), c.pin_pos(pin)) {
                if self.pos(n) != pos {
                    moves.push((n, pos));
                }
            }
        }
        if !moves.is_empty() {
            self.move_nodes(&moves);
        }
    }

    /// Room to rubber-band every wired pin of a component.
    pub(crate) fn pins_room(&self, id: crate::ComponentId) -> Result<(), CircuitError> {
        let Some(c) = self.components.get(&id) else { return Ok(()) };
        let nodes = (0..c.kind.pin_count()).filter_map(|pin| self.pin_node(PinRef { component: id, pin }));
        self.move_room(nodes)
    }

    /// Is any pin of the component wired?
    pub(crate) fn has_wires(&self, id: crate::ComponentId, pins: u8) -> bool {
        (0..pins).any(|pin| self.pin_node.contains_key(&PinRef { component: id, pin }))
    }

    /// Removes the nodes of a component's pins and their segments.
    pub(crate) fn detach_pins(&mut self, id: crate::ComponentId, pins: u8) {
        let mut work = Vec::new();
        for pin in 0..pins {
            let Some(n) = self.pin_node(PinRef { component: id, pin }) else { continue };
            for s in self.adj(n) {
                work.extend(self.segment(s).map(|sg| sg.other(n)));
                self.free_seg(s);
            }
            self.free_node(n);
        }
        self.cleanup(work);
    }

    /// Renumbers the wired pins of a component whose kind is about to
    /// change: `map[old] = Some(new)` moves the pin's node to the new pin,
    /// `None` detaches it (its segments go, dangling ends are trimmed).
    pub(crate) fn remap_pins(&mut self, id: crate::ComponentId, old_pins: u8, map: &[Option<u8>]) {
        let mut work = Vec::new();
        let mut moved = Vec::new();
        for pin in 0..old_pins {
            let pr = PinRef { component: id, pin };
            let Some(n) = self.pin_node(pr) else { continue };
            match map.get(usize::from(pin)).copied().flatten() {
                None => {
                    for s in self.adj(n) {
                        work.extend(self.segment(s).map(|sg| sg.other(n)));
                        self.free_seg(s);
                    }
                    self.free_node(n);
                }
                Some(new) => {
                    self.pin_node.remove(&pr);
                    self.log.pins.push(pr);
                    moved.push((n, new));
                }
            }
        }
        for (n, new) in moved {
            let pr = PinRef { component: id, pin: new };
            if let Some(w) = self.node_mut(n) {
                w.pin = Some(pr);
            }
            self.pin_node.insert(pr, n);
            self.log.pins.push(pr);
            self.log.nodes.push(n);
        }
        self.cleanup(work);
    }

    /// Checks every wiring invariant (tests / debugging). O(circuit).
    ///
    /// # Errors
    /// Describes the first violation found.
    #[doc(hidden)]
    pub fn check(&self) -> Result<(), String> {
        let mut live = 0;
        for (id, w) in self.nodes() {
            live += 1;
            for &s in w.segments() {
                let sg = self.segment(s).ok_or(format!("{id:?} lists missing {s:?}"))?;
                if sg.a != id && sg.b != id {
                    return Err(format!("{id:?} lists {s:?} which does not end there"));
                }
            }
            match w.pin {
                Some(p) => {
                    let at = self.components.get(&p.component).and_then(|c| c.pin_pos(p.pin));
                    if at != Some(w.pos) {
                        return Err(format!("pin node {id:?} at {:?}, pin at {at:?}", w.pos));
                    }
                    if self.pin_node.get(&p) != Some(&id) {
                        return Err(format!("pin map lost {p:?}"));
                    }
                    if w.degree() == 0 {
                        return Err(format!("pin node {id:?} without segments"));
                    }
                }
                None if w.degree() < 2 => return Err(format!("dangling free node {id:?}")),
                None => {}
            }
            if w.is_free() && w.degree() == 2 {
                let (p, q) = (w.segments()[0], w.segments()[1]);
                let (p, q) = (self.pos(self.segments_other(p, id)), self.pos(self.segments_other(q, id)));
                if collinear(p, w.pos, q) {
                    return Err(format!("straight bend {id:?} at {:?} not merged", w.pos));
                }
            }
        }
        if live != self.node_live || self.pin_node.len() > live {
            return Err("node count mismatch".into());
        }
        let mut segs = 0;
        let mut pairs = HashSet::new();
        for (id, s) in self.segments() {
            segs += 1;
            let (a, b) = (self.node(s.a).ok_or("missing end")?, self.node(s.b).ok_or("missing end")?);
            if !a.segments().contains(&id) || !b.segments().contains(&id) || s.a == s.b {
                return Err(format!("{id:?} adjacency broken"));
            }
            if !orthogonal(a.pos, b.pos) || (a.pos == b.pos && (a.is_free() || b.is_free())) {
                return Err(format!("{id:?} is diagonal or zero-length: {:?} {:?}", a.pos, b.pos));
            }
            if !pairs.insert((s.a.min(s.b), s.a.max(s.b))) {
                return Err(format!("{id:?} duplicates another segment"));
            }
        }
        if segs != self.seg_live {
            return Err("segment count mismatch".into());
        }
        let nets = self.nets();
        for i in 0..nets.len() {
            let drivers: Vec<PinRef> = nets.net(i).iter().copied().filter(|&p| self.is_output(p)).collect();
            self.check_drivers(&drivers).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn segments_other(&self, s: SegmentId, n: NodeId) -> NodeId {
        self.segment(s).map_or(n, |sg| sg.other(n))
    }

    // ---- loading -------------------------------------------------------------

    /// Builds the wire graph from file records (after the components).
    pub(crate) fn load_wiring(&mut self, nodes: Vec<NodeRecord>, segs: Vec<SegmentRecord>) -> Result<(), CircuitError> {
        let mut nodes = nodes;
        nodes.sort_by_key(|r| r.id);
        let mut map: HashMap<NodeId, NodeId> = HashMap::with_capacity(nodes.len());
        for r in nodes {
            let pos = match r.at {
                NodeAt::Free(p) => p,
                NodeAt::Pin(p) => {
                    if self.pin_node.contains_key(&p) {
                        return Err(CircuitError::DuplicatePinNode(p));
                    }
                    self.components
                        .get(&p.component)
                        .ok_or(CircuitError::NoSuchComponent(p.component))?
                        .pin_pos(p.pin)
                        .ok_or(CircuitError::NoSuchPin(p))?
                }
            };
            let pin = match r.at {
                NodeAt::Pin(p) => Some(p),
                NodeAt::Free(_) => None,
            };
            u32_of(self.nodes.len() + 1)?;
            let id = self.alloc_node(pos, pin);
            if map.insert(r.id, id).is_some() {
                return Err(CircuitError::DuplicateNode(r.id));
            }
        }
        let mut seen: HashSet<(NodeId, NodeId)> = HashSet::with_capacity(segs.len());
        for r in segs {
            let a = *map.get(&r.a).ok_or(CircuitError::NoSuchNode(r.a))?;
            let b = *map.get(&r.b).ok_or(CircuitError::NoSuchNode(r.b))?;
            let (pa, pb) = (self.pos(a), self.pos(b));
            let both_pins = self.node(a).is_some_and(|w| !w.is_free()) && self.node(b).is_some_and(|w| !w.is_free());
            if a == b || !orthogonal(pa, pb) || (pa == pb && !both_pins) {
                return Err(CircuitError::BadSegment(r.a, r.b));
            }
            if !seen.insert((a.min(b), a.max(b))) {
                return Err(CircuitError::BadSegment(r.a, r.b));
            }
            u32_of(self.segs.len() + 1)?;
            self.alloc_seg(a, b);
        }
        for (id, w) in self.nodes() {
            if w.degree() < if w.is_free() { 2 } else { 1 } {
                let file_id = map.iter().find(|&(_, &v)| v == id).map_or(id, |(&k, _)| k);
                return Err(CircuitError::DanglingNode(file_id));
            }
        }
        let nets = self.nets();
        for i in 0..nets.len() {
            let drivers: Vec<PinRef> = nets.net(i).iter().copied().filter(|&p| self.is_output(p)).collect();
            self.check_drivers(&drivers)?;
        }
        // A hand-written file may have straight bends: normalise them like
        // an edit would (files written by bitwerk never do).
        let straight: Vec<NodeId> =
            self.nodes().filter(|(_, w)| w.is_free() && w.degree() == 2).map(|(id, _)| id).collect();
        self.cleanup(straight);
        self.log = WireLog::default();
        Ok(())
    }

    /// File records of the wire graph (node ids renumbered densely, in id
    /// order, so a saved file loads back with the same ids).
    pub(crate) fn wiring_records(&self) -> (Vec<NodeRecord>, Vec<SegmentRecord>) {
        let mut dense = vec![0u32; self.nodes.len()];
        let mut nodes = Vec::with_capacity(self.node_live);
        for (id, w) in self.nodes() {
            let file = NodeId(u32::try_from(nodes.len()).unwrap_or(u32::MAX));
            dense[id.0 as usize] = file.0;
            nodes.push(NodeRecord { id: file, at: w.pin.map_or(NodeAt::Free(w.pos), NodeAt::Pin) });
        }
        let segs = self
            .segments()
            .map(|(_, s)| SegmentRecord { a: NodeId(dense[s.a.0 as usize]), b: NodeId(dense[s.b.0 as usize]) })
            .collect();
        (nodes, segs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_inserts_corners_and_drops_duplicates() {
        let p = |x, y| GridPos::new(x, y);
        assert_eq!(route(&[p(0, 0), p(4, 2)]), vec![p(0, 0), p(4, 0), p(4, 2)]);
        assert_eq!(route(&[p(0, 0), p(0, 0), p(3, 0)]), vec![p(0, 0), p(3, 0)]);
        assert_eq!(route(&[p(0, 0), p(2, 0), p(5, 0)]), vec![p(0, 0), p(5, 0)]);
        assert_eq!(route(&[p(1, 1)]), vec![p(1, 1)]);
    }

    #[test]
    fn adjacency_spills_and_shrinks() {
        let mut a = Adj::default();
        for i in 0..6 {
            a.push(SegmentId(i));
        }
        assert_eq!(a.as_slice().len(), 6);
        a.remove(SegmentId(2));
        assert_eq!(a.as_slice(), &[SegmentId(0), SegmentId(1), SegmentId(3), SegmentId(4), SegmentId(5)]);
        let mut b = Adj::default();
        b.push(SegmentId(7));
        b.push(SegmentId(8));
        b.remove(SegmentId(7));
        assert_eq!(b.as_slice(), &[SegmentId(8)]);
    }
}
