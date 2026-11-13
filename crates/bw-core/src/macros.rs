//! Macros: hierarchical sub-circuits.
//!
//! A [`MacroDef`] is a circuit (its *body*) plus an interface: the ordered
//! pins created by the [`ComponentKind::Tag`] components of the body, a
//! name, a visual template ([`Package`]) and an optional datasheet. A
//! circuit uses a macro through instances, components of kind
//! [`ComponentKind::Macro`] that **reference** a definition by
//! [`MacroId`]; they are never copies, so changing a definition changes
//! every instance.
//!
//! The root circuit owns the project's definition table ([`MacroTable`]);
//! the bodies of definitions refer to the same table, so macros nest.
//! Every change to the table goes through [`crate::Edit::DefineMacro`],
//! which refuses cycles (A contains B contains A), nesting deeper than
//! [`MAX_MACRO_DEPTH`] and designs whose flattened size exceeds
//! [`MAX_FLAT_COMPONENTS`] (a "macro bomb"), and keeps every instance's
//! pin layout ([`MacroShape`]) in sync, re-attaching wires by tag identity
//! when the interface changes.

#![allow(clippy::module_name_repetitions)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::circuit::{Applied, Circuit, CircuitError, CircuitRaw, Edit, MemoryRecord};
use crate::component::{
    Component, ComponentId, ComponentKind, DIL_SIZES, MAX_DIL_PINS, MAX_MACRO_SIDE_PINS, MAX_MACRO_WIDTH, MacroId,
    MacroShape, PinDir, PinRef, Rotation, TagDir,
};
use crate::geom::GridPos;
use crate::memenc;
use crate::wiring::{NodeAt, NodeId, NodeRecord, SegmentRecord, WireEnd};

/// Maximum nesting depth of macros (a macro without macros inside has
/// depth 1).
pub const MAX_MACRO_DEPTH: u32 = 16;
/// Maximum number of definitions in one project (also bounds the GPU mesh
/// slots of macro symbols).
pub const MAX_MACRO_DEFS: usize = 4096;
/// Maximum size of a fully flattened design, in primitive components
/// (every macro instance expanded recursively). Stops "macro bombs" such as
/// 16 levels of 10 instances each.
pub const MAX_FLAT_COMPONENTS: u64 = 2_000_000;
/// Maximum datasheet text size in bytes.
pub const MAX_DATASHEET_BYTES: usize = 64 * 1024;
/// Maximum macro name length in bytes.
pub const MAX_MACRO_NAME: usize = 64;
/// Maximum pin name length in bytes.
pub const MAX_PIN_NAME: usize = 12;
/// Maximum length of a datasheet link (a local file path) in bytes.
pub const MAX_DOC_LINK: usize = 1024;
/// Maximum length of a library key in bytes.
pub const MAX_LIBRARY_KEY: usize = 256;

/// One interface pin of a definition: the tag component in the body that
/// creates it, its name and (for a DIL template) its package pin number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacroPin {
    pub tag: ComponentId,
    pub name: String,
    /// DIL pin number (1-based); 0 = not assigned (auto-assigned).
    #[serde(default)]
    pub dil: u8,
}

/// Visual template of a macro instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Package {
    /// A box sized by the pin count: inputs left, outputs right.
    #[default]
    Auto,
    /// A dual-in-line package with this many pins (one of [`DIL_SIZES`]).
    Dil(u8),
}

/// Where a definition came from (a library entry and the content hash it
/// had when it was imported), for "update from library".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryRef {
    pub key: String,
    pub hash: u64,
}

/// A macro definition.
#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: String,
    /// The sub-circuit (a body circuit: its macro instances are checked
    /// against the project's table when the definition is stored).
    pub body: Circuit,
    /// Interface pins in order. Instances number their pins inputs first
    /// (in this order), then outputs (in this order).
    pub pins: Vec<MacroPin>,
    pub package: Package,
    /// Datasheet text (plain text / Markdown), shown in the app.
    pub datasheet: String,
    /// Optional link to a local PDF / HTML / text file, opened only on an
    /// explicit click and only for allow-listed extensions.
    pub doc_link: Option<String>,
    pub origin: Option<LibraryRef>,
}

impl PartialEq for MacroDef {
    fn eq(&self, o: &Self) -> bool {
        std::ptr::eq(self, o)
            || (self.name == o.name
                && self.pins == o.pins
                && self.package == o.package
                && self.datasheet == o.datasheet
                && self.doc_link == o.doc_link
                && self.origin == o.origin
                && self.body.content_eq(&o.body))
    }
}

impl Eq for MacroDef {}

/// Direction and tri-state flag of one instance pin, with its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IfacePin {
    pub tag: ComponentId,
    pub dir: TagDir,
    pub tristate: bool,
    /// Index into [`MacroDef::pins`].
    pub index: usize,
}

/// Width estimate of a label (stroke font, 0.6 units high).
fn label_w(n: usize) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let n = n as f32;
    n * 0.5
}

impl MacroDef {
    /// An empty definition named `name`.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            body: Circuit::new_body(),
            pins: Vec::new(),
            package: Package::Auto,
            datasheet: String::new(),
            doc_link: None,
            origin: None,
        }
    }

    /// Instance pin order: every pin with its tag's direction, inputs
    /// first. Pins whose tag is missing from the body are skipped (call
    /// [`Self::sync_pins`] first).
    #[must_use]
    pub fn interface(&self) -> Vec<IfacePin> {
        let mut ins = Vec::new();
        let mut outs = Vec::new();
        for (index, p) in self.pins.iter().enumerate() {
            let Some(ComponentKind::Tag { dir, tristate }) = self.body.component(p.tag).map(|c| c.kind) else {
                continue;
            };
            let ip = IfacePin { tag: p.tag, dir, tristate: tristate && dir == TagDir::Out, index };
            if dir == TagDir::In { ins.push(ip) } else { outs.push(ip) }
        }
        ins.extend(outs);
        ins
    }

    /// Reconciles the pin list with the tags of the body: pins whose tag
    /// was deleted disappear, new tags are appended (ordered by position)
    /// with a unique default name. Returns whether anything changed.
    pub fn sync_pins(&mut self) -> bool {
        let tags: BTreeMap<ComponentId, (TagDir, GridPos)> = self
            .body
            .components()
            .filter_map(|c| match c.kind {
                ComponentKind::Tag { dir, .. } => Some((c.id, (dir, c.pos))),
                _ => None,
            })
            .collect();
        let before = self.pins.len();
        self.pins.retain(|p| tags.contains_key(&p.tag));
        let mut changed = self.pins.len() != before;
        let known: BTreeSet<ComponentId> = self.pins.iter().map(|p| p.tag).collect();
        let mut new: Vec<(TagDir, GridPos, ComponentId)> =
            tags.iter().filter(|(id, _)| !known.contains(id)).map(|(&id, &(d, p))| (d, p, id)).collect();
        new.sort_by_key(|&(d, p, id)| (d == TagDir::Out, p.y, p.x, id));
        for (dir, _, tag) in new {
            let name = self.unique_pin_name(if dir == TagDir::In { "IN" } else { "OUT" });
            self.pins.push(MacroPin { tag, name, dil: 0 });
            changed = true;
        }
        changed
    }

    /// A pin name not used yet, `base` followed by a number.
    #[must_use]
    pub fn unique_pin_name(&self, base: &str) -> String {
        (0..=self.pins.len())
            .map(|i| format!("{base}{i}"))
            .find(|n| !self.pins.iter().any(|p| &p.name == n))
            .unwrap_or_default()
    }

    /// A copy whose nested macro instances refer to definitions renumbered
    /// by `map` (ids not in the map stay).
    ///
    /// # Errors
    /// Only if the body rejects the change (cannot happen for a valid body).
    pub fn with_macro_ids(&self, map: &HashMap<MacroId, MacroId>) -> Result<Self, CircuitError> {
        let mut d = self.clone();
        let insts: Vec<(ComponentId, MacroShape)> = d
            .body
            .components()
            .filter_map(|c| match c.kind {
                ComponentKind::Macro(s) => Some((c.id, s)),
                _ => None,
            })
            .collect();
        for (id, mut s) in insts {
            if let Some(&n) = map.get(&s.def) {
                s.def = n;
                d.body.set_kind(id, ComponentKind::Macro(s))?;
            }
        }
        d.body.log = crate::wiring::WireLog::default();
        Ok(d)
    }

    /// Fills missing / invalid DIL pin numbers with the lowest free ones
    /// (inputs first) and returns the instance pins' numbers.
    #[must_use]
    pub fn dil_numbers(&self, dil: u8) -> Vec<u8> {
        let iface = self.interface();
        let mut used = 0u32;
        let mut out: Vec<u8> = iface
            .iter()
            .map(|ip| {
                let n = self.pins[ip.index].dil;
                if n >= 1 && n <= dil && used & (1 << n) == 0 {
                    used |= 1 << n;
                    n
                } else {
                    0
                }
            })
            .collect();
        for n in &mut out {
            if *n == 0 {
                if let Some(free) = (1..=dil).find(|&k| used & (1 << k) == 0) {
                    used |= 1 << free;
                    *n = free;
                }
            }
        }
        out
    }

    /// The pin layout of instances of this definition (stored as `id`).
    #[must_use]
    pub fn shape(&self, id: MacroId) -> MacroShape {
        let iface = self.interface();
        let ins = iface.iter().filter(|p| p.dir == TagDir::In).count();
        let outs = iface.len() - ins;
        let mut s = MacroShape::unresolved(id);
        s.inputs = u8::try_from(ins).unwrap_or(u8::MAX);
        s.outputs = u8::try_from(outs).unwrap_or(u8::MAX);
        for (k, p) in iface[ins..].iter().enumerate() {
            if p.tristate && k < 32 {
                s.tristate |= 1 << k;
            }
        }
        let name_w = label_w(self.name.chars().count()) * 1.1;
        match self.package {
            Package::Dil(n) => {
                s.dil = n;
                for (k, num) in self.dil_numbers(n).into_iter().enumerate().take(MAX_DIL_PINS) {
                    s.dil_pins[k] = num;
                }
                // Pin numbers and names sit inside the body.
                let longest = iface.iter().map(|p| self.pins[p.index].name.chars().count()).max().unwrap_or(0);
                // Labels on both sides with the name running down the middle.
                let w = 2.0 * (label_w(longest) + 1.2) + 1.6;
                let _ = name_w;
                s.width = even_width(w);
            }
            Package::Auto => {
                let lw = |dir: TagDir| {
                    iface
                        .iter()
                        .filter(|p| p.dir == dir)
                        .map(|p| self.pins[p.index].name.chars().count())
                        .max()
                        .unwrap_or(0)
                };
                // Input labels, name and output labels side by side.
                let w = label_w(lw(TagDir::In)) + label_w(lw(TagDir::Out)) + name_w + 2.4;
                s.width = even_width(w);
            }
        }
        s
    }

    /// Checks the definition's own limits (not its relation to the table).
    ///
    /// # Errors
    /// [`CircuitError::BadMacro`] describing the first problem.
    pub fn validate(&self) -> Result<(), CircuitError> {
        let bad = |m: String| Err(CircuitError::BadMacro(m));
        if self.name.is_empty() || self.name.len() > MAX_MACRO_NAME || self.name.chars().any(char::is_control) {
            return bad(format!("macro name must be 1..={MAX_MACRO_NAME} bytes without control characters"));
        }
        if self.datasheet.len() > MAX_DATASHEET_BYTES {
            return bad(format!("datasheet larger than {MAX_DATASHEET_BYTES} bytes"));
        }
        if self.doc_link.as_ref().is_some_and(|l| l.is_empty() || l.len() > MAX_DOC_LINK || l.contains('\0')) {
            return bad("invalid datasheet link".into());
        }
        if self.origin.as_ref().is_some_and(|o| o.key.len() > MAX_LIBRARY_KEY) {
            return bad("library key too long".into());
        }
        let mut names = BTreeSet::new();
        let mut tags = BTreeSet::new();
        for p in &self.pins {
            if p.name.is_empty() || p.name.len() > MAX_PIN_NAME || p.name.chars().any(char::is_control) {
                return bad(format!("pin names must be 1..={MAX_PIN_NAME} bytes"));
            }
            if !names.insert(p.name.as_str()) {
                return bad(format!("duplicate pin name {:?}", p.name));
            }
            if !tags.insert(p.tag)
                || !matches!(self.body.component(p.tag).map(|c| c.kind), Some(ComponentKind::Tag { .. }))
            {
                return bad(format!("pin {:?} does not refer to a tag of the body", p.name));
            }
        }
        let tag_count = self.body.components().filter(|c| matches!(c.kind, ComponentKind::Tag { .. })).count();
        if tag_count != self.pins.len() {
            return bad("every tag of the body needs exactly one interface pin".into());
        }
        let iface = self.interface();
        let ins = iface.iter().filter(|p| p.dir == TagDir::In).count();
        let side = usize::from(MAX_MACRO_SIDE_PINS);
        if ins > side || iface.len() - ins > side {
            return bad(format!("at most {side} inputs and {side} outputs"));
        }
        if let Package::Dil(n) = self.package {
            if !DIL_SIZES.contains(&n) {
                return bad(format!("unsupported DIL package size {n}"));
            }
            if iface.len() > usize::from(n) {
                return bad(format!("{} pins do not fit a DIL-{n} package", iface.len()));
            }
            let mut used = BTreeSet::new();
            for p in &self.pins {
                if p.dil != 0 && (p.dil > n || !used.insert(p.dil)) {
                    return bad(format!("DIL pin number {} used twice or out of range", p.dil));
                }
            }
        }
        if !self.shape(MacroId(0)).is_valid() {
            return bad("instance layout out of range (too many pins or too wide)".into());
        }
        Ok(())
    }
}

fn even_width(w: f32) -> u8 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped")]
    let w = w.ceil().clamp(6.0, f32::from(MAX_MACRO_WIDTH)) as u8;
    (w + 1) & !1
}

/// A definition to store under an id (see [`MacroTable::import_plan`]).
pub type NewDef = (MacroId, Arc<MacroDef>);

/// One definition in the table, with the values derived from it and its
/// children.
#[derive(Debug, Clone)]
pub struct MacroEntry {
    pub def: Arc<MacroDef>,
    /// Layout of its instances.
    pub shape: MacroShape,
    /// Content hash (name, interface, template, datasheet, body; nested
    /// definitions by *their* content hash, not by id).
    pub hash: u64,
    /// Primitive components when fully flattened.
    pub flat: u64,
    /// Nesting depth (1 = no macros inside).
    pub depth: u32,
    /// Definitions instantiated directly in the body.
    pub children: BTreeSet<MacroId>,
}

static NEXT_TABLE_REVISION: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    NEXT_TABLE_REVISION.fetch_add(1, Ordering::Relaxed)
}

/// The definitions of a project. Cheap to clone (definitions are shared).
#[derive(Debug, Clone)]
pub struct MacroTable {
    entries: BTreeMap<MacroId, MacroEntry>,
    /// Unique per change in this process (renderers compare it per frame).
    revision: u64,
}

impl Default for MacroTable {
    fn default() -> Self {
        Self { entries: BTreeMap::new(), revision: next_revision() }
    }
}

/// FNV-1a, 64 bit: a stable content hash (independent of the process).
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

impl MacroTable {
    #[must_use]
    pub fn get(&self, id: MacroId) -> Option<&MacroEntry> {
        self.entries.get(&id)
    }

    #[must_use]
    pub fn def(&self, id: MacroId) -> Option<&Arc<MacroDef>> {
        self.entries.get(&id).map(|e| &e.def)
    }

    /// Definitions in id order.
    pub fn iter(&self) -> impl Iterator<Item = (MacroId, &MacroEntry)> {
        self.entries.iter().map(|(&id, e)| (id, e))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Changes whenever the table changes (unique within the process).
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The lowest unused id, if the table is not full.
    #[must_use]
    pub fn free_id(&self) -> Option<MacroId> {
        (0..u32::try_from(MAX_MACRO_DEFS).unwrap_or(u32::MAX)).map(MacroId).find(|id| !self.entries.contains_key(id))
    }

    /// A definition with exactly this content hash, if any.
    #[must_use]
    pub fn find_hash(&self, hash: u64) -> Option<MacroId> {
        self.entries.iter().find(|(_, e)| e.hash == hash).map(|(&id, _)| id)
    }

    /// Does `from` contain `target`, directly or through nested macros
    /// (or is it `target`)?
    #[must_use]
    pub fn reaches(&self, from: MacroId, target: MacroId) -> bool {
        let mut stack = vec![from];
        let mut seen = BTreeSet::new();
        while let Some(d) = stack.pop() {
            if d == target {
                return true;
            }
            if seen.insert(d) {
                if let Some(e) = self.entries.get(&d) {
                    stack.extend(e.children.iter().copied());
                }
            }
        }
        false
    }

    /// Definitions that contain `id` (transitively), innermost first.
    #[must_use]
    pub fn ancestors(&self, id: MacroId) -> Vec<MacroId> {
        let mut v: Vec<MacroId> = self.entries.keys().copied().filter(|&d| d != id && self.reaches(d, id)).collect();
        v.sort_by_key(|d| (self.entries.get(d).map_or(0, |e| e.depth), *d));
        v
    }

    /// Definitions that instantiate `id` directly.
    #[must_use]
    pub fn users(&self, id: MacroId) -> Vec<MacroId> {
        self.entries.iter().filter(|(_, e)| e.children.contains(&id)).map(|(&d, _)| d).collect()
    }

    /// Checks `def` as the definition stored under `id` against the rest of
    /// the table and computes its derived values.
    ///
    /// # Errors
    /// Invalid definition, unknown or mismatched nested macro, a cycle,
    /// too deep or too large when flattened.
    pub fn evaluate(&self, id: MacroId, def: Arc<MacroDef>) -> Result<MacroEntry, CircuitError> {
        def.validate()?;
        if id.0 as usize >= MAX_MACRO_DEFS {
            return Err(CircuitError::TooManyMacros);
        }
        let mut flat: u64 = 0;
        let mut depth = 1u32;
        let mut children = BTreeSet::new();
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        def.name.hash(&mut h);
        def.pins.hash(&mut h);
        def.package.hash(&mut h);
        def.datasheet.hash(&mut h);
        def.doc_link.hash(&mut h);
        for c in def.body.components() {
            c.id.hash(&mut h);
            c.pos.hash(&mut h);
            c.rotation.hash(&mut h);
            match c.kind {
                ComponentKind::Macro(shape) => {
                    let child = self.entries.get(&shape.def).ok_or(CircuitError::NoSuchMacro(shape.def))?;
                    if shape.def == id || self.reaches(shape.def, id) {
                        return Err(CircuitError::MacroCycle(shape.def));
                    }
                    let mut expect = child.shape;
                    expect.def = shape.def;
                    if shape != expect {
                        return Err(CircuitError::InvalidComponent(c.id));
                    }
                    flat = flat.saturating_add(child.flat);
                    depth = depth.max(child.depth.saturating_add(1));
                    children.insert(shape.def);
                    // Nested definitions count by content, not by id.
                    0xffu8.hash(&mut h);
                    child.hash.hash(&mut h);
                }
                ComponentKind::Tag { .. } => c.kind.hash(&mut h),
                k => {
                    k.hash(&mut h);
                    flat = flat.saturating_add(1);
                }
            }
        }
        let (nodes, segs) = def.body.wiring_records();
        for n in nodes {
            n.id.hash(&mut h);
            match n.at {
                NodeAt::Pin(p) => (0u8, p).hash(&mut h),
                NodeAt::Free(p) => (1u8, p).hash(&mut h),
            }
        }
        for s in segs {
            (s.a, s.b).hash(&mut h);
        }
        for (id, m) in &def.body.memories {
            id.hash(&mut h);
            m.hash(&mut h);
        }
        if depth > MAX_MACRO_DEPTH {
            return Err(CircuitError::MacroTooDeep { limit: MAX_MACRO_DEPTH });
        }
        if flat > MAX_FLAT_COMPONENTS {
            return Err(CircuitError::MacroTooLarge { size: flat, limit: MAX_FLAT_COMPONENTS });
        }
        Ok(MacroEntry { shape: def.shape(id), def, hash: h.finish(), flat, depth, children })
    }

    /// Adds a new definition (loaders: definitions must be added after the
    /// ones they use).
    ///
    /// # Errors
    /// Duplicate id, too many definitions, or anything [`Self::evaluate`]
    /// refuses.
    pub fn add(&mut self, id: MacroId, def: Arc<MacroDef>) -> Result<(), CircuitError> {
        if self.entries.contains_key(&id) {
            return Err(CircuitError::BadMacro(format!("duplicate macro id {}", id.0)));
        }
        if self.entries.len() >= MAX_MACRO_DEFS {
            return Err(CircuitError::TooManyMacros);
        }
        let e = self.evaluate(id, def)?;
        self.insert(id, e);
        Ok(())
    }

    /// Plans importing definition `root` of `source` (e.g. a library file)
    /// into this table: definitions with identical content already here are
    /// reused, the others get free ids. Returns the definitions to store,
    /// in order (each after the ones it uses), and the id `root` gets.
    ///
    /// # Errors
    /// The table is full, or a definition cannot be re-targeted.
    pub fn import_plan(&self, source: &Self, root: MacroId) -> Result<(Vec<NewDef>, MacroId), CircuitError> {
        let mut map: HashMap<MacroId, MacroId> = HashMap::new();
        let mut out = Vec::new();
        let mut taken: BTreeSet<MacroId> = self.entries.keys().copied().collect();
        for sid in source.closure(root) {
            let e = source.get(sid).ok_or(CircuitError::NoSuchMacro(sid))?;
            if let Some(existing) = self.find_hash(e.hash) {
                map.insert(sid, existing);
                continue;
            }
            let nid = (0..u32::try_from(MAX_MACRO_DEFS).unwrap_or(u32::MAX))
                .map(MacroId)
                .find(|id| !taken.contains(id))
                .ok_or(CircuitError::TooManyMacros)?;
            taken.insert(nid);
            map.insert(sid, nid);
            out.push((nid, Arc::new(e.def.with_macro_ids(&map)?)));
        }
        let new_root = *map.get(&root).ok_or(CircuitError::NoSuchMacro(root))?;
        Ok((out, new_root))
    }

    pub(crate) fn insert(&mut self, id: MacroId, e: MacroEntry) {
        self.entries.insert(id, e);
        self.revision = next_revision();
    }

    pub(crate) fn remove(&mut self, id: MacroId) -> Option<MacroEntry> {
        let r = self.entries.remove(&id);
        self.revision = next_revision();
        r
    }

    /// Definitions needed by `id` (itself and everything nested), in an
    /// order where each comes after the ones it uses.
    #[must_use]
    pub fn closure(&self, id: MacroId) -> Vec<MacroId> {
        let mut v: Vec<MacroId> = self.entries.keys().copied().filter(|&d| self.reaches(id, d)).collect();
        v.sort_by_key(|d| (self.entries.get(d).map_or(0, |e| e.depth), *d));
        v
    }
}

/// How the pins of instances map when a definition's interface changes:
/// `map[old pin] = new pin`, matched by tag; a pin whose tag disappeared or
/// changed direction or tri-state flag is dropped (its wires detached).
#[must_use]
pub fn pin_remap(old: &MacroDef, new: &MacroDef) -> Vec<Option<u8>> {
    let new_iface = new.interface();
    old.interface()
        .iter()
        .map(|op| {
            new_iface
                .iter()
                .position(|np| np.tag == op.tag && np.dir == op.dir && np.tristate == op.tristate)
                .and_then(|i| u8::try_from(i).ok())
        })
        .collect()
}

// ---- create a macro from a selection --------------------------------------

/// A net crossing the boundary of a selection being turned into a macro.
struct Crossing {
    inside: Vec<PinRef>,
    outside: Vec<PinRef>,
    dir: TagDir,
    tristate: bool,
    name: Option<String>,
}

/// What turning a selection into a macro does to the parent circuit.
#[derive(Debug, Clone)]
pub struct MacroPlan {
    pub def: MacroDef,
    /// Where the instance goes (top-left of the selection).
    pub origin: GridPos,
    /// Components moved into the macro.
    pub remove: Vec<ComponentId>,
    /// For each instance pin (instance order): the pins outside the
    /// selection that were on that net.
    pub links: Vec<Vec<PinRef>>,
}

fn sensible_name(pin_name: &str) -> Option<String> {
    let n = pin_name.split_whitespace().next().unwrap_or("");
    let n = n.replace('\u{304}', "N").replace('/', "");
    let generic = n.is_empty() || n.starts_with("in") || n == "out" || n.starts_with("out");
    (!generic && n.len() <= MAX_PIN_NAME && n.is_ascii()).then(|| n.to_ascii_uppercase())
}

impl Circuit {
    /// Plans turning the components `sel` into a macro named `name`.
    /// Nets entirely inside the selection are copied with their wiring;
    /// nets crossing the selection boundary become tags (an `In` tag when
    /// driven from outside or undriven, an `Out` tag when driven inside;
    /// tri-state if every inside driver is), named after the pin they
    /// connect to when that name is meaningful (`D`, `CLR`, `Q`…), else
    /// `A`, `B`, … for inputs and `Y0`, `Y1`, … for outputs.
    ///
    /// # Errors
    /// Empty selection, tags in the selection, or a bus driven from both
    /// sides of the boundary.
    #[allow(clippy::too_many_lines, reason = "one linear pass")]
    pub fn plan_macro(&self, sel: &[ComponentId], name: &str) -> Result<MacroPlan, CircuitError> {
        let sel: BTreeSet<ComponentId> = sel.iter().copied().filter(|id| self.components.contains_key(id)).collect();
        if sel.is_empty() {
            return Err(CircuitError::BadMacro("nothing selected".into()));
        }
        let mut lo = GridPos::new(i32::MAX, i32::MAX);
        let mut hi = GridPos::new(i32::MIN, i32::MIN);
        for id in &sel {
            let c = &self.components[id];
            if matches!(c.kind, ComponentKind::Tag { .. }) {
                return Err(CircuitError::BadMacro("macro tags cannot be moved into a new macro".into()));
            }
            let b = c.bounds();
            lo = GridPos::new(lo.x.min(b.min.x), lo.y.min(b.min.y));
            hi = GridPos::new(hi.x.max(b.max.x), hi.y.max(b.max.y));
        }
        // Body coordinates: selection shifted so that it starts at (12, 2).
        let shift = |p: GridPos| GridPos::new(p.x - lo.x + 12, p.y - lo.y + 2);
        let new_id: BTreeMap<ComponentId, ComponentId> =
            sel.iter().enumerate().map(|(i, &id)| (id, ComponentId(u32::try_from(i).unwrap_or(u32::MAX)))).collect();
        let mut raw = CircuitRaw::default();
        for (&old, &nid) in &new_id {
            let c = &self.components[&old];
            raw.components.push(Component { id: nid, kind: c.kind, pos: shift(c.pos), rotation: c.rotation });
            if let Some(m) = self.memories.get(&old) {
                raw.memories.push(MemoryRecord { id: nid, words: memenc::encode(m) });
            }
        }
        // Classify the nets touching the selection.
        let mut crossings: Vec<Crossing> = Vec::new();
        let mut seen_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let mut next_node = 0u32;
        let pins_of = |id: ComponentId| {
            let n = self.components[&id].kind.pin_count();
            (0..n).map(move |pin| PinRef { component: id, pin })
        };
        for &id in &sel {
            for pin in pins_of(id) {
                let Some(start) = self.pin_node(pin) else { continue };
                if seen_nodes.contains(&start) {
                    continue;
                }
                let mut nodes = Vec::new();
                let mut segs = Vec::new();
                self.visit_net(start, |n, _| nodes.push(n), |s| segs.push(s));
                seen_nodes.extend(nodes.iter().copied());
                let pins: Vec<PinRef> = nodes.iter().filter_map(|&n| self.node(n).and_then(|w| w.pin)).collect();
                let (inside, outside): (Vec<PinRef>, Vec<PinRef>) =
                    pins.into_iter().partition(|p| sel.contains(&p.component));
                if outside.is_empty() {
                    // Internal net: copy its wiring.
                    let mut map: HashMap<NodeId, NodeId> = HashMap::new();
                    for &n in &nodes {
                        let Some(w) = self.node(n) else { continue };
                        let at = match w.pin {
                            Some(p) => NodeAt::Pin(PinRef { component: new_id[&p.component], pin: p.pin }),
                            None => NodeAt::Free(shift(w.pos)),
                        };
                        let fid = NodeId(next_node);
                        next_node += 1;
                        map.insert(n, fid);
                        raw.nodes.push(NodeRecord { id: fid, at });
                    }
                    for s in segs {
                        if let Some(sg) = self.segment(s) {
                            raw.segments.push(SegmentRecord { a: map[&sg.a], b: map[&sg.b] });
                        }
                    }
                    continue;
                }
                let is_out = |p: &PinRef| {
                    self.components.get(&p.component).and_then(|c| c.pin_dir(p.pin)) == Some(PinDir::Output)
                };
                let tri = |p: &PinRef| {
                    self.components
                        .get(&p.component)
                        .is_some_and(|c| c.kind.output_tristate(p.pin.saturating_sub(c.kind.input_count())))
                };
                let in_drv: Vec<&PinRef> = inside.iter().filter(|p| is_out(p)).collect();
                let out_drv = outside.iter().any(is_out);
                if !in_drv.is_empty() && out_drv {
                    return Err(CircuitError::BadMacro(
                        "a net driven both inside and outside the selection (a bus) cannot cross the macro boundary"
                            .into(),
                    ));
                }
                let (dir, tristate) =
                    if in_drv.is_empty() { (TagDir::In, false) } else { (TagDir::Out, in_drv.iter().all(|p| tri(p))) };
                let names: BTreeSet<Option<String>> = inside
                    .iter()
                    .filter(|p| dir == TagDir::In || is_out(p))
                    .map(|p| sensible_name(&self.components[&p.component].kind.pin_name(p.pin)))
                    .collect();
                let name = if names.len() == 1 { names.into_iter().next().flatten() } else { None };
                crossings.push(Crossing { inside, outside, dir, tristate, name });
            }
        }
        // Order: inputs then outputs, each by position of their first pin.
        let pos_of = |p: &PinRef| self.components.get(&p.component).and_then(|c| c.pin_pos(p.pin)).unwrap_or_default();
        crossings.sort_by_key(|c| {
            let p = c.inside.iter().map(pos_of).min_by_key(|p| (p.y, p.x)).unwrap_or_default();
            (c.dir == TagDir::Out, p.y, p.x)
        });
        let mut def = MacroDef::new(name);
        def.body = Circuit::from_raw(raw, &MacroTable::default(), true)?;
        let (mut n_in, mut n_out) = (0usize, 0usize);
        let width = hi.x - lo.x;
        let mut used = BTreeSet::new();
        let mut links = Vec::new();
        for c in &crossings {
            let (k, x) = if c.dir == TagDir::In {
                n_in += 1;
                (n_in - 1, 2)
            } else {
                n_out += 1;
                (n_out - 1, width + 12 + 4 + i32::try_from(crossings.len()).unwrap_or(0))
            };
            let y = 2 + 3 * i32::try_from(k).unwrap_or(0);
            let tag = def.body.add_component(
                ComponentKind::Tag { dir: c.dir, tristate: c.tristate },
                GridPos::new(x, y),
                Rotation::R0,
            )?;
            let tag_pin = PinRef { component: tag, pin: 0 };
            let tag_pos = GridPos::new(if c.dir == TagDir::In { x + 4 } else { x }, y + 1);
            // Private column per tag between the tags and the selection.
            let col = if c.dir == TagDir::In {
                7 + i32::try_from(k).unwrap_or(0)
            } else {
                width + 13 + i32::try_from(k).unwrap_or(0)
            };
            let mut bend: Option<GridPos> = None;
            for p in &c.inside {
                let ip = PinRef { component: new_id[&p.component], pin: p.pin };
                let pp = def.body.component(ip.component).and_then(|cc| cc.pin_pos(ip.pin)).unwrap_or_default();
                let from = match bend.and_then(|b| def.body.node_at(b)) {
                    Some(n) => WireEnd::Node(n),
                    None => WireEnd::Pin(tag_pin),
                };
                let via = if bend.is_some() {
                    vec![GridPos::new(col, pp.y)]
                } else {
                    vec![GridPos::new(col, tag_pos.y), GridPos::new(col, pp.y)]
                };
                if def.body.apply(&Edit::AddWire { from, to: WireEnd::Pin(ip), via }).is_ok() && bend.is_none() {
                    bend = Some(GridPos::new(col, tag_pos.y));
                }
            }
            let base = c.name.clone().filter(|n| !used.contains(n));
            let name = base.unwrap_or_else(|| {
                if c.dir == TagDir::In {
                    let letters = "ABCDEFGHIJKLMNOPRSTUVWZ";
                    letters
                        .chars()
                        .map(String::from)
                        .chain((0..=used.len()).map(|i| format!("I{i}")))
                        .find(|n| !used.contains(n))
                        .unwrap_or_default()
                } else {
                    (0..=used.len()).map(|i| format!("Y{i}")).find(|n| !used.contains(n)).unwrap_or_default()
                }
            });
            used.insert(name.clone());
            def.pins.push(MacroPin { tag, name, dil: 0 });
            links.push(c.outside.clone());
        }
        // Instance pin order is inputs first: `crossings` is already sorted.
        Ok(MacroPlan { def, origin: lo, remove: sel.into_iter().collect(), links })
    }

    /// Node at a grid point (a linear scan; small body circuits only).
    fn node_at(&self, p: GridPos) -> Option<NodeId> {
        self.nodes().find(|(_, w)| w.pos == p).map(|(id, _)| id)
    }

    /// Carries out a [`MacroPlan`] on this (root) circuit, see
    /// [`run_macro_plan`].
    pub fn apply_macro_plan(&mut self, plan: MacroPlan, id: MacroId) -> (Vec<(Edit, Applied)>, Option<CircuitError>) {
        struct Root<'a>(&'a mut Circuit, Vec<(Edit, Applied)>);
        impl PlanTarget for Root<'_> {
            fn define(&mut self, e: Edit) -> Result<Applied, CircuitError> {
                let a = self.0.apply(&e)?;
                self.1.push((e, a.clone()));
                Ok(a)
            }
            fn edit(&mut self, e: Edit) -> Result<Applied, CircuitError> {
                self.define(e)
            }
            fn circuit(&self) -> &Circuit {
                self.0
            }
            fn table(&self) -> &MacroTable {
                &self.0.macros
            }
        }
        let mut r = Root(self, Vec::new());
        let err = run_macro_plan(&mut r, plan, id).err();
        (r.1, err)
    }
}

/// Where a [`MacroPlan`] is carried out: definitions go to the project's
/// root circuit, the other edits to the circuit being edited (the root, or
/// a definition body when editing inside a macro).
pub trait PlanTarget {
    /// Applies a [`Edit::DefineMacro`] to the root.
    ///
    /// # Errors
    /// As [`Circuit::apply`].
    fn define(&mut self, e: Edit) -> Result<Applied, CircuitError>;
    /// Applies an edit to the circuit being edited.
    ///
    /// # Errors
    /// As [`Circuit::apply`].
    fn edit(&mut self, e: Edit) -> Result<Applied, CircuitError>;
    /// The circuit being edited.
    fn circuit(&self) -> &Circuit;
    /// The project's definitions.
    fn table(&self) -> &MacroTable;
}

/// Carries out a [`MacroPlan`] as a sequence of ordinary edits: define the
/// macro as `id`, remove the selection, place an instance where it was and
/// wire each instance pin to the outside pins of its net (one wire per
/// distinct remaining net). Stops at the first error (the edits already
/// applied are valid on their own).
///
/// # Errors
/// The first edit that fails.
pub fn run_macro_plan(t: &mut dyn PlanTarget, plan: MacroPlan, id: MacroId) -> Result<ComponentId, CircuitError> {
    t.define(Edit::DefineMacro { id, def: Arc::new(plan.def) })?;
    for &r in &plan.remove {
        t.edit(Edit::RemoveComponent(r))?;
    }
    let shape = t.table().get(id).map(|e| e.shape).ok_or(CircuitError::NoSuchMacro(id))?;
    let inst = t
        .edit(Edit::AddComponent { kind: ComponentKind::Macro(shape), pos: plan.origin, rotation: Rotation::R0 })?
        .component()
        .ok_or(CircuitError::NoSuchMacro(id))?;
    for (k, outside) in plan.links.iter().enumerate() {
        let pin = PinRef { component: inst, pin: u8::try_from(k).unwrap_or(u8::MAX) };
        let mut done_nets: Vec<NodeId> = Vec::new();
        for &o in outside {
            let c = t.circuit();
            if c.component(o.component).is_none() {
                continue;
            }
            if let Some(n) = c.pin_node(o) {
                let mut same = false;
                c.visit_net(n, |m, _| same |= done_nets.contains(&m), |_| {});
                if same {
                    continue;
                }
            }
            t.edit(Edit::AddWire { from: WireEnd::Pin(pin), to: WireEnd::Pin(o), via: Vec::new() })?;
            if let Some(n) = t.circuit().pin_node(o) {
                done_nets.push(n);
            }
        }
    }
    Ok(inst)
}
