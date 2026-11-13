use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::component::{Color, Component, ComponentId, ComponentKind, MacroId, PinRef, Rotation};
use crate::geom::GridPos;
use crate::macros::{MAX_FLAT_COMPONENTS, MacroDef, MacroTable, pin_remap};
use crate::memenc;
use crate::note::Note;
use crate::wiring::{NodeId, NodeRecord, Segment, SegmentId, SegmentRecord, WireEnd, WireLog, WireNode};

/// Maximum number of bend points of one wire drawn in a single edit.
pub const MAX_WIRE_VERTICES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CircuitError {
    #[error("duplicate component id {0:?}")]
    DuplicateComponent(ComponentId),
    #[error("component {0:?} does not exist")]
    NoSuchComponent(ComponentId),
    #[error("component {0:?} has invalid parameters")]
    InvalidComponent(ComponentId),
    #[error("pin {0:?} does not exist")]
    NoSuchPin(PinRef),
    #[error("wire node {0:?} does not exist")]
    NoSuchNode(NodeId),
    #[error("wire segment {0:?} does not exist")]
    NoSuchSegment(SegmentId),
    #[error("duplicate wire node id {0:?}")]
    DuplicateNode(NodeId),
    #[error("pin {0:?} has more than one wire node")]
    DuplicatePinNode(PinRef),
    #[error("wire segment {0:?}–{1:?} is invalid (same node twice, diagonal, zero length or duplicated)")]
    BadSegment(NodeId, NodeId),
    #[error("wire node {0:?} is a dangling end")]
    DanglingNode(NodeId),
    #[error("point ({}, {}) is not on that wire", .0.x, .0.y)]
    NotOnSegment(GridPos),
    #[error("a wire needs two different ends")]
    SameNode,
    #[error(
        "cannot join two outputs on one net (component #{} pin {} and component #{} pin {}): only tri-state outputs may share a net",
        .0.component.0, .0.pin, .1.component.0, .1.pin
    )]
    TwoDrivers(PinRef, PinRef),
    #[error("only junctions and bends can be moved (node {0:?} is a pin)")]
    NotFreeNode(NodeId),
    #[error("wire segment {0:?} has no length and cannot be dragged")]
    ZeroLengthSegment(SegmentId),
    #[error("wire has too many vertices (max {MAX_WIRE_VERTICES})")]
    TooManyVertices,
    #[error("identifier space exhausted")]
    IdSpaceExhausted,
    #[error("component {0:?} is not a memory")]
    NotAMemory(ComponentId),
    #[error("memory address {addr} out of range")]
    AddressOutOfRange { addr: u32 },
    #[error("memory contents of {0:?}: {1}")]
    BadMemory(ComponentId, memenc::MemEncError),
    #[error("macro definition {} does not exist", .0.0)]
    NoSuchMacro(MacroId),
    #[error("a macro cannot contain itself (cycle through definition {})", .0.0)]
    MacroCycle(MacroId),
    #[error("macros nested deeper than {limit} levels")]
    MacroTooDeep { limit: u32 },
    #[error("design too large when flattened: {size} components (limit {limit})")]
    MacroTooLarge { size: u64, limit: u64 },
    #[error("macro definition {} is still used", .0.0)]
    MacroInUse(MacroId),
    #[error("too many macro definitions")]
    TooManyMacros,
    #[error("invalid macro: {0}")]
    BadMacro(String),
    #[error("macro definitions live in the root circuit only")]
    NotRoot,
    #[error("component {0:?} is not a text annotation")]
    NotAText(ComponentId),
    #[error("invalid text annotation {0:?} (too long, too many lines, control characters or bad height)")]
    BadNote(ComponentId),
}

/// How the simulator propagates changes (see `bw-sim`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum EvalMode {
    /// Unit delay: every gate takes one delta cycle; all changes of a delta
    /// are committed together (the native semantics).
    #[default]
    UnitDelay,
    /// Immediate propagation: gates are evaluated one after another and
    /// each new output is visible to the next evaluation at once (Digital
    /// Works compatible; gate-level latches settle instead of racing).
    Sequential,
}

impl EvalMode {
    pub const ALL: [Self; 2] = [Self::UnitDelay, Self::Sequential];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::UnitDelay => "Unit delay",
            Self::Sequential => "Sequential (Digital Works compatible)",
        }
    }

    #[allow(clippy::trivially_copy_pass_by_ref, reason = "serde skip predicate signature")]
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::UnitDelay)
    }
}

/// Contents of one memory device (exactly `kind.memory_words()` words, each
/// within the data width). Shared copy-on-write between circuit copies.
pub type MemoryImage = Arc<Vec<u16>>;

/// One atomic model change. Both the UI and the simulation thread apply the
/// same sequence of edits to their own copy of a [`Circuit`] (ids are
/// allocated deterministically, so both copies stay identical), which lets an
/// edit reach the simulator without cloning or sharing the whole circuit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    AddComponent {
        kind: ComponentKind,
        pos: GridPos,
        rotation: Rotation,
    },
    /// Removes a component and its pins' wire nodes (dangling wire ends are
    /// trimmed).
    RemoveComponent(ComponentId),
    /// Moves a component; attached wires are rubber-banded orthogonally.
    MoveComponent(ComponentId, GridPos),
    SetRotation(ComponentId, Rotation),
    SetKind(ComponentId, ComponentKind),
    /// Draws a wire from one end to the other through the bend points
    /// `via` (orthogonal; a missing corner is added, horizontal first). An
    /// end on the middle of a segment splits it (a junction). Joining two
    /// nets is refused if it would put two non-tri-state drivers together.
    AddWire {
        from: WireEnd,
        to: WireEnd,
        via: Vec<GridPos>,
    },
    /// Deletes one segment; dangling bits are removed and straight bends
    /// merged.
    RemoveSegment(SegmentId),
    /// Moves a junction or bend (geometry only).
    MoveNode(NodeId, GridPos),
    /// Drags a segment perpendicular to itself (geometry only).
    DragSegment(SegmentId, i32),
    /// Sets one word of a memory's contents (masked to the data width).
    SetMemoryWord {
        id: ComponentId,
        addr: u32,
        value: u16,
    },
    /// Replaces a memory's contents: longer data is truncated, shorter data
    /// is zero-filled, values are masked to the data width.
    SetMemory {
        id: ComponentId,
        words: Arc<[u16]>,
    },
    /// Adds or replaces a macro definition (root circuit only). Every
    /// instance follows: if the interface changed, instance pins are
    /// re-matched by tag and wires on vanished pins are detached. Refused
    /// on cycles, excessive depth or flattened size.
    DefineMacro {
        id: MacroId,
        def: Arc<MacroDef>,
    },
    /// Removes an unused macro definition.
    RemoveMacro(MacroId),
    /// Sets (or clears, `None` = default) a component's display colour.
    SetColor(ComponentId, Option<Color>),
    /// Replaces the text and style of a text annotation (its box follows).
    SetNote {
        id: ComponentId,
        note: Note,
    },
    /// Chooses how the simulator propagates changes (root circuit).
    SetEvalMode(EvalMode),
}

impl Edit {
    /// Does this edit change what the simulator computes (its netlist)?
    /// Moves, rotations, wire geometry and memory contents do not: they
    /// never change which pins are connected.
    #[must_use]
    pub const fn affects_netlist(&self) -> bool {
        !matches!(
            self,
            Self::MoveComponent(..)
                | Self::SetRotation(..)
                | Self::MoveNode(..)
                | Self::DragSegment(..)
                | Self::SetMemoryWord { .. }
                | Self::SetMemory { .. }
                | Self::RemoveMacro(..)
                | Self::SetColor(..)
                | Self::SetNote { .. }
        )
    }
}

/// What an applied [`Edit`] created, changed or removed (each list sorted,
/// without duplicates). The render scene patches exactly these objects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Components added, changed or removed.
    pub components: Vec<ComponentId>,
    /// Segments added, moved or removed.
    pub segments: Vec<SegmentId>,
    /// Nodes added, moved, removed or whose segment count changed.
    pub nodes: Vec<NodeId>,
    /// Pins that gained or lost their wire node.
    pub pins: Vec<PinRef>,
    /// Memory whose contents changed (no geometry).
    pub memory: Option<ComponentId>,
    /// Which pins are connected may have changed (nets must be recomputed).
    pub connectivity: bool,
    /// Macro definitions added, changed or removed.
    pub macros: Vec<MacroId>,
}

impl Applied {
    /// The (first) component the edit touched.
    #[must_use]
    pub fn component(&self) -> Option<ComponentId> {
        self.components.first().copied()
    }
}

/// A flat (single-level) circuit.
///
/// Fields are private so the invariants documented at crate level cannot be
/// broken from outside; deserialization goes through the same validation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(try_from = "CircuitRaw")]
pub struct Circuit {
    pub(crate) components: BTreeMap<ComponentId, Component>,
    /// Wire graph nodes (slab indexed by [`NodeId`]; `None` = free slot).
    pub(crate) nodes: Vec<Option<WireNode>>,
    pub(crate) node_free: Vec<u32>,
    pub(crate) node_live: usize,
    /// Wire segments (slab indexed by [`SegmentId`]).
    pub(crate) segs: Vec<Option<Segment>>,
    pub(crate) seg_free: Vec<u32>,
    pub(crate) seg_live: usize,
    /// Node of each wired pin.
    pub(crate) pin_node: HashMap<PinRef, NodeId>,
    /// Memory contents; a memory without an entry is all zeros.
    pub(crate) memories: BTreeMap<ComponentId, MemoryImage>,
    /// Text and style of every text annotation.
    pub(crate) notes: BTreeMap<ComponentId, Note>,
    /// Propagation semantics for the simulator (root circuit).
    pub(crate) eval: EvalMode,
    next_component: u32,
    /// Macro definitions (root circuit only; empty in definition bodies).
    pub(crate) macros: MacroTable,
    /// Instances of each definition in this circuit.
    pub(crate) macro_index: BTreeMap<MacroId, BTreeSet<ComponentId>>,
    /// Primitive components (not macro instances, not tags).
    pub(crate) prims: u64,
    /// A definition body: macro instances are checked against the
    /// project's table when the definition is stored, not here.
    pub(crate) body: bool,
    /// Changes of the edit being applied.
    pub(crate) log: WireLog,
}

/// On-disk shape of a memory's contents (see [`crate::memenc`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecord {
    pub id: ComponentId,
    pub words: String,
}

/// On-disk shape of a text annotation's note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteRecord {
    pub id: ComponentId,
    pub note: Note,
}

/// On-disk shape of a circuit; converted to [`Circuit`] only after validation.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitRaw {
    pub components: Vec<Component>,
    #[serde(default)]
    pub nodes: Vec<NodeRecord>,
    #[serde(default)]
    pub segments: Vec<SegmentRecord>,
    #[serde(default)]
    pub memories: Vec<MemoryRecord>,
    /// Text annotations (format 5).
    #[serde(default)]
    pub notes: Vec<NoteRecord>,
    /// Simulation semantics (format 5).
    #[serde(default)]
    pub eval: EvalMode,
}

#[derive(Serialize)]
struct MemoryRecordOut {
    id: ComponentId,
    words: String,
}

#[derive(Serialize)]
struct NoteRecordOut<'a> {
    id: ComponentId,
    note: &'a Note,
}

#[derive(Serialize)]
struct CircuitRef<'a> {
    components: Vec<&'a Component>,
    nodes: Vec<NodeRecord>,
    segments: Vec<SegmentRecord>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    memories: Vec<MemoryRecordOut>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<NoteRecordOut<'a>>,
    #[serde(skip_serializing_if = "EvalMode::is_default")]
    eval: EvalMode,
}

impl Serialize for Circuit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let memories = self
            .memories
            .iter()
            .filter(|(_, m)| m.iter().any(|&w| w != 0))
            .map(|(&id, m)| MemoryRecordOut { id, words: memenc::encode(m) })
            .collect();
        let (nodes, segments) = self.wiring_records();
        let note_recs = self.notes.iter().map(|(&id, note)| NoteRecordOut { id, note }).collect();
        CircuitRef {
            components: self.components.values().collect(),
            nodes,
            segments,
            memories,
            notes: note_recs,
            eval: self.eval,
        }
        .serialize(s)
    }
}

impl TryFrom<CircuitRaw> for Circuit {
    type Error = CircuitError;

    fn try_from(raw: CircuitRaw) -> Result<Self, Self::Error> {
        Self::from_raw(raw, &MacroTable::default(), false)
    }
}

impl Circuit {
    /// Builds a validated circuit from file records, resolving macro
    /// instances against `table`. A root circuit (`body == false`) takes a
    /// copy of the table as its own.
    ///
    /// # Errors
    /// Any invariant violation, an unknown macro, or a root whose
    /// flattened size exceeds [`MAX_FLAT_COMPONENTS`].
    pub fn from_raw(mut raw: CircuitRaw, table: &MacroTable, body: bool) -> Result<Self, CircuitError> {
        for comp in &mut raw.components {
            if let ComponentKind::Macro(s) = comp.kind {
                let e = table.get(s.def).ok_or(CircuitError::NoSuchMacro(s.def))?;
                comp.kind = ComponentKind::Macro(e.shape);
            }
        }
        let mut c = Self { body, eval: if body { EvalMode::default() } else { raw.eval }, ..Self::default() };
        if !body {
            c.macros = table.clone();
        }
        // Text annotations: the box follows the note (a missing note is
        // the default one).
        let mut notes: BTreeMap<ComponentId, Note> = BTreeMap::new();
        for rec in raw.notes {
            if !rec.note.is_valid() {
                return Err(CircuitError::BadNote(rec.id));
            }
            if notes.insert(rec.id, rec.note).is_some() {
                return Err(CircuitError::DuplicateComponent(rec.id));
            }
        }
        for comp in &mut raw.components {
            if matches!(comp.kind, ComponentKind::Text { .. }) {
                let note = notes.entry(comp.id).or_default();
                comp.kind = note.kind();
            }
        }
        for &id in notes.keys() {
            if !raw.components.iter().any(|k| k.id == id && matches!(k.kind, ComponentKind::Text { .. })) {
                return Err(CircuitError::NotAText(id));
            }
        }
        c.notes = notes;
        for comp in raw.components {
            if !comp.kind.is_valid() {
                return Err(CircuitError::InvalidComponent(comp.id));
            }
            let next = comp.id.0.checked_add(1).ok_or(CircuitError::IdSpaceExhausted)?;
            c.next_component = c.next_component.max(next);
            c.index_kind(comp.id, &comp.kind, true);
            if c.components.insert(comp.id, comp.clone()).is_some() {
                return Err(CircuitError::DuplicateComponent(comp.id));
            }
        }
        if !body {
            let flat = c.flat_size();
            if flat > MAX_FLAT_COMPONENTS {
                return Err(CircuitError::MacroTooLarge { size: flat, limit: MAX_FLAT_COMPONENTS });
            }
        }
        c.load_wiring(raw.nodes, raw.segments)?;
        for rec in raw.memories {
            let kind = c.components.get(&rec.id).ok_or(CircuitError::NoSuchComponent(rec.id))?.kind;
            let ComponentKind::Memory { data_bits, .. } = kind else {
                return Err(CircuitError::NotAMemory(rec.id));
            };
            if c.memories.contains_key(&rec.id) {
                return Err(CircuitError::DuplicateComponent(rec.id));
            }
            let words = memenc::decode(&rec.words, kind.memory_words(), data_bits)
                .map_err(|e| CircuitError::BadMemory(rec.id, e))?;
            c.memories.insert(rec.id, Arc::new(words));
        }
        Ok(c)
    }

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty definition body (see [`MacroDef::body`]).
    #[must_use]
    pub fn new_body() -> Self {
        Self { body: true, ..Self::default() }
    }

    /// Is this a definition body?
    #[must_use]
    pub fn is_body(&self) -> bool {
        self.body
    }

    /// The project's macro definitions (empty in a definition body).
    #[must_use]
    pub fn macros(&self) -> &MacroTable {
        &self.macros
    }

    /// Instances of definition `id` in this circuit.
    #[must_use]
    pub fn instances_of(&self, id: MacroId) -> Vec<ComponentId> {
        self.macro_index.get(&id).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    /// Total instances of `id` in the design: in this circuit and, through
    /// every definition that contains it, nested (counted per path).
    #[must_use]
    pub fn total_instances(&self, id: MacroId) -> u64 {
        fn count(t: &MacroTable, d: MacroId, target: MacroId, memo: &mut HashMap<MacroId, u64>) -> u64 {
            if let Some(&v) = memo.get(&d) {
                return v;
            }
            let Some(e) = t.get(d) else { return 0 };
            let mut n = 0u64;
            for c in e.def.body.components() {
                if let ComponentKind::Macro(s) = c.kind {
                    n = n.saturating_add(if s.def == target { 1 } else { count(t, s.def, target, memo) });
                }
            }
            memo.insert(d, n);
            n
        }
        let mut memo: HashMap<MacroId, u64> = HashMap::new();
        let mut n = 0u64;
        for (&d, insts) in &self.macro_index {
            let per = if d == id { 1 } else { count(&self.macros, d, id, &mut memo) };
            n = n.saturating_add(per.saturating_mul(insts.len() as u64));
        }
        n
    }

    /// Primitive components of the design with every macro expanded.
    #[must_use]
    pub fn flat_size(&self) -> u64 {
        let mut n = self.prims;
        for (d, insts) in &self.macro_index {
            let per = self.macros.get(*d).map_or(0, |e| e.flat);
            n = n.saturating_add(per.saturating_mul(insts.len() as u64));
        }
        n
    }

    /// Same components, wiring and memory contents (definition equality).
    #[must_use]
    pub fn content_eq(&self, o: &Self) -> bool {
        self.components == o.components
            && self.memories == o.memories
            && self.notes == o.notes
            && self.eval == o.eval
            && self.wiring_records() == o.wiring_records()
    }

    /// Text and style of a text annotation.
    #[must_use]
    pub fn note(&self, id: ComponentId) -> Option<&Note> {
        self.notes.get(&id)
    }

    /// Every text annotation's note, in id order.
    pub fn notes(&self) -> impl Iterator<Item = (ComponentId, &Note)> {
        self.notes.iter().map(|(&id, n)| (id, n))
    }

    /// How the simulator should propagate changes.
    #[must_use]
    pub fn eval_mode(&self) -> EvalMode {
        self.eval
    }

    /// Sets the propagation semantics (root circuits; ignored in bodies).
    pub fn set_eval_mode(&mut self, mode: EvalMode) {
        if !self.body {
            self.eval = mode;
        }
    }

    /// Replaces a text annotation's note; its box follows.
    ///
    /// # Errors
    /// Not a text annotation, or an invalid note.
    pub fn set_note(&mut self, id: ComponentId, note: Note) -> Result<(), CircuitError> {
        let c = self.components.get_mut(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        if !matches!(c.kind, ComponentKind::Text { .. }) {
            return Err(CircuitError::NotAText(id));
        }
        if !note.is_valid() {
            return Err(CircuitError::BadNote(id));
        }
        c.kind = note.kind();
        self.notes.insert(id, note);
        Ok(())
    }

    /// Sets a component's display colour (`None` = the kind's default).
    ///
    /// # Errors
    /// The component does not exist.
    pub fn set_color(&mut self, id: ComponentId, color: Option<Color>) -> Result<(), CircuitError> {
        self.components.get_mut(&id).ok_or(CircuitError::NoSuchComponent(id))?.color = color;
        Ok(())
    }

    /// Keeps the macro index and the primitive count up to date.
    fn index_kind(&mut self, id: ComponentId, kind: &ComponentKind, add: bool) {
        match kind {
            ComponentKind::Macro(s) => {
                if add {
                    self.macro_index.entry(s.def).or_default().insert(id);
                } else if let Some(set) = self.macro_index.get_mut(&s.def) {
                    set.remove(&id);
                    if set.is_empty() {
                        self.macro_index.remove(&s.def);
                    }
                }
            }
            ComponentKind::Tag { .. } | ComponentKind::Text { .. } => {}
            _ => {
                if add {
                    self.prims += 1;
                } else {
                    self.prims = self.prims.saturating_sub(1);
                }
            }
        }
    }

    /// Checks a macro instance kind against the table (root circuits) and
    /// the flattened size limit with `extra` more components.
    fn check_macro_kind(&self, id: ComponentId, kind: &ComponentKind) -> Result<(), CircuitError> {
        let add = match kind {
            ComponentKind::Macro(s) => {
                if self.body {
                    return Ok(());
                }
                let e = self.macros.get(s.def).ok_or(CircuitError::NoSuchMacro(s.def))?;
                if e.shape != *s {
                    return Err(CircuitError::InvalidComponent(id));
                }
                e.flat
            }
            ComponentKind::Tag { .. } | ComponentKind::Text { .. } => 0,
            _ => 1,
        };
        if !self.body {
            let size = self.flat_size().saturating_add(add);
            if size > MAX_FLAT_COMPONENTS {
                return Err(CircuitError::MacroTooLarge { size, limit: MAX_FLAT_COMPONENTS });
            }
        }
        Ok(())
    }

    /// Changes an instance's layout, re-attaching wires per `map` (old pin
    /// → new pin; unmapped wired pins are detached).
    pub(crate) fn reshape_instance(&mut self, id: ComponentId, kind: ComponentKind, map: &[Option<u8>]) {
        let Some(old) = self.components.get(&id).map(|c| c.kind) else { return };
        self.remap_pins(id, old.pin_count(), map);
        self.index_kind(id, &old, false);
        self.index_kind(id, &kind, true);
        if let Some(c) = self.components.get_mut(&id) {
            c.kind = kind;
        }
        self.follow_pins(id);
    }

    /// Stores definition `id` (see [`Edit::DefineMacro`]); returns the
    /// root instances that changed and the definitions updated.
    fn define_macro(
        &mut self,
        id: MacroId,
        def: &Arc<MacroDef>,
    ) -> Result<(Vec<ComponentId>, Vec<MacroId>), CircuitError> {
        if self.body {
            return Err(CircuitError::NotRoot);
        }
        let mut table = self.macros.clone();
        let entry = table.evaluate(id, Arc::clone(def))?;
        let old = table.get(id).cloned();
        let new_shape = entry.shape;
        let remap = old.as_ref().filter(|o| o.shape != new_shape).map(|o| pin_remap(&o.def, def));
        let mut changed = vec![id];
        table.insert(id, entry);
        for anc in table.ancestors(id) {
            let Some(cur) = table.get(anc).map(|e| Arc::clone(&e.def)) else { continue };
            let next = match &remap {
                Some(map) if cur.body.macro_index.contains_key(&id) => {
                    let mut d = (*cur).clone();
                    for inst in d.body.instances_of(id) {
                        d.body.reshape_instance(inst, ComponentKind::Macro(new_shape), map);
                    }
                    d.body.log = crate::wiring::WireLog::default();
                    Arc::new(d)
                }
                _ => cur,
            };
            let e = table.evaluate(anc, next)?;
            table.insert(anc, e);
            changed.push(anc);
        }
        // The whole design must still fit when flattened.
        let mut size = self.prims;
        for (d, insts) in &self.macro_index {
            let per = table.get(*d).map_or(0, |e| e.flat);
            size = size.saturating_add(per.saturating_mul(insts.len() as u64));
        }
        if size > MAX_FLAT_COMPONENTS {
            return Err(CircuitError::MacroTooLarge { size, limit: MAX_FLAT_COMPONENTS });
        }
        self.macros = table;
        let insts = self.instances_of(id);
        if let Some(map) = &remap {
            for &inst in &insts {
                self.reshape_instance(inst, ComponentKind::Macro(new_shape), map);
            }
        }
        changed.sort_unstable();
        Ok((if remap.is_some() || old.is_none() { insts } else { Vec::new() }, changed))
    }

    fn remove_macro(&mut self, id: MacroId) -> Result<(), CircuitError> {
        if self.body {
            return Err(CircuitError::NotRoot);
        }
        self.macros.get(id).ok_or(CircuitError::NoSuchMacro(id))?;
        if self.macro_index.contains_key(&id) || !self.macros.users(id).is_empty() {
            return Err(CircuitError::MacroInUse(id));
        }
        self.macros.remove(id);
        Ok(())
    }

    #[must_use]
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Components in ascending id order (deterministic).
    #[must_use]
    pub fn components(&self) -> impl ExactSizeIterator<Item = &Component> + Clone {
        self.components.values()
    }

    #[must_use]
    pub fn component(&self, id: ComponentId) -> Option<&Component> {
        self.components.get(&id)
    }

    /// Contents of a memory component (`None` for other kinds). A memory
    /// that was never written returns a zero-filled image.
    #[must_use]
    pub fn memory(&self, id: ComponentId) -> Option<MemoryImage> {
        let kind = self.components.get(&id)?.kind;
        if !matches!(kind, ComponentKind::Memory { .. }) {
            return None;
        }
        Some(self.memories.get(&id).cloned().unwrap_or_else(|| Arc::new(vec![0; kind.memory_words()])))
    }

    /// Adds a component and returns its new id.
    ///
    /// # Errors
    /// Fails if the kind parameters are out of range or ids are exhausted.
    pub fn add_component(
        &mut self,
        kind: ComponentKind,
        pos: GridPos,
        rotation: Rotation,
    ) -> Result<ComponentId, CircuitError> {
        let id = ComponentId(self.next_component);
        if !kind.is_valid() {
            return Err(CircuitError::InvalidComponent(id));
        }
        self.check_macro_kind(id, &kind)?;
        self.next_component = self.next_component.checked_add(1).ok_or(CircuitError::IdSpaceExhausted)?;
        let kind = if matches!(kind, ComponentKind::Text { .. }) {
            let note = Note::default();
            let k = note.kind();
            self.notes.insert(id, note);
            k
        } else {
            kind
        };
        self.index_kind(id, &kind, true);
        self.components.insert(id, Component { id, kind, pos, rotation, color: None });
        Ok(id)
    }

    /// Removes a component together with its pins' wire nodes and the
    /// segments ending there (dangling wire ends are trimmed). Cost: the
    /// wires attached to it, not the circuit size.
    ///
    /// # Errors
    /// Fails if the component does not exist.
    pub fn remove_component(&mut self, id: ComponentId) -> Result<Component, CircuitError> {
        let pins = self.components.get(&id).ok_or(CircuitError::NoSuchComponent(id))?.kind.pin_count();
        self.detach_pins(id, pins);
        let comp = self.components.remove(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        self.index_kind(id, &comp.kind, false);
        self.memories.remove(&id);
        self.notes.remove(&id);
        Ok(comp)
    }

    /// Moves a component; its wires are rubber-banded.
    ///
    /// # Errors
    /// Fails if the component does not exist.
    pub fn move_component(&mut self, id: ComponentId, pos: GridPos) -> Result<(), CircuitError> {
        self.components.get(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        self.pins_room(id)?;
        if let Some(c) = self.components.get_mut(&id) {
            c.pos = pos;
        }
        self.follow_pins(id);
        Ok(())
    }

    /// Sets a component's rotation (wires are rubber-banded to the pins).
    ///
    /// # Errors
    /// Fails if the component does not exist.
    pub fn set_rotation(&mut self, id: ComponentId, rotation: Rotation) -> Result<(), CircuitError> {
        self.components.get(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        self.pins_room(id)?;
        if let Some(c) = self.components.get_mut(&id) {
            c.rotation = rotation;
        }
        self.follow_pins(id);
        Ok(())
    }

    /// Replaces a component's kind (e.g. to toggle a switch or change a
    /// parameter). While wires are attached the pin counts must not change,
    /// and neither may the tri-state capability of the outputs (it decides
    /// whether an output may share a net). Memory contents follow the new
    /// size and width (truncated / zero-filled / masked).
    ///
    /// # Errors
    /// Fails if the component does not exist or the kind is invalid.
    pub fn set_kind(&mut self, id: ComponentId, kind: ComponentKind) -> Result<(), CircuitError> {
        if !kind.is_valid() {
            return Err(CircuitError::InvalidComponent(id));
        }
        let c = self.components.get(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        // A text annotation's box follows its note (see `set_note`).
        if matches!(c.kind, ComponentKind::Text { .. }) != matches!(kind, ComponentKind::Text { .. })
            || (matches!(kind, ComponentKind::Text { .. }) && c.kind != kind)
        {
            return Err(CircuitError::InvalidComponent(id));
        }
        let same_pins = c.kind.input_count() == kind.input_count() && c.kind.output_count() == kind.output_count();
        if (!same_pins || c.kind.tristate_mask() != kind.tristate_mask()) && self.has_wires(id, c.kind.pin_count()) {
            return Err(CircuitError::InvalidComponent(id));
        }
        let old = c.kind;
        self.index_kind(id, &old, false);
        let check = self.check_macro_kind(id, &kind);
        self.index_kind(id, &old, true);
        check?;
        self.pins_room(id)?;
        self.index_kind(id, &old, false);
        self.index_kind(id, &kind, true);
        if let Some(c) = self.components.get_mut(&id) {
            c.kind = kind;
        }
        self.follow_pins(id);
        match kind {
            ComponentKind::Memory { data_bits, .. } => {
                if let Some(m) = self.memories.get_mut(&id) {
                    let mask = memenc::width_mask(data_bits);
                    let m = Arc::make_mut(m);
                    m.resize(kind.memory_words(), 0);
                    for w in m.iter_mut() {
                        *w &= mask;
                    }
                }
            }
            _ => {
                self.memories.remove(&id);
            }
        }
        Ok(())
    }

    fn memory_kind(&self, id: ComponentId) -> Result<(usize, u16), CircuitError> {
        let kind = self.components.get(&id).ok_or(CircuitError::NoSuchComponent(id))?.kind;
        match kind {
            ComponentKind::Memory { data_bits, .. } => Ok((kind.memory_words(), memenc::width_mask(data_bits))),
            _ => Err(CircuitError::NotAMemory(id)),
        }
    }

    /// Writes one memory word (masked to the data width).
    ///
    /// # Errors
    /// Not a memory, or the address is out of range.
    pub fn set_memory_word(&mut self, id: ComponentId, addr: u32, value: u16) -> Result<(), CircuitError> {
        let (len, mask) = self.memory_kind(id)?;
        let i = usize::try_from(addr).ok().filter(|&i| i < len).ok_or(CircuitError::AddressOutOfRange { addr })?;
        let m = self.memories.entry(id).or_insert_with(|| Arc::new(vec![0; len]));
        Arc::make_mut(m)[i] = value & mask;
        Ok(())
    }

    /// Replaces a memory's contents (truncated / zero-filled / masked).
    ///
    /// # Errors
    /// Not a memory.
    pub fn set_memory(&mut self, id: ComponentId, words: &[u16]) -> Result<(), CircuitError> {
        let (len, mask) = self.memory_kind(id)?;
        let mut v: Vec<u16> = words.iter().take(len).map(|&w| w & mask).collect();
        v.resize(len, 0);
        self.memories.insert(id, Arc::new(v));
        Ok(())
    }

    /// Wires pin `from` to pin `to` along `via` (a convenience for
    /// [`Edit::AddWire`] between two pins; missing corners are added).
    ///
    /// # Errors
    /// See [`Edit::AddWire`].
    pub fn connect(&mut self, from: PinRef, to: PinRef, via: Vec<GridPos>) -> Result<Applied, CircuitError> {
        self.apply(&Edit::AddWire { from: WireEnd::Pin(from), to: WireEnd::Pin(to), via })
    }

    /// Applies one [`Edit`] and reports what changed.
    ///
    /// # Errors
    /// Same as the corresponding individual operation; on error the circuit
    /// is unchanged.
    pub fn apply(&mut self, edit: &Edit) -> Result<Applied, CircuitError> {
        self.log = WireLog::default();
        let mut components = Vec::new();
        let mut memory = None;
        let mut macros = Vec::new();
        let r = match edit {
            Edit::AddComponent { kind, pos, rotation } => {
                self.add_component(*kind, *pos, *rotation).map(|id| components.push(id))
            }
            Edit::RemoveComponent(id) => self.remove_component(*id).map(|_| components.push(*id)),
            Edit::MoveComponent(id, pos) => self.move_component(*id, *pos).map(|()| components.push(*id)),
            Edit::SetRotation(id, rot) => self.set_rotation(*id, *rot).map(|()| components.push(*id)),
            Edit::SetKind(id, kind) => self.set_kind(*id, *kind).map(|()| components.push(*id)),
            Edit::AddWire { from, to, via } => self.add_wire(*from, *to, via),
            Edit::RemoveSegment(s) => self.remove_segment(*s),
            Edit::MoveNode(n, p) => self.move_node(*n, *p),
            Edit::DragSegment(s, d) => self.drag_segment(*s, *d),
            Edit::SetMemoryWord { id, addr, value } => {
                self.set_memory_word(*id, *addr, *value).map(|()| memory = Some(*id))
            }
            Edit::SetMemory { id, words } => self.set_memory(*id, words).map(|()| memory = Some(*id)),
            Edit::DefineMacro { id, def } => self.define_macro(*id, def).map(|(insts, defs)| {
                components = insts;
                macros = defs;
            }),
            Edit::RemoveMacro(id) => self.remove_macro(*id).map(|()| macros.push(*id)),
            Edit::SetColor(id, color) => self.set_color(*id, *color).map(|()| components.push(*id)),
            Edit::SetNote { id, note } => self.set_note(*id, note.clone()).map(|()| components.push(*id)),
            Edit::SetEvalMode(mode) => {
                self.set_eval_mode(*mode);
                Ok(())
            }
        };
        let log = std::mem::take(&mut self.log);
        r?;
        let mut a = Applied {
            components,
            segments: log.segments,
            nodes: log.nodes,
            pins: log.pins,
            memory,
            connectivity: edit.affects_netlist(),
            macros,
        };
        a.segments.sort_unstable();
        a.segments.dedup();
        a.nodes.sort_unstable();
        a.nodes.dedup();
        a.pins.sort_unstable();
        a.pins.dedup();
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{FlipFlopType, GateOp, MemoryType};

    fn and2() -> ComponentKind {
        ComponentKind::Gate { op: GateOp::And, inputs: 2 }
    }

    #[test]
    fn connect_rules() {
        let mut c = Circuit::new();
        let s = c
            .add_component(
                ComponentKind::Switch { on: false, mode: crate::SwitchMode::Toggle },
                GridPos::new(0, 0),
                Rotation::R0,
            )
            .unwrap();
        let s2 = c
            .add_component(
                ComponentKind::Switch { on: false, mode: crate::SwitchMode::Toggle },
                GridPos::new(0, 6),
                Rotation::R0,
            )
            .unwrap();
        let g = c.add_component(and2(), GridPos::new(4, 0), Rotation::R0).unwrap();
        let out = PinRef { component: s, pin: 0 };
        let in0 = PinRef { component: g, pin: 0 };
        let gout = PinRef { component: g, pin: 2 };
        c.connect(out, in0, vec![]).unwrap();
        // A second output on the same net is refused, in either direction.
        let s2out = PinRef { component: s2, pin: 0 };
        assert_eq!(c.connect(s2out, in0, vec![]), Err(CircuitError::TwoDrivers(out, s2out)));
        assert_eq!(c.connect(out, gout, vec![]), Err(CircuitError::TwoDrivers(out, gout)));
        assert_eq!(c.connect(out, out, vec![]), Err(CircuitError::SameNode));
        c.remove_component(s).unwrap();
        assert_eq!((c.segment_count(), c.node_count()), (0, 0), "dangling wire removed with its driver");
    }

    /// Only tri-state outputs may share a net (a bus); a plain output can
    /// neither join nor start one.
    #[test]
    fn tristate_bus_relaxes_single_driver_rule_only_for_tristates() {
        let mut c = Circuit::new();
        let t1 = c.add_component(ComponentKind::TriState, GridPos::new(0, 0), Rotation::R0).unwrap();
        let t2 = c.add_component(ComponentKind::TriState, GridPos::new(0, 6), Rotation::R0).unwrap();
        let sw = c
            .add_component(
                ComponentKind::Switch { on: true, mode: crate::SwitchMode::Toggle },
                GridPos::new(0, 12),
                Rotation::R0,
            )
            .unwrap();
        let led = c.add_component(ComponentKind::Led, GridPos::new(10, 0), Rotation::R0).unwrap();
        let led2 = c.add_component(ComponentKind::Led, GridPos::new(10, 6), Rotation::R0).unwrap();
        let o = |id| PinRef { component: id, pin: 2 };
        let li = PinRef { component: led, pin: 0 };
        c.connect(o(t1), li, vec![]).unwrap();
        c.connect(o(t2), li, vec![]).unwrap();
        assert_eq!(c.drivers_of(li).len(), 2);
        let sw_out = PinRef { component: sw, pin: 0 };
        assert!(matches!(c.connect(sw_out, li, vec![]), Err(CircuitError::TwoDrivers(..))), "plain output");
        // A plain output first: no tri-state may join it.
        let l2 = PinRef { component: led2, pin: 0 };
        c.connect(sw_out, l2, vec![]).unwrap();
        assert!(matches!(c.connect(o(t1), l2, vec![]), Err(CircuitError::TwoDrivers(..))));
        c.remove_component(t1).unwrap();
        assert_eq!(c.drivers_of(li), vec![o(t2)]);
        // A wired tri-state cannot become a plain gate with the same pin count.
        assert!(c.set_kind(t2, and2()).is_err());
        c.remove_component(led).unwrap();
        c.set_kind(t2, and2()).unwrap();
        c.check().unwrap();
    }

    #[test]
    fn memory_contents_follow_kind_and_edits() {
        let mut c = Circuit::new();
        let ram = ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 4, data_bits: 8 };
        let m = c.add_component(ram, GridPos::default(), Rotation::R0).unwrap();
        assert_eq!(c.memory(m).unwrap().len(), 16);
        c.apply(&Edit::SetMemoryWord { id: m, addr: 3, value: 0x1ff }).unwrap();
        assert_eq!(c.memory(m).unwrap()[3], 0xff, "masked to 8 bits");
        assert!(c.set_memory_word(m, 16, 1).is_err());
        c.apply(&Edit::SetMemory { id: m, words: Arc::from(vec![7u16; 40]) }).unwrap();
        assert_eq!(*c.memory(m).unwrap(), vec![7; 16], "truncated");
        let bigger = ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 5, data_bits: 2 };
        c.set_kind(m, bigger).unwrap();
        let img = c.memory(m).unwrap();
        assert_eq!(img.len(), 32);
        assert_eq!((img[0], img[31]), (3, 0), "masked to 2 bits and zero-filled");
        let ff = c.add_component(ComponentKind::flip_flop(FlipFlopType::D), GridPos::default(), Rotation::R0).unwrap();
        assert_eq!(c.set_memory_word(ff, 0, 1), Err(CircuitError::NotAMemory(ff)));
        assert!(c.memory(ff).is_none());
    }

    #[test]
    fn edits_replay_identically() {
        let pin = |c, p| PinRef { component: ComponentId(c), pin: p };
        let edits = [
            Edit::AddComponent {
                kind: ComponentKind::Switch { on: true, mode: crate::SwitchMode::Toggle },
                pos: GridPos::new(0, 0),
                rotation: Rotation::R0,
            },
            Edit::AddComponent { kind: and2(), pos: GridPos::new(6, 0), rotation: Rotation::R0 },
            Edit::AddWire { from: WireEnd::Pin(pin(0, 0)), to: WireEnd::Pin(pin(1, 0)), via: vec![] },
            Edit::AddWire {
                from: WireEnd::OnSegment(SegmentId(0), GridPos::new(3, 1)),
                to: WireEnd::Pin(pin(1, 1)),
                via: vec![GridPos::new(3, 3)],
            },
            Edit::MoveComponent(ComponentId(1), GridPos::new(8, 2)),
            Edit::SetRotation(ComponentId(1), Rotation::R90),
            Edit::RemoveSegment(SegmentId(0)),
            Edit::RemoveComponent(ComponentId(0)),
        ];
        let mut a = Circuit::new();
        let mut b = Circuit::new();
        for e in &edits {
            let ra = a.apply(e).unwrap();
            assert_eq!(b.apply(e).unwrap(), ra);
            a.check().unwrap();
        }
        assert_eq!(a.component_count(), 1);
        assert_eq!(a.components().collect::<Vec<_>>(), b.components().collect::<Vec<_>>());
        assert!(a.apply(&Edit::RemoveComponent(ComponentId(0))).is_err());
    }

    #[test]
    fn invalid_gate_rejected() {
        let mut c = Circuit::new();
        let bad = ComponentKind::Gate { op: GateOp::Or, inputs: 200 };
        assert!(c.add_component(bad, GridPos::default(), Rotation::R0).is_err());
    }
}
