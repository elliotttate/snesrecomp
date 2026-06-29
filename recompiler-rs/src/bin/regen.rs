//! `regen` — Rust port of `tools/v2_regen.py`.
//!
//! Drives the full v2 pipeline over every `bankXX.cfg`: load + name map, the
//! autoroute passes (wrapper → tail_call → pha_rts → dispatch-helper discovery →
//! exit_mx), per-(m,x) variant discovery to a fixpoint, the emit-truth /
//! reference-taint variant prune, auto-promotion of unresolved call targets, and
//! the per-bank emit (parallelized with rayon) + dispatch table + stub file.
//!
//! Functional-equivalence target vs the Python: same set of output files, same
//! set of emitted functions per file, same function bodies. Anywhere the Python
//! iterates an unordered `set` that reaches output, this uses `BTree*`/sorting so
//! the result is deterministic run-to-run.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

use snesrecomp_regen::autoroute::{
    discover_dispatch_helpers, exit_mx_detect_and_route, pha_rts_detect_and_route,
    tail_call_detect_and_route, wrapper_detect_and_route,
};
use snesrecomp_regen::cfg::{load_bank_cfg, BankCfg, BankEntry, NameDecl};
use snesrecomp_regen::codegen::{EmitCtx, EmitOutcome};
use snesrecomp_regen::decoder::{
    analyze_function_exit_mx_modes, decode_function, DecodeEnv, FunctionDecodeGraph,
    IndirectDispatchSite,
};
use snesrecomp_regen::emit::{emit_bank, BankEntrySpec, EmitHle};
use snesrecomp_regen::rom::{load_rom, RelocRegion};

type VKey = (u32, u8, u8); // (addr24, m, x)
type Variants = BTreeMap<u32, BTreeSet<(u8, u8)>>;
type CalleeExitMx = HashMap<(u32, u8, u8), (u8, u8)>;
type CalleeExitMxModes = HashMap<(u32, u8, u8), Vec<(u8, u8)>>;
type ValidVariants = HashMap<u32, BTreeSet<(u8, u8)>>;

// ── Stub-lint markers (port of `_STUB_MARKERS`). ─────────────────────────────
const STUB_MARKERS: &[&str] = &[
    "IndirectGoto: target",
    "IndirectGoto: dispatch table",
    "Call indirect SUPPRESSED",
    "Call: target unknown",
    "unresolvable cross-fn goto",
    "cpu_trace_unresolved_goto_trap",
    "cpu_trace_unresolved_stub_trap",
    "Goto with no successor",
    "unresolvable cross-bank goto",
    "unresolved IndirectGoto",
];

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

/// `bankXX.cfg` filename → bank id (matches `_BANK_CFG_RE`).
fn filename_bank(p: &Path) -> Option<u32> {
    let name = p.file_name()?.to_str()?;
    let rest = name.strip_prefix("bank")?;
    let hex = rest.strip_suffix(".cfg")?;
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

fn write_if_changed(path: &Path, content: &str) -> bool {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return false;
        }
    }
    std::fs::write(path, content).expect("write output file");
    true
}

fn try_decode(
    rom: &[u8],
    bank: u32,
    start: u32,
    m: u8,
    x: u8,
    end: Option<u32>,
    env: &DecodeEnv,
) -> Option<FunctionDecodeGraph> {
    catch_unwind(AssertUnwindSafe(|| decode_function(rom, bank, start, m, x, end, env))).ok()
}

fn in_data_region(target: u32, data_regions: &[(u32, u32, u32)]) -> bool {
    if data_regions.is_empty() {
        return false;
    }
    let tbank = (target >> 16) & 0xFF;
    let tpc = target & 0xFFFF;
    data_regions
        .iter()
        .any(|&(b, s, e)| (b & 0xFF) == tbank && (s & 0xFFFF) <= tpc && tpc < (e & 0xFFFF))
}

/// JSR/JSL/JML static call target of an insn (mirrors the variant-discovery
/// target selection), or None.
fn call_target(mnem: &str, length: u8, addr: u32, operand: u32) -> Option<u32> {
    if mnem == "JSR" && length == 3 {
        let src_bank = (addr >> 16) & 0xFF;
        Some((src_bank << 16) | (operand & 0xFFFF))
    } else if mnem == "JSL" {
        Some(operand & 0xFFFFFF)
    } else if mnem == "JMP" && length == 4 {
        Some(operand & 0xFFFFFF)
    } else {
        None
    }
}

// ── Emitted-body parsing (replaces the Python regexes). ──────────────────────

/// `^RecompReturn <base>_M{m}X{x}(CpuState` → (base, m, x).
fn parse_recomp_return_def(line: &str) -> Option<(&str, u8, u8)> {
    let rest = line.strip_prefix("RecompReturn ")?;
    let paren = rest.find("(CpuState")?;
    let full = &rest[..paren];
    let b = full.as_bytes();
    let n = b.len();
    if n < 5 {
        return None;
    }
    if b[n - 5] != b'_' || b[n - 4] != b'M' || b[n - 2] != b'X' {
        return None;
    }
    let m = match b[n - 3] {
        b'0' => 0,
        b'1' => 1,
        _ => return None,
    };
    let x = match b[n - 1] {
        b'0' => 0,
        b'1' => 1,
        _ => return None,
    };
    Some((&full[..n - 5], m, x))
}

/// `^void <name>(CpuState` — the entry-point alias wrapper.
fn is_void_alias(line: &str) -> bool {
    line.starts_with("void ") && line.contains("(CpuState")
}

/// `^\s*(case [0-3]:|default:)` — a runtime-(m,x) dispatch switch case.
fn is_mx_dispatch_case(line: &str) -> bool {
    let t = line.trim_start();
    if let Some(after) = t.strip_prefix("case ") {
        let after = after.trim_start();
        let mut chars = after.chars();
        if let Some(d) = chars.next() {
            if ('0'..='3').contains(&d) {
                let tail = chars.as_str().trim_start();
                return tail.starts_with(':');
            }
        }
        false
    } else if let Some(after) = t.strip_prefix("default") {
        after.trim_start().starts_with(':')
    } else {
        false
    }
}

/// Find every `Name_MmXx(cpu)` direct variant call in a line → (name, m, x).
fn find_variant_calls(line: &str) -> Vec<(&str, u8, u8)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let needle = b"(cpu)";
    let mut i = 0usize;
    while i + needle.len() <= b.len() {
        if &b[i..i + needle.len()] == needle {
            // chars [i-5..i] must be _M{d}X{d}
            if i >= 5
                && b[i - 5] == b'_'
                && b[i - 4] == b'M'
                && b[i - 2] == b'X'
                && (b[i - 3] == b'0' || b[i - 3] == b'1')
                && (b[i - 1] == b'0' || b[i - 1] == b'1')
            {
                // identifier ends at i-5, scan backward over \w
                let mut j = i - 5;
                while j > 0 {
                    let c = b[j - 1];
                    if c == b'_' || c.is_ascii_alphanumeric() {
                        j -= 1;
                    } else {
                        break;
                    }
                }
                let name = &line[j..i - 5];
                // First char must be [A-Za-z_] (regex `[A-Za-z_]\w*`).
                let first_ok =
                    name.as_bytes().first().map_or(false, |&c| c == b'_' || c.is_ascii_alphabetic());
                if !name.is_empty() && first_ok {
                    let m = b[i - 3] - b'0';
                    let x = b[i - 1] - b'0';
                    out.push((name, m, x));
                }
            }
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// `^bank_BB_AAAA$` synthetic name → addr24.
fn resolve_synthetic(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("bank_")?;
    let mut parts = rest.splitn(2, '_');
    let bb = parts.next()?;
    let aaaa = parts.next()?;
    if bb.len() != 2 || aaaa.len() != 4 {
        return None;
    }
    if !bb.chars().all(|c| c.is_ascii_hexdigit()) || !aaaa.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let b = u32::from_str_radix(bb, 16).ok()?;
    let a = u32::from_str_radix(aaaa, 16).ok()?;
    Some((b << 16) | a)
}

// ── Result struct returned by per-bank emit. ─────────────────────────────────
struct BankResult {
    bank: u32,
    src: String,
    outcome: EmitOutcome,
}

/// base_start map: (bank, base_name) → start pc16.
fn build_base_start(parsed: &[BankCfg]) -> HashMap<(u32, String), u32> {
    let mut m = HashMap::new();
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            let bn = e
                .name
                .clone()
                .unwrap_or_else(|| format!("bank_{:02X}_{:04X}", bank, e.start & 0xFFFF));
            m.insert((bank, bn), e.start & 0xFFFF);
        }
    }
    m
}

/// `_scan_dirty_variants` → (dirty, emitted).
fn scan_dirty_variants(
    results: &[BankResult],
    base_start: &HashMap<(u32, String), u32>,
) -> (BTreeSet<VKey>, BTreeSet<VKey>) {
    let mut dirty: BTreeSet<VKey> = BTreeSet::new();
    let mut emitted: BTreeSet<VKey> = BTreeSet::new();
    for r in results {
        let bank = r.bank;
        let mut cur: Option<VKey> = None;
        for line in r.src.split('\n') {
            if let Some((base, m, x)) = parse_recomp_return_def(line) {
                cur = base_start
                    .get(&(bank, base.to_string()))
                    .map(|&start| ((bank << 16) | start, m, x));
                if let Some(k) = cur {
                    emitted.insert(k);
                }
                continue;
            }
            let k = match cur {
                Some(k) => k,
                None => continue,
            };
            if STUB_MARKERS.iter().any(|mk| line.contains(mk)) {
                dirty.insert(k);
            }
        }
    }
    (dirty, emitted)
}

/// `_compute_prunable`.
fn compute_prunable(
    dirty: &BTreeSet<VKey>,
    emitted: &BTreeSet<VKey>,
    canonical: &Variants,
) -> BTreeSet<VKey> {
    let mut dirty_mx: HashMap<u32, HashSet<(u8, u8)>> = HashMap::new();
    for &(addr, m, x) in dirty {
        dirty_mx.entry(addr).or_default().insert((m, x));
    }
    let mut emitted_mx: HashMap<u32, HashSet<(u8, u8)>> = HashMap::new();
    for &(addr, m, x) in emitted {
        emitted_mx.entry(addr).or_default().insert((m, x));
    }
    let default_canon: BTreeSet<(u8, u8)> = [(1u8, 1u8)].into_iter().collect();
    let mut prunable: BTreeSet<VKey> = BTreeSet::new();
    for &(addr, m, x) in dirty {
        let canon = canonical.get(&addr).filter(|s| !s.is_empty()).unwrap_or(&default_canon);
        if canon.contains(&(m, x)) {
            continue;
        }
        let clean_canon = canon.iter().any(|c| {
            emitted_mx.get(&addr).map_or(false, |s| s.contains(c))
                && !dirty_mx.get(&addr).map_or(false, |s| s.contains(c))
        });
        if clean_canon {
            prunable.insert((addr, m, x));
        }
    }
    prunable
}

/// `_scan_variant_refs` → DIRECT cross-variant reference graph.
fn scan_variant_refs(results: &[BankResult], parsed: &[BankCfg]) -> BTreeMap<VKey, BTreeSet<VKey>> {
    let mut name_to_addr: HashMap<String, u32> = HashMap::new();
    let mut base_start: HashMap<(u32, String), u32> = HashMap::new();
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            if let Some(name) = &e.name {
                name_to_addr.insert(name.clone(), (bank << 16) | (e.start & 0xFFFF));
            }
            let bn = e
                .name
                .clone()
                .unwrap_or_else(|| format!("bank_{:02X}_{:04X}", bank, e.start & 0xFFFF));
            base_start.insert((bank, bn), e.start & 0xFFFF);
        }
    }
    let resolve = |nm: &str| -> Option<u32> {
        if let Some(a) = resolve_synthetic(nm) {
            return Some(a);
        }
        name_to_addr.get(nm).copied()
    };

    let mut refs: BTreeMap<VKey, BTreeSet<VKey>> = BTreeMap::new();
    for r in results {
        let bank = r.bank;
        let mut cur: Option<VKey> = None;
        for line in r.src.split('\n') {
            if let Some((base, m, x)) = parse_recomp_return_def(line) {
                cur = base_start.get(&(bank, base.to_string())).map(|&s| ((bank << 16) | s, m, x));
                continue;
            }
            if is_void_alias(line) {
                cur = None;
                continue;
            }
            let cv = match cur {
                Some(k) => k,
                None => continue,
            };
            if is_mx_dispatch_case(line) {
                continue;
            }
            for (nm, m, x) in find_variant_calls(line) {
                if let Some(taddr) = resolve(nm) {
                    let tv = (taddr, m, x);
                    if tv == cv {
                        continue;
                    }
                    refs.entry(cv).or_default().insert(tv);
                }
            }
        }
    }
    refs
}

/// `_propagate_reference_taint`.
fn propagate_reference_taint(
    dirty: &BTreeSet<VKey>,
    refs: &BTreeMap<VKey, BTreeSet<VKey>>,
    emitted: &BTreeSet<VKey>,
    bank_set: &BTreeSet<u32>,
    pruned: &BTreeSet<VKey>,
) -> BTreeSet<VKey> {
    let mut tainted: BTreeSet<VKey> = dirty.clone();
    let emitted_now: BTreeSet<VKey> = emitted.difference(pruned).copied().collect();
    let mut changed = true;
    while changed {
        changed = false;
        for (v, targets) in refs {
            if tainted.contains(v) {
                continue;
            }
            for &(taddr, tm, tx) in targets {
                let tbank = (taddr >> 16) & 0xFF;
                let dangling = bank_set.contains(&tbank) && !emitted_now.contains(&(taddr, tm, tx));
                if dangling || tainted.contains(&(taddr, tm, tx)) {
                    tainted.insert(*v);
                    changed = true;
                    break;
                }
            }
        }
    }
    tainted
}

/// `_apply_variant_prune`: drop pruned entries, rebuild valid-variants map.
fn apply_variant_prune(parsed: &mut [BankCfg], pruned: &BTreeSet<VKey>) -> ValidVariants {
    let mut prune_by_bank: HashMap<u32, HashSet<(u32, u8, u8)>> = HashMap::new();
    for &(addr, m, x) in pruned {
        prune_by_bank.entry((addr >> 16) & 0xFF).or_default().insert((addr & 0xFFFF, m, x));
    }
    for cfg in parsed.iter_mut() {
        let bank = cfg.bank as u32;
        if let Some(pset) = prune_by_bank.get(&bank) {
            cfg.entries.retain(|e| !pset.contains(&(e.start & 0xFFFF, e.entry_m & 1, e.entry_x & 1)));
        }
    }
    let mut vvm: ValidVariants = HashMap::new();
    for cfg in parsed.iter() {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            let a = (bank << 16) | (e.start & 0xFFFF);
            vvm.entry(a).or_default().insert((e.entry_m & 1, e.entry_x & 1));
        }
    }
    vvm
}

/// `_build_callee_inline_skip`.
fn build_callee_inline_skip(parsed: &[BankCfg]) -> HashMap<u32, i32> {
    let mut skip = HashMap::new();
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            if let Some(n) = e.inline_skip {
                if n != 0 {
                    skip.insert((bank << 16) | (e.start & 0xFFFF), n);
                }
            }
        }
    }
    skip
}

/// `_rebuild_callee_exit_mx`.
fn rebuild_callee_exit_mx(parsed: &[BankCfg], variants: &Variants) -> CalleeExitMx {
    let mut callee_exit_mx: CalleeExitMx = HashMap::new();
    let mut declared: BTreeMap<(u8, u32), (u8, u8)> = BTreeMap::new();
    for cfg in parsed {
        for &(b_id, addr16, m_val, x_val) in &cfg.exit_mx_at {
            declared.insert((b_id & 0xFF, addr16 & 0xFFFF), (m_val, x_val));
        }
    }
    for (&(b_id, addr16), &(ex_m, ex_xf)) in &declared {
        let target = ((b_id as u32) << 16) | addr16;
        match variants.get(&target) {
            Some(mx_set) if !mx_set.is_empty() => {
                for &(em, ex2) in mx_set {
                    callee_exit_mx.insert((target, em, ex2), (ex_m, ex_xf));
                }
            }
            _ => {
                callee_exit_mx.insert((target, 1, 1), (ex_m, ex_xf));
            }
        }
    }
    for cfg in parsed {
        for &(b_id, addr16, em_in, ex_in, ex_m, ex_xf) in &cfg.exit_mx_at_per_variant {
            let target = (((b_id as u32) & 0xFF) << 16) | (addr16 & 0xFFFF);
            callee_exit_mx.insert((target, em_in & 1, ex_in & 1), (ex_m & 1, ex_xf & 1));
        }
    }
    callee_exit_mx
}

#[allow(clippy::too_many_arguments)]
fn build_callee_exit_mx_modes(
    parsed: &[BankCfg],
    rom: &[u8],
    helpers: &HashMap<u32, String>,
    all_data: &[(u32, u32, u32)],
    callee_map: &CalleeExitMx,
    reloc: &[RelocRegion],
    inline_skip: &HashMap<u32, i32>,
) -> CalleeExitMxModes {
    let mut mode_map: CalleeExitMxModes = HashMap::new();
    let data_opt = if all_data.is_empty() { None } else { Some(all_data) };
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            let target = (bank << 16) | (e.start & 0xFFFF);
            let key = (target, e.entry_m & 1, e.entry_x & 1);
            let env = DecodeEnv {
                reloc_regions: Some(reloc),
                global_inline_skip: Some(inline_skip),
                dispatch_helpers: Some(helpers),
                data_regions: data_opt,
                callee_exit_mx: Some(callee_map),
                ..Default::default()
            };
            let graph = match try_decode(rom, bank, e.start, e.entry_m, e.entry_x, e.end, &env) {
                Some(g) => g,
                None => continue,
            };
            if let Some(modes) = analyze_function_exit_mx_modes(&graph, Some(callee_map)) {
                if modes.len() > 1 {
                    mode_map.insert(key, modes);
                }
            }
        }
    }
    mode_map
}

/// `_discover_variants_from_current_entries`. Mutates `variants`, `decoded`, and
/// appends `BankEntry` clones for known function addresses. Returns count added.
#[allow(clippy::too_many_arguments)]
fn discover_variants_from_current_entries(
    parsed: &mut [BankCfg],
    rom: &[u8],
    variants: &mut Variants,
    decoded: &mut HashSet<VKey>,
    helpers: &HashMap<u32, String>,
    all_data: &[(u32, u32, u32)],
    pruned: &BTreeSet<VKey>,
    pending_entries: Option<&BTreeSet<VKey>>,
    callee_exit_mx: Option<&CalleeExitMx>,
    callee_exit_mx_modes: Option<&CalleeExitMxModes>,
    reloc: &[RelocRegion],
    inline_skip: &HashMap<u32, i32>,
) -> usize {
    let mut addr_to_end: HashMap<u32, Option<u32>> = HashMap::new();
    let mut addr_to_bank: HashMap<u32, u32> = HashMap::new();
    let mut entry_start: HashMap<u32, u32> = HashMap::new();
    for cfg in parsed.iter() {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            let addr = (bank << 16) | (e.start & 0xFFFF);
            addr_to_end.entry(addr).or_insert(e.end);
            addr_to_bank.entry(addr).or_insert(bank);
            entry_start.entry(addr).or_insert(e.start);
        }
    }

    let data_opt = if all_data.is_empty() { None } else { Some(all_data) };

    // Seed.
    let seed: BTreeSet<VKey> = match pending_entries {
        Some(p) => p.clone(),
        None => {
            let mut s = BTreeSet::new();
            for cfg in parsed.iter() {
                let bank = cfg.bank as u32;
                for e in &cfg.entries {
                    s.insert(((bank << 16) | (e.start & 0xFFFF), e.entry_m & 1, e.entry_x & 1));
                }
            }
            s
        }
    };

    let mut queue: Vec<(u32, u32, u8, u8, Option<u32>)> = Vec::new();
    for &(addr, em, ex) in &seed {
        if pruned.contains(&(addr, em, ex)) || !addr_to_end.contains_key(&addr) {
            continue;
        }
        variants.entry(addr).or_default().insert((em, ex));
        if !decoded.contains(&(addr, em, ex)) {
            let bank = addr_to_bank[&addr];
            queue.push((bank, entry_start[&addr], em, ex, addr_to_end[&addr]));
        }
    }

    while let Some((bank, start, em, ex, end)) = queue.pop() {
        let addr = (bank << 16) | (start & 0xFFFF);
        if decoded.contains(&(addr, em, ex)) || pruned.contains(&(addr, em, ex)) {
            continue;
        }
        decoded.insert((addr, em, ex));
        let env = DecodeEnv {
            reloc_regions: Some(reloc),
            global_inline_skip: Some(inline_skip),
            dispatch_helpers: Some(helpers),
            data_regions: data_opt,
            callee_exit_mx,
            callee_exit_mx_modes,
            ..Default::default()
        };
        let graph = match try_decode(rom, bank, start, em, ex, end, &env) {
            Some(g) => g,
            None => continue,
        };
        for di in graph.insns() {
            let ins = &di.insn;
            let mut target = call_target(ins.mnem, ins.length, ins.addr, ins.operand);
            if let Some(t) = target {
                if in_data_region(t, all_data) {
                    target = None;
                }
            }
            if let Some(t) = target {
                let em2 = ins.m_flag & 1;
                let ex2 = ins.x_flag & 1;
                if !pruned.contains(&(t, em2, ex2)) {
                    let set = variants.entry(t).or_default();
                    if !set.contains(&(em2, ex2)) {
                        set.insert((em2, ex2));
                    }
                    if addr_to_end.contains_key(&t) && !decoded.contains(&(t, em2, ex2)) {
                        queue.push((addr_to_bank[&t], t & 0xFFFF, em2, ex2, addr_to_end[&t]));
                    }
                }
            }
            if let Some(entries) = &ins.dispatch_entries {
                let is_long = ins.dispatch_kind.as_deref() == Some("long");
                for &d_target in entries {
                    if d_target == 0 {
                        continue;
                    }
                    let d_addr = if is_long {
                        d_target & 0xFFFFFF
                    } else {
                        (((ins.addr >> 16) & 0xFF) << 16) | (d_target & 0xFFFF)
                    };
                    if in_data_region(d_addr, all_data) {
                        continue;
                    }
                    let em2 = ins.m_flag & 1;
                    let ex2 = ins.x_flag & 1;
                    if pruned.contains(&(d_addr, em2, ex2)) {
                        continue;
                    }
                    let set = variants.entry(d_addr).or_default();
                    if !set.contains(&(em2, ex2)) {
                        set.insert((em2, ex2));
                    }
                    if addr_to_end.contains_key(&d_addr) && !decoded.contains(&(d_addr, em2, ex2)) {
                        queue.push((
                            addr_to_bank[&d_addr],
                            d_addr & 0xFFFF,
                            em2,
                            ex2,
                            addr_to_end[&d_addr],
                        ));
                    }
                }
            }
        }
    }

    // Append missing BankEntry clones for known function PCs.
    let mut added = 0usize;
    for cfg in parsed.iter_mut() {
        let bank = cfg.bank as u32;
        let mut current_keys: HashSet<(u32, u8, u8)> = cfg
            .entries
            .iter()
            .map(|e| ((bank << 16) | (e.start & 0xFFFF), e.entry_m & 1, e.entry_x & 1))
            .collect();
        let mut by_pc: HashMap<u32, BankEntry> = HashMap::new();
        for e in &cfg.entries {
            by_pc.entry(e.start & 0xFFFF).or_insert_with(|| e.clone());
        }
        for (&addr, mxs) in variants.iter() {
            if ((addr >> 16) & 0xFF) != bank {
                continue;
            }
            let base = match by_pc.get(&(addr & 0xFFFF)) {
                Some(b) => b,
                None => continue,
            };
            for &(em, ex) in mxs {
                let key = (addr, em, ex);
                if current_keys.contains(&key) || pruned.contains(&key) {
                    continue;
                }
                let mut ne = BankEntry::new(base.name.clone(), base.start);
                ne.end = base.end;
                ne.entry_m = em;
                ne.entry_x = ex;
                // NOTE: variant clones do NOT inherit force_host_return_sites — the
                // Python clone (v2_regen) builds a fresh BankEntry without it, so
                // only the canonical (cfg-declared) variant host-returns.
                cfg.entries.push(ne);
                current_keys.insert(key);
                added += 1;
            }
        }
    }
    added
}

/// `_autopromote_targets`: add BankEntry records for demand tuples not yet
/// represented. Returns the newly-added (addr24, m, x) set.
fn autopromote_targets(
    parsed: &mut [BankCfg],
    demands: &BTreeSet<VKey>,
    all_data: &[(u32, u32, u32)],
) -> BTreeSet<VKey> {
    let mut added: BTreeSet<VKey> = BTreeSet::new();
    if demands.is_empty() {
        return added;
    }
    let bank_set: BTreeSet<u32> = parsed.iter().map(|c| c.bank as u32).collect();
    let mut by_bank: BTreeMap<u32, Vec<(u32, u8, u8)>> = BTreeMap::new();
    for &(addr, em, ex) in demands {
        let mut tbank = (addr >> 16) & 0xFF;
        let tpc = addr & 0xFFFF;
        if !bank_set.contains(&tbank) {
            let mirror = tbank ^ 0x80;
            if (tbank < 0x40 || (0x80..0xC0).contains(&tbank)) && bank_set.contains(&mirror) {
                tbank = mirror;
            }
        }
        if in_data_region((tbank << 16) | tpc, all_data) {
            continue;
        }
        by_bank.entry(tbank).or_default().push((tpc, em, ex));
    }
    // bank -> index
    let mut bank_index: HashMap<u32, usize> = HashMap::new();
    for (i, c) in parsed.iter().enumerate() {
        bank_index.insert(c.bank as u32, i);
    }
    for (bank, items) in by_bank {
        let idx = match bank_index.get(&bank) {
            Some(&i) => i,
            None => continue,
        };
        let cfg = &mut parsed[idx];
        let mut existing_keys: HashSet<(u32, u8, u8)> = cfg
            .entries
            .iter()
            .map(|e| (e.start & 0xFFFF, e.entry_m & 1, e.entry_x & 1))
            .collect();
        let mut entries_by_pc: HashMap<u32, BankEntry> = HashMap::new();
        for e in &cfg.entries {
            entries_by_pc.entry(e.start & 0xFFFF).or_insert_with(|| e.clone());
        }
        for (pc, em, ex) in items {
            let key = (pc, em, ex);
            if existing_keys.contains(&key) {
                continue;
            }
            let new_entry = if let Some(base) = entries_by_pc.get(&pc) {
                let mut ne = BankEntry::new(base.name.clone(), pc);
                ne.end = base.end;
                ne.entry_m = em;
                ne.entry_x = ex;
                // (no force_host_return_sites on clones — matches the Python clone)
                ne
            } else {
                let synth = format!("bank_{bank:02X}_{pc:04X}");
                let mut ne = BankEntry::new(Some(synth), pc);
                ne.entry_m = em;
                ne.entry_x = ex;
                entries_by_pc.insert(pc, ne.clone());
                ne
            };
            cfg.entries.push(new_entry);
            existing_keys.insert(key);
            added.insert(((bank << 16) | (pc & 0xFFFF), em, ex));
        }
    }
    added
}

// ── Per-bank emit ────────────────────────────────────────────────────────────
struct BankInput {
    bank: u32,
    entries: Vec<BankEntrySpec>,
    data_regions: Vec<(u32, u32, u32)>,
    indirect_dispatch: HashMap<u32, IndirectDispatchSite>,
    inline_loop_pcs: BTreeSet<u32>,
    hle_func: BTreeMap<u32, String>,
    hle_dispatch: BTreeMap<u32, String>,
    hle_spc_upload: BTreeSet<u32>,
    exclude_ranges: Vec<(u32, u32)>,
}

fn bank_entry_to_spec(e: &BankEntry) -> BankEntrySpec {
    BankEntrySpec {
        name: e.name.clone(),
        start: e.start,
        end: e.end,
        entry_m: e.entry_m,
        entry_x: e.entry_x,
        tail_call_pc16: e.tail_call_pc16,
        entry_s_offset: e.entry_s_offset,
        force_host_return_sites: e.force_host_return_sites.clone(),
    }
}

fn main() {
    // Suppress decoder panic spam (panics are caught and mapped to skips).
    std::panic::set_hook(Box::new(|_| {}));

    let args: Vec<String> = std::env::args().collect();
    let rom_path = arg_value(&args, "--rom").expect("--rom required");
    let cfg_dir = arg_value(&args, "--cfg-dir").expect("--cfg-dir required");
    let out_dir = arg_value(&args, "--out-dir").expect("--out-dir required");
    let jobs: usize = arg_value(&args, "--jobs").and_then(|s| s.parse().ok()).unwrap_or_else(num_cpus_guess);
    let only_banks: Option<BTreeSet<u32>> = arg_value(&args, "--banks").map(|s| {
        s.split(',')
            .filter_map(|t| {
                let t = t.trim();
                if t.is_empty() {
                    None
                } else {
                    u32::from_str_radix(t, 16).ok()
                }
            })
            .collect()
    });

    rayon::ThreadPoolBuilder::new().num_threads(jobs.max(1)).build_global().ok();

    let start_time = Instant::now();
    let rom = load_rom(&rom_path).expect("load rom");
    let out_dir = PathBuf::from(&out_dir);
    std::fs::create_dir_all(&out_dir).expect("mkdir out-dir");

    let mut cfg_paths: Vec<PathBuf> = std::fs::read_dir(&cfg_dir)
        .expect("read cfg dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("bank") && n.ends_with(".cfg"))
                .unwrap_or(false)
        })
        .collect();
    cfg_paths.sort();
    if cfg_paths.is_empty() {
        eprintln!("v2_regen: no bank*.cfg under {cfg_dir}");
        std::process::exit(2);
    }

    // ── Load cfgs + build name map / cross-bank names. ──
    let mut parsed: Vec<BankCfg> = Vec::new();
    let mut name_map: HashMap<u32, String> = HashMap::new();
    let mut cross_bank_names: HashMap<u32, Vec<NameDecl>> = HashMap::new();
    for p in &cfg_paths {
        let bank = match filename_bank(p) {
            Some(b) => b,
            None => continue,
        };
        let mut cfg = match load_bank_cfg(p) {
            Ok(c) => c,
            Err(e) => {
                println!("  PARSE-FAIL bank ${bank:02X}: {e}");
                continue;
            }
        };
        cfg.bank = bank as i32; // key off filename, not the `bank=` directive
        for e in &cfg.entries {
            if let Some(name) = &e.name {
                name_map.insert((bank << 16) | (e.start & 0xFFFF), name.clone());
            }
        }
        for nd in &cfg.names {
            let addr = nd.addr_24 & 0xFFFFFF;
            name_map.insert(addr, nd.name.clone());
            cross_bank_names.entry((addr >> 16) & 0xFF).or_default().push(nd.clone());
        }
        parsed.push(cfg);
    }

    // ── inline-skip + reloc aggregation. ──
    let callee_inline_skip = build_callee_inline_skip(&parsed);
    let mut reloc_regions: Vec<RelocRegion> = Vec::new();
    for cfg in &parsed {
        reloc_regions.extend(cfg.reloc_regions.iter().copied());
    }

    // ── auto_vectors expansion (bank 0). ──
    expand_auto_vectors(&mut parsed, &rom, &mut name_map);

    // ── Autoroute passes (wrapper → tail_call → pha_rts → helpers → exit_mx). ──
    let basic_env = DecodeEnv {
        reloc_regions: Some(&reloc_regions),
        global_inline_skip: Some(&callee_inline_skip),
        ..Default::default()
    };
    let wrapper_fixes = wrapper_detect_and_route(&parsed, &mut name_map, &mut cross_bank_names, &rom);
    println!("Auto-routing DB-transition wrappers... {} fix(es)", wrapper_fixes.len());
    let tail_fixes = tail_call_detect_and_route(&mut parsed, &rom, &basic_env);
    println!("Auto-detecting tail-call fallthrough sites... {} fix(es)", tail_fixes.len());
    let pha_fixes = pha_rts_detect_and_route(&mut parsed, &rom);
    println!("Auto-detecting PHA-RTS dispatch sites... {} fix(es)", pha_fixes.len());
    let dispatch_helpers_bt = discover_dispatch_helpers(&parsed, &rom, &basic_env);
    let dispatch_helpers: HashMap<u32, String> =
        dispatch_helpers_bt.iter().map(|(k, v)| (*k, v.clone())).collect();
    println!("Auto-detected {} JSL dispatch helpers", dispatch_helpers.len());
    let exit_fixes = exit_mx_detect_and_route(&mut parsed, &rom, &dispatch_helpers, &reloc_regions);
    println!("Auto-detecting leaf exit-(M,X) mutations... {} fix(es)", exit_fixes.len());

    // ── Promote cross-bank `name` decls into owning-bank entries (global dedup). ──
    {
        let mut global_names: HashSet<String> = HashSet::new();
        for cfg in &parsed {
            for e in &cfg.entries {
                if let Some(n) = &e.name {
                    global_names.insert(n.clone());
                }
            }
        }
        for cfg in parsed.iter_mut() {
            let bank = cfg.bank as u32;
            let mut existing_starts: HashSet<u32> =
                cfg.entries.iter().map(|e| e.start & 0xFFFF).collect();
            let mut existing_names: HashSet<String> =
                cfg.entries.iter().filter_map(|e| e.name.clone()).collect();
            if let Some(list) = cross_bank_names.get(&bank) {
                for nd in list {
                    let local_pc = nd.addr_24 & 0xFFFF;
                    if existing_starts.contains(&local_pc) {
                        continue;
                    }
                    if existing_names.contains(&nd.name) || global_names.contains(&nd.name) {
                        continue;
                    }
                    cfg.entries.push(BankEntry::new(Some(nd.name.clone()), local_pc));
                    existing_starts.insert(local_pc);
                    existing_names.insert(nd.name.clone());
                    global_names.insert(nd.name.clone());
                }
            }
        }
    }

    // ── force_variant_at aggregation. ──
    let mut force_variant_map: HashMap<u32, (u8, u8)> = HashMap::new();
    for cfg in &parsed {
        for (&site, &(m, x)) in &cfg.force_variant_at {
            force_variant_map.entry(site).or_insert((m, x));
        }
    }

    // ── Aggregate data regions. ──
    let mut all_data_regions: Vec<(u32, u32, u32)> = Vec::new();
    for cfg in &parsed {
        all_data_regions.extend(cfg.data_regions.iter().copied());
    }

    // ── Initial variant discovery (fixpoint). ──
    let mut variants: Variants = BTreeMap::new();
    {
        let mut addr_to_end: HashMap<u32, Option<u32>> = HashMap::new();
        let mut addr_to_bank: HashMap<u32, u32> = HashMap::new();
        let mut queue: Vec<(u32, u32, u8, u8, Option<u32>)> = Vec::new();
        for cfg in &parsed {
            let bank = cfg.bank as u32;
            for e in &cfg.entries {
                let addr = (bank << 16) | (e.start & 0xFFFF);
                addr_to_end.insert(addr, e.end);
                addr_to_bank.insert(addr, bank);
                let mx = (e.entry_m & 1, e.entry_x & 1);
                let set = variants.entry(addr).or_default();
                if !set.contains(&mx) {
                    set.insert(mx);
                    queue.push((bank, e.start, mx.0, mx.1, e.end));
                }
            }
        }
        let data_opt = if all_data_regions.is_empty() { None } else { Some(all_data_regions.as_slice()) };
        let mut decoded: HashSet<VKey> = HashSet::new();
        while let Some((bank, start, em, ex, end)) = queue.pop() {
            let addr = (bank << 16) | (start & 0xFFFF);
            if decoded.contains(&(addr, em, ex)) {
                continue;
            }
            decoded.insert((addr, em, ex));
            let env = DecodeEnv {
                reloc_regions: Some(&reloc_regions),
                global_inline_skip: Some(&callee_inline_skip),
                dispatch_helpers: Some(&dispatch_helpers),
                data_regions: data_opt,
                ..Default::default()
            };
            let graph = match try_decode(&rom, bank, start, em, ex, end, &env) {
                Some(g) => g,
                None => continue,
            };
            for di in graph.insns() {
                let ins = &di.insn;
                let mut target = call_target(ins.mnem, ins.length, ins.addr, ins.operand);
                if let Some(t) = target {
                    if in_data_region(t, &all_data_regions) {
                        target = None;
                    }
                }
                if let Some(t) = target {
                    let em2 = ins.m_flag & 1;
                    let ex2 = ins.x_flag & 1;
                    let set = variants.entry(t).or_default();
                    if !set.contains(&(em2, ex2)) {
                        set.insert((em2, ex2));
                        if let Some(&end2) = addr_to_end.get(&t) {
                            queue.push((addr_to_bank[&t], t & 0xFFFF, em2, ex2, end2));
                        }
                    }
                }
                if let Some(entries) = &ins.dispatch_entries {
                    let is_long = ins.dispatch_kind.as_deref() == Some("long");
                    for &d_target in entries {
                        if d_target == 0 {
                            continue;
                        }
                        let d_addr = if is_long {
                            d_target & 0xFFFFFF
                        } else {
                            (((ins.addr >> 16) & 0xFF) << 16) | (d_target & 0xFFFF)
                        };
                        let em2 = ins.m_flag & 1;
                        let ex2 = ins.x_flag & 1;
                        let set = variants.entry(d_addr).or_default();
                        if !set.contains(&(em2, ex2)) {
                            set.insert((em2, ex2));
                            if let Some(&end2) = addr_to_end.get(&d_addr) {
                                queue.push((addr_to_bank[&d_addr], d_addr & 0xFFFF, em2, ex2, end2));
                            }
                        }
                    }
                }
            }
        }
    }
    let multi = variants.values().filter(|v| v.len() > 1).count();
    println!("  variants for {} targets; {} multi-(m,x)", variants.len(), multi);

    // ── callee_exit_mx from cfg directives + autoroute per-variant. ──
    let mut callee_exit_mx = rebuild_callee_exit_mx(&parsed, &variants);

    // ── Capture canonical variants (incl. force_variants), then add force to variants. ──
    let mut canonical_variants: Variants = BTreeMap::new();
    for cfg in &parsed {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            let addr = (bank << 16) | (e.start & 0xFFFF);
            canonical_variants.entry(addr).or_default().insert((e.entry_m & 1, e.entry_x & 1));
            if let Some(fv) = &e.force_variants {
                for &(em, ex) in fv {
                    canonical_variants.entry(addr).or_default().insert((em & 1, ex & 1));
                    variants.entry(addr).or_default().insert((em & 1, ex & 1));
                }
            }
        }
    }

    // ── Clone cfg entries for each extra (m,x) variant. ──
    clone_variant_entries(&mut parsed, &variants);

    // ── Initial callee_exit_mx_modes. ──
    let mut callee_exit_mx_modes = build_callee_exit_mx_modes(
        &parsed, &rom, &dispatch_helpers, &all_data_regions, &callee_exit_mx, &reloc_regions,
        &callee_inline_skip,
    );

    // ── exit-mx variant refresh loop. ──
    for round in 0..16 {
        let added = discover_variants_from_current_entries(
            &mut parsed, &rom, &mut variants, &mut HashSet::new(), &dispatch_helpers,
            &all_data_regions, &BTreeSet::new(), None, Some(&callee_exit_mx),
            Some(&callee_exit_mx_modes), &reloc_regions, &callee_inline_skip,
        );
        if added == 0 {
            break;
        }
        for cfg in parsed.iter_mut() {
            cfg.exit_mx_at_per_variant.clear();
        }
        let _ = exit_mx_detect_and_route(&mut parsed, &rom, &dispatch_helpers, &reloc_regions);
        callee_exit_mx = rebuild_callee_exit_mx(&parsed, &variants);
        callee_exit_mx_modes = build_callee_exit_mx_modes(
            &parsed, &rom, &dispatch_helpers, &all_data_regions, &callee_exit_mx, &reloc_regions,
            &callee_inline_skip,
        );
        println!("  exit-mx variant refresh {}: added {} entry variant(s)", round + 1, added);
    }

    dump_entry_fingerprint(&parsed, "pre_emit");

    // ── Iterative emit + auto-promote + prune loop. ──
    let total = parsed.len();
    let bank_set: BTreeSet<u32> = parsed.iter().map(|c| c.bank as u32).collect();
    let max_passes = 24;
    let mut valid_variants_map: ValidVariants = HashMap::new();
    let mut cumulative_dirty: BTreeSet<VKey> = BTreeSet::new();
    let mut cumulative_emitted: BTreeSet<VKey> = BTreeSet::new();
    let mut cumulative_pruned: BTreeSet<VKey> = BTreeSet::new();
    let mut cumulative_rejected: BTreeSet<u32> = BTreeSet::new();
    let mut last_unresolved: BTreeSet<VKey> = BTreeSet::new();
    let mut pending_variant_entries: BTreeSet<VKey> = BTreeSet::new();
    let mut exit_mx_rescan_all = false;
    let mut decoded_for_refresh: HashSet<VKey> = HashSet::new();
    let mut succeeded = 0usize;
    let mut failed: Vec<(u32, String)> = Vec::new();

    const RT_TOTAL_LIMIT: usize = 40;
    let mut rt_total_passes = 0usize;

    for pass_idx in 0..max_passes {
        succeeded = 0;
        failed.clear();

        // Variant discovery refresh from current entries.
        let mut variant_added = 0usize;
        if !pending_variant_entries.is_empty() || exit_mx_rescan_all {
            let (seed, mut scan_decoded): (Option<BTreeSet<VKey>>, HashSet<VKey>) = if exit_mx_rescan_all {
                (None, HashSet::new())
            } else {
                (Some(pending_variant_entries.clone()), std::mem::take(&mut decoded_for_refresh))
            };
            variant_added = discover_variants_from_current_entries(
                &mut parsed, &rom, &mut variants, &mut scan_decoded, &dispatch_helpers,
                &all_data_regions, &cumulative_pruned, seed.as_ref(), Some(&callee_exit_mx),
                Some(&callee_exit_mx_modes), &reloc_regions, &callee_inline_skip,
            );
            decoded_for_refresh = scan_decoded;
            pending_variant_entries.clear();
            // exit_mx_rescan_all is re-armed at each non-breaking pass end below.
        }
        if variant_added > 0 && !valid_variants_map.is_empty() {
            valid_variants_map = apply_variant_prune(&mut parsed, &cumulative_pruned);
        }

        // Build emit ctx.
        let mut ctx = EmitCtx {
            rom_size: rom.len(),
            reloc_regions: reloc_regions.clone(),
            valid_variants: valid_variants_map.clone(),
            force_variant_at: force_variant_map.clone(),
            ..Default::default()
        };
        ctx.set_name_resolver(&name_map);

        // Build per-bank inputs.
        let mut inputs: Vec<BankInput> = Vec::new();
        for cfg in &parsed {
            let bank = cfg.bank as u32;
            if let Some(filter) = &only_banks {
                if !filter.contains(&bank) {
                    continue;
                }
            }
            let mut indirect_dispatch: HashMap<u32, IndirectDispatchSite> = HashMap::new();
            for d in &cfg.indirect_dispatch {
                indirect_dispatch.insert(
                    (bank << 16) | (d.site_pc16 & 0xFFFF),
                    IndirectDispatchSite {
                        count: d.count,
                        idx_reg: d.idx_reg,
                        table_bases: d.table_bases.clone(),
                    },
                );
            }
            inputs.push(BankInput {
                bank,
                entries: cfg.entries.iter().map(bank_entry_to_spec).collect(),
                data_regions: cfg.data_regions.clone(),
                indirect_dispatch,
                inline_loop_pcs: cfg.inline_dispatch_loops.iter().copied().collect(),
                hle_func: cfg.hle_func.clone(),
                hle_dispatch: cfg.hle_dispatch.clone(),
                hle_spc_upload: cfg.hle_spc_upload.iter().copied().collect(),
                exclude_ranges: cfg.exclude_ranges.clone(),
            });
        }

        // Emit (rayon).
        let ctx_ref = &ctx;
        let rom_ref = &rom;
        let helpers_ref = &dispatch_helpers;
        let inline_skip_ref = &callee_inline_skip;
        let cem_ref = &callee_exit_mx;
        let cemm_ref = &callee_exit_mx_modes;
        let reloc_ref = &reloc_regions;
        let results: Vec<BankResult> = inputs
            .par_iter()
            .map(|inp| {
                let data_opt =
                    if inp.data_regions.is_empty() { None } else { Some(inp.data_regions.as_slice()) };
                let inline_opt =
                    if inp.inline_loop_pcs.is_empty() { None } else { Some(&inp.inline_loop_pcs) };
                let env = DecodeEnv {
                    dispatch_helpers: Some(helpers_ref),
                    indirect_dispatch: Some(&inp.indirect_dispatch),
                    data_regions: data_opt,
                    reloc_regions: Some(reloc_ref),
                    callee_inline_skip: Some(inline_skip_ref),
                    callee_exit_mx: Some(cem_ref),
                    callee_exit_mx_modes: Some(cemm_ref),
                    inline_dispatch_loop_pcs: inline_opt,
                    ..Default::default()
                };
                let hle = EmitHle {
                    hle_spc_upload: if inp.hle_spc_upload.is_empty() { None } else { Some(&inp.hle_spc_upload) },
                    hle_func: if inp.hle_func.is_empty() { None } else { Some(&inp.hle_func) },
                    hle_dispatch: if inp.hle_dispatch.is_empty() { None } else { Some(&inp.hle_dispatch) },
                    exclude_ranges: if inp.exclude_ranges.is_empty() { None } else { Some(inp.exclude_ranges.as_slice()) },
                    ..Default::default()
                };
                let mut outcome = EmitOutcome::default();
                let res = catch_unwind(AssertUnwindSafe(|| {
                    emit_bank(ctx_ref, rom_ref, inp.bank, &inp.entries, &env, &hle, None, &mut outcome)
                }));
                match res {
                    Ok(src) => BankResult { bank: inp.bank, src, outcome },
                    Err(_) => BankResult {
                        bank: inp.bank,
                        src: String::new(),
                        outcome: EmitOutcome::default(),
                    },
                }
            })
            .collect();

        // Write files + aggregate.
        let mut pass_unresolved: BTreeSet<VKey> = BTreeSet::new();
        for r in &results {
            if r.src.is_empty() {
                failed.push((r.bank, "emit panic".to_string()));
                continue;
            }
            let out_path = out_dir.join(format!("bank{:02x}_v2.c", r.bank));
            write_if_changed(&out_path, &r.src);
            for &k in &r.outcome.unresolved_call_targets {
                pass_unresolved.insert(k);
            }
            for &a in &r.outcome.rejected_call_targets {
                cumulative_rejected.insert(a);
            }
            succeeded += 1;
        }

        // Emit-truth prune.
        let base_start = build_base_start(&parsed);
        let (dirty_now, emitted_now) = scan_dirty_variants(&results, &base_start);
        cumulative_dirty.extend(dirty_now.iter().copied());
        cumulative_emitted.extend(emitted_now.iter().copied());
        let prunable = compute_prunable(&cumulative_dirty, &cumulative_emitted, &canonical_variants);
        let newly_pruned: BTreeSet<VKey> =
            prunable.difference(&cumulative_pruned).copied().collect();
        if !newly_pruned.is_empty() {
            cumulative_pruned.extend(newly_pruned.iter().copied());
            valid_variants_map = apply_variant_prune(&mut parsed, &cumulative_pruned);
            println!(
                "  emit-truth prune: dropped {} variant(s) ({} total)",
                newly_pruned.len(),
                cumulative_pruned.len()
            );
        }

        // Call-target demands → auto-promote.
        let unresolved_calls: BTreeSet<VKey> =
            pass_unresolved.difference(&cumulative_pruned).copied().collect();
        last_unresolved = unresolved_calls.clone();
        let added_entries = if unresolved_calls.is_empty() {
            BTreeSet::new()
        } else {
            autopromote_targets(&mut parsed, &unresolved_calls, &all_data_regions)
        };
        pending_variant_entries.extend(added_entries.iter().copied());
        let added = added_entries.len();

        if added == 0 && newly_pruned.is_empty() && variant_added == 0 {
            // Reference-taint prune (convergence guard).
            let ref_graph = scan_variant_refs(&results, &parsed);
            let mut ref_prunable: BTreeSet<VKey> = BTreeSet::new();
            loop {
                let mut combined = cumulative_pruned.clone();
                combined.extend(ref_prunable.iter().copied());
                let tainted =
                    propagate_reference_taint(&cumulative_dirty, &ref_graph, &emitted_now, &bank_set, &combined);
                let next: BTreeSet<VKey> = compute_prunable(&tainted, &emitted_now, &canonical_variants)
                    .difference(&cumulative_pruned)
                    .copied()
                    .filter(|k| !ref_prunable.contains(k))
                    .collect();
                if next.is_empty() {
                    break;
                }
                ref_prunable.extend(next);
            }
            if ref_prunable.is_empty() {
                break;
            }
            cumulative_pruned.extend(ref_prunable.iter().copied());
            valid_variants_map = apply_variant_prune(&mut parsed, &cumulative_pruned);
            println!(
                "  reference-taint prune: dropped {} caller clone(s) ({} total)",
                ref_prunable.len(),
                cumulative_pruned.len()
            );
            rt_total_passes += 1;
            if rt_total_passes >= RT_TOTAL_LIMIT {
                eprintln!("!!! reference-taint prune non-convergence ({rt_total_passes} passes)");
                std::process::exit(3);
            }
        }

        // Exit-mx refresh for next pass.
        for cfg in parsed.iter_mut() {
            cfg.exit_mx_at_per_variant.clear();
        }
        let _ = exit_mx_detect_and_route(&mut parsed, &rom, &dispatch_helpers, &reloc_regions);
        callee_exit_mx = rebuild_callee_exit_mx(&parsed, &variants);
        callee_exit_mx_modes = build_callee_exit_mx_modes(
            &parsed, &rom, &dispatch_helpers, &all_data_regions, &callee_exit_mx, &reloc_regions,
            &callee_inline_skip,
        );
        exit_mx_rescan_all = true;

        // Refresh name resolver with newly-synthesized entries.
        for cfg in &parsed {
            let bank = cfg.bank as u32;
            for e in &cfg.entries {
                if let Some(n) = &e.name {
                    name_map.insert((bank << 16) | (e.start & 0xFFFF), n.clone());
                }
            }
        }

        println!(
            "  auto-promote pass {}: added {} entries, {} refreshed variants (calls={})",
            pass_idx + 1,
            added,
            variant_added,
            unresolved_calls.len()
        );
    }

    dump_entry_fingerprint(&parsed, "final");
    if let Ok(dir) = std::env::var("SF_REGEN_DUMP") {
        let body: String =
            cumulative_pruned.iter().map(|(a, m, x)| format!("{a:06X}:{m}:{x}\n")).collect();
        let _ = std::fs::write(format!("{dir}/pruned.txt"), body);
        let body2: String =
            cumulative_dirty.iter().map(|(a, m, x)| format!("{a:06X}:{m}:{x}\n")).collect();
        let _ = std::fs::write(format!("{dir}/dirty.txt"), body2);
    }

    // ── Final stub file for cross-ROM-bank demands. ──
    let mut by_bank: BTreeMap<u32, BTreeSet<(u32, u8, u8)>> = BTreeMap::new();
    let mut cfg_entry_pcs: HashMap<u32, HashSet<u32>> = HashMap::new();
    for cfg in &parsed {
        cfg_entry_pcs
            .entry(cfg.bank as u32)
            .or_default()
            .extend(cfg.entries.iter().map(|e| e.start & 0xFFFF));
    }
    for &(addr, em, ex) in &last_unresolved {
        let bank = (addr >> 16) & 0xFF;
        if bank_set.contains(&bank) {
            continue;
        }
        let mirror = bank ^ 0x80;
        if (bank < 0x40 || (0x80..0xC0).contains(&bank)) && bank_set.contains(&mirror) {
            if cfg_entry_pcs.get(&mirror).map_or(false, |s| s.contains(&(addr & 0xFFFF))) {
                continue;
            }
        }
        by_bank.entry(bank).or_default().insert((addr & 0xFFFF, em & 1, ex & 1));
    }

    if only_banks.is_some() {
        // --banks: preserve existing stub/dispatch files; skip the rewrite.
        let final_elapsed = start_time.elapsed().as_secs_f64();
        println!("v2_regen: {succeeded}/{total} banks emitted (--banks filter); {final_elapsed:.1}s");
        std::process::exit(if failed.is_empty() { 0 } else { 1 });
    }

    write_stub_file(&out_dir, &by_bank);

    // ── PEI-trampoline dispatch table. ──
    write_dispatch_table(&out_dir, &parsed, &by_bank, &cumulative_pruned);

    println!();
    println!("v2_regen: {succeeded}/{total} banks emitted");
    if !failed.is_empty() {
        println!("failed banks:");
        for (b, m) in &failed {
            println!("  ${b:02X}: {m}");
        }
    }

    let final_elapsed = start_time.elapsed().as_secs_f64();
    println!("v2_regen wall-clock: {final_elapsed:.1}s");

    // ── Stub lint. ──
    let lint_hits = lint_stubs(&out_dir);
    if !lint_hits.is_empty() {
        println!("=== STUB LINT — {} stub(s) in emitted output ===", lint_hits.len());
        let mut by_marker: BTreeMap<String, usize> = BTreeMap::new();
        for (_p, _ln, marker, _t) in &lint_hits {
            *by_marker.entry(marker.clone()).or_default() += 1;
        }
        for (marker, n) in &by_marker {
            println!("  [{marker}] x{n}");
        }
        std::process::exit(1);
    }
    std::process::exit(if failed.is_empty() { 0 } else { 1 });
}

fn dump_entry_fingerprint(parsed: &[BankCfg], tag: &str) {
    if let Ok(dir) = std::env::var("SF_REGEN_DUMP") {
        let mut keys: Vec<(u32, u32, u8, u8, String)> = Vec::new();
        for cfg in parsed {
            let bank = cfg.bank as u32;
            for e in &cfg.entries {
                keys.push((
                    bank,
                    e.start & 0xFFFF,
                    e.entry_m & 1,
                    e.entry_x & 1,
                    e.name.clone().unwrap_or_default(),
                ));
            }
        }
        keys.sort();
        let body: String =
            keys.iter().map(|(b, s, m, x, n)| format!("{b:02X}:{s:04X}:{m}:{x}:{n}\n")).collect();
        let _ = std::fs::write(format!("{dir}/entries_{tag}.txt"), body);
    }
}

fn num_cpus_guess() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// auto_vectors expansion (bank 0 vector table).
fn expand_auto_vectors(parsed: &mut [BankCfg], rom: &[u8], name_map: &mut HashMap<u32, String>) {
    for cfg in parsed.iter_mut() {
        let bank = cfg.bank as u32;
        if !cfg.auto_vectors {
            continue;
        }
        if bank != 0 {
            continue;
        }
        let rom_off = 0x7FE0usize;
        if rom_off + 32 > rom.len() {
            continue;
        }
        let vec_at = |slot: usize| -> u32 {
            ((rom[rom_off + slot + 1] as u32) << 8) | (rom[rom_off + slot] as u32)
        };
        let seed = [
            ("I_RESET", vec_at(0x1C)),
            ("I_NMI", vec_at(0x0A)),
            ("I_IRQ", vec_at(0x0E)),
        ];
        let mut existing_starts: HashSet<u32> = cfg.entries.iter().map(|e| e.start & 0xFFFF).collect();
        let mut existing_names: HashSet<String> = cfg.entries.iter().filter_map(|e| e.name.clone()).collect();
        for (name, pc) in seed {
            if pc == 0x0000 || pc == 0xFFFF {
                continue;
            }
            if existing_starts.contains(&pc) || existing_names.contains(name) {
                continue;
            }
            cfg.entries.push(BankEntry::new(Some(name.to_string()), pc));
            name_map.insert((bank << 16) | pc, name.to_string());
            existing_starts.insert(pc);
            existing_names.insert(name.to_string());
        }
    }
}

/// Clone cfg entries for each extra discovered (m,x) variant.
fn clone_variant_entries(parsed: &mut [BankCfg], variants: &Variants) {
    for cfg in parsed.iter_mut() {
        let bank = cfg.bank as u32;
        let mut new_entries: Vec<BankEntry> = Vec::new();
        let mut seen_keys: HashSet<(u32, (u8, u8))> = HashSet::new();
        for entry in &cfg.entries {
            let addr = (bank << 16) | (entry.start & 0xFFFF);
            let decl_mx = (entry.entry_m & 1, entry.entry_x & 1);
            seen_keys.insert((addr, decl_mx));
            new_entries.push(entry.clone());
            if let Some(set) = variants.get(&addr) {
                let mut extras: Vec<(u8, u8)> =
                    set.iter().copied().filter(|mx| *mx != decl_mx).collect();
                extras.sort();
                for (em, ex) in extras {
                    let key = (addr, (em, ex));
                    if seen_keys.contains(&key) {
                        continue;
                    }
                    seen_keys.insert(key);
                    let mut ne = BankEntry::new(entry.name.clone(), entry.start);
                    ne.end = entry.end;
                    ne.entry_m = em;
                    ne.entry_x = ex;
                    new_entries.push(ne);
                }
            }
        }
        cfg.entries = new_entries;
    }
}

fn write_stub_file(out_dir: &Path, by_bank: &BTreeMap<u32, BTreeSet<(u32, u8, u8)>>) {
    let mut lines: Vec<String> = vec![
        "/* Auto-generated by snesrecomp v2 v2_regen. Do NOT hand-edit.".to_string(),
        " *".to_string(),
        " * Stub bodies for Call targets that resolved to a ROM bank not".to_string(),
        " * in the cfg set. These are typically data decoded as code".to_string(),
        " * (garbled JSL operands). Real execution paths should never".to_string(),
        " * reach them; each stub chains into cpu_trace_unresolved_stub_trap".to_string(),
        " * so a runtime fire is captured (loud stderr line + TCP-queryable".to_string(),
        " * snapshot via unresolved_stub_get) instead of silently returning.".to_string(),
        " * One stub per (target, m, x) variant requested by the gen.".to_string(),
        " *".to_string(),
        " * Always emitted — file may be empty (no stubs needed) when".to_string(),
        " * every (target, m, x) demand resolved within the cfg set.".to_string(),
        " */".to_string(),
        String::new(),
        "#include \"cpu_state.h\"".to_string(),
        "#include \"cpu_trace.h\"".to_string(),
        String::new(),
    ];
    let mut total = 0usize;
    for (&bank, set) in by_bank {
        for &(pc, em, ex) in set {
            let name = format!("bank_{bank:02X}_{pc:04X}_M{em}X{ex}");
            let target = (bank << 16) | (pc & 0xFFFF);
            lines.push(format!(
                "RecompReturn {name}(CpuState *cpu) {{ return cpu_trace_unresolved_stub_trap(cpu, 0x{target:06x}, \"{name}\"); }}"
            ));
            total += 1;
        }
    }
    let content = lines.join("\n") + "\n";
    write_if_changed(&out_dir.join("unresolved_stubs_v2.c"), &content);
    if total > 0 {
        println!("  emitted stubs for {total} cross-ROM-bank (target, m, x) variants");
    } else {
        println!("  no cross-ROM-bank stubs needed; emitted empty stub file");
    }
}

fn write_dispatch_table(
    out_dir: &Path,
    parsed: &[BankCfg],
    by_bank: &BTreeMap<u32, BTreeSet<(u32, u8, u8)>>,
    cumulative_pruned: &BTreeSet<VKey>,
) {
    let mut name_for_pc24: HashMap<u32, String> = HashMap::new();
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            if let Some(n) = &e.name {
                name_for_pc24.insert((bank << 16) | (e.start & 0xFFFF), n.clone());
            }
        }
    }
    let disp_name = |pc24: u32| -> String {
        name_for_pc24.get(&pc24).cloned().unwrap_or_else(|| {
            format!("bank_{:02X}_{:04X}", (pc24 >> 16) & 0xFF, pc24 & 0xFFFF)
        })
    };

    let mut disp_variants: BTreeMap<u32, BTreeSet<(u8, u8)>> = BTreeMap::new();
    for cfg in parsed {
        let bank = cfg.bank as u32;
        for e in &cfg.entries {
            let pc24 = (bank << 16) | (e.start & 0xFFFF);
            disp_variants.entry(pc24).or_default().insert((e.entry_m & 1, e.entry_x & 1));
        }
    }
    for (&bank, set) in by_bank {
        for &(pc, em, ex) in set {
            disp_variants.entry((bank << 16) | (pc & 0xFFFF)).or_default().insert((em, ex));
        }
    }
    for &(addr, em, ex) in cumulative_pruned {
        if let Some(s) = disp_variants.get_mut(&addr) {
            s.remove(&(em & 1, ex & 1));
        }
    }

    let sorted_pc24s: Vec<u32> = disp_variants.keys().copied().collect();
    let mut lines: Vec<String> = vec![
        "/* Auto-generated by snesrecomp v2 v2_regen. Do NOT hand-edit.".to_string(),
        " *".to_string(),
        " * PEI-trampoline dispatch table — runtime cpu_dispatch_pc() looks".to_string(),
        " * up function entries here when an RTS/RTL on a trampoline-flagged".to_string(),
        " * function hits the unbalanced-cpu->S branch in _emit_return.".to_string(),
        " *".to_string(),
        " * Sorted by pc24 for binary search. variant[] holds fnptrs for".to_string(),
        " * (M0X0, M0X1, M1X0, M1X1) — NULL when that variant wasn't emitted.".to_string(),
        " */".to_string(),
        String::new(),
        "#include \"cpu_state.h\"".to_string(),
        String::new(),
    ];
    let mut fwd_seen: HashSet<String> = HashSet::new();
    for &pc24 in &sorted_pc24s {
        let base = disp_name(pc24);
        for em in 0..=1u8 {
            for ex in 0..=1u8 {
                if disp_variants[&pc24].contains(&(em, ex)) {
                    let name = format!("{base}_M{em}X{ex}");
                    if fwd_seen.insert(name.clone()) {
                        lines.push(format!("RecompReturn {name}(CpuState *cpu);"));
                    }
                }
            }
        }
    }
    lines.push(String::new());
    lines.push("const DispatchEntry g_dispatch_table[] = {".to_string());
    if sorted_pc24s.is_empty() {
        lines.push("    { 0xFFFFFFu, { NULL, NULL, NULL, NULL } },  /* sentinel — empty cfg */".to_string());
    }
    for &pc24 in &sorted_pc24s {
        let base = disp_name(pc24);
        let mut slots = ["NULL".to_string(), "NULL".to_string(), "NULL".to_string(), "NULL".to_string()];
        for &(em, ex) in &disp_variants[&pc24] {
            let idx = (((em & 1) << 1) | (ex & 1)) as usize;
            slots[idx] = format!("{base}_M{}X{}", em & 1, ex & 1);
        }
        lines.push(format!(
            "    {{ 0x{pc24:06X}u, {{ {} }} }},  /* {base} */",
            slots.join(", ")
        ));
    }
    lines.push("};".to_string());
    lines.push(String::new());
    lines.push("const unsigned g_dispatch_table_count = (unsigned)(sizeof(g_dispatch_table) / sizeof(g_dispatch_table[0]));".to_string());
    lines.push(String::new());
    let content = lines.join("\n") + "\n";
    write_if_changed(&out_dir.join("dispatch_v2.c"), &content);
    println!("  emitted dispatch table with {} entries", sorted_pc24s.len());
}

fn lint_stubs(out_dir: &Path) -> Vec<(String, usize, String, String)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("bank") && n.ends_with("_v2.c"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    for fname in ["dispatch_v2.c", "unresolved_stubs_v2.c"] {
        let p = out_dir.join(fname);
        if p.exists() {
            paths.push(p);
        }
    }
    let mut hits = Vec::new();
    for p in &paths {
        if let Ok(text) = std::fs::read_to_string(p) {
            for (ln, raw) in text.lines().enumerate() {
                for marker in STUB_MARKERS {
                    if raw.contains(marker) {
                        hits.push((p.display().to_string(), ln + 1, marker.to_string(), raw.to_string()));
                        break;
                    }
                }
            }
        }
    }
    hits
}
