//! Running the board in the simulator (tests and the report).

use bw_core::{Circuit, ComponentId, ComponentKind, MacroId};
use bw_sim::{Netlist, SimError, SimState};

use crate::board::BoardIds;

/// CU output index of the HALT state line.
pub const CU_HALT: u8 = 75;

/// The board compiled and reset, with its clock-cycle count.
pub struct Board {
    pub circuit: Circuit,
    pub ids: BoardIds,
    pub sim: SimState,
    /// Rising clock edges so far.
    pub cycles: u64,
    pub ticks: u64,
}

impl Board {
    /// Compiles and resets `circuit`.
    ///
    /// # Errors
    /// If the circuit does not compile or settle.
    pub fn new(circuit: Circuit, ids: BoardIds) -> Result<Self, SimError> {
        let net = Netlist::compile(&circuit)?;
        let mut sim = SimState::new(net, &circuit);
        sim.reset()?;
        Ok(Self { circuit, ids, sim, cycles: 0, ticks: 0 })
    }

    #[must_use]
    pub fn halted(&self) -> bool {
        self.sim.output(self.ids.cu, CU_HALT) == Some(1)
    }

    /// One tick (one clock edge).
    ///
    /// # Errors
    /// Race condition or bus contention.
    pub fn tick(&mut self) -> Result<(), SimError> {
        self.sim.tick()?;
        self.ticks += 1;
        if self.sim.output(self.ids.clock, 0) == Some(1) {
            self.cycles += 1;
        }
        Ok(())
    }

    /// Runs until HALT (or `max_cycles`); returns whether it halted.
    ///
    /// # Errors
    /// Race condition or bus contention.
    pub fn run_to_halt(&mut self, max_cycles: u64) -> Result<bool, SimError> {
        while !self.halted() {
            if self.cycles >= max_cycles {
                return Ok(false);
            }
            self.tick()?;
        }
        Ok(true)
    }

    fn word(&self, id: ComponentId, addr: usize) -> u16 {
        self.sim.memory(id).and_then(|(w, _)| w.get(addr).copied()).unwrap_or(0)
    }

    /// The live 128-bit RAM word at `addr` (0..512).
    #[must_use]
    pub fn ram(&self, addr: usize) -> u128 {
        crate::board::ram_word(&self.ids.ram, |id, a| self.word(id, a), addr)
    }

    /// Level of output `k` of root component `id` as 0/1 (`None` for Z/X).
    #[must_use]
    pub fn bit(&self, id: ComponentId, k: u8) -> Option<bool> {
        match self.sim.output(id, k) {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        }
    }

    /// The OUT register (`None` if any bit is not 0/1).
    #[must_use]
    pub fn out_register(&self) -> Option<u128> {
        let mut v = 0u128;
        for lane in 0..8 {
            for i in 0..16u8 {
                if self.bit(self.ids.out_latches[lane], i)? {
                    v |= 1 << (16 * lane + usize::from(i));
                }
            }
        }
        Some(v)
    }

    /// Program counter (CU outputs 0–7).
    #[must_use]
    pub fn pc(&self) -> Option<u8> {
        (0..8u8).try_fold(0u8, |acc, k| Some(acc | u8::from(self.bit(self.ids.cu, k)?) << k))
    }

    /// Flags Z, C, N (CU outputs 76–78).
    #[must_use]
    pub fn flags(&self) -> Option<(bool, bool, bool)> {
        Some((self.bit(self.ids.cu, 76)?, self.bit(self.ids.cu, 77)?, self.bit(self.ids.cu, 78)?))
    }

    /// Register `r` (1..8) read from the flip-flops inside the slices.
    #[must_use]
    pub fn reg(&self, r: usize) -> Option<u128> {
        let table = self.circuit.macros();
        let def_of = |id: ComponentId, body: &Circuit| -> Option<MacroId> {
            match body.component(id)?.kind {
                ComponentKind::Macro(s) => Some(s.def),
                _ => None,
            }
        };
        let mut v = 0u128;
        for k in 0..4 {
            let slice = self.ids.slices[k];
            let sdef = def_of(slice, &self.circuit)?;
            let sbody = &table.get(sdef)?.def.body;
            // The register file is the first macro instance of the slice.
            let mut insts: Vec<&bw_core::Component> =
                sbody.components().filter(|c| matches!(c.kind, ComponentKind::Macro(_))).collect();
            insts.sort_by_key(|c| c.id);
            let rf = insts.first()?.id;
            let rfdef = def_of(rf, sbody)?;
            let rbody = &table.get(rfdef)?.def.body;
            let reg32 = table.iter().find(|(_, e)| e.def.name == "Reg32")?.0;
            let mut regs: Vec<ComponentId> = rbody
                .components()
                .filter(|c| matches!(c.kind, ComponentKind::Macro(s) if s.def == reg32))
                .map(|c| c.id)
                .collect();
            regs.sort_unstable();
            let reg = *regs.get(r.checked_sub(1)?)?;
            for i in 0..32u8 {
                let net = self.sim.netlist().view_output_net(&[slice, rf], reg, i)?;
                match self.sim.values()[net as usize] {
                    0 => {}
                    1 => v |= 1 << (32 * k + usize::from(i)),
                    _ => return None,
                }
            }
        }
        Some(v)
    }
}
