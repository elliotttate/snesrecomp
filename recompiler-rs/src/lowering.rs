//! Lower a decoded `Insn` into a list of IR ops (port of
//! `recompiler/v2/lowering.py`).
//!
//! Width comes from the entry M/X flags stamped on `Insn.m_flag` / `Insn.x_flag`
//! (the v2 decoder guarantees these are correct per DecodeKey). Lowering does NOT
//! optimize — each insn produces a faithful op sequence.

use crate::insn::{Insn, Mode};
use crate::ir::{
    AluOp, Call, IROp, MoveDir, Reg, Return, SegKind, SegRef, ShiftOp, Value,
};

/// Mints fresh, function-unique `Value` ids (monotonic counter). Mirrors the
/// Python `value_factory` closure.
#[derive(Debug, Default)]
pub struct ValueFactory {
    counter: u32,
}

impl ValueFactory {
    pub fn new() -> Self {
        ValueFactory { counter: 0 }
    }

    /// Next fresh Value (1-based, matching the Python `counter[0] += 1` order).
    pub fn next(&mut self) -> Value {
        self.counter += 1;
        Value { vid: self.counter }
    }
}

// ── Addressing-mode -> SegRef ────────────────────────────────────────────────

fn segref_for(insn: &Insn) -> SegRef {
    let op = insn.operand as i32;
    let s = |kind: SegKind| SegRef::new(kind);
    use Mode::*;
    match insn.mode {
        Dp => SegRef { offset: op, ..s(SegKind::Direct) },
        DpX => SegRef { offset: op, index: Some(Reg::X), ..s(SegKind::Direct) },
        DpY => SegRef { offset: op, index: Some(Reg::Y), ..s(SegKind::Direct) },

        Abs => SegRef { offset: op, ..s(SegKind::AbsBank) },
        AbsX => SegRef { offset: op, index: Some(Reg::X), ..s(SegKind::AbsBank) },
        AbsY => SegRef { offset: op, index: Some(Reg::Y), ..s(SegKind::AbsBank) },

        Long => SegRef {
            offset: (insn.operand & 0xFFFF) as i32,
            bank: Some(((insn.operand >> 16) & 0xFF) as u8),
            ..s(SegKind::Long)
        },
        LongX => SegRef {
            offset: (insn.operand & 0xFFFF) as i32,
            bank: Some(((insn.operand >> 16) & 0xFF) as u8),
            index: Some(Reg::X),
            ..s(SegKind::Long)
        },

        Stk => SegRef { offset: op, ..s(SegKind::Stack) },
        StkIy => SegRef { offset: op, ..s(SegKind::StackRelIndirectY) },

        Indir => SegRef { offset: op, ..s(SegKind::AbsIndirect) },
        IndirX => SegRef { offset: op, ..s(SegKind::AbsIndirectX) },

        DpIndir => SegRef { offset: op, ..s(SegKind::DpIndirect) },
        IndirY => SegRef { offset: op, index: Some(Reg::Y), ..s(SegKind::DpIndirect) },
        IndirDpx => SegRef { offset: op, ..s(SegKind::DpIndirectX) },

        IndirL => SegRef { offset: op, ..s(SegKind::DpIndirectLong) },
        IndirLy => SegRef { offset: op, index: Some(Reg::Y), ..s(SegKind::DpIndirectLong) },

        other => panic!("segref_for: mode {other:?} not memory-referencing"),
    }
}

// ── Width helpers ────────────────────────────────────────────────────────────

fn width_a(insn: &Insn) -> u8 {
    if insn.m_flag != 0 { 1 } else { 2 }
}

fn width_x(insn: &Insn) -> u8 {
    if insn.x_flag != 0 { 1 } else { 2 }
}

// ── Top-level dispatch ──────────────────────────────────────────────────────

/// Lower one `Insn` to a list of IR ops. Unknown mnemonics become `Nop`.
pub fn lower(insn: &Insn, vf: &mut ValueFactory) -> Vec<IROp> {
    use Mode::*;
    match insn.mnem {
        "LDA" => {
            let w = width_a(insn);
            if insn.mode == Imm {
                let v = vf.next();
                vec![
                    IROp::ConstI { value: insn.operand as i64, width: w, out: v },
                    IROp::WriteReg { reg: Reg::A, src: v },
                    IROp::SetNZ { src: v, width: w },
                ]
            } else {
                let seg = segref_for(insn);
                let v = vf.next();
                vec![
                    IROp::Read { seg, width: w, out: v },
                    IROp::WriteReg { reg: Reg::A, src: v },
                    IROp::SetNZ { src: v, width: w },
                ]
            }
        }
        "LDX" => ld_reg(insn, vf, Reg::X, width_x(insn)),
        "LDY" => ld_reg(insn, vf, Reg::Y, width_x(insn)),

        "STA" => st_reg(insn, vf, Reg::A, width_a(insn)),
        "STX" => st_reg(insn, vf, Reg::X, width_x(insn)),
        "STY" => st_reg(insn, vf, Reg::Y, width_x(insn)),
        "STZ" => {
            let seg = segref_for(insn);
            let w = width_a(insn);
            let z = vf.next();
            vec![
                IROp::ConstI { value: 0, width: w, out: z },
                IROp::Write { seg, src: z, width: w },
            ]
        }

        "ADC" => alu(insn, vf, AluOp::Add, Reg::A, width_a(insn)),
        "SBC" => alu(insn, vf, AluOp::Sub, Reg::A, width_a(insn)),
        "AND" => alu(insn, vf, AluOp::And, Reg::A, width_a(insn)),
        "ORA" => alu(insn, vf, AluOp::Or, Reg::A, width_a(insn)),
        "EOR" => alu(insn, vf, AluOp::Xor, Reg::A, width_a(insn)),
        "CMP" => alu(insn, vf, AluOp::Cmp, Reg::A, width_a(insn)),
        "CPX" => alu(insn, vf, AluOp::Cmp, Reg::X, width_x(insn)),
        "CPY" => alu(insn, vf, AluOp::Cmp, Reg::Y, width_x(insn)),

        "ASL" => shift(insn, vf, ShiftOp::Asl),
        "LSR" => shift(insn, vf, ShiftOp::Lsr),
        "ROL" => shift(insn, vf, ShiftOp::Rol),
        "ROR" => shift(insn, vf, ShiftOp::Ror),

        "INC" => {
            if insn.mode == Acc {
                vec![IROp::IncReg { reg: Reg::A, delta: 1 }]
            } else {
                vec![IROp::IncMem { seg: segref_for(insn), width: width_a(insn), delta: 1 }]
            }
        }
        "DEC" => {
            if insn.mode == Acc {
                vec![IROp::IncReg { reg: Reg::A, delta: -1 }]
            } else {
                vec![IROp::IncMem { seg: segref_for(insn), width: width_a(insn), delta: -1 }]
            }
        }
        "INX" => vec![IROp::IncReg { reg: Reg::X, delta: 1 }],
        "INY" => vec![IROp::IncReg { reg: Reg::Y, delta: 1 }],
        "DEX" => vec![IROp::IncReg { reg: Reg::X, delta: -1 }],
        "DEY" => vec![IROp::IncReg { reg: Reg::Y, delta: -1 }],

        "BIT" => {
            let width = width_a(insn);
            if insn.mode == Imm {
                let rhs = vf.next();
                vec![
                    IROp::ConstI { value: insn.operand as i64, width, out: rhs },
                    IROp::BitTest { operand: rhs, width },
                ]
            } else {
                let seg = segref_for(insn);
                let rhs = vf.next();
                vec![
                    IROp::Read { seg, width, out: rhs },
                    IROp::BitTest { operand: rhs, width },
                ]
            }
        }
        "TSB" => vec![IROp::BitSetMem { seg: segref_for(insn), width: width_a(insn) }],
        "TRB" => vec![IROp::BitClearMem { seg: segref_for(insn), width: width_a(insn) }],

        "TAX" => xfer(Reg::A, Reg::X),
        "TXA" => xfer(Reg::X, Reg::A),
        "TAY" => xfer(Reg::A, Reg::Y),
        "TYA" => xfer(Reg::Y, Reg::A),
        "TXY" => xfer(Reg::X, Reg::Y),
        "TYX" => xfer(Reg::Y, Reg::X),
        "TSX" => xfer(Reg::S, Reg::X),
        "TXS" => xfer(Reg::X, Reg::S),
        "TCD" => xfer(Reg::A, Reg::D),
        "TDC" => xfer(Reg::D, Reg::A),
        "TCS" => xfer(Reg::A, Reg::S),
        "TSC" => xfer(Reg::S, Reg::A),

        "CLC" => flag_set(Reg::C, 0),
        "SEC" => flag_set(Reg::C, 1),
        "CLI" => flag_set(Reg::I, 0),
        "SEI" => flag_set(Reg::I, 1),
        "CLD" => flag_set(Reg::Df, 0),
        "SED" => flag_set(Reg::Df, 1),
        "CLV" => flag_set(Reg::V, 0),

        "REP" => vec![IROp::RepFlags { mask: insn.operand as u8 }],
        "SEP" => vec![IROp::SepFlags { mask: insn.operand as u8 }],
        "XCE" => vec![IROp::Xce],
        "XBA" => vec![IROp::Xba],

        "PHA" => push_reg(insn, Reg::A),
        "PLA" => pull_reg(insn, Reg::A),
        "PHX" => push_reg(insn, Reg::X),
        "PLX" => pull_reg(insn, Reg::X),
        "PHY" => push_reg(insn, Reg::Y),
        "PLY" => pull_reg(insn, Reg::Y),
        "PHB" => push_reg(insn, Reg::Db),
        "PLB" => pull_reg(insn, Reg::Db),
        "PHD" => push_reg(insn, Reg::D),
        "PLD" => pull_reg(insn, Reg::D),
        "PHK" => push_reg(insn, Reg::Pb),
        "PHP" => push_reg(insn, Reg::P),
        "PLP" => pull_reg(insn, Reg::P),

        "PEA" => vec![IROp::PushEffectiveAddress {
            seg: SegRef { offset: insn.operand as i32, ..SegRef::new(SegKind::AbsBank) },
        }],
        "PER" => vec![IROp::PushEffectiveAddress {
            seg: SegRef { offset: insn.operand as i32, ..SegRef::new(SegKind::AbsBank) },
        }],
        "PEI" => vec![IROp::PushEffectiveAddress {
            seg: SegRef { offset: insn.operand as i32, ..SegRef::new(SegKind::DpIndirect) },
        }],
        "MVN" => vec![IROp::BlockMove {
            direction: MoveDir::Mvn,
            src_bank: ((insn.operand >> 8) & 0xFF) as u8,
            dst_bank: (insn.operand & 0xFF) as u8,
        }],
        "MVP" => vec![IROp::BlockMove {
            direction: MoveDir::Mvp,
            src_bank: ((insn.operand >> 8) & 0xFF) as u8,
            dst_bank: (insn.operand & 0xFF) as u8,
        }],

        "BPL" => cond_branch(insn, Reg::N, 0),
        "BMI" => cond_branch(insn, Reg::N, 1),
        "BVC" => cond_branch(insn, Reg::V, 0),
        "BVS" => cond_branch(insn, Reg::V, 1),
        "BCC" => cond_branch(insn, Reg::C, 0),
        "BCS" => cond_branch(insn, Reg::C, 1),
        "BNE" => cond_branch(insn, Reg::Zf, 0),
        "BEQ" => cond_branch(insn, Reg::Zf, 1),

        "BRA" => vec![IROp::Goto],
        "BRL" => vec![IROp::Goto],
        "JMP" => {
            if insn.mode == Indir || insn.mode == IndirX {
                vec![IROp::IndirectGoto { seg: segref_for(insn) }]
            } else {
                // LONG and ABS both -> Goto (cfg tracks the cross-bank successor).
                vec![IROp::Goto]
            }
        }
        "JSR" => {
            let em = insn.m_flag;
            let ex = insn.x_flag;
            if insn.mode == IndirX {
                vec![IROp::Call(Call {
                    target: None,
                    long: false,
                    indirect: true,
                    entry_m: em,
                    entry_x: ex,
                    source_pc24: Some(insn.addr & 0xFFFFFF),
                    table_base: Some(insn.operand & 0xFFFF),
                })]
            } else {
                let src_bank = (insn.addr >> 16) & 0xFF;
                let target = (src_bank << 16) | (insn.operand & 0xFFFF);
                vec![IROp::Call(Call {
                    target: Some(target),
                    long: false,
                    indirect: false,
                    entry_m: em,
                    entry_x: ex,
                    source_pc24: Some(insn.addr & 0xFFFFFF),
                    table_base: None,
                })]
            }
        }
        "JSL" => vec![IROp::Call(Call {
            target: Some(insn.operand),
            long: true,
            indirect: false,
            entry_m: insn.m_flag,
            entry_x: insn.x_flag,
            source_pc24: Some(insn.addr & 0xFFFFFF),
            table_base: None,
        })],
        "RTS" => vec![IROp::Return(Return {
            long: false,
            interrupt: false,
            source_pc24: Some(insn.addr & 0xFFFFFF),
        })],
        "RTL" => vec![IROp::Return(Return {
            long: true,
            interrupt: false,
            source_pc24: Some(insn.addr & 0xFFFFFF),
        })],
        "RTI" => vec![IROp::Return(Return {
            long: true,
            interrupt: true,
            source_pc24: Some(insn.addr & 0xFFFFFF),
        })],

        "NOP" => vec![IROp::Nop],
        "WDM" => vec![IROp::Nop],
        "BRK" => vec![IROp::Break { cop: false }],
        "COP" => vec![IROp::Break { cop: true }],
        "STP" => vec![IROp::Stop { wait: false }],
        "WAI" => vec![IROp::Stop { wait: true }],

        _ => vec![IROp::Nop],
    }
}

fn ld_reg(insn: &Insn, vf: &mut ValueFactory, reg: Reg, w: u8) -> Vec<IROp> {
    if insn.mode == Mode::Imm {
        let v = vf.next();
        vec![
            IROp::ConstI { value: insn.operand as i64, width: w, out: v },
            IROp::WriteReg { reg, src: v },
            IROp::SetNZ { src: v, width: w },
        ]
    } else {
        let seg = segref_for(insn);
        let v = vf.next();
        vec![
            IROp::Read { seg, width: w, out: v },
            IROp::WriteReg { reg, src: v },
            IROp::SetNZ { src: v, width: w },
        ]
    }
}

fn st_reg(insn: &Insn, vf: &mut ValueFactory, reg: Reg, w: u8) -> Vec<IROp> {
    let seg = segref_for(insn);
    let v = vf.next();
    vec![IROp::ReadReg { reg, out: v }, IROp::Write { seg, src: v, width: w }]
}

fn alu(insn: &Insn, vf: &mut ValueFactory, op: AluOp, lhs_reg: Reg, width: u8) -> Vec<IROp> {
    let mut ops: Vec<IROp> = Vec::new();
    let rhs;
    if insn.mode == Mode::Imm {
        rhs = vf.next();
        ops.push(IROp::ConstI { value: insn.operand as i64, width, out: rhs });
    } else {
        let seg = segref_for(insn);
        rhs = vf.next();
        ops.push(IROp::Read { seg, width, out: rhs });
    }
    let lhs = vf.next();
    let out = if op == AluOp::Cmp { None } else { Some(vf.next()) };
    ops.push(IROp::ReadReg { reg: lhs_reg, out: lhs });
    ops.push(IROp::Alu { op, lhs, rhs, width, out });
    if let Some(o) = out {
        ops.push(IROp::WriteReg { reg: lhs_reg, src: o });
    }
    ops
}

fn shift(insn: &Insn, vf: &mut ValueFactory, op: ShiftOp) -> Vec<IROp> {
    let width = width_a(insn);
    if insn.mode == Mode::Acc {
        let src = vf.next();
        let out = vf.next();
        vec![
            IROp::ReadReg { reg: Reg::A, out: src },
            IROp::Shift { op, src, width, out },
            IROp::WriteReg { reg: Reg::A, src: out },
        ]
    } else {
        let seg = segref_for(insn);
        let src = vf.next();
        let out = vf.next();
        vec![
            IROp::Read { seg, width, out: src },
            IROp::Shift { op, src, width, out },
            IROp::Write { seg, src: out, width },
        ]
    }
}

fn xfer(src: Reg, dst: Reg) -> Vec<IROp> {
    vec![IROp::Transfer { src, dst }]
}

fn flag_set(flag: Reg, value: u8) -> Vec<IROp> {
    vec![IROp::SetFlag { flag, value }]
}

fn push_reg(insn: &Insn, reg: Reg) -> Vec<IROp> {
    vec![IROp::PushReg { reg, static_m: Some(insn.m_flag), static_x: Some(insn.x_flag) }]
}

fn pull_reg(insn: &Insn, reg: Reg) -> Vec<IROp> {
    vec![IROp::PullReg { reg, static_m: Some(insn.m_flag), static_x: Some(insn.x_flag) }]
}

fn cond_branch(insn: &Insn, flag: Reg, take_if: u8) -> Vec<IROp> {
    if insn.const_z_fold_unconditional {
        vec![IROp::Goto]
    } else {
        vec![IROp::CondBranch { flag, take_if }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insn::decode_insn;

    fn dec(bytes: &[u8], m: u8, x: u8) -> Insn {
        let mut data = bytes.to_vec();
        data.resize(8, 0);
        let mut i = decode_insn(&data, 0, 0x8000, 0x00, m, x).unwrap();
        i.m_flag = m;
        i.x_flag = x;
        i
    }

    #[test]
    fn lda_imm_m1() {
        // A9 05 = LDA #$05, m=1 -> ConstI(5,w1), WriteReg(A), SetNZ
        let mut vf = ValueFactory::new();
        let ops = lower(&dec(&[0xA9, 0x05], 1, 1), &mut vf);
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], IROp::ConstI { value: 5, width: 1, .. }));
        assert!(matches!(ops[1], IROp::WriteReg { reg: Reg::A, .. }));
        assert!(matches!(ops[2], IROp::SetNZ { width: 1, .. }));
    }

    #[test]
    fn lda_abs_m0_width2() {
        // AD 34 12 = LDA $1234, m=0 -> Read(w2), WriteReg, SetNZ
        let mut vf = ValueFactory::new();
        let ops = lower(&dec(&[0xAD, 0x34, 0x12], 0, 1), &mut vf);
        assert!(matches!(ops[0], IROp::Read { width: 2, .. }));
    }

    #[test]
    fn sta_dp() {
        // 85 10 = STA $10 -> ReadReg(A), Write
        let mut vf = ValueFactory::new();
        let ops = lower(&dec(&[0x85, 0x10], 1, 1), &mut vf);
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], IROp::ReadReg { reg: Reg::A, .. }));
        assert!(matches!(ops[1], IROp::Write { width: 1, .. }));
    }

    #[test]
    fn cmp_has_no_out() {
        // C9 00 = CMP #$00 -> ConstI, ReadReg(A), Alu(Cmp, out=None)
        let mut vf = ValueFactory::new();
        let ops = lower(&dec(&[0xC9, 0x00], 1, 1), &mut vf);
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[2], IROp::Alu { op: AluOp::Cmp, out: None, .. }));
    }

    #[test]
    fn adc_has_out_and_writeback() {
        // 69 01 = ADC #$01 -> ConstI, ReadReg, Alu(Add, out=Some), WriteReg
        let mut vf = ValueFactory::new();
        let ops = lower(&dec(&[0x69, 0x01], 1, 1), &mut vf);
        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[2], IROp::Alu { op: AluOp::Add, out: Some(_), .. }));
        assert!(matches!(ops[3], IROp::WriteReg { reg: Reg::A, .. }));
    }

    #[test]
    fn rts_and_jsr() {
        let mut vf = ValueFactory::new();
        assert!(matches!(lower(&dec(&[0x60], 1, 1), &mut vf)[0], IROp::Return(_)));
        // 20 00 90 = JSR $9000 (same bank 0) -> Call target 0x009000
        let ops = lower(&dec(&[0x20, 0x00, 0x90], 1, 1), &mut vf);
        match &ops[0] {
            IROp::Call(c) => {
                assert_eq!(c.target, Some(0x009000));
                assert!(!c.long && !c.indirect);
            }
            _ => panic!("expected Call"),
        }
    }

    #[test]
    fn rep_sep_xce() {
        let mut vf = ValueFactory::new();
        assert!(matches!(lower(&dec(&[0xC2, 0x30], 1, 1), &mut vf)[0], IROp::RepFlags { mask: 0x30 }));
        assert!(matches!(lower(&dec(&[0xE2, 0x30], 1, 1), &mut vf)[0], IROp::SepFlags { mask: 0x30 }));
        assert!(matches!(lower(&dec(&[0xFB], 1, 1), &mut vf)[0], IROp::Xce));
    }

    #[test]
    fn value_factory_monotonic() {
        let mut vf = ValueFactory::new();
        assert_eq!(vf.next().vid, 1);
        assert_eq!(vf.next().vid, 2);
        assert_eq!(vf.next().vid, 3);
    }
}
