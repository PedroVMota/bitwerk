//! Compilation of a [`bw_core::Circuit`] into a flat struct-of-arrays netlist
//! and the event-driven (unit-delay, delta-cycle) evaluator.
//!
//! * A **node** is a component with behaviour (everything but LEDs). Its
//!   output pins are **drivers**, numbered contiguously per node.
//! * A **net** is what an input pin reads. Normally a net has exactly one
//!   driver; tri-state outputs wired to the same input pins form one net
//!   with several drivers (a bus), resolved every time a driver changes:
//!   no active driver → Z, one → its value, more → X. Two or more drivers
//!   still driving 0/1 once the circuit has settled is **bus contention**:
//!   the simulation halts (transient overlaps while signals propagate are
//!   not contention, see [`SimState::settle`]).
//! * Net levels are four-state (see [`crate::logic`]), one byte per net
//!   while simulating; snapshots pack them at 2 bits per net (16 per `u32`,
//!   [`pack_levels`]), the form the GPU uses.

#![allow(clippy::many_single_char_names, reason = "index-heavy evaluator code")]

use std::collections::HashMap;
use std::sync::Arc;

use bw_core::{Circuit, ComponentId, ComponentKind, FfTrigger, FlipFlopType, GateOp, MemoryImage, MemoryType, TagDir};

use crate::SimError;
use crate::flatten::{CompileStats, Flat, FlattenCache, Local, NO_HANDLE};
use crate::logic::{self, FfState, L0, L1, X, Z};

/// Sentinel for "input pin not connected" (reads as Z).
pub const NO_NET: u32 = u32::MAX;
/// Flag in `drv_tag` of a driver whose net has several drivers (a bus).
const BUS: u32 = 1 << 31;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeOp {
    Gate(GateOp),
    Not,
    Switch,
    Clock {
        half_period: u32,
    },
    TriState,
    /// Index into `Netlist::ffs`.
    FlipFlop(u32),
    /// Index into `Netlist::mems`.
    Memory(u32),
}

#[derive(Debug, Clone, Copy)]
struct FfCfg {
    /// Flat primitive index (see [`Hier`]).
    prim: u32,
    ty: FlipFlopType,
    trigger: FfTrigger,
    preset_clear: bool,
}

#[derive(Debug, Clone)]
struct MemCfg {
    prim: u32,
    ram: bool,
    addr_bits: u8,
    data_bits: u8,
    /// Design contents (restored by a full reset).
    init: MemoryImage,
}

/// Which output pins drive each net, for mapping net levels onto the
/// renderer's per-pin signal slots: the drivers of net `n` are
/// `pins[start[n]..start[n + 1]]`, as (component, output index) of the
/// circuit being viewed (the root, or the body of one macro instance).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetMap {
    pub start: Vec<u32>,
    pub pins: Vec<(ComponentId, u8)>,
}

/// Everything the evaluator needs about one node, in one record (one
/// cache line fetch per evaluation instead of one per array).
#[derive(Debug, Clone, Copy)]
struct Hot {
    op: NodeOp,
    in_start: u32,
    /// First driver.
    out: u32,
    /// `drv_tag` of the first driver.
    tag: u32,
    in_len: u8,
}

/// The hierarchy behind a flat netlist: instance scopes, each primitive's
/// origin and the net of every net handle (inside views, error paths).
#[derive(Debug)]
pub struct Hier {
    pub flat: Flat,
    /// Node of each flat primitive (`NO_NET` for LEDs).
    prim_node: Vec<u32>,
    /// Net of each net handle (`NO_NET` if undriven).
    handle_net: Vec<u32>,
}

/// Flat netlist.
#[derive(Debug, Clone)]
pub struct Netlist {
    hot: Vec<Hot>,
    op: Vec<NodeOp>,
    /// Flat primitive of each node.
    node_prim: Vec<u32>,
    in_start: Vec<u32>,
    in_len: Vec<u8>,
    /// Flattened input net indices of all nodes (or [`NO_NET`]).
    inputs: Vec<u32>,
    /// Drivers of node `i` are `out_start[i]..out_start[i + 1]`.
    out_start: Vec<u32>,
    /// Net of each driver.
    drv_net: Vec<u32>,
    /// Net of each driver, with [`BUS`] set when the net has several
    /// drivers (the hot path then needs no other lookup).
    drv_tag: Vec<u32>,
    /// CSR of all drivers per net.
    net_drv_start: Vec<u32>,
    net_drvs: Vec<u32>,
    /// CSR fan-out: nodes reading net `n` are `fanout[fan_start[n]..fan_start[n+1]]`.
    fan_start: Vec<u32>,
    fanout: Vec<u32>,
    /// Root-level component id → node index.
    node_of: HashMap<ComponentId, u32>,
    /// Indices of clock nodes.
    clocks: Vec<u32>,
    /// Initial level of switch nodes (by node).
    src_init: Vec<u8>,
    ffs: Vec<FfCfg>,
    mems: Vec<MemCfg>,
    hier: Arc<Hier>,
}

fn u32_of(n: usize) -> Result<u32, SimError> {
    u32::try_from(n).map_err(|_| SimError::TooLarge)
}

/// Union-find over drivers (nets of tri-state buses).
fn find(parent: &mut [u32], mut i: u32) -> u32 {
    while parent[i as usize] != i {
        let p = parent[i as usize];
        parent[i as usize] = parent[p as usize];
        i = p;
    }
    i
}

impl Netlist {
    /// Compiles a circuit (flattening its macros) without a template cache.
    ///
    /// # Errors
    /// Returns [`SimError::TooLarge`] if the circuit cannot be indexed with `u32`.
    pub fn compile(circuit: &Circuit) -> Result<Self, SimError> {
        Self::compile_cached(circuit, &mut FlattenCache::default()).map(|(n, _)| n)
    }

    /// Compiles a circuit, reusing cached definition templates.
    ///
    /// # Errors
    /// Returns [`SimError::TooLarge`] if the circuit cannot be indexed with `u32`.
    #[allow(clippy::too_many_lines, reason = "one linear compilation pass")]
    pub fn compile_cached(circuit: &Circuit, cache: &mut FlattenCache) -> Result<(Self, CompileStats), SimError> {
        let mut stats = CompileStats::default();
        let mut flat = Flat::expand(circuit, cache, &mut stats)?;
        let t0 = std::time::Instant::now();
        let n_guess = flat.prims.len();
        let mut op = Vec::with_capacity(n_guess);
        let mut node_prim = Vec::with_capacity(n_guess);
        let mut node_ins: Vec<u8> = Vec::with_capacity(n_guess);
        let mut out_start = Vec::with_capacity(n_guess + 1);
        let mut node_of = HashMap::new();
        let mut prim_node = vec![NO_NET; n_guess];
        let mut clocks = Vec::new();
        let mut src_init = Vec::with_capacity(n_guess);
        let mut ffs = Vec::new();
        let mut mems = Vec::new();
        let mut drivers: u32 = 0;

        // Pass 1: nodes and their drivers.
        for (pi, p) in flat.prims.iter().enumerate() {
            let prim = u32_of(pi)?;
            let this_op = match p.kind {
                ComponentKind::Gate { op, .. } => NodeOp::Gate(op),
                ComponentKind::Not => NodeOp::Not,
                ComponentKind::Switch { .. } => NodeOp::Switch,
                ComponentKind::Clock { half_period } => NodeOp::Clock { half_period },
                ComponentKind::TriState => NodeOp::TriState,
                ComponentKind::FlipFlop { ty, trigger, preset_clear } => {
                    ffs.push(FfCfg { prim, ty, trigger, preset_clear });
                    NodeOp::FlipFlop(u32_of(ffs.len() - 1)?)
                }
                ComponentKind::Memory { ty, addr_bits, data_bits } => {
                    let init = p.mem.clone().unwrap_or_else(|| Arc::new(vec![0; p.kind.memory_words()]));
                    mems.push(MemCfg { prim, ram: ty == MemoryType::Ram, addr_bits, data_bits, init });
                    NodeOp::Memory(u32_of(mems.len() - 1)?)
                }
                ComponentKind::Led | ComponentKind::Tag { .. } | ComponentKind::Macro(_) => continue,
            };
            let node = u32_of(op.len())?;
            if matches!(this_op, NodeOp::Clock { .. }) {
                clocks.push(node);
            }
            src_init.push(match p.kind {
                ComponentKind::Switch { on } => u8::from(on),
                _ => L0,
            });
            op.push(this_op);
            node_prim.push(prim);
            prim_node[pi] = node;
            node_ins.push(p.kind.input_count());
            if p.scope == 0 {
                node_of.insert(p.id, node);
            }
            out_start.push(drivers);
            drivers = drivers.checked_add(u32::from(p.kind.output_count())).ok_or(SimError::TooLarge)?;
        }
        out_start.push(drivers);
        let n = op.len();
        let nd = drivers as usize;

        // Pass 2: drivers whose pins share a (flattened) net share a net;
        // each input reads its net's first driver.
        let mut parent: Vec<u32> = (0..drivers).collect();
        let mut first = vec![NO_NET; flat.handle_count()];
        for node in 0..n {
            let prim = node_prim[node] as usize;
            let ps = flat.pin_start[prim] as usize + usize::from(node_ins[node]);
            for d in out_start[node]..out_start[node + 1] {
                let h = flat.pin_handle[ps + (d - out_start[node]) as usize];
                if h == NO_HANDLE {
                    continue;
                }
                let r = flat.find(h) as usize;
                if first[r] == NO_NET {
                    first[r] = d;
                } else {
                    let (a, b) = (find(&mut parent, first[r]), find(&mut parent, d));
                    if a != b {
                        parent[a.max(b) as usize] = a.min(b);
                    }
                }
            }
        }

        // Pass 3: number the nets in driver order (deterministic).
        let mut net_of_root = vec![NO_NET; nd];
        let mut drv_net = vec![0u32; nd];
        let mut nets: u32 = 0;
        for d in 0..drivers {
            let r = find(&mut parent, d) as usize;
            if net_of_root[r] == NO_NET {
                net_of_root[r] = nets;
                nets += 1;
            }
            drv_net[d as usize] = net_of_root[r];
        }
        let nn = nets as usize;
        let mut net_drv_start = vec![0u32; nn + 1];
        for &net in &drv_net {
            net_drv_start[net as usize + 1] += 1;
        }
        for i in 0..nn {
            net_drv_start[i + 1] += net_drv_start[i];
        }
        let mut cursor = net_drv_start.clone();
        let mut net_drvs = vec![0u32; nd];
        for (d, &net) in drv_net.iter().enumerate() {
            let slot = &mut cursor[net as usize];
            net_drvs[*slot as usize] = u32_of(d)?;
            *slot += 1;
        }
        if nets >= BUS {
            return Err(SimError::TooLarge);
        }
        let drv_tag: Vec<u32> = drv_net
            .iter()
            .map(|&net| {
                let multi = net_drv_start[net as usize + 1] - net_drv_start[net as usize] > 1;
                if multi { net | BUS } else { net }
            })
            .collect();
        // Net of every handle (views): the net of its set's first driver.
        let mut handle_net = vec![NO_NET; flat.handle_count()];
        for (h, slot) in handle_net.iter_mut().enumerate() {
            let r = flat.find(u32_of(h)?) as usize;
            if first[r] != NO_NET {
                *slot = drv_net[first[r] as usize];
            }
        }

        // Pass 4: inputs of every node.
        let mut in_start = Vec::with_capacity(n);
        let mut in_len = Vec::with_capacity(n);
        let mut inputs: Vec<u32> = Vec::new();
        for node in 0..n {
            in_start.push(u32_of(inputs.len())?);
            let count = node_ins[node];
            in_len.push(count);
            let ps = flat.pin_start[node_prim[node] as usize] as usize;
            for pin in 0..usize::from(count) {
                let h = flat.pin_handle[ps + pin];
                inputs.push(if h == NO_HANDLE { NO_NET } else { handle_net[h as usize] });
            }
        }

        // Fan-out CSR.
        let mut counts = vec![0u32; nn + 1];
        for &net in &inputs {
            if net != NO_NET {
                counts[net as usize + 1] += 1;
            }
        }
        for i in 0..nn {
            counts[i + 1] += counts[i];
        }
        let fan_start = counts.clone();
        let mut cursor = counts;
        let mut fanout = vec![0u32; inputs.len()];
        for (node, (&start, &len)) in in_start.iter().zip(&in_len).enumerate() {
            let node = u32_of(node)?;
            for &net in &inputs[start as usize..start as usize + len as usize] {
                if net != NO_NET {
                    let slot = &mut cursor[net as usize];
                    fanout[*slot as usize] = node;
                    *slot += 1;
                }
            }
        }
        fanout.truncate(fan_start[nn] as usize);

        let hot = (0..n)
            .map(|i| Hot {
                op: op[i],
                in_start: in_start[i],
                out: out_start[i],
                tag: drv_tag.get(out_start[i] as usize).copied().unwrap_or(0),
                in_len: in_len[i],
            })
            .collect();
        // The pin tables are only needed while compiling.
        flat.pin_handle = Vec::new();
        flat.pin_start = Vec::new();
        stats.nets = nets;
        stats.netlist = t0.elapsed();
        Ok((
            Self {
                hot,
                op,
                node_prim,
                in_start,
                in_len,
                inputs,
                out_start,
                drv_net,
                drv_tag,
                net_drv_start,
                net_drvs,
                fan_start,
                fanout,
                node_of,
                clocks,
                src_init,
                ffs,
                mems,
                hier: Arc::new(Hier { flat, prim_node, handle_net }),
            },
            stats,
        ))
    }

    #[must_use]
    pub fn net_count(&self) -> usize {
        self.net_drv_start.len().saturating_sub(1)
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.op.len()
    }

    /// The flattened hierarchy (scopes, primitive origins).
    #[must_use]
    pub fn hierarchy(&self) -> &Arc<Hier> {
        &self.hier
    }

    /// Net driven by output `k` (0-based among the outputs) of a root-level
    /// component.
    #[must_use]
    pub fn output_net(&self, id: ComponentId, k: u8) -> Option<u32> {
        self.view_output_net(&[], id, k)
    }

    /// Net of output `k` of component `id` as seen inside the instance at
    /// `path` (`[]` = root): a primitive's output, a nested instance's
    /// output port, or the net an `In` tag brings in.
    #[must_use]
    pub fn view_output_net(&self, path: &[ComponentId], id: ComponentId, k: u8) -> Option<u32> {
        let h = &self.hier;
        let s = h.flat.scope_of(path)?;
        self.scope_output_net(s, id, k)
    }

    fn scope_output_net(&self, s: u32, id: ComponentId, k: u8) -> Option<u32> {
        let h = &self.hier;
        let sc = &h.flat.scopes[s as usize];
        let net = match *sc.template.local.get(&id)? {
            Local::Prim(j) => {
                let node = *h.prim_node.get((sc.prim_base + j) as usize)?;
                if node == NO_NET {
                    return None;
                }
                let d = self.out_start[node as usize] + u32::from(k);
                if d >= self.out_start[node as usize + 1] {
                    return None;
                }
                self.drv_net[d as usize]
            }
            Local::Inst(j) => {
                let inst = sc.template.insts.get(j as usize)?;
                let cs = *h.flat.child.get(&(s, inst.id))?;
                let port = usize::from(inst.inputs) + usize::from(k);
                let ph = *h.flat.scopes[cs as usize].ports.get(port)?;
                h.handle_net[ph as usize]
            }
            Local::Port(p) => {
                if k != 0 || sc.template.port_dir.get(usize::from(p)) != Some(&TagDir::In) {
                    return None;
                }
                let ph = *sc.ports.get(usize::from(p))?;
                h.handle_net[ph as usize]
            }
        };
        (net != NO_NET).then_some(net)
    }

    /// The output pins driving each net, as components of the circuit
    /// viewed at instance `path` (`[]` = the root circuit). `None` if the
    /// path does not exist in this compilation.
    #[must_use]
    pub fn view_map(&self, path: &[ComponentId]) -> Option<NetMap> {
        let h = &self.hier;
        let s = h.flat.scope_of(path)?;
        let t = &h.flat.scopes[s as usize].template;
        let mut pairs: Vec<(u32, ComponentId, u8)> = Vec::with_capacity(t.local.len());
        let mut push_outputs = |id: ComponentId, outs: u8| {
            for k in 0..outs {
                if let Some(net) = self.scope_output_net(s, id, k) {
                    pairs.push((net, id, k));
                }
            }
        };
        for p in &t.prims {
            push_outputs(p.id, p.kind.output_count());
        }
        for i in &t.insts {
            push_outputs(i.id, i.outputs);
        }
        for (&id, l) in &t.local {
            if matches!(l, Local::Port(_)) {
                push_outputs(id, 1);
            }
        }
        pairs.sort_unstable();
        let nn = self.net_count();
        let mut start = vec![0u32; nn + 1];
        for &(net, ..) in &pairs {
            if (net as usize) < nn {
                start[net as usize + 1] += 1;
            }
        }
        for i in 0..nn {
            start[i + 1] += start[i];
        }
        let pins = pairs.into_iter().filter(|&(net, ..)| (net as usize) < nn).map(|(_, id, k)| (id, k)).collect();
        Some(NetMap { start, pins })
    }

    /// The drivers of every net as root-level (component, output index).
    #[must_use]
    pub fn net_map(&self) -> NetMap {
        self.view_map(&[]).unwrap_or_default()
    }

    fn drivers_of(&self, net: u32) -> &[u32] {
        let s = self.net_drv_start[net as usize] as usize;
        let e = self.net_drv_start[net as usize + 1] as usize;
        &self.net_drvs[s..e]
    }

    fn node_of_driver(&self, d: u32) -> usize {
        // out_start is sorted: the node is the last start <= d.
        self.out_start.partition_point(|&s| s <= d).saturating_sub(1)
    }

    /// Instance path + id of a node's primitive.
    fn node_path(&self, node: usize) -> Vec<ComponentId> {
        self.hier.flat.prim_path(self.node_prim.get(node).copied().unwrap_or(u32::MAX) as usize)
    }

    fn node_place(&self, node: usize) -> String {
        self.hier.flat.describe(self.node_prim.get(node).copied().unwrap_or(u32::MAX) as usize)
    }
}

/// Live contents of one memory instance.
#[derive(Debug, Clone)]
struct MemState {
    words: Vec<u16>,
    /// Bumped on every change (UI refresh throttling).
    generation: u64,
}

/// Mutable simulation state over a [`Netlist`].
#[derive(Debug, Clone)]
pub struct SimState {
    pub(crate) net: Netlist,
    /// Current level of each net (four-state, see [`crate::logic`]).
    values: Vec<u8>,
    /// Current value of each driver of a bus net (unused for single-driver
    /// nets, whose value is the net's).
    drv: Vec<u8>,
    /// Switch / clock levels, indexed by node.
    src: Vec<u8>,
    ff: Vec<FfState>,
    mem: Vec<MemState>,
    pub tick: u64,
    /// While true (initial evaluation after a reset) clock transitions are
    /// not edges.
    initializing: bool,
    // Scratch buffers reused across settles (no per-tick allocation).
    pending: Vec<u32>,
    next: Vec<u32>,
    stamp: Vec<u32>,
    epoch: u32,
    changes: Vec<(u32, u8)>,
    net_stamp: Vec<u32>,
    net_epoch: u32,
    multi: Vec<u32>,
    bus_stamp: Vec<u32>,
    bus_epoch: u32,
    bus_touched: Vec<u32>,
}

/// Packs four-state levels, 16 per `u32` (net `i` at bits `2(i % 16)`).
#[must_use]
pub fn pack_levels(levels: &[u8]) -> Vec<u32> {
    levels.chunks(16).map(|c| c.iter().enumerate().fold(0u32, |w, (k, &v)| w | (u32::from(v & 3) << (2 * k)))).collect()
}

/// Level of net `i` in a packed array.
#[must_use]
pub fn packed_level(packed: &[u32], i: usize) -> u8 {
    #[allow(clippy::cast_possible_truncation, reason = "masked to 2 bits")]
    packed.get(i / 16).map_or(Z, |w| (w >> (2 * (i % 16)) & 3) as u8)
}

impl SimState {
    #[must_use]
    pub fn new(net: Netlist, _circuit: &Circuit) -> Self {
        let n = net.node_count();
        let nn = net.net_count();
        let nd = net.drv_net.len();
        let src = net.src_init.clone();
        let ff = vec![FfState::RESET; net.ffs.len()];
        let mem = net.mems.iter().map(|m| MemState { words: m.init.to_vec(), generation: 0 }).collect();
        Self {
            values: vec![Z; nn],
            drv: vec![Z; nd],
            src,
            ff,
            mem,
            tick: 0,
            initializing: false,
            pending: Vec::new(),
            next: Vec::new(),
            stamp: vec![0; n],
            epoch: 0,
            changes: Vec::new(),
            net_stamp: vec![0; nn],
            net_epoch: 0,
            multi: Vec::new(),
            bus_stamp: vec![0; nn],
            bus_epoch: 0,
            bus_touched: Vec::new(),
            net,
        }
    }

    /// Current four-state level of every net.
    #[must_use]
    pub fn values(&self) -> &[u8] {
        &self.values
    }

    /// The levels packed 2 bits per net (see [`pack_levels`]).
    #[must_use]
    pub fn packed(&self) -> Vec<u32> {
        pack_levels(&self.values)
    }

    #[must_use]
    pub fn netlist(&self) -> &Netlist {
        &self.net
    }

    /// Level of output `k` (0-based among the outputs) of a component.
    #[must_use]
    pub fn output(&self, id: ComponentId, k: u8) -> Option<u8> {
        self.net.output_net(id, k).map(|n| self.values[n as usize])
    }

    /// Carries flip-flop states and memory contents over from the state of
    /// an earlier compilation (matched by component id and compatible
    /// parameters), so that editing a running circuit does not wipe them.
    pub fn transfer_from(&mut self, old: &Self) {
        // Matched by full instance path + component id.
        let path = |net: &Netlist, prim: u32| net.hier.flat.prim_path(prim as usize);
        let old_ff: HashMap<Vec<ComponentId>, (FlipFlopType, FfState)> =
            old.net.ffs.iter().zip(&old.ff).map(|(c, s)| (path(&old.net, c.prim), (c.ty, *s))).collect();
        for (cfg, st) in self.net.ffs.iter().zip(&mut self.ff) {
            if let Some(&(ty, s)) = old_ff.get(&path(&self.net, cfg.prim)) {
                if ty == cfg.ty {
                    *st = FfState { clk: X, ..s };
                }
            }
        }
        let old_mem: HashMap<Vec<ComponentId>, (usize, u8)> =
            old.net.mems.iter().enumerate().map(|(i, m)| (path(&old.net, m.prim), (i, m.data_bits))).collect();
        for (i, cfg) in self.net.mems.iter().enumerate() {
            if let Some(&(j, bits)) = old_mem.get(&path(&self.net, cfg.prim)) {
                if bits == cfg.data_bits && old.mem[j].words.len() == self.mem[i].words.len() {
                    self.mem[i].words.clone_from(&old.mem[j].words);
                    self.mem[i].generation = old.mem[j].generation + 1;
                }
            }
        }
    }

    /// Full reset: flip-flops to Q = 0, memories to their design contents,
    /// clocks low; then evaluates everything.
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] or [`SimError::BusContention`].
    pub fn reset(&mut self) -> Result<(), SimError> {
        self.ff.iter_mut().for_each(|s| *s = FfState::RESET);
        for (m, cfg) in self.mem.iter_mut().zip(&self.net.mems) {
            m.words.clone_from(&cfg.init);
            m.generation += 1;
        }
        self.reinit()
    }

    /// Re-evaluates the whole circuit from floating nets, keeping flip-flop
    /// states and memory contents (used after a recompilation).
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] or [`SimError::BusContention`].
    pub fn reinit(&mut self) -> Result<(), SimError> {
        self.tick = 0;
        self.values.iter_mut().for_each(|v| *v = Z);
        self.drv.iter_mut().for_each(|v| *v = Z);
        for &c in &self.net.clocks {
            self.src[c as usize] = L0;
        }
        for s in &mut self.ff {
            s.clk = X;
        }
        self.pending.clear();
        let n = u32_of(self.net.node_count())?;
        self.pending.extend(0..n);
        self.initializing = true;
        let r = self.settle();
        self.initializing = false;
        r
    }

    /// Sets a switch level and propagates.
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] or [`SimError::BusContention`].
    pub fn set_switch(&mut self, id: ComponentId, on: bool) -> Result<(), SimError> {
        if let Some(&i) = self.net.node_of.get(&id) {
            if self.net.op[i as usize] == NodeOp::Switch {
                self.src[i as usize] = u8::from(on);
                self.pending.push(i);
                return self.settle();
            }
        }
        Ok(())
    }

    fn mem_index(&self, id: ComponentId) -> Option<usize> {
        match self.net.op[*self.net.node_of.get(&id)? as usize] {
            NodeOp::Memory(i) => Some(i as usize),
            _ => None,
        }
    }

    /// Live contents of a memory and their change counter.
    #[must_use]
    pub fn memory(&self, id: ComponentId) -> Option<(&[u16], u64)> {
        self.mem_index(id).map(|i| (self.mem[i].words.as_slice(), self.mem[i].generation))
    }

    /// Writes one word of a memory, both live and in the design contents
    /// (restored by a reset), and propagates.
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] or [`SimError::BusContention`].
    pub fn write_memory_word(&mut self, id: ComponentId, addr: usize, value: u16) -> Result<(), SimError> {
        let Some(i) = self.mem_index(id) else { return Ok(()) };
        let mask = bw_core::memenc::width_mask(self.net.mems[i].data_bits);
        if let Some(w) = self.mem[i].words.get_mut(addr) {
            *w = value & mask;
            self.mem[i].generation += 1;
            if let Some(v) = std::sync::Arc::make_mut(&mut self.net.mems[i].init).get_mut(addr) {
                *v = value & mask;
            }
        }
        self.reevaluate(id)
    }

    /// Replaces a memory's live and design contents and propagates.
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] or [`SimError::BusContention`].
    pub fn load_memory(&mut self, id: ComponentId, image: &MemoryImage) -> Result<(), SimError> {
        let Some(i) = self.mem_index(id) else { return Ok(()) };
        if image.len() == self.mem[i].words.len() {
            self.mem[i].words.clone_from(image);
            self.mem[i].generation += 1;
            self.net.mems[i].init = Arc::clone(image);
        }
        self.reevaluate(id)
    }

    fn reevaluate(&mut self, id: ComponentId) -> Result<(), SimError> {
        if let Some(&node) = self.net.node_of.get(&id) {
            self.pending.push(node);
        }
        self.settle()
    }

    /// Advances time by one tick: toggles due clocks and propagates.
    ///
    /// Returns `true` if any clock toggled (a clock edge happened).
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] or [`SimError::BusContention`].
    pub fn tick(&mut self) -> Result<bool, SimError> {
        self.tick = self.tick.wrapping_add(1);
        let mut edge = false;
        for i in 0..self.net.clocks.len() {
            let node = self.net.clocks[i];
            if let NodeOp::Clock { half_period } = self.net.op[node as usize] {
                if self.tick % u64::from(half_period.max(1)) == 0 {
                    self.src[node as usize] ^= 1;
                    self.pending.push(node);
                    edge = true;
                }
            }
        }
        self.settle()?;
        Ok(edge)
    }

    #[must_use]
    pub fn has_clocks(&self) -> bool {
        !self.net.clocks.is_empty()
    }

    /// Evaluates a single-output, stateless node (the hot path).
    #[allow(clippy::inline_always, reason = "measured: ~10% of the tick cost in the 100k stress circuit")]
    #[inline(always)]
    fn eval_comb(&self, i: usize) -> u8 {
        let h = &self.net.hot[i];
        let s = h.in_start as usize;
        let ins = &self.net.inputs[s..s + usize::from(h.in_len)];
        let values = &self.values;
        let rd = |net: u32| if net == NO_NET { X } else { logic::input(values[net as usize]) };
        match h.op {
            NodeOp::Gate(op) => logic::gate(op, ins.iter().map(|&n| rd(n))),
            NodeOp::Not => logic::not(ins.first().map_or(X, |&n| rd(n))),
            NodeOp::TriState => logic::tristate(rd(ins[0]), rd(ins[1])),
            _ => self.src[i],
        }
    }

    /// Evaluates a flip-flop or memory into `out`; returns the number of
    /// outputs.
    fn eval_seq(&mut self, node: u32, out: &mut [u8; 16]) -> usize {
        let i = node as usize;
        let s = self.net.in_start[i] as usize;
        let ins = &self.net.inputs[s..s + self.net.in_len[i] as usize];
        let values = &self.values;
        let rd = |net: u32| if net == NO_NET { X } else { logic::input(values[net as usize]) };
        // Optional control pins: an unconnected pin takes its inactive default.
        let rd_or = |net: u32, default: u8| if net == NO_NET { default } else { logic::input(values[net as usize]) };
        match self.net.op[i] {
            NodeOp::Gate(_) | NodeOp::Not | NodeOp::Switch | NodeOp::Clock { .. } | NodeOp::TriState => {
                out[0] = self.eval_comb(i);
                1
            }
            NodeOp::FlipFlop(f) => {
                let cfg = self.net.ffs[f as usize];
                let sync = usize::from(cfg.ty.sync_inputs());
                let a = rd(ins[0]);
                let clk = rd(ins[usize::from(FlipFlopType::CLOCK_INPUT)]);
                let b = if sync == 3 { rd(ins[2]) } else { L0 };
                let async_pins = cfg.preset_clear.then(|| (rd_or(ins[sync], L0), rd_or(ins[sync + 1], L0)));
                let edges = !self.initializing;
                let q = logic::flip_flop(&mut self.ff[f as usize], cfg.ty, cfg.trigger, a, b, clk, async_pins, edges);
                out[0] = q;
                out[1] = logic::not(q);
                2
            }
            NodeOp::Memory(m) => {
                let cfg = &self.net.mems[m as usize];
                let (aw, dw) = (usize::from(cfg.addr_bits), usize::from(cfg.data_bits));
                let cs = rd_or(ins[ins.len() - 1], L0);
                let rw = if cfg.ram { rd_or(ins[aw + dw], L1) } else { L1 };
                let mut addr = Some(0usize);
                for (bit, &n) in ins[..aw].iter().enumerate() {
                    match rd(n) {
                        L1 => addr = addr.map(|a| a | 1 << bit),
                        L0 => {}
                        _ => addr = None,
                    }
                }
                let fill = |out: &mut [u8; 16], v: u8| out[..dw].iter_mut().for_each(|o| *o = v);
                match (cs, rw) {
                    (L1, _) => fill(out, Z),
                    (L0, L1) => match addr {
                        Some(a) => {
                            let w = self.mem[m as usize].words.get(a).copied().unwrap_or(0);
                            for (k, o) in out[..dw].iter_mut().enumerate() {
                                *o = u8::from(w >> k & 1 == 1);
                            }
                        }
                        None => fill(out, X),
                    },
                    (L0, L0) => {
                        // Writing (RAM): outputs float, the word follows the
                        // data inputs (X / Z data bits are stored as 0).
                        fill(out, Z);
                        if let Some(a) = addr {
                            let mut w = 0u16;
                            for (k, &n) in ins[aw..aw + dw].iter().enumerate() {
                                if rd(n) == L1 {
                                    w |= 1 << k;
                                }
                            }
                            let st = &mut self.mem[m as usize];
                            if let Some(slot) = st.words.get_mut(a) {
                                if *slot != w {
                                    *slot = w;
                                    st.generation += 1;
                                }
                            }
                        }
                    }
                    _ => fill(out, X),
                }
                dw
            }
        }
    }

    /// Records a driver value for commit if it differs from the current one.
    #[inline]
    fn note_change(&mut self, d: u32, v: u8) {
        let tag = self.net.drv_tag[d as usize];
        let old = if tag & BUS == 0 { self.values[tag as usize] } else { self.drv[d as usize] };
        if old != v {
            self.changes.push((d, v));
        }
    }

    fn set_net(&mut self, net: u32, v: u8) {
        let n = net as usize;
        if self.values[n] == v {
            return;
        }
        self.values[n] = v;
        let s = self.net.fan_start[n] as usize;
        let e = self.net.fan_start[n + 1] as usize;
        for k in s..e {
            let reader = self.net.fanout[k];
            let st = &mut self.stamp[reader as usize];
            if *st != self.epoch {
                *st = self.epoch;
                self.next.push(reader);
            }
        }
    }

    /// Resolves a multi-driver net: no active driver → Z, one → its value,
    /// more → X.
    fn resolve(&self, net: u32) -> u8 {
        let mut v = Z;
        for &d in self.net.drivers_of(net) {
            let x = self.drv[d as usize];
            if x != Z {
                if v != Z {
                    return X;
                }
                v = x;
            }
        }
        v
    }

    /// Unit-delay delta-cycle propagation until quiescence.
    ///
    /// An acyclic circuit settles in at most `depth + 1` deltas and stable
    /// feedback (latches) settles quickly too; a circuit still changing after
    /// `nodes + 16` deltas can never settle → race condition. Buses are
    /// checked for contention once everything has settled, so the brief
    /// overlap of two enables switching through gates of different depth is
    /// not reported.
    fn settle(&mut self) -> Result<(), SimError> {
        let max_deltas = self.net.node_count().saturating_add(16);
        let mut deltas = 0usize;
        let mut out = [Z; 16];
        self.bus_epoch = self.bus_epoch.wrapping_add(1);
        if self.bus_epoch == 0 {
            self.bus_stamp.iter_mut().for_each(|s| *s = 0);
            self.bus_epoch = 1;
        }
        self.bus_touched.clear();
        while !self.pending.is_empty() {
            deltas += 1;
            if deltas > max_deltas {
                let node = self.pending.first().copied().unwrap_or(0) as usize;
                let net = self.net.drv_net.get(self.net.out_start[node] as usize).copied().unwrap_or(0);
                self.pending.clear();
                return Err(SimError::RaceCondition {
                    net,
                    path: self.net.node_path(node),
                    place: self.net.node_place(node),
                });
            }
            // Phase 1: evaluate all pending nodes against current values.
            self.changes.clear();
            for k in 0..self.pending.len() {
                let node = self.pending[k];
                let h = self.net.hot[node as usize];
                if matches!(h.op, NodeOp::FlipFlop(_) | NodeOp::Memory(_)) {
                    let n = self.eval_seq(node, &mut out);
                    for (j, &v) in out[..n].iter().enumerate() {
                        #[allow(clippy::cast_possible_truncation, reason = "at most 16 outputs")]
                        self.note_change(h.out + j as u32, v);
                    }
                } else {
                    // Single output: its tag is in the node record.
                    let v = self.eval_comb(node as usize);
                    let old = if h.tag & BUS == 0 { self.values[h.tag as usize] } else { self.drv[h.out as usize] };
                    if old != v {
                        self.changes.push((h.out, v));
                    }
                }
            }
            // Phase 2: commit and schedule fan-out (deduplicated by epoch stamp).
            self.epoch = self.epoch.wrapping_add(1);
            if self.epoch == 0 {
                self.stamp.iter_mut().for_each(|s| *s = 0);
                self.epoch = 1;
            }
            self.next.clear();
            self.multi.clear();
            for k in 0..self.changes.len() {
                let (d, v) = self.changes[k];
                let tag = self.net.drv_tag[d as usize];
                if tag & BUS == 0 {
                    self.set_net(tag, v);
                } else {
                    self.drv[d as usize] = v;
                    let net = tag & !BUS;
                    if self.multi.is_empty() {
                        // First bus change of this delta: new dedup epoch
                        // (bus-free deltas pay nothing for buses).
                        self.net_epoch = self.net_epoch.wrapping_add(1);
                        if self.net_epoch == 0 {
                            self.net_stamp.iter_mut().for_each(|s| *s = 0);
                            self.net_epoch = 1;
                        }
                    }
                    if self.net_stamp[net as usize] != self.net_epoch {
                        self.net_stamp[net as usize] = self.net_epoch;
                        self.multi.push(net);
                    }
                }
            }
            for k in 0..self.multi.len() {
                let net = self.multi[k];
                let v = self.resolve(net);
                self.set_net(net, v);
                if self.bus_stamp[net as usize] != self.bus_epoch {
                    self.bus_stamp[net as usize] = self.bus_epoch;
                    self.bus_touched.push(net);
                }
            }
            std::mem::swap(&mut self.pending, &mut self.next);
        }
        self.check_contention()
    }

    /// Bus contention: two or more drivers of a settled net drive 0/1.
    fn check_contention(&self) -> Result<(), SimError> {
        for &net in &self.bus_touched {
            let active: Vec<u32> =
                self.net.drivers_of(net).iter().copied().filter(|&d| self.drv[d as usize] <= L1).collect();
            if active.len() > 1 {
                let mut nodes: Vec<usize> = active.iter().map(|&d| self.net.node_of_driver(d)).collect();
                nodes.dedup();
                let drivers = nodes.iter().map(|&n| self.net.node_path(n)).collect();
                let place = nodes.iter().map(|&n| self.net.node_place(n)).collect::<Vec<_>>().join(" and ");
                return Err(SimError::BusContention { net, drivers, place });
            }
        }
        Ok(())
    }
}
