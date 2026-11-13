//! Flattening of the macro hierarchy into one flat list of primitive
//! components and nets, at compile time (the simulator has no runtime cost
//! for hierarchy).
//!
//! * Every definition body (and the root circuit) is turned once into a
//!   shallow [`Template`]: its own primitive components, its macro
//!   instances and its wire nets as lists of *ends* (a primitive pin, an
//!   instance pin, or one of the definition's own ports = tags). Templates
//!   are cached per definition by content hash ([`FlattenCache`]), so after
//!   editing a definition only that template (and the root's) is rebuilt.
//! * [`Flat::expand`] walks the hierarchy from the root, appending the
//!   primitives of every instance and joining nets across levels with a
//!   union-find over *net handles* (one per local net; an instance's port is
//!   unified with the parent net wired to that pin).
//! * Every instance gets a [`Scope`] (its instance path) recording where its
//!   primitives start and the handles of its ports, which is what the
//!   inside view and error messages use.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bw_core::{Circuit, ComponentId, ComponentKind, MacroId, MacroTable, MemoryImage, PinRef, TagDir};

use crate::SimError;

/// Sentinel: no net handle.
pub const NO_HANDLE: u32 = u32::MAX;

/// One end of a local net.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// Pin of the `i`-th primitive.
    Prim(u32, u8),
    /// Pin of the `i`-th macro instance.
    Inst(u32, u8),
    /// The definition's own port `k` (a tag).
    Port(u8),
}

/// What a component of a template's circuit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    Prim(u32),
    Inst(u32),
    Port(u8),
}

/// A primitive component of a template.
#[derive(Debug, Clone)]
pub struct PrimRec {
    pub id: ComponentId,
    pub kind: ComponentKind,
    pub mem: Option<MemoryImage>,
}

/// A macro instance inside a template.
#[derive(Debug, Clone, Copy)]
pub struct InstRec {
    pub id: ComponentId,
    pub def: MacroId,
    pub inputs: u8,
    pub outputs: u8,
}

/// The shallow, cacheable compilation of one circuit level.
#[derive(Debug, Default)]
pub struct Template {
    pub prims: Vec<PrimRec>,
    pub insts: Vec<InstRec>,
    /// CSR of local nets: net `i` has `ends[net_start[i]..net_start[i + 1]]`.
    net_start: Vec<u32>,
    ends: Vec<End>,
    /// Number of ports (tags in interface order).
    pub ports: usize,
    /// Direction of each port.
    pub port_dir: Vec<TagDir>,
    pub local: HashMap<ComponentId, Local>,
}

impl Template {
    /// Compiles one circuit level. `iface` lists the tags in instance pin
    /// order (empty for the root, whose tags get unconnected ports).
    #[must_use]
    pub fn build(c: &Circuit, iface: &[ComponentId]) -> Self {
        let mut t = Self::default();
        t.local.reserve(c.component_count());
        let port_of: HashMap<ComponentId, u8> =
            iface.iter().enumerate().map(|(k, &id)| (id, u8::try_from(k).unwrap_or(u8::MAX))).collect();
        let mut extra_ports = 0usize;
        for comp in c.components() {
            match comp.kind {
                ComponentKind::Macro(s) => {
                    let i = u32::try_from(t.insts.len()).unwrap_or(u32::MAX);
                    t.insts.push(InstRec { id: comp.id, def: s.def, inputs: s.inputs, outputs: s.outputs });
                    t.local.insert(comp.id, Local::Inst(i));
                }
                ComponentKind::Tag { dir, .. } => {
                    let k = port_of.get(&comp.id).copied().unwrap_or_else(|| {
                        extra_ports += 1;
                        u8::try_from(iface.len() + extra_ports - 1).unwrap_or(u8::MAX)
                    });
                    t.local.insert(comp.id, Local::Port(k));
                    let k = usize::from(k);
                    if t.port_dir.len() <= k {
                        t.port_dir.resize(k + 1, TagDir::In);
                    }
                    t.port_dir[k] = dir;
                }
                kind => {
                    let i = u32::try_from(t.prims.len()).unwrap_or(u32::MAX);
                    let mem = matches!(kind, ComponentKind::Memory { .. }).then(|| c.memory(comp.id)).flatten();
                    t.prims.push(PrimRec { id: comp.id, kind, mem });
                    t.local.insert(comp.id, Local::Prim(i));
                }
            }
        }
        t.ports = t.port_dir.len().max(iface.len());
        t.port_dir.resize(t.ports, TagDir::In);
        let nets = c.nets();
        t.net_start.reserve(nets.len() + 1);
        t.net_start.push(0);
        t.ends.reserve(nets.pins.len());
        for i in 0..nets.len() {
            for &PinRef { component, pin } in nets.net(i) {
                let end = match t.local.get(&component) {
                    Some(Local::Prim(j)) => End::Prim(*j, pin),
                    Some(Local::Inst(j)) => End::Inst(*j, pin),
                    Some(Local::Port(k)) => End::Port(*k),
                    None => continue,
                };
                t.ends.push(end);
            }
            t.net_start.push(u32::try_from(t.ends.len()).unwrap_or(u32::MAX));
        }
        t
    }

    #[must_use]
    pub fn net_count(&self) -> usize {
        self.net_start.len().saturating_sub(1)
    }

    fn net(&self, i: usize) -> &[End] {
        &self.ends[self.net_start[i] as usize..self.net_start[i + 1] as usize]
    }
}

/// Cached templates of definitions, by content hash.
#[derive(Debug, Default)]
pub struct FlattenCache {
    templates: HashMap<MacroId, (u64, Arc<Template>)>,
}

impl FlattenCache {
    fn get(&mut self, table: &MacroTable, id: MacroId, stats: &mut CompileStats) -> Option<Arc<Template>> {
        let e = table.get(id)?;
        if let Some((h, t)) = self.templates.get(&id) {
            if *h == e.hash {
                stats.templates_reused += 1;
                return Some(Arc::clone(t));
            }
        }
        let iface: Vec<ComponentId> = e.def.interface().iter().map(|p| p.tag).collect();
        let t = Arc::new(Template::build(&e.def.body, &iface));
        stats.templates_built += 1;
        self.templates.insert(id, (e.hash, Arc::clone(&t)));
        Some(t)
    }

    /// Drops templates of definitions no longer in `table`.
    pub fn retain(&mut self, table: &MacroTable) {
        self.templates.retain(|id, (h, _)| table.get(*id).is_some_and(|e| e.hash == *h));
    }
}

/// An instance path (root = scope 0).
#[derive(Debug, Clone)]
pub struct Scope {
    pub parent: u32,
    /// The instance component (in the parent's circuit).
    pub inst: ComponentId,
    pub def: Option<MacroId>,
    pub name: Arc<str>,
    pub template: Arc<Template>,
    /// Index of the scope's first primitive in [`Flat::prims`].
    pub prim_base: u32,
    /// Net handle of each port.
    pub ports: Vec<u32>,
}

/// One primitive of the flattened design.
#[derive(Debug, Clone)]
pub struct FlatPrim {
    pub kind: ComponentKind,
    pub scope: u32,
    pub id: ComponentId,
    pub mem: Option<MemoryImage>,
}

/// Timings and counters of one compilation.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CompileStats {
    pub templates_built: u32,
    pub templates_reused: u32,
    pub instances: u32,
    pub primitives: u32,
    pub nets: u32,
    pub flatten: Duration,
    pub netlist: Duration,
}

impl CompileStats {
    #[must_use]
    pub fn total(&self) -> Duration {
        self.flatten + self.netlist
    }
}

/// The flattened design.
#[derive(Debug, Default)]
pub struct Flat {
    pub prims: Vec<FlatPrim>,
    /// Pins of primitive `i` are `pin_handle[pin_start[i]..pin_start[i + 1]]`.
    pub pin_start: Vec<u32>,
    pub pin_handle: Vec<u32>,
    parent: Vec<u32>,
    pub scopes: Vec<Scope>,
    /// (scope, instance component) → child scope.
    pub child: HashMap<(u32, ComponentId), u32>,
}

impl Flat {
    fn new_handle(&mut self) -> Result<u32, SimError> {
        let h = u32::try_from(self.parent.len()).ok().filter(|&h| h != NO_HANDLE).ok_or(SimError::TooLarge)?;
        self.parent.push(h);
        Ok(h)
    }

    /// Representative of a handle's set.
    pub fn find(&mut self, mut i: u32) -> u32 {
        while self.parent[i as usize] != i {
            let p = self.parent[i as usize];
            self.parent[i as usize] = self.parent[p as usize];
            i = p;
        }
        i
    }

    fn union(&mut self, a: u32, b: u32) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a.max(b) as usize] = a.min(b);
        }
    }

    #[must_use]
    pub fn handle_count(&self) -> usize {
        self.parent.len()
    }

    /// Flattens `root` (with its table) into primitives and net handles.
    ///
    /// # Errors
    /// [`SimError::TooLarge`] if the design cannot be indexed with `u32`.
    pub fn expand(root: &Circuit, cache: &mut FlattenCache, stats: &mut CompileStats) -> Result<Self, SimError> {
        let t0 = Instant::now();
        let table = root.macros();
        cache.retain(table);
        let root_t = Arc::new(Template::build(root, &[]));
        let size = usize::try_from(root.flat_size()).unwrap_or(usize::MAX);
        let mut f = Self {
            prims: Vec::with_capacity(size),
            pin_start: Vec::with_capacity(size + 1),
            pin_handle: Vec::with_capacity(size * 3),
            ..Self::default()
        };
        f.pin_start.push(0);
        let mut ports = Vec::with_capacity(root_t.ports);
        for _ in 0..root_t.ports {
            ports.push(f.new_handle()?);
        }
        f.scopes.push(Scope {
            parent: 0,
            inst: ComponentId(u32::MAX),
            def: None,
            name: Arc::from("root"),
            template: root_t,
            prim_base: 0,
            ports,
        });
        f.expand_scope(0, table, cache, stats, 0)?;
        stats.instances = u32::try_from(f.scopes.len() - 1).unwrap_or(u32::MAX);
        stats.primitives = u32::try_from(f.prims.len()).unwrap_or(u32::MAX);
        stats.flatten = t0.elapsed();
        Ok(f)
    }

    fn expand_scope(
        &mut self,
        s: u32,
        table: &MacroTable,
        cache: &mut FlattenCache,
        stats: &mut CompileStats,
        depth: u32,
    ) -> Result<(), SimError> {
        if depth > bw_core::MAX_MACRO_DEPTH + 1 {
            return Err(SimError::TooLarge);
        }
        let t = Arc::clone(&self.scopes[s as usize].template);
        let base = u32::try_from(self.prims.len()).map_err(|_| SimError::TooLarge)?;
        self.scopes[s as usize].prim_base = base;
        for p in &t.prims {
            self.prims.push(FlatPrim { kind: p.kind, scope: s, id: p.id, mem: p.mem.clone() });
            let pins = usize::from(p.kind.pin_count());
            self.pin_handle.extend(std::iter::repeat_n(NO_HANDLE, pins));
            self.pin_start.push(u32::try_from(self.pin_handle.len()).map_err(|_| SimError::TooLarge)?);
        }
        let mut inst_pins: Vec<Vec<u32>> =
            t.insts.iter().map(|i| vec![NO_HANDLE; usize::from(i.inputs) + usize::from(i.outputs)]).collect();
        for n in 0..t.net_count() {
            let h = self.new_handle()?;
            for &end in t.net(n) {
                match end {
                    End::Prim(j, pin) => {
                        let at = self.pin_start[(base + j) as usize] as usize + usize::from(pin);
                        if let Some(slot) = self.pin_handle.get_mut(at) {
                            *slot = h;
                        }
                    }
                    End::Inst(j, pin) => {
                        if let Some(slot) = inst_pins.get_mut(j as usize).and_then(|v| v.get_mut(usize::from(pin))) {
                            *slot = h;
                        }
                    }
                    End::Port(k) => {
                        if let Some(&p) = self.scopes[s as usize].ports.get(usize::from(k)) {
                            self.union(h, p);
                        }
                    }
                }
            }
        }
        for (j, inst) in t.insts.iter().enumerate() {
            let Some(child_t) = cache.get(table, inst.def, stats) else { continue };
            let mut ports = std::mem::take(&mut inst_pins[j]);
            for h in &mut ports {
                if *h == NO_HANDLE {
                    *h = self.new_handle()?;
                }
            }
            while ports.len() < child_t.ports {
                ports.push(self.new_handle()?);
            }
            let name = table.def(inst.def).map_or_else(|| Arc::from("?"), |d| Arc::from(d.name.as_str()));
            let cs = u32::try_from(self.scopes.len()).map_err(|_| SimError::TooLarge)?;
            self.scopes.push(Scope {
                parent: s,
                inst: inst.id,
                def: Some(inst.def),
                name,
                template: child_t,
                prim_base: 0,
                ports,
            });
            self.child.insert((s, inst.id), cs);
            self.expand_scope(cs, table, cache, stats, depth + 1)?;
        }
        Ok(())
    }

    /// The scope of an instance path (`[]` = root).
    #[must_use]
    pub fn scope_of(&self, path: &[ComponentId]) -> Option<u32> {
        let mut s = 0u32;
        for id in path {
            s = *self.child.get(&(s, *id))?;
        }
        Some(s)
    }

    /// Instance path of a scope (root first, excluding the root).
    #[must_use]
    pub fn path_of(&self, mut s: u32) -> Vec<ComponentId> {
        let mut v = Vec::new();
        while s != 0 {
            let sc = &self.scopes[s as usize];
            v.push(sc.inst);
            s = sc.parent;
        }
        v.reverse();
        v
    }

    /// Human-readable location of primitive `p`: `root › Counter#3 › NAND#2`.
    #[must_use]
    pub fn describe(&self, p: usize) -> String {
        let Some(prim) = self.prims.get(p) else { return "?".into() };
        let mut parts = Vec::new();
        let mut s = prim.scope;
        while s != 0 {
            let sc = &self.scopes[s as usize];
            parts.push(format!("{}#{}", sc.name, sc.inst.0));
            s = sc.parent;
        }
        parts.push("root".into());
        parts.reverse();
        parts.push(format!("{}#{}", prim.kind.label(), prim.id.0));
        parts.join(" › ")
    }

    /// Full path of primitive `p`: instance path + its own id.
    #[must_use]
    pub fn prim_path(&self, p: usize) -> Vec<ComponentId> {
        let Some(prim) = self.prims.get(p) else { return Vec::new() };
        let mut v = self.path_of(prim.scope);
        v.push(prim.id);
        v
    }
}
