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

use bw_core::{Circuit, ComponentId, ComponentKind, FfTrigger, FlipFlopType, GateOp, MemoryImage, MemoryType};

use crate::SimError;
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
    component: ComponentId,
    ty: FlipFlopType,
    trigger: FfTrigger,
    preset_clear: bool,
}

#[derive(Debug, Clone)]
struct MemCfg {
    component: ComponentId,
    ram: bool,
    addr_bits: u8,
    data_bits: u8,
    /// Design contents (restored by a full reset).
    init: MemoryImage,
}

/// Which output pins drive each net, for mapping net levels onto the
/// renderer's per-pin signal slots: the drivers of net `n` are
/// `pins[start[n]..start[n + 1]]`, as (component, output index).
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

/// Flat netlist.
#[derive(Debug, Clone)]
pub struct Netlist {
    hot: Vec<Hot>,
    op: Vec<NodeOp>,
    node_comp: Vec<ComponentId>,
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
    /// Component id → node index.
    node_of: HashMap<ComponentId, u32>,
    /// Indices of clock nodes.
    clocks: Vec<u32>,
    ffs: Vec<FfCfg>,
    mems: Vec<MemCfg>,
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
    /// Compiles a circuit.
    ///
    /// # Errors
    /// Returns [`SimError::TooLarge`] if the circuit cannot be indexed with `u32`.
    #[allow(clippy::too_many_lines, reason = "one linear compilation pass")]
    pub fn compile(circuit: &Circuit) -> Result<Self, SimError> {
        let n_guess = circuit.component_count();
        let mut op = Vec::with_capacity(n_guess);
        let mut node_comp = Vec::with_capacity(n_guess);
        let mut node_ins: Vec<u8> = Vec::with_capacity(n_guess);
        let mut out_start = Vec::with_capacity(n_guess + 1);
        let mut node_of = HashMap::with_capacity(n_guess);
        let mut clocks = Vec::new();
        let mut ffs = Vec::new();
        let mut mems = Vec::new();
        let mut drivers: u32 = 0;

        // Pass 1: nodes and their drivers.
        for c in circuit.components() {
            let this_op = match c.kind {
                ComponentKind::Gate { op, .. } => NodeOp::Gate(op),
                ComponentKind::Not => NodeOp::Not,
                ComponentKind::Switch { .. } => NodeOp::Switch,
                ComponentKind::Clock { half_period } => NodeOp::Clock { half_period },
                ComponentKind::TriState => NodeOp::TriState,
                ComponentKind::FlipFlop { ty, trigger, preset_clear } => {
                    ffs.push(FfCfg { component: c.id, ty, trigger, preset_clear });
                    NodeOp::FlipFlop(u32_of(ffs.len() - 1)?)
                }
                ComponentKind::Memory { ty, addr_bits, data_bits } => {
                    let init = circuit.memory(c.id).unwrap_or_default();
                    mems.push(MemCfg { component: c.id, ram: ty == MemoryType::Ram, addr_bits, data_bits, init });
                    NodeOp::Memory(u32_of(mems.len() - 1)?)
                }
                ComponentKind::Led => continue,
            };
            let node = u32_of(op.len())?;
            if matches!(this_op, NodeOp::Clock { .. }) {
                clocks.push(node);
            }
            op.push(this_op);
            node_comp.push(c.id);
            node_ins.push(c.kind.input_count());
            node_of.insert(c.id, node);
            out_start.push(drivers);
            drivers = drivers.checked_add(u32::from(c.kind.output_count())).ok_or(SimError::TooLarge)?;
        }
        out_start.push(drivers);
        let n = op.len();
        let nd = drivers as usize;

        // Pass 2: drivers on the same wire net (connected component of the
        // wire graph) share a net; each input reads its net's first driver.
        let driver_of = |pin: bw_core::PinRef| -> Option<u32> {
            let node = *node_of.get(&pin.component)? as usize;
            let k = pin.pin.checked_sub(node_ins[node])?;
            let d = out_start[node] + u32::from(k);
            (d < out_start[node + 1]).then_some(d)
        };
        let mut parent: Vec<u32> = (0..drivers).collect();
        let wire_nets = circuit.nets();
        let mut first: HashMap<(ComponentId, u8), u32> = HashMap::with_capacity(wire_nets.pins.len());
        for i in 0..wire_nets.len() {
            let pins = wire_nets.net(i);
            let mut root: Option<u32> = None;
            for &p in pins {
                let Some(d) = driver_of(p) else { continue };
                match root {
                    None => root = Some(d),
                    Some(f) => {
                        let (a, b) = (find(&mut parent, f), find(&mut parent, d));
                        if a != b {
                            parent[a.max(b) as usize] = a.min(b);
                        }
                    }
                }
            }
            let Some(r) = root else { continue };
            for &p in pins {
                if driver_of(p).is_none() {
                    first.insert((p.component, p.pin), r);
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

        // Pass 4: inputs of every node.
        let mut in_start = Vec::with_capacity(n);
        let mut in_len = Vec::with_capacity(n);
        let mut inputs: Vec<u32> = Vec::new();
        for c in circuit.components() {
            if !node_of.contains_key(&c.id) {
                continue;
            }
            in_start.push(u32_of(inputs.len())?);
            let count = c.kind.input_count();
            in_len.push(count);
            for pin in 0..count {
                let net = first.get(&(c.id, pin)).map_or(NO_NET, |&d| drv_net[d as usize]);
                inputs.push(net);
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
        Ok(Self {
            hot,
            op,
            node_comp,
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
            ffs,
            mems,
        })
    }

    #[must_use]
    pub fn net_count(&self) -> usize {
        self.net_drv_start.len().saturating_sub(1)
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.op.len()
    }

    /// Net driven by output `k` (0-based among the outputs) of a component.
    #[must_use]
    pub fn output_net(&self, id: ComponentId, k: u8) -> Option<u32> {
        let node = *self.node_of.get(&id)? as usize;
        let d = self.out_start[node] + u32::from(k);
        (d < self.out_start[node + 1]).then(|| self.drv_net[d as usize])
    }

    /// The drivers of every net as (component, output index).
    #[must_use]
    pub fn net_map(&self) -> NetMap {
        let mut drv_pin = vec![(ComponentId(u32::MAX), 0u8); self.drv_net.len()];
        for (node, &comp) in self.node_comp.iter().enumerate() {
            let (s, e) = (self.out_start[node], self.out_start[node + 1]);
            for d in s..e {
                drv_pin[d as usize] = (comp, u8::try_from(d - s).unwrap_or(u8::MAX));
            }
        }
        NetMap { start: self.net_drv_start.clone(), pins: self.net_drvs.iter().map(|&d| drv_pin[d as usize]).collect() }
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
    pub fn new(net: Netlist, circuit: &Circuit) -> Self {
        let n = net.node_count();
        let nn = net.net_count();
        let nd = net.drv_net.len();
        let mut src = vec![L0; n];
        for c in circuit.components() {
            if let (ComponentKind::Switch { on }, Some(&i)) = (c.kind, net.node_of.get(&c.id)) {
                src[i as usize] = u8::from(on);
            }
        }
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
        let old_ff: HashMap<ComponentId, (FlipFlopType, FfState)> =
            old.net.ffs.iter().zip(&old.ff).map(|(c, s)| (c.component, (c.ty, *s))).collect();
        for (cfg, st) in self.net.ffs.iter().zip(&mut self.ff) {
            if let Some(&(ty, s)) = old_ff.get(&cfg.component) {
                if ty == cfg.ty {
                    *st = FfState { clk: X, ..s };
                }
            }
        }
        let old_mem: HashMap<ComponentId, (usize, u8)> =
            old.net.mems.iter().enumerate().map(|(i, m)| (m.component, (i, m.data_bits))).collect();
        for (i, cfg) in self.net.mems.iter().enumerate() {
            if let Some(&(j, bits)) = old_mem.get(&cfg.component) {
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
                return Err(SimError::RaceCondition { net });
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
                let mut drivers: Vec<ComponentId> =
                    active.iter().map(|&d| self.net.node_comp[self.net.node_of_driver(d)]).collect();
                drivers.dedup();
                return Err(SimError::BusContention { net, drivers });
            }
        }
        Ok(())
    }
}
