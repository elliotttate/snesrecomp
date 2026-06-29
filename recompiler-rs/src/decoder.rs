//! 65816 function decoder (port of `recompiler/v2/decoder.py`).
//!
//! Decodes a function starting at (bank, start) with an entry (m, x) state into
//! a `FunctionDecodeGraph` — a worklist walk over `DecodeKey`s where the same PC
//! reached under divergent (m, x) produces multiple keyed instances.
//!
//! Unlike the Python, decode-affecting inputs are bundled in one immutable
//! `DecodeEnv` threaded by `&` (no process-global registries). The dependency-
//! keyed decode cache is a Phase 5 concern; correctness lives in the uncached
//! walk.

use std::collections::HashMap;

use crate::insn::{Insn, Mode};
use crate::rom::RelocRegion;

pub const PHP_STACK_MAX_DEPTH: usize = 8;

/// Mnemonics with no fall-through successor.
pub fn is_terminator(mnem: &str) -> bool {
    matches!(mnem, "RTS" | "RTL" | "RTI" | "STP" | "WAI" | "BRK")
}

/// Conditional-branch mnemonics.
pub fn is_cond_branch(mnem: &str) -> bool {
    matches!(
        mnem,
        "BPL" | "BMI" | "BVC" | "BVS" | "BCC" | "BCS" | "BNE" | "BEQ"
    )
}

/// 24-bit address from (bank, pc).
#[inline]
pub fn addr24(bank: u32, pc: u32) -> u32 {
    ((bank & 0xFF) << 16) | (pc & 0xFFFF)
}

/// Identifies a decoded instruction by 24-bit address + entry M/X + PHP/PLP
/// stack history. Two keys are equal iff (pc, m, x, p_stack) all match.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DecodeKey {
    pub pc: u32, // 24-bit ((bank << 16) | local_pc)
    pub m: u8,
    pub x: u8,
    pub p_stack: Vec<(u8, u8)>, // PHP-pushed (m, x) LIFO
}

impl DecodeKey {
    pub fn new(pc: u32, m: u8, x: u8) -> Self {
        DecodeKey { pc, m, x, p_stack: Vec::new() }
    }
    pub fn with_stack(pc: u32, m: u8, x: u8, p_stack: Vec<(u8, u8)>) -> Self {
        DecodeKey { pc, m, x, p_stack }
    }
}

/// One instruction decoded at one specific (pc, m, x) entry state.
#[derive(Debug, Clone)]
pub struct DecodedInsn {
    pub key: DecodeKey,
    pub insn: Insn, // m_flag/x_flag set to entry m/x
    pub successors: Vec<DecodeKey>,
}

/// JSR (abs,X) site whose fall-through edge was severed (no cfg dispatch table).
#[derive(Debug, Clone)]
pub struct SuppressedIndirectCall {
    pub site_pc24: u32,
    pub table_base: u32,
    pub function_entry_pc24: u32,
    pub entry_m: u8,
    pub entry_x: u8,
}

/// Dispatch-table entry the decoder refused because the target lands in a
/// cfg `data_region`.
#[derive(Debug, Clone)]
pub struct DispatchTargetSuppressed {
    pub site_pc24: u32,
    pub target_pc24: u32,
    pub reason: String, // 'data_region' (extensible)
    pub table_index: u32,
}

/// Indirect JMP/JML/JSR whose static target list could not be recovered.
/// v2_regen hard-fails on any non-empty list (no-stub policy).
#[derive(Debug, Clone)]
pub struct UnresolvedIndirect {
    pub site_pc24: u32,
    pub mnem: String, // 'JMP' | 'JML' | 'JSR'
    pub mode: Mode,
    pub operand: u32,
    pub function_entry_pc24: u32,
    pub entry_m: u8,
    pub entry_x: u8,
}

/// BEQ/BNE rewritten to an unconditional Goto by the constant-Z fold pass.
#[derive(Debug, Clone)]
pub struct ConstZFold {
    pub branch_pc24: u32,
    pub prev_pc24: u32,
    pub branch_mnem: String,
    pub prev_mnem: String,
    pub prev_imm: u32,
    pub width_bits: u8,
    pub z_value: u8,
    pub taken_kind: String, // 'jump' | 'fall'
    pub live_pc24: u32,
    pub dead_pc24: u32,
    pub func_entry_pc24: u32,
    pub entry_m: u8,
    pub entry_x: u8,
}

/// Output of `decode_function` for one function entry. `insns` preserves
/// insertion order (the Python used an insertion-ordered dict); lookup is via
/// the `index` side-map.
#[derive(Debug, Clone, Default)]
pub struct FunctionDecodeGraph {
    pub entry: Option<DecodeKey>,
    insns_vec: Vec<DecodedInsn>,
    index: HashMap<DecodeKey, usize>,
    pub suppressed_indirect_calls: Vec<SuppressedIndirectCall>,
    pub const_z_folds: Vec<ConstZFold>,
    pub dispatch_targets_suppressed: Vec<DispatchTargetSuppressed>,
    pub unresolved_indirects: Vec<UnresolvedIndirect>,
}

impl FunctionDecodeGraph {
    pub fn new(entry: DecodeKey) -> Self {
        FunctionDecodeGraph { entry: Some(entry), ..Default::default() }
    }

    /// Insert or replace a decoded insn, preserving first-insertion order.
    pub fn insert(&mut self, di: DecodedInsn) {
        if let Some(&i) = self.index.get(&di.key) {
            self.insns_vec[i] = di;
        } else {
            let i = self.insns_vec.len();
            self.index.insert(di.key.clone(), i);
            self.insns_vec.push(di);
        }
    }

    pub fn get(&self, key: &DecodeKey) -> Option<&DecodedInsn> {
        self.index.get(key).map(|&i| &self.insns_vec[i])
    }

    pub fn contains(&self, key: &DecodeKey) -> bool {
        self.index.contains_key(key)
    }

    /// All decoded insns in insertion order.
    pub fn insns(&self) -> &[DecodedInsn] {
        &self.insns_vec
    }

    pub fn len(&self) -> usize {
        self.insns_vec.len()
    }

    pub fn is_empty(&self) -> bool {
        self.insns_vec.is_empty()
    }

    /// All DecodeKeys at this 24-bit PC (across entry mode states).
    pub fn keys_at_pc(&self, pc24: u32) -> Vec<DecodeKey> {
        self.insns_vec.iter().filter(|d| d.key.pc == pc24).map(|d| d.key.clone()).collect()
    }
}

/// Immutable bundle of decode-affecting inputs, threaded by `&` (replaces the
/// Python kwargs + process-global registries). Borrowed maps so the orchestrator
/// can build them once and share across all decodes.
#[derive(Debug, Clone, Default)]
pub struct DecodeEnv<'a> {
    pub dispatch_helpers: Option<&'a HashMap<u32, String>>, // target_pc24 -> 'short'|'long'
    pub indirect_call_tables: Option<&'a HashMap<u32, IndirectCallTable>>,
    pub indirect_dispatch: Option<&'a HashMap<u32, IndirectDispatchSite>>,
    pub hle_dispatch: Option<&'a HashMap<u32, String>>,
    pub data_regions: Option<&'a [(u32, u32, u32)]>, // (bank, start, end_excl)
    pub callee_exit_mx: Option<&'a HashMap<(u32, u8, u8), (u8, u8)>>,
    pub callee_exit_mx_modes: Option<&'a HashMap<(u32, u8, u8), Vec<(u8, u8)>>>,
    pub sibling_entry_pcs: Option<&'a std::collections::BTreeSet<u32>>,
    pub reloc_regions: Option<&'a [RelocRegion]>,
    pub callee_inline_skip: Option<&'a HashMap<u32, i32>>,
    pub inline_dispatch_loop_pcs: Option<&'a std::collections::BTreeSet<u32>>,
    /// Folded-in process-global the Python decode path read (set_global_inline_skip).
    pub global_inline_skip: Option<&'a HashMap<u32, i32>>,
}

/// `indirect_call_tables` value shape: {'base': int, 'count': int, 'kind': str}.
#[derive(Debug, Clone)]
pub struct IndirectCallTable {
    pub base: u32,
    pub count: u32,
    pub kind: String,
}

/// `indirect_dispatch` resolved-site value shape: count + idx_reg + table_bases.
#[derive(Debug, Clone)]
pub struct IndirectDispatchSite {
    pub count: u32,
    pub idx_reg: char,         // 'X' | 'Y'
    pub table_bases: Vec<u32>, // 0..3 entries
}

/// Compute (m, x, p_stack) AFTER executing `insn`, given entry state. REP/SEP
/// clear/set M/X per the operand bitmask; PHP pushes the current (m, x); PLP
/// pops and restores it. Other M/X-affecting ops keep the current state.
pub fn post_state(insn: &Insn, in_m: u8, in_x: u8, in_p_stack: &[(u8, u8)]) -> (u8, u8, Vec<(u8, u8)>) {
    let mnem = insn.mnem;
    match mnem {
        "REP" => {
            let m = if insn.operand & 0x20 != 0 { 0 } else { in_m };
            let x = if insn.operand & 0x10 != 0 { 0 } else { in_x };
            (m, x, in_p_stack.to_vec())
        }
        "SEP" => {
            let m = if insn.operand & 0x20 != 0 { 1 } else { in_m };
            let x = if insn.operand & 0x10 != 0 { 1 } else { in_x };
            (m, x, in_p_stack.to_vec())
        }
        "PHP" => {
            if in_p_stack.len() < PHP_STACK_MAX_DEPTH {
                let mut s = in_p_stack.to_vec();
                s.push((in_m, in_x));
                (in_m, in_x, s)
            } else {
                (in_m, in_x, in_p_stack.to_vec())
            }
        }
        "PLP" => {
            if let Some(&(pm, px)) = in_p_stack.last() {
                (pm, px, in_p_stack[..in_p_stack.len() - 1].to_vec())
            } else {
                (in_m, in_x, in_p_stack.to_vec())
            }
        }
        _ => (in_m, in_x, in_p_stack.to_vec()),
    }
}

/// Back-compat: (m, x) without p_stack tracking.
pub fn post_mx(insn: &Insn, in_m: u8, in_x: u8) -> (u8, u8) {
    let (m, x, _) = post_state(insn, in_m, in_x, &[]);
    (m, x)
}

// ── The heavy body: ported in the delegated Phase-2 sub-task, gated by the
// differential oracle (scripts/dump_decode.py). Signatures fixed here. ────────

/// Decode a function starting at (bank, start) with entry (m, x) state.
pub fn decode_function(
    _rom: &[u8],
    _bank: u32,
    _start: u32,
    _entry_m: u8,
    _entry_x: u8,
    _end: Option<u32>,
    _env: &DecodeEnv,
) -> FunctionDecodeGraph {
    todo!("Phase 2: port _decode_function_uncached body, gated by dump_decode oracle")
}

/// Exit (m, x) meet across a function's return paths; (None, None) if ambiguous.
pub fn analyze_function_exit_mx(
    _graph: &FunctionDecodeGraph,
    _callee_exit_mx: Option<&HashMap<(u32, u8, u8), (u8, u8)>>,
) -> (Option<u8>, Option<u8>) {
    todo!("Phase 2")
}

/// Concrete set of (m, x) states at exits, or None.
pub fn analyze_function_exit_mx_modes(
    _graph: &FunctionDecodeGraph,
    _callee_exit_mx: Option<&HashMap<(u32, u8, u8), (u8, u8)>>,
) -> Option<Vec<(u8, u8)>> {
    todo!("Phase 2")
}

/// Classify a JSL-jump-table dispatch helper: Some("short"|"long") or None.
pub fn classify_dispatch_helper(_rom: &[u8], _bank: u32, _addr: u32) -> Option<&'static str> {
    todo!("Phase 2")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insn::decode_insn;

    #[test]
    fn post_state_sep_rep() {
        // SEP #$30 sets both m and x.
        let data = [0xE2, 0x30];
        let i = decode_insn(&data, 0, 0x8000, 0, 0, 0).unwrap();
        let (m, x, _) = post_state(&i, 0, 0, &[]);
        assert_eq!((m, x), (1, 1));
        // REP #$20 clears m only.
        let data = [0xC2, 0x20];
        let i = decode_insn(&data, 0, 0x8000, 1, 1, 1).unwrap();
        let (m, x, _) = post_state(&i, 1, 1, &[]);
        assert_eq!((m, x), (0, 1));
    }

    #[test]
    fn php_plp_brackets_mx() {
        // PHP pushes (m,x); SEP changes them; PLP restores.
        let php = decode_insn(&[0x08], 0, 0x8000, 0, 1, 1).unwrap();
        let sep = decode_insn(&[0xE2, 0x30], 0, 0x8001, 0, 1, 1).unwrap();
        let plp = decode_insn(&[0x28], 0, 0x8003, 0, 1, 1).unwrap();
        let (m, x, s1) = post_state(&php, 0, 1, &[]);
        assert_eq!((m, x), (0, 1));
        assert_eq!(s1, vec![(0, 1)]);
        let (m, x, s2) = post_state(&sep, m, x, &s1);
        assert_eq!((m, x), (1, 1));
        assert_eq!(s2, vec![(0, 1)]); // SEP doesn't touch p_stack
        let (m, x, s3) = post_state(&plp, m, x, &s2);
        assert_eq!((m, x), (0, 1)); // restored
        assert!(s3.is_empty());
    }
}
