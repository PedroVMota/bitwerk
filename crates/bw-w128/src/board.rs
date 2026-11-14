//! The W-128 mainboard: clock and reset, program ROM, control unit, four
//! 32-bit CPU slices with the carry-lookahead chip, the RAM module (two
//! ranks of eight 16-bit chips), the I/O card (OUT register) and the
//! displays.
//!
//! RAM and ROM chips are primitives on the board itself, so the Memory
//! Contents window shows them live while the program runs.

use bw_core::{Circuit, Color, ComponentId, ComponentKind, GateOp, MemoryType, NumFormat, SwitchMode};

use crate::blocks::{HEX16, Lib, library};
use crate::layout::{Body, H, Sig, add_note};

/// Top of the first datapath lane and the distance between lanes (grid
/// units): room for a slice and the buses that loop back below it.
const LANE_Y0: i32 = 230;
const LANE_PITCH: i32 = 300;

/// Clock half-period in ticks (one tick per edge: the fastest clock).
pub const CLOCK_HALF_PERIOD: u32 = 1;

/// Component ids of the board parts the tests and the app look at.
#[derive(Debug, Clone)]
pub struct BoardIds {
    /// Program ROM chips: instruction bits 0–15 and 16–31.
    pub rom_lo: ComponentId,
    pub rom_hi: ComponentId,
    /// RAM chips `[rank][lane]`; lane `j` holds bits `16j..16j+15`.
    pub ram: [[ComponentId; 8]; 2],
    pub cu: ComponentId,
    pub slices: [ComponentId; 4],
    /// The OUT register latches (lane `j` = bits `16j..`).
    pub out_latches: [ComponentId; 8],
    pub halt_led: ComponentId,
    pub clock: ComponentId,
    pub reset: ComponentId,
}

/// Statistics of a generated board.
#[derive(Debug, Clone, Default)]
pub struct BoardStats {
    pub conflicts: usize,
}

fn u(k: usize) -> u8 {
    u8::try_from(k).unwrap_or(u8::MAX)
}

const RAM_CHIP: ComponentKind = ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 8, data_bits: 16 };
const ROM_CHIP: ComponentKind = ComponentKind::Memory { ty: MemoryType::Rom, addr_bits: 8, data_bits: 16 };

/// Builds the board with `program` in ROM and `ram` as initial RAM
/// contents.
///
/// # Panics
/// If the model rejects a generated part (a generator bug).
#[must_use]
#[allow(clippy::too_many_lines, reason = "the board netlist, one block per board area")]
pub fn board(program: &[u32], ram: &[u128]) -> (Circuit, BoardIds, BoardStats) {
    let mut lib: Lib = library();
    let k_cu = lib.kind("cu");
    let k_slice = lib.kind("slice32");
    let k_cla = lib.kind("cla4");
    let k_latch = lib.kind("latch16");
    let mut b = Body::new();
    b.title("ANDORINHA W-128  -  128-bit bit-slice computer  -  mainboard rev. A");

    // Column 0: clock, reset, ground.
    let clk = b.add(ComponentKind::Clock { half_period: CLOCK_HALF_PERIOD }, 0);
    b.label(clk, "CLOCK", 14, Color::Yellow);
    b.color(clk, Color::Yellow);
    let rst = b.add(ComponentKind::Switch { on: false, mode: SwitchMode::Momentary }, 0);
    b.label(rst, "RESET", 14, Color::Red);
    let gnd = b.add(ComponentKind::Const { high: false }, 0);
    b.label(gnd, "GND", 10, Color::Grey);
    let (clk, rst, gnd) = (b.out(clk, 0), b.out(rst, 0), b.out(gnd, 0));

    // Column 1: program ROM ("BIOS").
    let rom_hi = b.add(ROM_CHIP, 1);
    b.label(rom_hi, "PROGRAM ROM\nI[31:16]", 14, Color::Orange);
    let rom_lo = b.add(ROM_CHIP, 1);
    b.label(rom_lo, "I[15:0]", 14, Color::Orange);
    b.gap(rom_lo, 8);

    // Column 2: control unit.
    let cu = b.add(k_cu, 2);
    b.label(cu, "CONTROL UNIT", 20, Color::Cyan);
    let cu_out = |b: &Body, k: usize| b.out(cu, u(k));
    // CU outputs: PC 0-7, MAR 8-17, RA 18-20, RB 21-23, RD 24-26, WE 27,
    // ctl 28-38, WSTB 39, IR 40-71, FETCH 72, EXEC 73, MEM 74, HALT 75,
    // FZ 76, FC 77, FN 78.
    let pc: Vec<Sig> = (0..8).map(|i| cu_out(&b, i)).collect();
    let mar: Vec<Sig> = (0..10).map(|i| cu_out(&b, 8 + i)).collect();
    let ra: Vec<Sig> = (0..3).map(|i| cu_out(&b, 18 + i)).collect();
    let rbs: Vec<Sig> = (0..3).map(|i| cu_out(&b, 21 + i)).collect();
    let rd: Vec<Sig> = (0..3).map(|i| cu_out(&b, 24 + i)).collect();
    let we = cu_out(&b, 27);
    let ctl: Vec<Sig> = (0..11).map(|i| cu_out(&b, 28 + i)).collect();
    let sub = ctl[3];
    let wstb = cu_out(&b, 39);
    let ir: Vec<Sig> = (0..32).map(|i| cu_out(&b, 40 + i)).collect();
    let status: Vec<Sig> = (0..7).map(|i| cu_out(&b, 72 + i)).collect();
    b.feed(rom_hi, 0, &pc);
    b.feed(rom_lo, 0, &pc);
    let rom_bits: Vec<Sig> = (0..16).map(|i| b.out(rom_lo, u(i))).chain((0..16).map(|i| b.out(rom_hi, u(i)))).collect();
    b.feed(cu, 0, &[clk, rst]);
    b.feed(cu, 2, &rom_bits);

    // The datapath is four horizontal lanes of 32 bits, top (bits 127..96)
    // to bottom: in each lane a CPU slice, its four RAM chips (two in each
    // DIMM), two OUT latches and the lane's displays, aligned so the data
    // buses run straight across and loop back through the gap below.
    let lane_y = |s: usize| LANE_Y0 + i32::try_from(s).unwrap_or(0) * LANE_PITCH;

    // Column 3: address decoder, the four slices, CPU status.
    let nio = b.not(mar[9], 3);
    let n8 = b.not(mar[8], 3);
    let cs0 = b.gate(GateOp::Or, &[mar[9], mar[8]], 3);
    let cs1 = b.gate(GateOp::Or, &[mar[9], n8], 3);
    let nmemw = b.gate(GateOp::Nand, &[wstb, nio], 3);
    let iow = b.gate(GateOp::And, &[wstb, mar[9]], 3);
    b.label(nio.comp, "ADDRESS\nDECODER", 14, Color::Lime);
    let slices: Vec<H> = (0..4).map(|_| b.add(k_slice, 3)).collect();
    // Top to bottom: slice 3 (bits 127..96) … slice 0 (bits 31..0).
    let slice = |k: usize| slices[3 - k];
    for k in (0..4).rev() {
        let s = slice(k);
        b.label(s, &format!("CPU SLICE {k}\nbits {}..{}", 32 * k + 31, 32 * k), 16, Color::Cyan);
        b.at_y(s, lane_y(3 - k));
    }
    let pc_disp = b.add(ComponentKind::Numeric { bits: 8, format: NumFormat::Hex, signed: false }, 3);
    b.label(pc_disp, "PC", 16, Color::Orange);
    b.color(pc_disp, Color::Orange);
    b.gap(pc_disp, 40);
    b.feed(pc_disp, 0, &pc);
    let mar_disp = b.add(ComponentKind::Numeric { bits: 10, format: NumFormat::Hex, signed: false }, 3);
    b.label(mar_disp, "MAR", 16, Color::Orange);
    b.color(mar_disp, Color::Orange);
    b.feed(mar_disp, 0, &mar);
    let ir_hi = b.add(HEX16, 3);
    b.label(ir_hi, "IR", 16, Color::Orange);
    b.color(ir_hi, Color::Orange);
    b.feed(ir_hi, 0, &ir[16..32]);
    let ir_lo = b.add(HEX16, 3);
    b.color(ir_lo, Color::Orange);
    b.feed(ir_lo, 0, &ir[0..16]);
    let led_names = ["FETCH", "EXEC", "MEM", "HALT", "Z", "C", "N"];
    let led_colors = [Color::Lime, Color::Lime, Color::Lime, Color::Red, Color::Yellow, Color::Yellow, Color::Yellow];
    let mut leds = Vec::new();
    for (k, s) in status.iter().enumerate() {
        let h = b.add(ComponentKind::Led, 3);
        b.color(h, led_colors[k]);
        b.label(h, led_names[k], 12, led_colors[k]);
        b.connect(*s, h, 0);
        leds.push(h);
    }
    let wled = b.add(ComponentKind::Led, 3);
    b.color(wled, Color::Orange);
    b.label(wled, "WSTB", 12, Color::Orange);
    b.connect(wstb, wled, 0);

    // Column 4: carry lookahead over the slices, then DIMM 0.
    let cla = b.add(k_cla, 4);
    b.label(cla, "CARRY\nLOOKAHEAD", 14, Color::Cyan);
    let mut ys: Vec<Sig> = Vec::new();
    let mut douts: Vec<Sig> = Vec::new();
    for k in 0..4 {
        ys.extend((0..32).map(|i| b.out(slice(k), u(i))));
    }
    for k in 0..4 {
        douts.extend((0..32).map(|i| b.out(slice(k), u(32 + i))));
    }
    // Slice outputs: Y 0-31, DOUT 32-63, G 64, P 65, Z 66, A0 67, A31 68.
    b.feed(cla, 0, &[sub]);
    for k in 0..4 {
        let (g, p) = (b.out(slice(k), 64), b.out(slice(k), 65));
        b.feed(cla, u(1 + 2 * k), &[g, p]);
    }
    let (cla_g, cla_p) = (b.out(cla, 3), b.out(cla, 4));
    let t = b.gate(GateOp::And, &[cla_p, sub], 4);
    let c128 = b.gate(GateOp::Or, &[cla_g, t], 4);
    // Slice inputs: CLK, RA0-2, RB0-2, RD0-2, WE, ctl (11), CIN, SLI, SRI,
    // IMM0-15, IMMX, DIN0-31.
    let imm15 = ir[15];
    for k in 0..4 {
        let s = slice(k);
        b.feed(s, 0, &[clk]);
        b.feed(s, 1, &ra);
        b.feed(s, 4, &rbs);
        b.feed(s, 7, &rd);
        b.feed(s, 10, &[we]);
        b.feed(s, 11, &ctl);
        let cin = if k == 0 { sub } else { b.out(cla, u(k - 1)) };
        let sli = if k == 0 { gnd } else { b.out(slice(k - 1), 68) };
        let sri = if k == 3 { gnd } else { b.out(slice(k + 1), 67) };
        b.feed(s, 22, &[cin, sli, sri]);
        if k == 0 {
            b.feed(s, 25, &ir[0..16]);
        } else {
            b.feed(s, 25, &[imm15; 16]);
        }
        b.feed(s, 41, &[imm15]);
    }
    let zs: Vec<Sig> = (0..4).map(|k| b.out(slice(k), 66)).collect();
    let a0 = b.out(slice(0), 67);
    let a127 = b.out(slice(3), 68);
    // CU inputs: CLK, RST, I0-31, Y0-9, Z0-3, C128, A0, A127, N.
    b.feed(cu, 34, &ys[0..10]);
    b.feed(cu, 44, &zs);
    b.feed(cu, 48, &[c128, a0, a127, ys[127]]);

    // Columns 4 and 5: the two DIMMs (ranks), two chips per lane each.
    let mut ram_ids = [[0usize; 8]; 2];
    let mut din: Vec<Option<Sig>> = vec![None; 128];
    for rank in 0..2 {
        for k in (0..4).rev() {
            for lane in [2 * k + 1, 2 * k] {
                let h = b.add(RAM_CHIP, 4 + rank);
                let base = 256 * rank;
                let text = format!(
                    "U{} {:#05x}-{:#05x} bits {}..{}",
                    10 + 8 * rank + lane,
                    base,
                    base + 255,
                    16 * lane + 15,
                    16 * lane
                );
                if lane == 7 {
                    b.label(h, &format!("DIMM {rank}  (RANK {rank})\n{text}"), 16, Color::Lime);
                } else {
                    b.label(h, &text, 10, Color::Lime);
                }
                if lane == 2 * k + 1 {
                    b.at_y(h, lane_y(3 - k));
                }
                // RAM inputs: A0-7, D0-15, R/W, CS.
                b.feed(h, 0, &mar[0..8]);
                b.feed(h, 8, &douts[16 * lane..16 * lane + 16]);
                b.feed(h, 24, &[nmemw, if rank == 0 { cs0 } else { cs1 }]);
                for i in 0..16 {
                    let q = b.out(h, u(i));
                    match din[16 * lane + i] {
                        Some(d) => b.join(d, q),
                        None => din[16 * lane + i] = Some(q),
                    }
                }
                ram_ids[rank][lane] = h;
            }
        }
    }
    let din: Vec<Sig> = din.into_iter().map(|d| d.unwrap_or(gnd)).collect();
    for k in 0..4 {
        b.feed(slice(k), 42, &din[32 * k..32 * k + 32]);
    }

    // Column 6: the I/O card's OUT register and the data-in bus displays;
    // column 7: the OUT display.
    let mut latches = [0usize; 8];
    for k in (0..4).rev() {
        for (j, lane) in [2 * k + 1, 2 * k].into_iter().enumerate() {
            let h = b.add(k_latch, 6);
            if j == 0 {
                b.at_y(h, lane_y(3 - k));
            }
            let title = if lane == 7 { "I/O CARD: OUT REGISTER (0x200)\n" } else { "" };
            b.label(h, &format!("{title}OUT bits {}..{}", 16 * lane + 15, 16 * lane), 12, Color::White);
            b.feed(h, 0, &douts[16 * lane..16 * lane + 16]);
            b.feed(h, 16, &[iow]);
            latches[lane] = h;
        }
        for lane in [2 * k + 1, 2 * k] {
            let h = b.add(HEX16, 6);
            b.color(h, Color::Lime);
            b.label(h, &format!("DATA IN bits {}..{}", 16 * lane + 15, 16 * lane), 12, Color::Lime);
            b.feed(h, 0, &din[16 * lane..16 * lane + 16]);
        }
    }
    for k in (0..4).rev() {
        for (j, lane) in [2 * k + 1, 2 * k].into_iter().enumerate() {
            let h = b.add(HEX16, 7);
            b.color(h, Color::White);
            if j == 0 {
                b.at_y(h, lane_y(3 - k) + 2);
            }
            let title = if lane == 7 { "OUT [127:0]\n" } else { "" };
            b.label(h, &format!("{title}bits {}..{}", 16 * lane + 15, 16 * lane), 12, Color::White);
            let qs: Vec<Sig> = (0..16).map(|i| b.out(latches[lane], u(i))).collect();
            b.feed(h, 0, &qs);
        }
    }

    let lay = b.build_into(&mut lib.c);
    let id = |h: H| lay.ids[h];
    let z = ComponentId(0);
    let mut ids = BoardIds {
        rom_lo: id(rom_lo),
        rom_hi: id(rom_hi),
        ram: [[z; 8]; 2],
        cu: id(cu),
        slices: [z; 4],
        out_latches: [z; 8],
        halt_led: id(leds[3]),
        clock: lay.ids[0],
        reset: lay.ids[1],
    };
    for k in 0..4 {
        ids.slices[k] = id(slice(k));
    }
    for rank in 0..2 {
        for lane in 0..8 {
            ids.ram[rank][lane] = id(ram_ids[rank][lane]);
        }
    }
    for lane in 0..8 {
        ids.out_latches[lane] = id(latches[lane]);
    }
    // Silkscreen: the instruction set at the bottom left.
    add_note(
        &mut lib.c,
        bw_core::GridPos::new(lay.min.x, lay.max.y + 6),
        "W-128 ISA: r0-r7 (r0 = 0), 128-bit; flags Z C N\nADD SUB AND OR XOR SHL SHR  rd, ra, rb    ADDI SUBI ANDI ORI XORI  rd, ra, imm16\nLD rd, [ra+imm]   ST rb, [ra+imm]   JMP JZ JNZ JC JNC JN JNN  addr   HALT\nRAM 0x000-0x1FF (2 ranks x 8 chips x 16 bits), OUT register at 0x200",
        14,
        false,
        Some(Color::Grey),
    );
    let c = load_memories(lib.c, &ids, program, ram);
    (c, ids, BoardStats { conflicts: lib.conflicts + lay.conflicts })
}

/// Writes the program into the ROM chips and `ram` into the RAM chips.
///
/// # Panics
/// If the ids are not memories of the right size (a generator bug).
#[must_use]
pub fn load_memories(mut c: Circuit, ids: &BoardIds, program: &[u32], ram: &[u128]) -> Circuit {
    let lo: Vec<u16> = (0..256).map(|i| program.get(i).map_or(0, |w| (w & 0xffff) as u16)).collect();
    let hi: Vec<u16> = (0..256).map(|i| program.get(i).map_or(0, |w| (w >> 16) as u16)).collect();
    c.set_memory(ids.rom_lo, &lo).unwrap_or_else(|e| panic!("ROM: {e}"));
    c.set_memory(ids.rom_hi, &hi).unwrap_or_else(|e| panic!("ROM: {e}"));
    for rank in 0..2 {
        for lane in 0..8 {
            let words: Vec<u16> =
                (0..256).map(|a| ram.get(256 * rank + a).map_or(0, |w| ((w >> (16 * lane)) & 0xffff) as u16)).collect();
            c.set_memory(ids.ram[rank][lane], &words).unwrap_or_else(|e| panic!("RAM: {e}"));
        }
    }
    c
}

/// The 128-bit RAM word at `addr` from the eight chips of its rank.
#[must_use]
pub fn ram_word(chips: &[[ComponentId; 8]; 2], read: impl Fn(ComponentId, usize) -> u16, addr: usize) -> u128 {
    let rank = addr / 256;
    (0..8).fold(0u128, |acc, lane| acc | u128::from(read(chips[rank][lane], addr % 256)) << (16 * lane))
}

/// Name of the example project.
pub const PROJECT_NAME: &str = "Andorinha W-128";

/// The example board with the assembled demo program.
///
/// # Errors
/// If the demo does not assemble (a bug).
pub fn demo_board() -> Result<(Circuit, BoardIds, BoardStats), crate::asm::AsmError> {
    let prog = crate::asm::assemble(crate::program::DEMO)?;
    Ok(board(&prog.words, &[]))
}

/// The text of `examples/w128-computer.bitwerk`.
///
/// # Errors
/// If the demo does not assemble or the project cannot be saved.
pub fn project_text() -> Result<String, Box<dyn std::error::Error>> {
    let (c, _, _) = demo_board()?;
    Ok(bw_io::save_to_string(&bw_io::Project { name: PROJECT_NAME.into(), circuit: c })?)
}
