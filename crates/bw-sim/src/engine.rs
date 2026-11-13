//! Simulation thread, command channel and lock-free snapshot publication.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use bw_core::{Circuit, ComponentId, ComponentKind, Edit};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::SimError;
use crate::netlist::{NetMap, Netlist, SimState};

/// Minimum interval between two published snapshots (caps UI upload work).
const PUBLISH_INTERVAL: Duration = Duration::from_millis(8);
/// Minimum interval between two published memory views (the contents of
/// the one memory whose window is open; never part of the snapshots).
const MEMORY_PUBLISH_INTERVAL: Duration = Duration::from_millis(100);
/// Maximum ticks simulated per loop iteration before checking commands again,
/// so a high tick rate can never make the thread unresponsive.
const MAX_TICKS_PER_BATCH: u32 = 10_000;
/// Hard ceiling for the tick rate.
pub const MAX_TICK_RATE: f64 = 1_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Stopped,
    Running,
    Paused,
    /// Halted by an error (see [`Snapshot::error`]).
    Halted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepMode {
    /// Advance to the next clock edge.
    #[default]
    HalfCycle,
    /// Advance two clock edges.
    FullCycle,
}

/// Commands sent from the UI to the simulation thread.
#[derive(Debug, Clone)]
pub enum Command {
    /// Replace the whole circuit (file load, new, stress test). `revision`
    /// is echoed in snapshots. The simulator keeps its own copy.
    Load {
        circuit: Arc<Circuit>,
        revision: u64,
    },
    /// One incremental edit, applied to the simulator's copy of the circuit
    /// (the UI applies the same edit to its copy). Consecutive edits are
    /// batched into one recompilation.
    Edit {
        edit: Edit,
        revision: u64,
    },
    SetSwitch(ComponentId, bool),
    /// Publish the live contents of this memory (throttled, only when they
    /// change) through [`SimHandle::memory_view`]; `None` stops it.
    WatchMemory(Option<ComponentId>),
    Run,
    Stop,
    /// Toggles pause/resume.
    Pause,
    Step(StepMode),
    /// Simulation ticks per second while running.
    SetTickRate(f64),
    Shutdown,
}

/// Immutable view of the simulation, published after changes.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Monotonic counter, increments on every publication.
    pub generation: u64,
    /// Circuit revision these values belong to.
    pub revision: u64,
    pub state: RunState,
    pub tick: u64,
    /// Four-state level of every net, packed 2 bits per net (16 per word,
    /// see [`crate::pack_levels`]).
    pub values: Vec<u32>,
    pub net_count: usize,
    /// Output pins driving each net. Shared by all snapshots of one
    /// compilation, so cloning it is free.
    pub net_map: Arc<NetMap>,
    /// Changes whenever the net numbering changes (each compilation).
    pub compile_id: u64,
    pub error: Option<SimError>,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            generation: 0,
            revision: 0,
            state: RunState::Stopped,
            tick: 0,
            values: Vec::new(),
            net_count: 0,
            net_map: Arc::new(NetMap::default()),
            compile_id: 0,
            error: None,
        }
    }

    /// Level of net `i` (Z if out of range).
    #[must_use]
    pub fn level(&self, i: usize) -> u8 {
        crate::packed_level(&self.values, i)
    }
}

/// Live contents of the watched memory (see [`Command::WatchMemory`]).
#[derive(Debug, Clone)]
pub struct MemoryView {
    pub id: ComponentId,
    pub words: Arc<[u16]>,
    /// Increments with every published change.
    pub generation: u64,
}

/// Owner handle of the simulation thread. Dropping it shuts the thread down.
pub struct SimHandle {
    tx: Sender<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    memory: Arc<ArcSwapOption<MemoryView>>,
    thread: Option<JoinHandle<()>>,
}

impl SimHandle {
    /// Spawns the simulation thread. `on_publish` is called (from the sim
    /// thread) after each new snapshot, e.g. to request a UI repaint.
    ///
    /// # Errors
    /// Fails if the OS refuses to spawn a thread.
    pub fn spawn(on_publish: Box<dyn Fn() + Send>) -> std::io::Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let snapshot = Arc::new(ArcSwap::from_pointee(Snapshot::empty()));
        let memory = Arc::new(ArcSwapOption::empty());
        let (snap2, mem2) = (Arc::clone(&snapshot), Arc::clone(&memory));
        let thread = std::thread::Builder::new()
            .name("bw-sim".into())
            .spawn(move || Worker::new(rx, snap2, mem2, on_publish).run())?;
        Ok(Self { tx, snapshot, memory, thread: Some(thread) })
    }

    /// Non-blocking send. Errors (thread gone) are logged and ignored.
    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_err() {
            tracing::error!("simulation thread is gone");
        }
    }

    /// Latest snapshot. Wait-free.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// Latest contents of the watched memory, if any. Wait-free.
    #[must_use]
    pub fn memory_view(&self) -> Option<Arc<MemoryView>> {
        self.memory.load_full()
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    rx: Receiver<Command>,
    out: Arc<ArcSwap<Snapshot>>,
    mem_out: Arc<ArcSwapOption<MemoryView>>,
    on_publish: Box<dyn Fn() + Send>,
    sim: Option<SimState>,
    /// The simulator's own copy of the circuit (kept in sync through
    /// `Load` and `Edit` commands).
    circuit: Circuit,
    /// The circuit changed since the last compilation.
    needs_compile: bool,
    /// The next compilation starts from scratch (a new circuit was loaded)
    /// instead of carrying flip-flop states and memory contents over.
    fresh: bool,
    net_map: Arc<NetMap>,
    compile_id: u64,
    /// Memory whose contents are published, and the (compilation, change
    /// counter) last published.
    watched: Option<ComponentId>,
    watched_published: Option<(u64, u64)>,
    mem_generation: u64,
    last_mem_publish: Instant,
    revision: u64,
    state: RunState,
    error: Option<SimError>,
    tick_period: Duration,
    next_tick: Instant,
    generation: u64,
    dirty: bool,
    last_publish: Instant,
}

impl Worker {
    fn new(
        rx: Receiver<Command>,
        out: Arc<ArcSwap<Snapshot>>,
        mem_out: Arc<ArcSwapOption<MemoryView>>,
        on_publish: Box<dyn Fn() + Send>,
    ) -> Self {
        let now = Instant::now();
        Self {
            rx,
            out,
            mem_out,
            on_publish,
            sim: None,
            circuit: Circuit::new(),
            needs_compile: false,
            fresh: true,
            net_map: Arc::new(NetMap::default()),
            compile_id: 0,
            watched: None,
            watched_published: None,
            mem_generation: 0,
            last_mem_publish: now,
            revision: 0,
            state: RunState::Stopped,
            error: None,
            tick_period: Duration::from_millis(50),
            next_tick: now,
            generation: 0,
            dirty: true,
            last_publish: now,
        }
    }

    fn run(mut self) {
        loop {
            // Wait for a command, a due tick or a due publication.
            let mut deadline: Option<Instant> = None;
            if self.state == RunState::Running {
                deadline = Some(self.next_tick);
            }
            if self.dirty {
                let p = self.last_publish + PUBLISH_INTERVAL;
                deadline = Some(deadline.map_or(p, |d| d.min(p)));
            }
            if self.memory_view_stale() {
                let p = self.last_mem_publish + MEMORY_PUBLISH_INTERVAL;
                deadline = Some(deadline.map_or(p, |d| d.min(p)));
            }
            let cmd = match deadline {
                None => self.rx.recv().ok(),
                Some(d) => match self.rx.recv_deadline(d) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            if let Some(cmd) = cmd {
                // Drain everything already queued before doing time-based work.
                let mut batch = vec![cmd];
                batch.extend(self.rx.try_iter());
                // Coalesce: a `Load` supersedes every earlier `Load`/`Edit`,
                // and all queued edits share one recompilation.
                let last_load = batch.iter().rposition(|c| matches!(c, Command::Load { .. }));
                for (i, cmd) in batch.into_iter().enumerate() {
                    let superseded = last_load.is_some_and(|l| i < l);
                    if superseded && matches!(cmd, Command::Load { .. } | Command::Edit { .. }) {
                        continue;
                    }
                    if !self.handle(cmd) {
                        return;
                    }
                }
                self.compile_if_needed();
            } else if deadline.is_none() {
                return; // channel disconnected
            }
            if self.state == RunState::Running {
                self.run_due_ticks();
            }
            if self.dirty && self.last_publish.elapsed() >= PUBLISH_INTERVAL {
                self.publish();
            }
            if self.memory_view_stale() && self.last_mem_publish.elapsed() >= MEMORY_PUBLISH_INTERVAL {
                self.publish_memory();
            }
        }
    }

    /// The watched memory changed since it was last published.
    fn memory_view_stale(&self) -> bool {
        let Some(id) = self.watched else { return false };
        let current = self.sim.as_ref().and_then(|s| s.memory(id)).map(|(_, g)| (self.compile_id, g));
        current.is_some() && current != self.watched_published
    }

    fn publish_memory(&mut self) {
        let Some(id) = self.watched else { return };
        let Some((words, g)) = self.sim.as_ref().and_then(|s| s.memory(id)) else { return };
        self.watched_published = Some((self.compile_id, g));
        self.mem_generation += 1;
        self.mem_out.store(Some(Arc::new(MemoryView { id, words: Arc::from(words), generation: self.mem_generation })));
        self.last_mem_publish = Instant::now();
        (self.on_publish)();
    }

    /// Compiles the simulator's circuit copy if edits are pending.
    fn compile_if_needed(&mut self) {
        if !self.needs_compile {
            return;
        }
        self.needs_compile = false;
        let tick = self.sim.as_ref().map_or(0, |s| s.tick);
        match Netlist::compile(&self.circuit) {
            Ok(net) => {
                self.net_map = Arc::new(net.net_map());
                self.compile_id += 1;
                let mut sim = SimState::new(net, &self.circuit);
                // An edit keeps the flip-flop states and memory contents of
                // the running circuit; a newly loaded circuit starts afresh.
                let r = match (self.sim.take(), std::mem::take(&mut self.fresh)) {
                    (Some(old), false) => {
                        sim.transfer_from(&old);
                        sim.reinit()
                    }
                    _ => sim.reset(),
                };
                sim.tick = tick;
                self.sim = Some(sim);
                self.apply(r);
            }
            Err(e) => self.halt(e),
        }
        self.dirty = true;
    }

    /// Returns `false` on shutdown.
    fn handle(&mut self, cmd: Command) -> bool {
        // Everything except model changes acts on the compiled state.
        if !matches!(cmd, Command::Load { .. } | Command::Edit { .. }) {
            self.compile_if_needed();
        }
        match cmd {
            Command::Shutdown => return false,
            Command::Load { circuit, revision } => {
                // Copy on this thread and drop the Arc right away, so the UI
                // can keep editing its copy without cloning.
                self.revision = revision;
                self.circuit = (*circuit).clone();
                drop(circuit);
                self.needs_compile = true;
                self.fresh = true;
            }
            Command::Edit { edit, revision } => {
                self.revision = revision;
                if let Err(e) = self.circuit.apply(&edit) {
                    // Cannot happen while UI and simulator replay the same
                    // edits; logged so a divergence would be noticed.
                    tracing::error!("simulator could not apply {edit:?}: {e}");
                } else if edit.affects_netlist() {
                    self.needs_compile = true;
                } else {
                    self.apply_live_edit(&edit);
                }
            }
            Command::WatchMemory(id) => {
                self.watched = id;
                self.watched_published = None;
                if id.is_none() {
                    self.mem_out.store(None);
                }
            }
            Command::SetSwitch(id, on) => {
                // Keep the copy in sync so a later recompilation keeps the level.
                if matches!(self.circuit.component(id).map(|c| c.kind), Some(ComponentKind::Switch { .. })) {
                    let _ = self.circuit.set_kind(id, ComponentKind::Switch { on });
                }
                if let Some(sim) = self.sim.as_mut() {
                    let r = sim.set_switch(id, on);
                    self.apply(r);
                }
            }
            Command::Run => {
                if self.state == RunState::Halted {
                    self.reset();
                }
                if self.state != RunState::Halted {
                    self.state = RunState::Running;
                    self.next_tick = Instant::now() + self.tick_period;
                }
            }
            Command::Stop => {
                self.reset();
                if self.state != RunState::Halted {
                    self.state = RunState::Stopped;
                }
            }
            Command::Pause => match self.state {
                RunState::Running => self.state = RunState::Paused,
                RunState::Paused => {
                    self.state = RunState::Running;
                    self.next_tick = Instant::now() + self.tick_period;
                }
                RunState::Stopped | RunState::Halted => {}
            },
            Command::Step(mode) => {
                if matches!(self.state, RunState::Stopped | RunState::Paused) {
                    self.step(mode);
                }
            }
            Command::SetTickRate(rate) => {
                let rate = if rate.is_finite() { rate.clamp(0.1, MAX_TICK_RATE) } else { 20.0 };
                self.tick_period = Duration::from_secs_f64(1.0 / rate);
            }
        }
        self.dirty = true;
        true
    }

    /// Applies a memory edit to the running state (no recompilation).
    fn apply_live_edit(&mut self, edit: &Edit) {
        if self.needs_compile {
            return; // the next compilation picks the contents up from the circuit
        }
        let Some(sim) = self.sim.as_mut() else { return };
        let r = match edit {
            Edit::SetMemoryWord { id, addr, value } => {
                sim.write_memory_word(*id, usize::try_from(*addr).unwrap_or(usize::MAX), *value)
            }
            Edit::SetMemory { id, .. } => match self.circuit.memory(*id) {
                Some(image) => sim.load_memory(*id, &image),
                None => Ok(()),
            },
            _ => Ok(()),
        };
        self.apply(r);
    }

    fn reset(&mut self) {
        self.error = None;
        if self.state == RunState::Halted {
            self.state = RunState::Stopped;
        }
        if let Some(sim) = self.sim.as_mut() {
            let r = sim.reset();
            self.apply(r);
        }
    }

    fn step(&mut self, mode: StepMode) {
        let edges = match mode {
            StepMode::HalfCycle => 1,
            StepMode::FullCycle => 2,
        };
        let Some(sim) = self.sim.as_mut() else { return };
        if !sim.has_clocks() {
            let r = sim.tick().map(|_| ());
            self.apply(r);
            return;
        }
        let mut seen = 0;
        // Bounded: a clock with the max half-period edges within that many ticks.
        for _ in 0..=(2 * bw_core::MAX_CLOCK_HALF_PERIOD) {
            match sim.tick() {
                Ok(true) => {
                    seen += 1;
                    if seen >= edges {
                        break;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    self.halt(e);
                    return;
                }
            }
        }
    }

    fn run_due_ticks(&mut self) {
        let now = Instant::now();
        let mut n = 0u32;
        while self.next_tick <= now && n < MAX_TICKS_PER_BATCH {
            let Some(sim) = self.sim.as_mut() else { break };
            if let Err(e) = sim.tick() {
                self.halt(e);
                return;
            }
            self.next_tick += self.tick_period;
            n += 1;
            self.dirty = true;
        }
        if n == MAX_TICKS_PER_BATCH && self.next_tick < now {
            // Can't keep up: drop the backlog instead of spiralling.
            self.next_tick = now + self.tick_period;
        }
    }

    fn apply(&mut self, r: Result<(), SimError>) {
        if let Err(e) = r {
            self.halt(e);
        }
    }

    fn halt(&mut self, e: SimError) {
        tracing::warn!("simulation halted: {e}");
        self.error = Some(e);
        self.state = RunState::Halted;
        self.dirty = true;
    }

    fn publish(&mut self) {
        self.generation += 1;
        let (tick, values, net_count) =
            self.sim.as_ref().map_or((0, Vec::new(), 0), |s| (s.tick, s.packed(), s.values().len()));
        self.out.store(Arc::new(Snapshot {
            generation: self.generation,
            revision: self.revision,
            state: self.state,
            tick,
            values,
            net_count,
            net_map: Arc::clone(&self.net_map),
            compile_id: self.compile_id,
            error: self.error.clone(),
        }));
        self.dirty = false;
        self.last_publish = Instant::now();
        (self.on_publish)();
    }
}
