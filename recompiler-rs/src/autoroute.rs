//! Port of the cfg-mutating "autoroute" analysis passes from
//! `recompiler/v2/{wrapper,tail_call,pha_rts,exit_mx}_autoroute.py`, plus the
//! JSL dispatch-helper discovery that precedes the exit-(m, x) pass in
//! `tools/v2_regen.py`.
//!
//! Each pass takes the parsed bank cfgs and MUTATES entries in place (adds
//! tail-call targets / synthetic `indirect_dispatch` entries / per-variant
//! exit-(m, x) records) and/or rewrites the cross-bank name map; the wrapper
//! pass mutates `name_map` + `cross_bank_names` rather than the cfgs.
//!
//! Mutation semantics mirror the Python exactly. The differential oracle
//! (`scripts/dump_autoroute.py` ↔ `dump-autoroute` bin ↔ `scripts/diff_autoroute.py`)
//! validates functional equivalence against the live Python passes.
//!
//! Determinism: where the Python iterates `set`s, this uses `BTree*` so output
//! ordering is stable regardless of input order.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::cfg::{BankCfg, IndirectDispatch, NameDecl};
use crate::decoder::{
    analyze_function_exit_mx, classify_dispatch_helper, decode_function, DecodeCache, DecodeEnv,
    FunctionDecodeGraph,
};
use crate::rom::{lorom_offset, RelocRegion};

/// Python truthiness on `entry.name` — a present, non-empty name. The cfg parser
/// never produces an empty name, but a programmatic caller could, and the Python
/// passes test `if entry.name:` (skipping both `None` and `""`).
fn name_is_truthy(name: &Option<String>) -> bool {
    name.as_deref().map_or(false, |s| !s.is_empty())
}

// =============================================================================
// wrapper_autoroute.py
// =============================================================================

/// One detected SMW DB-transition wrapper-bypass that was auto-routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperFix {
    pub bank: u32,
    pub wrapper_pc16: u32,
    pub body_pc16: u32,
    pub orig_name: String,
    pub synthetic_name: String,
}

const WRAP_SIG_HEAD: [u8; 4] = [0x8B, 0x4B, 0xAB, 0x20]; // PHB PHK PLB JSR
const WRAP_SIG_TAIL: [u8; 2] = [0xAB, 0x6B]; // PLB RTL
const WRAP_SIG_LEN: usize = 8;

/// Scan one bank's ROM ($8000-$FFFF) for the wrapper signature. Returns
/// `(wrapper_pc16, body_pc16)` pairs in the bank-local 16-bit space.
fn scan_bank_for_wrappers(rom: &[u8], bank: u32) -> Vec<(u32, u32)> {
    let bank_start = lorom_offset(bank, 0x8000);
    let bank_end = bank_start + 0x8000;
    if bank_end > rom.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let end_off = bank_end - WRAP_SIG_LEN;
    let mut off = bank_start;
    while off <= end_off {
        if rom[off..off + 4] == WRAP_SIG_HEAD && rom[off + 6..off + 8] == WRAP_SIG_TAIL {
            let wrapper_pc16 = 0x8000 + (off - bank_start) as u32;
            let body_pc16 = rom[off + 4] as u32 | ((rom[off + 5] as u32) << 8);
            if (0x8000..=0xFFFF).contains(&body_pc16) && body_pc16 != wrapper_pc16 {
                hits.push((wrapper_pc16, body_pc16));
            }
            off += WRAP_SIG_LEN; // disjoint hits only
        } else {
            off += 1;
        }
    }
    hits
}

fn build_synthetic_name(orig: &str, bank: u32, wrapper_pc16: u32) -> String {
    format!("_AutoWrap_{orig}__{bank:02X}_{wrapper_pc16:04X}")
}

/// Detect SMW PHB/PHK/PLB/JSR/PLB/RTL wrapper-bypass cfg aliases and rewrite
/// them in place. Mutates `name_map` and the `name` field of `NameDecl` records
/// in `cross_bank_names`. Returns the list of applied fixes.
pub fn wrapper_detect_and_route(
    parsed: &[BankCfg],
    name_map: &mut HashMap<u32, String>,
    cross_bank_names: &mut HashMap<u32, Vec<NameDecl>>,
    rom: &[u8],
) -> Vec<WrapperFix> {
    // name -> canonical declared func PC (first-seen wins).
    let mut name_to_func_pc: HashMap<String, u32> = HashMap::new();
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for entry in &cfg.entries {
            if name_is_truthy(&entry.name) {
                let name = entry.name.clone().unwrap();
                let addr_24 = (bank << 16) | (entry.start & 0xFFFF);
                name_to_func_pc.entry(name).or_insert(addr_24);
            }
        }
    }

    let mut fixes = Vec::new();
    let mut seen_wrapper_addrs: HashSet<u32> = HashSet::new();

    for cfg in parsed {
        let bank = cfg.bank as u32;
        for (wrapper_pc16, body_pc16) in scan_bank_for_wrappers(rom, bank) {
            let wrapper_addr = (bank << 16) | wrapper_pc16;
            if seen_wrapper_addrs.contains(&wrapper_addr) {
                continue;
            }
            let wrapper_name = match name_map.get(&wrapper_addr) {
                Some(n) => n.clone(),
                None => continue,
            };
            let declared_pc = match name_to_func_pc.get(&wrapper_name) {
                Some(&p) => p,
                None => continue,
            };
            if declared_pc == wrapper_addr {
                continue; // wrapper has its own declaration — fine
            }
            seen_wrapper_addrs.insert(wrapper_addr);
            let synth = build_synthetic_name(&wrapper_name, bank, wrapper_pc16);
            name_map.insert(wrapper_addr, synth.clone());
            if let Some(list) = cross_bank_names.get_mut(&bank) {
                for nd in list.iter_mut() {
                    if (nd.addr_24 & 0xFFFFFF) == wrapper_addr && nd.name == wrapper_name {
                        nd.name = synth.clone();
                    }
                }
            }
            fixes.push(WrapperFix {
                bank,
                wrapper_pc16,
                body_pc16,
                orig_name: wrapper_name,
                synthetic_name: synth,
            });
        }
    }
    fixes
}

// =============================================================================
// tail_call_autoroute.py
// =============================================================================

/// One detected tail-call fallthrough that was auto-routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailCallFix {
    pub bank: u32,
    pub src_pc16: u32,
    pub src_name: String,
    pub dst_pc16: u32,
    pub dst_name: String,
    pub last_insn_pc16: u32,
    pub last_insn_mnem: String,
}

const TAIL_TERMINAL_MNEMS: &[&str] = &["RTS", "RTL", "RTI", "JMP", "JML", "BRA", "BRL"];

/// Find the decoded insn with the highest `insn.addr`. Matches Python
/// `max(values(), key=...)` which returns the FIRST maximum in iteration order,
/// so we only replace on a strictly-greater address.
fn last_insn_by_addr(graph: &FunctionDecodeGraph) -> Option<usize> {
    let insns = graph.insns();
    if insns.is_empty() {
        return None;
    }
    let mut best = 0usize;
    for (i, di) in insns.iter().enumerate().skip(1) {
        if di.insn.addr > insns[best].insn.addr {
            best = i;
        }
    }
    Some(best)
}

/// Detect cfg `func A end:<pc>` entries where `<pc>` is the start of another
/// `func B` AND A's last decoded instruction falls through to exactly `<pc>`.
/// Sets `A.tail_call_pc16` in place. Returns applied fixes.
pub fn tail_call_detect_and_route(
    parsed: &mut [BankCfg],
    rom: &[u8],
    env: &DecodeEnv,
) -> Vec<TailCallFix> {
    let mut fixes = Vec::new();

    for cfg in parsed.iter_mut() {
        let bank = cfg.bank as u32;
        // start_pc16 -> entry index. Python dict comprehension: LAST wins.
        let mut by_start: HashMap<u32, usize> = HashMap::new();
        for (i, e) in cfg.entries.iter().enumerate() {
            if name_is_truthy(&e.name) {
                by_start.insert(e.start & 0xFFFF, i);
            }
        }

        for i in 0..cfg.entries.len() {
            let (start, entry_m, entry_x, end, has_name, tc_set) = {
                let e = &cfg.entries[i];
                (
                    e.start & 0xFFFF,
                    e.entry_m,
                    e.entry_x,
                    e.end,
                    name_is_truthy(&e.name),
                    e.tail_call_pc16.is_some(),
                )
            };
            if !has_name || tc_set {
                continue;
            }
            let end_val = match end {
                Some(v) => v,
                None => continue,
            };
            let end_pc = end_val & 0xFFFF;
            let sibling_idx = match by_start.get(&end_pc) {
                Some(&j) => j,
                None => continue,
            };
            if sibling_idx == i {
                continue;
            }

            let graph = match catch_unwind(AssertUnwindSafe(|| {
                decode_function(rom, bank, start, entry_m, entry_x, end, env)
            })) {
                Ok(g) => g,
                Err(_) => continue,
            };
            if graph.is_empty() {
                continue;
            }
            let last_idx = match last_insn_by_addr(&graph) {
                Some(v) => v,
                None => continue,
            };
            let last_insn = &graph.insns()[last_idx].insn;
            let last_pc16 = last_insn.addr & 0xFFFF;
            if last_pc16 + last_insn.length as u32 != end_pc {
                continue;
            }
            if TAIL_TERMINAL_MNEMS.contains(&last_insn.mnem) {
                continue;
            }

            let src_name = cfg.entries[i].name.clone().unwrap();
            let dst_name = cfg.entries[sibling_idx]
                .name
                .clone()
                .unwrap_or_else(|| format!("_anon_{end_pc:04X}"));
            let last_mnem = last_insn.mnem.to_string();

            cfg.entries[i].tail_call_pc16 = Some(end_pc);
            fixes.push(TailCallFix {
                bank,
                src_pc16: start,
                src_name,
                dst_pc16: end_pc,
                dst_name,
                last_insn_pc16: last_pc16,
                last_insn_mnem: last_mnem,
            });
        }
    }
    fixes
}

// =============================================================================
// pha_rts_autoroute.py
// =============================================================================

/// One detected PHA-RTS dispatch site that was auto-routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaRtsFix {
    pub bank: u32,
    pub pha_pc16: u32,
    pub table_pc16: u32,
    pub count: u32,
    pub enclosing_func: Option<String>,
}

const PHA_MAX_TABLE_ENTRIES: u32 = 256;

/// LoROM PC -> linear ROM offset (no $8000-range assertion, mirrors
/// `_rom_offset_lorom`). Accepts any 16-bit address.
fn pha_rom_offset(bank: u32, addr16: u32) -> usize {
    (((bank & 0x7F) << 15) | (addr16 & 0x7FFF)) as usize
}

fn infer_table_count(rom: &[u8], bank: u32, table_pc16: u32, bank_func_starts: &[u32]) -> u32 {
    let mut next_label = 0x10000u32;
    for &s in bank_func_starts {
        if s > table_pc16 && s < next_label {
            next_label = s;
        }
    }
    let mut count = 0u32;
    let mut cur = table_pc16;
    while count < PHA_MAX_TABLE_ENTRIES {
        if cur + 2 > next_label {
            break;
        }
        let off = pha_rom_offset(bank, cur);
        if off + 2 > rom.len() {
            break;
        }
        let lo = rom[off] as u32;
        let hi = rom[off + 1] as u32;
        let entry = lo | (hi << 8);
        if entry != 0 {
            if hi < 0x80 {
                break;
            }
            if table_pc16 <= entry && entry < table_pc16 + (count + 1) * 2 {
                break;
            }
        }
        count += 1;
        cur += 2;
    }
    count
}

/// Scan ROM byte range `[func_start, func_end)` for the 8-byte PHA-RTS pattern.
/// Yields `(pha_pc16, table_pc16, count)` per match.
fn scan_function_for_pha_rts(
    rom: &[u8],
    bank: u32,
    func_start: u32,
    func_end: u32,
    bank_func_starts: &[u32],
) -> Vec<(u32, u32, u32)> {
    let mut out = Vec::new();
    if func_end <= func_start {
        return out;
    }
    let start_off = pha_rom_offset(bank, func_start);
    let mut end_off = pha_rom_offset(bank, func_start.max(func_end - 1)) + 1;
    if end_off > rom.len() {
        end_off = rom.len();
    }
    if start_off >= end_off {
        return out;
    }
    // span = end_off - start_off - 7; range(span). Empty if span <= 0.
    if end_off < start_off + 8 {
        return out;
    }
    let span = end_off - start_off - 7;
    for i in 0..span {
        let off = start_off + i;
        if rom[off] == 0xB9
            && rom[off + 3] == 0x3A
            && rom[off + 4] == 0x48
            && rom[off + 5] == 0xE2
            && rom[off + 6] == 0x30
            && rom[off + 7] == 0x60
        {
            let table_pc16 = rom[off + 1] as u32 | ((rom[off + 2] as u32) << 8);
            let pha_pc16 = (func_start + i as u32 + 4) & 0xFFFF;
            let count = infer_table_count(rom, bank, table_pc16, bank_func_starts);
            if count == 0 {
                continue;
            }
            out.push((pha_pc16, table_pc16, count));
        }
    }
    out
}

/// Scan every parsed bank cfg for PHA-RTS dispatch sites and synthesise
/// `indirect_dispatch` cfg entries for those not already authorised. Mutates
/// `cfg.indirect_dispatch` in place. Returns the list of detected fixes.
pub fn pha_rts_detect_and_route(parsed: &mut [BankCfg], rom: &[u8]) -> Vec<PhaRtsFix> {
    let mut fixes = Vec::new();
    for cfg in parsed.iter_mut() {
        let bank = cfg.bank as u32;
        let mut existing_pcs: HashSet<u32> =
            cfg.indirect_dispatch.iter().map(|d| d.site_pc16 & 0xFFFF).collect();
        let mut bank_func_starts: Vec<u32> =
            cfg.entries.iter().map(|e| e.start & 0xFFFF).collect();
        bank_func_starts.sort();

        // Snapshot per-entry (start, end, name) so the indirect_dispatch push
        // below doesn't alias the entries borrow.
        let entries_info: Vec<(u32, Option<u32>, Option<String>)> = cfg
            .entries
            .iter()
            .map(|e| (e.start & 0xFFFF, e.end, e.name.clone()))
            .collect();

        for (func_start, end_opt, ename) in entries_info {
            let func_end = match end_opt {
                Some(e) => e & 0xFFFF,
                None => {
                    let mut next_start = 0x10000u32;
                    for &s in &bank_func_starts {
                        if s > func_start && s < next_start {
                            next_start = s;
                        }
                    }
                    next_start
                }
            };
            for (pha_pc16, table_pc16, count) in
                scan_function_for_pha_rts(rom, bank, func_start, func_end, &bank_func_starts)
            {
                if existing_pcs.contains(&pha_pc16) {
                    continue;
                }
                cfg.indirect_dispatch.push(IndirectDispatch {
                    site_pc16: pha_pc16,
                    count,
                    idx_reg: 'Y',
                    table_bases: vec![table_pc16],
                });
                existing_pcs.insert(pha_pc16);
                fixes.push(PhaRtsFix {
                    bank,
                    pha_pc16,
                    table_pc16,
                    count,
                    enclosing_func: ename.clone(),
                });
            }
        }
    }
    fixes
}

// =============================================================================
// dispatch-helper discovery (tools/v2_regen.py)
// =============================================================================

/// Decode every cfg entry, collect JSL / JML (JMP-long) targets, and classify
/// each as a 'short'/'long' dispatch helper. Mirrors the `classify_dispatch_helper`
/// loop in `tools/v2_regen.py`. Returns `pc24 -> 'short'|'long'`.
pub fn discover_dispatch_helpers(
    parsed: &[BankCfg],
    rom: &[u8],
    env: &DecodeEnv,
) -> BTreeMap<u32, String> {
    let mut jsl_targets: BTreeSet<u32> = BTreeSet::new();
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for entry in &cfg.entries {
            let graph = match catch_unwind(AssertUnwindSafe(|| {
                decode_function(rom, bank, entry.start, entry.entry_m, entry.entry_x, entry.end, env)
            })) {
                Ok(g) => g,
                Err(_) => continue,
            };
            for di in graph.insns() {
                let ins = &di.insn;
                if ins.mnem == "JSL" {
                    jsl_targets.insert(ins.operand & 0xFFFFFF);
                } else if ins.mnem == "JMP" && ins.length == 4 {
                    jsl_targets.insert(ins.operand & 0xFFFFFF);
                }
            }
        }
    }
    let mut helpers: BTreeMap<u32, String> = BTreeMap::new();
    for tgt in jsl_targets {
        let tbank = (tgt >> 16) & 0xFF;
        let taddr = tgt & 0xFFFF;
        if let Some(kind) = classify_dispatch_helper(rom, tbank, taddr) {
            helpers.insert(tgt, kind.to_string());
        }
    }
    helpers
}

// =============================================================================
// exit_mx_autoroute.py
// =============================================================================

/// One function whose exit (M, X) state at some entry variant differs from
/// entry (the "mutating" set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitMxFix {
    pub bank: u32,
    pub addr16: u32,
    pub fn_name: String,
    pub entry_m: u8,
    pub entry_x: u8,
    pub exit_m: u8,
    pub exit_x: u8,
}

const MX_COMBOS: [(u8, u8); 4] = [(0, 0), (0, 1), (1, 0), (1, 1)];
const EXIT_MX_MAX_ITERS: usize = 12;

/// Decode one variant and return its unambiguous exit (m, x), or None.
#[allow(clippy::too_many_arguments)]
fn decode_variant_exit(
    rom: &[u8],
    bank: u32,
    addr16: u32,
    em: u8,
    ex: u8,
    end: Option<u32>,
    env: &DecodeEnv,
    cache: Option<&DecodeCache>,
) -> Option<(u8, u8)> {
    let graph = match catch_unwind(AssertUnwindSafe(|| match cache {
        Some(c) => c.get_or_decode(rom, bank, addr16, em, ex, end, env),
        None => std::sync::Arc::new(decode_function(rom, bank, addr16, em, ex, end, env)),
    })) {
        Ok(g) => g,
        Err(_) => return None,
    };
    if graph.is_empty() {
        return None;
    }
    let (exit_m, exit_x) = analyze_function_exit_mx(&graph, env.callee_exit_mx);
    match (exit_m, exit_x) {
        (Some(m), Some(x)) => Some((m & 1, x & 1)),
        _ => None,
    }
}

struct ExitEntry {
    cfg_idx: usize,
    bank: u32,
    addr16: u32,
    end: Option<u32>,
    target_pc24: u32,
    name: String,
}

/// Auto-detect per-variant function exit-(M, X) state via an iterative fixpoint.
/// Appends mutating records to each `cfg.exit_mx_at_per_variant`. Returns fixes.
///
/// `reloc_regions` and `dispatch_helpers` mirror the process-global state
/// installed by v2_regen before this pass runs; the per-decode `callee_inline_skip`
/// is built internally (same as the Python builder).
pub fn exit_mx_detect_and_route(
    parsed: &mut [BankCfg],
    rom: &[u8],
    dispatch_helpers: &HashMap<u32, String>,
    reloc_regions: &[RelocRegion],
    cache: Option<&DecodeCache>,
) -> Vec<ExitMxFix> {
    let mut fixes = Vec::new();

    // Seed callee_exit_mx + seeded_keys from hand-written exit_mx_at directives.
    let mut callee_exit_mx: HashMap<(u32, u8, u8), (u8, u8)> = HashMap::new();
    let mut seeded_keys: HashSet<(u32, u8, u8)> = HashSet::new();
    for cfg in parsed.iter() {
        for &(b_id, addr16, m_val, x_val) in &cfg.exit_mx_at {
            let target = ((b_id as u32 & 0xFF) << 16) | (addr16 & 0xFFFF);
            for &(em, ex) in &MX_COMBOS {
                let key = (target, em, ex);
                callee_exit_mx.insert(key, (m_val & 1, x_val & 1));
                seeded_keys.insert(key);
            }
        }
    }

    // JSR-inline-param skip map (target_pc24 -> N), `if n:` semantics.
    let mut callee_inline_skip: HashMap<u32, i32> = HashMap::new();
    for cfg in parsed.iter() {
        let bank = cfg.bank as u32;
        for entry in &cfg.entries {
            if let Some(n) = entry.inline_skip {
                if n != 0 {
                    callee_inline_skip.insert((bank << 16) | (entry.start & 0xFFFF), n);
                }
            }
        }
    }

    // Flatten named entries (load order) for the fixpoint + emit walks.
    let mut all_entries: Vec<ExitEntry> = Vec::new();
    for (cfg_idx, cfg) in parsed.iter().enumerate() {
        let bank = cfg.bank as u32;
        for entry in &cfg.entries {
            if name_is_truthy(&entry.name) {
                let name = entry.name.clone().unwrap();
                let addr16 = entry.start & 0xFFFF;
                all_entries.push(ExitEntry {
                    cfg_idx,
                    bank,
                    addr16,
                    end: entry.end,
                    target_pc24: (bank << 16) | addr16,
                    name: name.clone(),
                });
            }
        }
    }

    // Iterative fixpoint with per-pass re-derivation.
    for _iter in 0..EXIT_MX_MAX_ITERS {
        let mut dirty = false;
        for e in &all_entries {
            for &(em, ex) in &MX_COMBOS {
                let key = (e.target_pc24, em, ex);
                if seeded_keys.contains(&key) {
                    continue;
                }
                let exit_pair = {
                    let env = DecodeEnv {
                        reloc_regions: Some(reloc_regions),
                        callee_inline_skip: Some(&callee_inline_skip),
                        dispatch_helpers: Some(dispatch_helpers),
                        callee_exit_mx: Some(&callee_exit_mx),
                        ..Default::default()
                    };
                    decode_variant_exit(rom, e.bank, e.addr16, em, ex, e.end, &env, cache)
                };
                match exit_pair {
                    None => {
                        if callee_exit_mx.remove(&key).is_some() {
                            dirty = true;
                        }
                    }
                    Some(p) => {
                        if callee_exit_mx.get(&key) != Some(&p) {
                            callee_exit_mx.insert(key, p);
                            dirty = true;
                        }
                    }
                }
            }
        }
        if !dirty {
            break;
        }
    }

    // Emit per-variant records for mutating (exit != entry) variants.
    for e in &all_entries {
        for &(em, ex) in &MX_COMBOS {
            let key = (e.target_pc24, em, ex);
            if seeded_keys.contains(&key) {
                continue;
            }
            if let Some(&(exit_m, exit_x)) = callee_exit_mx.get(&key) {
                if exit_m != em || exit_x != ex {
                    parsed[e.cfg_idx]
                        .exit_mx_at_per_variant
                        .push((e.bank as u8, e.addr16, em, ex, exit_m, exit_x));
                    fixes.push(ExitMxFix {
                        bank: e.bank,
                        addr16: e.addr16,
                        fn_name: e.name.clone(),
                        entry_m: em,
                        entry_x: ex,
                        exit_m,
                        exit_x,
                    });
                }
            }
        }
    }

    fixes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfg::{parse_bank_cfg, BankEntry};

    /// One zeroed LoROM bank ($8000 bytes).
    fn one_bank_rom() -> Vec<u8> {
        vec![0u8; 0x8000]
    }

    #[test]
    fn wrapper_routes_bypass_alias() {
        let mut rom = one_bank_rom();
        // Wrapper at bank0 $8000 (rom offset 0), body operand $8010.
        rom[0..8].copy_from_slice(&[0x8B, 0x4B, 0xAB, 0x20, 0x10, 0x80, 0xAB, 0x6B]);

        // cfg declares Foo at the BODY ($8010), so the wrapper alias bypasses it.
        let cfg = parse_bank_cfg("bank = 00\nfunc Foo 8010\n", "t").unwrap();
        let parsed = vec![cfg];

        let mut name_map: HashMap<u32, String> = HashMap::new();
        name_map.insert(0x00_8000, "Foo".to_string()); // cross-bank alias at wrapper
        name_map.insert(0x00_8010, "Foo".to_string()); // body declaration
        let mut cross: HashMap<u32, Vec<NameDecl>> = HashMap::new();
        cross.insert(
            0x00,
            vec![NameDecl { addr_24: 0x00_8000, name: "Foo".to_string() }],
        );

        let fixes = wrapper_detect_and_route(&parsed, &mut name_map, &mut cross, &rom);
        assert_eq!(fixes.len(), 1);
        let synth = "_AutoWrap_Foo__00_8000";
        assert_eq!(name_map.get(&0x00_8000).map(String::as_str), Some(synth));
        assert_eq!(cross[&0x00][0].name, synth);
    }

    #[test]
    fn wrapper_no_fix_when_wrapper_self_declared() {
        let mut rom = one_bank_rom();
        rom[0..8].copy_from_slice(&[0x8B, 0x4B, 0xAB, 0x20, 0x10, 0x80, 0xAB, 0x6B]);
        // Foo declared AT the wrapper PC ($8000) → declared_pc == wrapper_addr.
        let parsed = vec![parse_bank_cfg("bank = 00\nfunc Foo 8000\n", "t").unwrap()];
        let mut name_map: HashMap<u32, String> = HashMap::new();
        name_map.insert(0x00_8000, "Foo".to_string());
        let mut cross: HashMap<u32, Vec<NameDecl>> = HashMap::new();
        let fixes = wrapper_detect_and_route(&parsed, &mut name_map, &mut cross, &rom);
        assert!(fixes.is_empty());
        assert_eq!(name_map.get(&0x00_8000).map(String::as_str), Some("Foo"));
    }

    #[test]
    fn tail_call_routes_fallthrough() {
        let mut rom = one_bank_rom();
        // $8000: NOP NOP ; falls through to sibling at $8002.
        rom[0] = 0xEA;
        rom[1] = 0xEA;
        let mut cfg = parse_bank_cfg(
            "bank = 00\nfunc A 8000 end:8002\nfunc B 8002\n",
            "t",
        )
        .unwrap();
        // Sanity: A has no tail_call yet.
        assert!(cfg.entries[0].tail_call_pc16.is_none());
        let mut parsed = vec![std::mem::take(&mut cfg)];
        let env = DecodeEnv::default();
        let fixes = tail_call_detect_and_route(&mut parsed, &rom, &env);
        assert_eq!(fixes.len(), 1);
        assert_eq!(parsed[0].entries[0].tail_call_pc16, Some(0x8002));
        assert_eq!(fixes[0].last_insn_mnem, "NOP");
    }

    #[test]
    fn tail_call_skips_terminal_last_insn() {
        let mut rom = one_bank_rom();
        // $8000: NOP RTS ; last insn RTS is terminal → no tail call.
        rom[0] = 0xEA;
        rom[1] = 0x60;
        let mut parsed = vec![parse_bank_cfg(
            "bank = 00\nfunc A 8000 end:8002\nfunc B 8002\n",
            "t",
        )
        .unwrap()];
        let env = DecodeEnv::default();
        let fixes = tail_call_detect_and_route(&mut parsed, &rom, &env);
        assert!(fixes.is_empty());
        assert!(parsed[0].entries[0].tail_call_pc16.is_none());
    }

    #[test]
    fn pha_rts_synthesises_dispatch() {
        let mut rom = one_bank_rom();
        // Function $8000..$8020. PHA-RTS pattern at offset 0:
        //   B9 10 80  (LDA $8010,Y)  3A  48  E2 30  60
        rom[0..8].copy_from_slice(&[0xB9, 0x10, 0x80, 0x3A, 0x48, 0xE2, 0x30, 0x60]);
        // Dispatch table at $8010: two valid pointers (hi >= $80), then a $00 stop
        // is allowed but next_label clamps. Put one valid then a low-byte-bad entry.
        // $8010: ptr $9000 (00 90), $8012: ptr $9100 (00 91), $8014: $0040 (hi<$80) stop.
        let t = pha_rom_offset(0, 0x8010);
        rom[t] = 0x00;
        rom[t + 1] = 0x90;
        rom[t + 2] = 0x00;
        rom[t + 3] = 0x91;
        rom[t + 4] = 0x40;
        rom[t + 5] = 0x00;
        let mut parsed = vec![parse_bank_cfg("bank = 00\nfunc Disp 8000 end:8020\n", "t").unwrap()];
        let fixes = pha_rts_detect_and_route(&mut parsed, &rom);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].pha_pc16, 0x8004); // func_start + 0 + 4
        assert_eq!(fixes[0].table_pc16, 0x8010);
        assert_eq!(fixes[0].count, 2);
        assert_eq!(parsed[0].indirect_dispatch.len(), 1);
        let d = &parsed[0].indirect_dispatch[0];
        assert_eq!(d.site_pc16, 0x8004);
        assert_eq!(d.idx_reg, 'Y');
        assert_eq!(d.table_bases, vec![0x8010]);
    }

    #[test]
    fn pha_rts_respects_existing_hint() {
        let mut rom = one_bank_rom();
        rom[0..8].copy_from_slice(&[0xB9, 0x10, 0x80, 0x3A, 0x48, 0xE2, 0x30, 0x60]);
        let t = pha_rom_offset(0, 0x8010);
        rom[t + 1] = 0x90; // one valid ptr $9000
        // Hand-written indirect_dispatch already authorises site $8004.
        let mut parsed = vec![parse_bank_cfg(
            "bank = 00\nfunc Disp 8000 end:8020\nindirect_dispatch 8004 4 idx:Y\n",
            "t",
        )
        .unwrap()];
        let before = parsed[0].indirect_dispatch.len();
        let fixes = pha_rts_detect_and_route(&mut parsed, &rom);
        assert!(fixes.is_empty());
        assert_eq!(parsed[0].indirect_dispatch.len(), before);
    }

    #[test]
    fn exit_mx_records_leaf_mutation() {
        let mut rom = one_bank_rom();
        // $8000: SEP #$20 (E2 20) ; RTS (60). SEP#$20 forces m=1.
        rom[0] = 0xE2;
        rom[1] = 0x20;
        rom[2] = 0x60;
        let mut parsed = vec![parse_bank_cfg("bank = 00\nfunc Leaf 8000 end:8003\n", "t").unwrap()];
        let helpers: HashMap<u32, String> = HashMap::new();
        let reloc: Vec<RelocRegion> = Vec::new();
        let fixes = exit_mx_detect_and_route(&mut parsed, &rom, &helpers, &reloc, None);
        // Entry variants (0,0)->(1,0) and (0,1)->(1,1) mutate m; (1,*) preserved.
        let mut got: Vec<(u8, u8, u8, u8)> = fixes
            .iter()
            .map(|f| (f.entry_m, f.entry_x, f.exit_m, f.exit_x))
            .collect();
        got.sort();
        assert_eq!(got, vec![(0, 0, 1, 0), (0, 1, 1, 1)]);
        assert_eq!(parsed[0].exit_mx_at_per_variant.len(), 2);
    }

    #[test]
    fn discover_helpers_empty_when_no_jsl() {
        let mut rom = one_bank_rom();
        rom[0] = 0x60; // just an RTS
        let parsed = vec![parse_bank_cfg("bank = 00\nfunc F 8000 end:8001\n", "t").unwrap()];
        let env = DecodeEnv::default();
        let helpers = discover_dispatch_helpers(&parsed, &rom, &env);
        assert!(helpers.is_empty());
    }

    // Keep BankEntry import used even if a future edit drops the manual ctor.
    #[allow(dead_code)]
    fn _uses_bank_entry() -> BankEntry {
        BankEntry::new(Some("x".into()), 0x8000)
    }
}
