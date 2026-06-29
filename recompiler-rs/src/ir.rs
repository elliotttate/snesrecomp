//! Stateful IR for the v2 pipeline (port of `recompiler/v2/ir.py`).
//!
//! IR ops are typed transformations on a CpuState (A, B, X, Y, S, D, DB, PB, P,
//! M flag, X flag, emulation flag). Each 65816 instruction lowers to a list of
//! IR ops via `lowering`. Register values are `Value` handles produced by ops;
//! merges across joins are real phi nodes on the IR (later phase), not C-string
//! manipulation.

/// Symbolic 65816 CPU register / flag identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reg {
    A,
    B,
    X,
    Y,
    S,
    D,
    Db,
    Pb,
    P, // full P byte
    // Individual P-flag bits, exposed as their own slots so IR ops don't need
    // to mask P every time:
    M,  // accumulator/memory width: 1=8-bit, 0=16-bit
    Xf, // index width: 1=8-bit, 0=16-bit
    E,  // emulation flag (XCE)
    N,
    V,
    Zf, // 'Z' clashes with stylistic uses; use Zf for the flag
    C,
    I,  // interrupt-disable
    Df, // decimal mode
}

impl Reg {
    /// The Python `Reg` enum value string (e.g. `Reg.DB.value == 'DB'`), used
    /// anywhere the Python emitted the raw enum value into text or keys.
    pub fn as_str(self) -> &'static str {
        match self {
            Reg::A => "A",
            Reg::B => "B",
            Reg::X => "X",
            Reg::Y => "Y",
            Reg::S => "S",
            Reg::D => "D",
            Reg::Db => "DB",
            Reg::Pb => "PB",
            Reg::P => "P",
            Reg::M => "M",
            Reg::Xf => "XF",
            Reg::E => "E",
            Reg::N => "N",
            Reg::V => "V",
            Reg::Zf => "ZF",
            Reg::C => "C",
            Reg::I => "I",
            Reg::Df => "DF",
        }
    }
}

/// How an addressing mode resolves at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegKind {
    Direct,            // D + dp_offset (+X / +Y if indexed)
    AbsBank,           // DB << 16 | abs (+X / +Y if indexed)
    Long,              // bank << 16 | abs (+X if indexed)
    Stack,             // S + offset (or stack-relative-indirect-Y composed)
    DpIndirect,        // ((D + dp) word) (+Y if indirect-Y), data-bank
    DpIndirectLong,    // ((D + dp) long) (+Y)
    AbsIndirect,       // ((PB:abs)) — indirect JMP, PB-bank
    AbsIndirectLong,   // ((abs)) long indirect
    AbsIndirectX,      // ((PB:abs+X)) — indirect-X JMP/JSR
    DpIndirectX,       // ((D + dp + X)) data-bank
    StackRelIndirectY, // ((S + offs)) + Y, data-bank
}

impl SegKind {
    /// Python enum value string (e.g. `'dp_indirect'`).
    pub fn as_str(self) -> &'static str {
        match self {
            SegKind::Direct => "direct",
            SegKind::AbsBank => "abs_bank",
            SegKind::Long => "long",
            SegKind::Stack => "stack",
            SegKind::DpIndirect => "dp_indirect",
            SegKind::DpIndirectLong => "dp_indirect_long",
            SegKind::AbsIndirect => "abs_indirect",
            SegKind::AbsIndirectLong => "abs_indirect_long",
            SegKind::AbsIndirectX => "abs_indirect_x",
            SegKind::DpIndirectX => "dp_indirect_x",
            SegKind::StackRelIndirectY => "stack_rel_indirect_y",
        }
    }
}

/// Resolved memory reference for one IR Read/Write. Components depend on `kind`;
/// unused fields stay `None` / 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegRef {
    pub kind: SegKind,
    pub offset: i32,          // immediate component (dp byte, abs word, long 24-bit)
    pub bank: Option<u8>,     // for LONG kind (else taken from CpuState.DB/.PB at runtime)
    pub index: Option<Reg>,   // Reg::X or Reg::Y if indexed; else None
}

impl SegRef {
    pub fn new(kind: SegKind) -> Self {
        SegRef { kind, offset: 0, bank: None, index: None }
    }
}

/// Opaque handle for an IR-produced value. `vid` is unique within a function;
/// width (8/16) is intrinsic to the producing op, not stored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value {
    pub vid: u32,
}

/// ALU operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AluOp {
    Add, // with carry
    Sub, // with borrow
    And,
    Or,
    Xor,
    Cmp, // sub for flags only, no destination
}

/// Shift / rotate operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShiftOp {
    Asl,
    Lsr,
    Rol,
    Ror,
}

/// Block-move direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoveDir {
    Mvn, // incrementing
    Mvp, // decrementing
}

/// A single IR op. Closed set; mirrors the `IROp` subclass hierarchy in ir.py.
/// Variants that "produce a value" carry an `out: Value` field, matching the
/// Python ops' `out` attribute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IROp {
    // Memory access
    Read { seg: SegRef, width: u8, out: Value },
    Write { seg: SegRef, src: Value, width: u8 },

    // Register reads / writes (CpuState slots)
    ReadReg { reg: Reg, out: Value },
    WriteReg { reg: Reg, src: Value },

    // Constants
    ConstI { value: i64, width: u8, out: Value },

    // ALU (out is None for CMP)
    Alu { op: AluOp, lhs: Value, rhs: Value, width: u8, out: Option<Value> },

    // Shifts / rotates (updates C implicitly per kind)
    Shift { op: ShiftOp, src: Value, width: u8, out: Value },

    // Increment / decrement of a register (+1 / -1)
    IncReg { reg: Reg, delta: i8 },

    // Increment / decrement of memory (no carry-in; sets Z/N; leaves C/V)
    IncMem { seg: SegRef, width: u8, delta: i8 },

    // BIT — sets N V Z based on operand & A
    BitTest { operand: Value, width: u8 },
    // TSB — set bits in memory
    BitSetMem { seg: SegRef, width: u8 },
    // TRB — clear bits in memory
    BitClearMem { seg: SegRef, width: u8 },

    // Flag / mode ops
    SetFlag { flag: Reg, value: u8 },
    // Update N/Z from a value's bits
    SetNZ { src: Value, width: u8 },
    // REP #imm — clear masked bits
    RepFlags { mask: u8 },
    // SEP #imm — set masked bits
    SepFlags { mask: u8 },
    // Exchange C and emulation flag
    Xce,

    // Stack ops
    Push { src: Value, width: u8 },
    Pull { width: u8, out: Value },
    // PHA/PHX/PHY/PHB/PHD/PHK/PHP — static_m/static_x pin width when known
    PushReg { reg: Reg, static_m: Option<u8>, static_x: Option<u8> },
    // PLA/PLX/PLY/PLB/PLD/PLP
    PullReg { reg: Reg, static_m: Option<u8>, static_x: Option<u8> },

    // MVN / MVP
    BlockMove { direction: MoveDir, src_bank: u8, dst_bank: u8 },

    // Control flow
    // Conditional branch on a P-flag bit (flag ∈ N/V/ZF/C; take_if 0 or 1)
    CondBranch { flag: Reg, take_if: u8 },
    // Unconditional transfer (BRA/BRL/JMP ABS/fall-through); target via CFG edge
    Goto,
    // Indirect JMP — successors from cfg or stub
    IndirectGoto { seg: SegRef },
    // JSR ABS / JSR (abs,X) / JSL
    Call(Call),
    // RTS / RTL / RTI
    Return(Return),

    // Misc
    Transfer { src: Reg, dst: Reg },
    Xba,
    Nop,
    Break { cop: bool },
    Stop { wait: bool },
    // PEA #abs / PER label / PEI (dp) — pushes a 16-bit value
    PushEffectiveAddress { seg: SegRef },
}

/// JSR ABS / JSR (abs,X) / JSL. `target` is the resolved 24-bit address for ABS
/// and LONG; `None` for indirect-X dispatch where the cfg supplies the table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Call {
    pub target: Option<u32>,
    pub long: bool,          // True for JSL (24-bit return), False for JSR
    pub indirect: bool,      // True for JSR (abs,X)
    pub entry_m: u8,         // (m, x) the callee enters with — drives per-variant body select
    pub entry_x: u8,
    pub source_pc24: Option<u32>, // JSR (abs,X) only
    pub table_base: Option<u32>,  // JSR (abs,X) only
}

impl Default for Call {
    fn default() -> Self {
        Call {
            target: None,
            long: false,
            indirect: false,
            entry_m: 1,
            entry_x: 1,
            source_pc24: None,
            table_base: None,
        }
    }
}

/// RTS / RTL / RTI. `source_pc24` is the 24-bit address of the source asm
/// instruction (drives the PEI-trampoline stack-delta analysis); `None` when
/// synthesized by an internal pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Return {
    pub long: bool,          // True for RTL/RTI, False for RTS
    pub interrupt: bool,     // True for RTI
    pub source_pc24: Option<u32>,
}

/// An ordered sequence of IR ops corresponding to one V2Block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IRBlock {
    pub ops: Vec<IROp>,
}
