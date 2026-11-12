//! Compilation of a [`bw_core::Circuit`] into a flat struct-of-arrays netlist
//! and the event-driven (unit-delay, delta-cycle) evaluator.

use std::collections::HashMap;

use bw_core::{Circuit, ComponentId, ComponentKind, GateOp};

use crate::SimError;

/// Sentinel for "input pin not connected" (reads as logic 0).
pub const NO_NET: u32 = u32::MAX;

/// Deterministic numbering of nets: every component with an output gets one
/// net, numbered in ascending component-id order. The renderer uses the same
/// function so it can colour wires/LEDs from a snapshot without extra
/// round-trips to the simulation thread.
#[must_use]
pub fn net_numbering(circuit: &Circuit) -> HashMap<ComponentId, u32> {
    let mut map = HashMap::with_capacity(circuit.component_count());
    let mut next: u32 = 0;
    for c in circuit.components() {
        if c.kind.output_count() > 0 {
            map.insert(c.id, next);
            next = next.saturating_add(1);
        }
    }
    map
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeOp {
    Gate(GateOp),
    Not,
    Switch,
    Clock { half_period: u32 },
}

/// Flat netlist. Node `i` drives net `i` (one output per node).
#[derive(Debug, Clone)]
pub struct Netlist {
    op: Vec<NodeOp>,
    in_start: Vec<u32>,
    in_len: Vec<u8>,
    /// Flattened input net indices of all nodes.
    inputs: Vec<u32>,
    /// CSR fan-out: nodes reading net `n` are `fanout[fan_start[n]..fan_start[n+1]]`.
    fan_start: Vec<u32>,
    fanout: Vec<u32>,
    /// Component id → node/net index.
    pub(crate) node_of: HashMap<ComponentId, u32>,
    /// Indices of clock nodes.
    clocks: Vec<u32>,
}

impl Netlist {
    /// Compiles a circuit.
    ///
    /// # Errors
    /// Returns [`SimError::TooLarge`] if the circuit cannot be indexed with `u32`.
    pub fn compile(circuit: &Circuit) -> Result<Self, SimError> {
        let node_of = net_numbering(circuit);
        let n = node_of.len();
        let mut op = Vec::with_capacity(n);
        let mut in_start = Vec::with_capacity(n);
        let mut in_len = Vec::with_capacity(n);
        let mut inputs: Vec<u32> = Vec::new();
        let mut clocks = Vec::new();

        // Input pin → driving net, built once (O(W)).
        let mut driver: HashMap<(ComponentId, u8), u32> = HashMap::with_capacity(circuit.wire_count());
        for w in circuit.wires() {
            if let Some(&net) = node_of.get(&w.from.component) {
                driver.insert((w.to.component, w.to.pin), net);
            }
        }

        for c in circuit.components() {
            let this_op = match c.kind {
                ComponentKind::Gate { op, .. } => NodeOp::Gate(op),
                ComponentKind::Not => NodeOp::Not,
                ComponentKind::Switch { .. } => NodeOp::Switch,
                ComponentKind::Clock { half_period } => NodeOp::Clock { half_period },
                ComponentKind::Led => continue,
            };
            if matches!(this_op, NodeOp::Clock { .. }) {
                clocks.push(u32::try_from(op.len()).map_err(|_| SimError::TooLarge)?);
            }
            op.push(this_op);
            in_start.push(u32::try_from(inputs.len()).map_err(|_| SimError::TooLarge)?);
            let count = c.kind.input_count();
            in_len.push(count);
            for pin in 0..count {
                inputs.push(driver.get(&(c.id, pin)).copied().unwrap_or(NO_NET));
            }
        }

        // Fan-out CSR.
        let mut counts = vec![0u32; n + 1];
        for &net in &inputs {
            if net != NO_NET {
                counts[net as usize + 1] += 1;
            }
        }
        for i in 0..n {
            counts[i + 1] += counts[i];
        }
        let fan_start = counts.clone();
        let mut cursor = counts;
        let mut fanout = vec![0u32; inputs.len()];
        for (node, (&start, &len)) in in_start.iter().zip(&in_len).enumerate() {
            let node = u32::try_from(node).map_err(|_| SimError::TooLarge)?;
            for &net in &inputs[start as usize..start as usize + len as usize] {
                if net != NO_NET {
                    let slot = &mut cursor[net as usize];
                    fanout[*slot as usize] = node;
                    *slot += 1;
                }
            }
        }
        fanout.truncate(fan_start[n] as usize);

        Ok(Self { op, in_start, in_len, inputs, fan_start, fanout, node_of, clocks })
    }

    #[must_use]
    pub fn net_count(&self) -> usize {
        self.op.len()
    }

    /// Component driving each net (index = net).
    #[must_use]
    pub fn net_components(&self) -> Vec<ComponentId> {
        let mut v = vec![ComponentId(u32::MAX); self.op.len()];
        for (&id, &i) in &self.node_of {
            if let Some(slot) = v.get_mut(i as usize) {
                *slot = id;
            }
        }
        v
    }

    fn fanout_of(&self, net: u32) -> &[u32] {
        let s = self.fan_start[net as usize] as usize;
        let e = self.fan_start[net as usize + 1] as usize;
        &self.fanout[s..e]
    }
}

/// Mutable simulation state over a [`Netlist`].
#[derive(Debug, Clone)]
pub struct SimState {
    pub(crate) net: Netlist,
    /// Current level of each net (0/1).
    pub values: Vec<u8>,
    /// Switch levels, indexed by node (only meaningful for switch nodes).
    switch: Vec<u8>,
    pub tick: u64,
    // Scratch buffers reused across settles (no per-tick allocation).
    pending: Vec<u32>,
    next: Vec<u32>,
    stamp: Vec<u32>,
    epoch: u32,
    changes: Vec<(u32, u8)>,
}

impl SimState {
    #[must_use]
    pub fn new(net: Netlist, circuit: &Circuit) -> Self {
        let n = net.net_count();
        let mut switch = vec![0u8; n];
        for c in circuit.components() {
            if let (ComponentKind::Switch { on }, Some(&i)) = (c.kind, net.node_of.get(&c.id)) {
                switch[i as usize] = u8::from(on);
            }
        }
        Self {
            net,
            values: vec![0; n],
            switch,
            tick: 0,
            pending: Vec::new(),
            next: Vec::new(),
            stamp: vec![0; n],
            epoch: 0,
            changes: Vec::new(),
        }
    }

    /// Re-initialises all nets to 0 and evaluates everything.
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] if the circuit never settles.
    pub fn reset(&mut self) -> Result<(), SimError> {
        self.tick = 0;
        self.values.iter_mut().for_each(|v| *v = 0);
        self.pending.clear();
        let n = u32::try_from(self.net.net_count()).map_err(|_| SimError::TooLarge)?;
        let ops = &self.net.op;
        // Clocks start low; everything else is evaluated once.
        self.pending.extend((0..n).filter(|&i| !matches!(ops[i as usize], NodeOp::Clock { .. })));
        self.settle()
    }

    /// Sets a switch level and propagates.
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] if the circuit never settles.
    pub fn set_switch(&mut self, id: ComponentId, on: bool) -> Result<(), SimError> {
        if let Some(&i) = self.net.node_of.get(&id) {
            if self.net.op[i as usize] == NodeOp::Switch {
                self.switch[i as usize] = u8::from(on);
                self.pending.push(i);
                return self.settle();
            }
        }
        Ok(())
    }

    /// Advances time by one tick: toggles due clocks and propagates.
    ///
    /// Returns `true` if any clock toggled (a clock edge happened).
    ///
    /// # Errors
    /// [`SimError::RaceCondition`] if the circuit never settles.
    pub fn tick(&mut self) -> Result<bool, SimError> {
        self.tick = self.tick.wrapping_add(1);
        let mut edge = false;
        for i in 0..self.net.clocks.len() {
            let node = self.net.clocks[i];
            if let NodeOp::Clock { half_period } = self.net.op[node as usize] {
                if self.tick % u64::from(half_period.max(1)) == 0 {
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

    fn eval(&self, node: u32) -> u8 {
        let i = node as usize;
        let read = |net: u32| net != NO_NET && self.values[net as usize] != 0;
        let s = self.net.in_start[i] as usize;
        let ins = &self.net.inputs[s..s + self.net.in_len[i] as usize];
        match self.net.op[i] {
            NodeOp::Gate(op) => u8::from(op.eval(ins.iter().map(|&n| read(n)))),
            NodeOp::Not => u8::from(!ins.first().is_some_and(|&n| read(n))),
            NodeOp::Switch => self.switch[i],
            // A pending clock node means "toggle now".
            NodeOp::Clock { .. } => self.values[i] ^ 1,
        }
    }

    /// Unit-delay delta-cycle propagation until quiescence.
    ///
    /// An acyclic circuit settles in at most `depth + 1` deltas and stable
    /// feedback (latches) settles quickly too; a circuit still changing after
    /// `nodes + 16` deltas can never settle → race condition.
    fn settle(&mut self) -> Result<(), SimError> {
        let max_deltas = self.net.net_count().saturating_add(16);
        let mut deltas = 0usize;
        while !self.pending.is_empty() {
            deltas += 1;
            if deltas > max_deltas {
                let net = self.pending.first().copied().unwrap_or(0);
                self.pending.clear();
                return Err(SimError::RaceCondition { net });
            }
            // Phase 1: evaluate all pending nodes against current values.
            self.changes.clear();
            for k in 0..self.pending.len() {
                let node = self.pending[k];
                let v = self.eval(node);
                if v != self.values[node as usize] {
                    self.changes.push((node, v));
                }
            }
            // Phase 2: commit and schedule fan-out (deduplicated by epoch stamp).
            self.epoch = self.epoch.wrapping_add(1);
            if self.epoch == 0 {
                self.stamp.iter_mut().for_each(|s| *s = 0);
                self.epoch = 1;
            }
            self.next.clear();
            for &(node, v) in &self.changes {
                self.values[node as usize] = v;
                for &reader in self.net.fanout_of(node) {
                    let st = &mut self.stamp[reader as usize];
                    if *st != self.epoch {
                        *st = self.epoch;
                        self.next.push(reader);
                    }
                }
            }
            std::mem::swap(&mut self.pending, &mut self.next);
        }
        Ok(())
    }
}
