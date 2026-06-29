//! Per-instruction 65C816 master-cycle estimates (port of
//! `recompiler/v2/cycle_tables.py`).
//!
//! Proportional, not cycle-perfect: a static recompiler can't know runtime
//! penalties (page crossing, branch taken, DMA stalls). WDC datasheet
//! "standard" timings; the 8/16-bit `+m`/`+x` width adjustments ARE applied
//! because the decoder pins `m_flag`/`x_flag` per instruction.

use crate::insn::{Insn, Mode};

const DEFAULT: u32 = 2; // conservative fallback for anything unmodeled

/// Base cycles by addressing mode for the ALU/load/store class.
fn alu_mode(mode: Mode) -> Option<u32> {
    use Mode::*;
    Some(match mode {
        Imm => 2,
        Dp => 3,
        DpX => 4,
        DpY => 4,
        Abs => 4,
        AbsX => 4,
        AbsY => 4,
        Long => 5,
        LongX => 5,
        DpIndir => 5,  // (dp)
        IndirY => 5,   // (dp),y
        IndirDpx => 6, // (dp,x)
        IndirL => 6,   // [dp]
        IndirLy => 6,  // [dp],y
        Stk => 4,      // d,s
        StkIy => 7,    // (d,s),y
        _ => return None,
    })
}

fn is_alu_a(mnem: &str) -> bool {
    matches!(
        mnem,
        "LDA" | "STA" | "ADC" | "SBC" | "AND" | "ORA" | "EOR" | "CMP" | "BIT" | "STZ"
    )
}

fn is_alu_x(mnem: &str) -> bool {
    matches!(mnem, "LDX" | "LDY" | "STX" | "STY" | "CPX" | "CPY")
}

fn rmw_mode(mode: Mode) -> u32 {
    use Mode::*;
    match mode {
        Acc => 2,
        Dp => 5,
        DpX => 6,
        Abs => 6,
        AbsX => 7,
        _ => 6,
    }
}

fn is_rmw(mnem: &str) -> bool {
    matches!(
        mnem,
        "ASL" | "LSR" | "ROL" | "ROR" | "INC" | "DEC" | "TSB" | "TRB"
    )
}

fn is_branch(mnem: &str) -> bool {
    matches!(
        mnem,
        "BCC" | "BCS" | "BEQ" | "BNE" | "BMI" | "BPL" | "BVC" | "BVS" | "BRA"
    )
}

fn jmp_mode(mode: Mode) -> u32 {
    use Mode::*;
    match mode {
        Abs => 3,
        Long => 4,
        Indir => 5,
        IndirX => 6,
        IndirL => 6,
        _ => 3,
    }
}

fn fixed_cost(mnem: &str) -> Option<u32> {
    Some(match mnem {
        "TAX" | "TXA" | "TAY" | "TYA" | "TSX" | "TXS" | "TXY" | "TYX" | "TCD" | "TDC" | "TCS"
        | "TSC" | "DEX" | "DEY" | "INX" | "INY" | "NOP" | "XCE" | "WDM" | "CLC" | "SEC" | "CLD"
        | "SED" | "CLI" | "SEI" | "CLV" => 2,
        "XBA" => 3,
        "REP" | "SEP" => 3,
        "PHA" | "PHX" | "PHY" | "PHP" | "PHB" | "PHK" => 3,
        "PHD" => 4,
        "PEA" => 5,
        "PEI" | "PER" => 6,
        "PLA" | "PLX" | "PLY" | "PLP" | "PLB" => 4,
        "PLD" => 5,
        "JSR" => 6,
        "JSL" => 8,
        "RTS" | "RTL" => 6,
        "RTI" => 7,
        "BRK" | "COP" => 7,
        "BRL" => 3,
        "MVN" | "MVP" => 7,
        "WAI" | "STP" => 3,
        _ => return None,
    })
}

/// Estimated master-cycle cost for one decoded instruction. Robust to unmodeled
/// ops — falls back to a small constant so the clock always advances.
pub fn estimate_cycles(insn: &Insn) -> u32 {
    let mnem = insn.mnem;
    let mode = insn.mode;
    let m = insn.m_flag;
    let x = insn.x_flag;

    if is_alu_a(mnem) {
        let mut c = alu_mode(mode).unwrap_or(4);
        if m == 0 {
            c += 1;
        }
        return c;
    }
    if is_alu_x(mnem) {
        let mut c = alu_mode(mode).unwrap_or(4);
        if x == 0 {
            c += 1;
        }
        return c;
    }
    if is_rmw(mnem) {
        let mut c = rmw_mode(mode);
        if mode != Mode::Acc && m == 0 {
            c += 2;
        }
        return c;
    }
    if is_branch(mnem) {
        return 2;
    }
    if mnem == "JMP" || mnem == "JML" {
        return jmp_mode(mode);
    }
    fixed_cost(mnem).unwrap_or(DEFAULT)
}

/// Sum estimated cycles over a block's decoded instructions. 0 for empty.
pub fn estimate_block_cycles(insns: &[Insn]) -> u32 {
    insns.iter().map(estimate_cycles).sum()
}
