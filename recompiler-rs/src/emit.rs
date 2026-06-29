//! Per-function + per-bank emit drivers (port of `recompiler/v2/emit_function.py`
//! and `emit_bank.py`).
//!
//! Under the controlled oracle env the NLR-skip and PEI-trampoline classification
//! machinery is INERT (`nlr_skip_by_block` stays empty and `_emit_return` never
//! consults the trampoline set), so this port omits that dead-for-output code and
//! emits every PLA/PLP and Return normally — matching the Python output exactly.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::cfgbuild::build_cfg;
use crate::codegen::{emit_op, reg_field, variant_suffix, EmitCtx, EmitOutcome};
use crate::cycles::estimate_cycles;
use crate::decoder::{decode_function, DecodeEnv, DecodeKey};
use crate::insn::{Insn, Mode};
use crate::ir::IROp;
use crate::lowering::{lower, ValueFactory};

/// Optional HLE / boundary directives the controlled oracle env leaves unset.
/// Bank-level fields (`hle_*`, `exclude_ranges`) plus per-entry tail-call fields.
/// `Default` = all unset, which reproduces the oracle env exactly.
#[derive(Default, Clone, Copy)]
pub struct EmitHle<'a> {
    pub hle_spc_upload: Option<&'a BTreeSet<u32>>,
    pub hle_func: Option<&'a BTreeMap<u32, String>>,
    pub hle_dispatch: Option<&'a BTreeMap<u32, String>>,
    pub exclude_ranges: Option<&'a [(u32, u32)]>,
    pub tail_call_pc16: Option<u32>,
    pub tail_call_target_name: Option<&'a str>,
}

fn label_for(key: &DecodeKey) -> String {
    let pc = key.pc & 0xFFFF;
    format!("L_{pc:04X}_M{}X{}", key.m, key.x)
}

fn default_func_name(bank: u32, start: u32) -> String {
    format!("bank_{bank:02X}_{start:04X}")
}

/// `_tail_call_stmt`.
fn tail_call_stmt(call_expr: &str, comment: &str, prefix: &str) -> String {
    format!(
        "{prefix}{{ cpu->host_return_valid = _hrv; cpu_tailcall_inherit_return_context(_entry_s, _hrv); RecompReturn _tc = {call_expr}; RecompStackPop(); return _tc; }}  {comment}"
    )
}

#[allow(clippy::too_many_arguments)]
fn goto_or_return(
    ctx: &EmitCtx,
    target: &DecodeKey,
    prefix: &str,
    source_pc24: Option<u32>,
    same_bank: u32,
    func_name: &str,
    local_labels: &HashSet<String>,
    hle: &EmitHle,
    outcome: &mut EmitOutcome,
) -> String {
    let label = label_for(target);
    if local_labels.contains(&label) {
        return format!("{prefix}goto {label};");
    }
    // cfg exclude_range: target is HLE-replaced data -> return to host.
    if let Some(ranges) = hle.exclude_ranges {
        let tpc = target.pc & 0xFFFF;
        for &(lo, hi) in ranges {
            if lo <= tpc && tpc < hi {
                return format!(
                    "{prefix}return RECOMP_RETURN_NORMAL; /* {label} HLE-replaced (cfg exclude_range {lo:04X}-{hi:04X}) */"
                );
            }
        }
    }
    // cfg tail_call: directive — declared sibling fall-through.
    if let (Some(tc), Some(tname)) = (hle.tail_call_pc16, hle.tail_call_target_name) {
        if (target.pc & 0xFFFF) == (tc & 0xFFFF) {
            let sib_suffix = variant_suffix(target.m, target.x);
            let tail_pc24 = (same_bank << 16) | (target.pc & 0xFFFF);
            outcome.unresolved_call_targets.insert((tail_pc24, target.m & 1, target.x & 1));
            return tail_call_stmt(
                &format!("{tname}{sib_suffix}(cpu)"),
                &format!(
                    "/* tail_call into sibling fn at ${:04X} (cfg tail_call: directive) */",
                    target.pc & 0xFFFF
                ),
                prefix,
            );
        }
    }
    let target_pc24 = (same_bank << 16) | (target.pc & 0xFFFF);
    let src_pc24 = source_pc24.unwrap_or(0);
    if let Some(sibling_name) = ctx.get_name_for_pc(target_pc24) {
        let sibling_name = sibling_name.to_string();
        let sib_suffix = variant_suffix(target.m, target.x);
        outcome.unresolved_call_targets.insert((target_pc24, target.m & 1, target.x & 1));
        return tail_call_stmt(
            &format!("{sibling_name}{sib_suffix}(cpu)"),
            &format!(
                "/* tail-call past end: into {sibling_name}{sib_suffix} at ${:04X} */",
                target.pc & 0xFFFF
            ),
            prefix,
        );
    }
    format!(
        "{prefix}return cpu_trace_unresolved_goto_trap(cpu, 0x{src_pc24:06X}, 0x{target_pc24:06X}, \"{func_name}\", \"{label}\"); /* {label} unresolvable cross-fn goto — target outside this bank's import range */"
    )
}

/// Emit a complete v2 C function source for one 65816 function.
#[allow(clippy::too_many_arguments)]
pub fn emit_function(
    ctx: &EmitCtx,
    rom: &[u8],
    bank: u32,
    start: u32,
    entry_m: u8,
    entry_x: u8,
    end: Option<u32>,
    func_name: Option<&str>,
    entry_s_offset: i32,
    env: &DecodeEnv,
    hle: &EmitHle,
    outcome: &mut EmitOutcome,
) -> String {
    let base_func_name = func_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_func_name(bank, start));

    // HLE bypass: whole-function SPC-upload stub.
    if let Some(set) = hle.hle_spc_upload {
        if set.contains(&(start & 0xFFFF)) {
            let variant_name = format!("{base_func_name}{}", variant_suffix(entry_m, entry_x));
            let pc24 = ((bank & 0xFF) << 16) | (start & 0xFFFF);
            return [
                format!("RecompReturn {variant_name}(CpuState *cpu) {{"),
                "  extern const char *g_last_recomp_func;".to_string(),
                "  extern bool RtlUploadSpcImageFromDp(CpuState *cpu);".to_string(),
                format!("  g_last_recomp_func = \"{variant_name}\";"),
                format!("  RecompStackPush(\"{variant_name}\");"),
                format!("  cpu_dbg_funcname(\"{variant_name}\");"),
                format!("  cpu_trace_func_entry(cpu, 0x{pc24:06X}, \"{variant_name}\");"),
                format!("  cpu_trace_block(cpu, 0x{pc24:06X});"),
                "  WatchdogCheck();".to_string(),
                "  if (!RtlUploadSpcImageFromDp(cpu)) {".to_string(),
                format!("    fprintf(stderr, \"[apu] {base_func_name} HLE upload failed\\n\");"),
                "  }".to_string(),
                "  RecompStackPop();".to_string(),
                "  return RECOMP_RETURN_NORMAL;".to_string(),
                "}".to_string(),
                String::new(),
            ]
            .join("\n");
        }
    }
    // Generic HLE: forward to a named C helper.
    if let Some(map) = hle.hle_func {
        if let Some(c_helper) = map.get(&(start & 0xFFFF)) {
            let variant_name = format!("{base_func_name}{}", variant_suffix(entry_m, entry_x));
            let pc24 = ((bank & 0xFF) << 16) | (start & 0xFFFF);
            return [
                format!("RecompReturn {variant_name}(CpuState *cpu) {{"),
                "  extern const char *g_last_recomp_func;".to_string(),
                format!("  extern RecompReturn {c_helper}(CpuState *cpu);"),
                format!("  g_last_recomp_func = \"{variant_name}\";"),
                format!("  RecompStackPush(\"{variant_name}\");"),
                format!("  cpu_dbg_funcname(\"{variant_name}\");"),
                format!("  cpu_trace_func_entry(cpu, 0x{pc24:06X}, \"{variant_name}\");"),
                format!("  cpu_trace_block(cpu, 0x{pc24:06X});"),
                "  WatchdogCheck();".to_string(),
                format!("  RecompReturn _r = {c_helper}(cpu);"),
                "  RecompStackPop();".to_string(),
                "  return _r;".to_string(),
                "}".to_string(),
                String::new(),
            ]
            .join("\n");
        }
    }

    let graph = decode_function(rom, bank, start, entry_m, entry_x, end, env);
    let cfg = build_cfg(&graph);

    let func_name = format!("{base_func_name}{}", variant_suffix(entry_m, entry_x));

    let mut vf = ValueFactory::new();

    // Block order: pre-order DFS from entry following successors.
    let mut block_order: Vec<DecodeKey> = Vec::new();
    let mut visited: HashSet<DecodeKey> = HashSet::new();
    let mut stack: Vec<DecodeKey> = vec![cfg.entry.clone()];
    while let Some(k) = stack.pop() {
        if visited.contains(&k) || !cfg.blocks.contains_key(&k) {
            continue;
        }
        visited.insert(k.clone());
        block_order.push(k.clone());
        // Push successors reversed so they pop in original order.
        let succs = &cfg.blocks[&k].successors;
        for s in succs.iter().rev() {
            if !visited.contains(s) && cfg.blocks.contains_key(s) {
                stack.push(s.clone());
            }
        }
    }
    // Any block not reached via DFS (defensive).
    for k in cfg.blocks.keys() {
        if !visited.contains(k) {
            block_order.push(k.clone());
        }
    }

    let local_labels: HashSet<String> = block_order.iter().map(label_for).collect();

    // Pre-lower IR for every block (advances vf exactly once per insn, in order).
    // pairs: per block, Vec<(Insn, Vec<IROp>)>.
    let mut block_pairs: Vec<(DecodeKey, Vec<(Insn, Vec<IROp>)>)> = Vec::new();
    for key in &block_order {
        let mut pairs: Vec<(Insn, Vec<IROp>)> = Vec::new();
        for di in &cfg.blocks[key].insns {
            let ops = lower(&di.insn, &mut vf);
            pairs.push((di.insn.clone(), ops));
        }
        block_pairs.push((key.clone(), pairs));
    }

    let same_bank = bank & 0xFF;

    // Emit each block's body.
    let mut block_lines: Vec<(DecodeKey, Vec<String>)> = Vec::new();
    for (bi, key) in block_order.iter().enumerate() {
        let block = &cfg.blocks[key];
        let pairs = &block_pairs[bi].1;
        let mut lines: Vec<String> = Vec::new();
        let mut block_terminated = false;

        // Pre-scan #1: JML-with-dispatch_entries -> skip preceding PHK/PEA/PER.
        let mut skip_emit_idx: HashSet<usize> = HashSet::new();
        for (ji, (jdi, _)) in pairs.iter().enumerate() {
            if jdi.mnem == "JMP" && jdi.length == 4 && jdi.dispatch_entries.is_some() {
                let mut k2: i64 = ji as i64 - 1;
                while k2 >= 0 {
                    let prev = &pairs[k2 as usize].0;
                    if matches!(prev.mnem, "PHK" | "PEA" | "PER") {
                        skip_emit_idx.insert(k2 as usize);
                        k2 -= 1;
                    } else {
                        break;
                    }
                }
            }
        }

        let blk_pc24 = (bank << 16) | (key.pc & 0xFFFF);

        for (ii, (di_insn, ir_ops)) in pairs.iter().enumerate() {
            if skip_emit_idx.contains(&ii) {
                lines.push(format!(
                    "/* trampoline setup {} skipped — inlined into synthesized dispatch below */",
                    di_insn.mnem
                ));
                continue;
            }
            for op in ir_ops {
                match op {
                    IROp::CondBranch { flag, take_if } => {
                        let succs = &block.successors;
                        let fall = succs.first();
                        let taken = succs.get(1);
                        let pred = format!("{} == {take_if}", reg_field(*flag));
                        if let Some(taken) = taken {
                            let target_stmt = goto_or_return(
                                ctx, taken, "", Some(blk_pc24), same_bank, &func_name,
                                &local_labels, hle, outcome,
                            );
                            lines.push(format!("if ({pred}) {{ {target_stmt} }}"));
                        }
                        if let Some(fall) = fall {
                            let s = goto_or_return(
                                ctx, fall, "", Some(blk_pc24), same_bank, &func_name,
                                &local_labels, hle, outcome,
                            );
                            lines.push(format!("{s} /* fall-through */"));
                            block_terminated = true;
                        }
                    }
                    IROp::Goto => {
                        let insn = di_insn;
                        if insn.dispatch_entries.is_some() {
                            for ln in ctx.emit_dispatch(insn, outcome) {
                                lines.push(ln);
                            }
                            block_terminated = true;
                        } else {
                            let succs = &block.successors;
                            if !succs.is_empty() {
                                lines.push(goto_or_return(
                                    ctx, &succs[0], "", Some(blk_pc24), same_bank,
                                    &func_name, &local_labels, hle, outcome,
                                ));
                                block_terminated = true;
                            } else {
                                // Cross-bank JML / out-of-import-range goto.
                                let mut target_pc24: Option<u32> = None;
                                if insn.mnem == "JMP" && insn.mode == Mode::Long {
                                    target_pc24 = Some(insn.operand & 0xFFFFFF);
                                }
                                let mut handled = false;
                                if let Some(tpc) = target_pc24 {
                                    if let Some(tgt_name) = ctx.get_name_for_pc(tpc) {
                                        let tgt_name = tgt_name.to_string();
                                        let sib_suffix = variant_suffix(key.m, key.x);
                                        outcome
                                            .unresolved_call_targets
                                            .insert((tpc, key.m & 1, key.x & 1));
                                        let tgt_bank = (tpc >> 16) & 0xFF;
                                        lines.push(format!(
                                            "cpu->PB = 0x{tgt_bank:02X}; /* JML into bank ${tgt_bank:02X} */"
                                        ));
                                        lines.push(tail_call_stmt(
                                            &format!("{tgt_name}{sib_suffix}(cpu)"),
                                            &format!(
                                                "/* tail-call cross-bank into {tgt_name}{sib_suffix} at ${tpc:06X} (JML unresolved successor) */"
                                            ),
                                            "",
                                        ));
                                        block_terminated = true;
                                        handled = true;
                                    }
                                }
                                if !handled {
                                    let src_pc24 = blk_pc24;
                                    let tgt_str = match target_pc24 {
                                        Some(t) => format!("0x{t:06X}"),
                                        None => "0x000000".to_string(),
                                    };
                                    if let Some(tpc) = target_pc24 {
                                        if ((tpc >> 16) & 0xFF) != bank {
                                            if ctx_is_invalid(ctx, tpc)
                                                && ctx.get_name_for_pc(tpc).is_none()
                                            {
                                                lines.push(format!(
                                                    "return RECOMP_RETURN_NORMAL; /* cross-bank JML to ${tpc:06X} skipped — not a valid LoROM code address (decoder followed garbage operand past an RTS) */"
                                                ));
                                            } else {
                                                outcome
                                                    .unresolved_call_targets
                                                    .insert((tpc, key.m & 1, key.x & 1));
                                                let sib_suffix = variant_suffix(key.m, key.x);
                                                let tgt_bank = (tpc >> 16) & 0xFF;
                                                let tgt16 = tpc & 0xFFFF;
                                                let synth_name =
                                                    format!("bank_{tgt_bank:02X}_{tgt16:04X}");
                                                lines.push(format!(
                                                    "cpu->PB = 0x{tgt_bank:02X}; /* JML into bank ${tgt_bank:02X} */"
                                                ));
                                                lines.push(tail_call_stmt(
                                                    &format!("{synth_name}{sib_suffix}(cpu)"),
                                                    &format!(
                                                        "/* tail-call cross-bank into {synth_name}{sib_suffix} at ${tpc:06X} (auto-promoted via Call demand) */"
                                                    ),
                                                    "",
                                                ));
                                            }
                                        } else {
                                            lines.push(unresolved_cross_bank_trap(
                                                src_pc24, &tgt_str, &func_name, key,
                                            ));
                                        }
                                    } else {
                                        lines.push(unresolved_cross_bank_trap(
                                            src_pc24, &tgt_str, &func_name, key,
                                        ));
                                    }
                                    block_terminated = true;
                                }
                            }
                        }
                    }
                    IROp::Return(_) => {
                        for ln in emit_op(ctx, op, outcome) {
                            lines.push(ln);
                        }
                        block_terminated = true;
                    }
                    IROp::IndirectGoto { .. } => {
                        let insn = di_insn;
                        if insn.dispatch_entries.is_some() {
                            for ln in ctx.emit_indirect_dispatch(insn, Some(&local_labels), outcome) {
                                lines.push(ln);
                            }
                            block_terminated = true;
                        } else {
                            let site_pc24 = insn.addr & 0xFFFFFF;
                            let site_pc16 = site_pc24 & 0xFFFF;
                            let helper = hle.hle_dispatch.and_then(|m| m.get(&site_pc16));
                            if let Some(c_helper) = helper {
                                lines.push(format!(
                                    "{{ extern RecompReturn {c_helper}(CpuState *cpu); RecompReturn _r = {c_helper}(cpu); RecompStackPop(); return _r; }} /* hle_dispatch ${site_pc24:06X} — host-side dispatcher */"
                                ));
                            } else {
                                lines.push(format!(
                                    "return cpu_trace_dispatch_oob(cpu, 0x{site_pc24:06x}, 0xFFFF); /* unresolved IndirectGoto — HLE pending */"
                                ));
                            }
                            block_terminated = true;
                        }
                    }
                    IROp::PushReg { .. } if di_insn.dispatch_entries.is_some() => {
                        for ln in ctx.emit_indirect_dispatch(di_insn, Some(&local_labels), outcome) {
                            lines.push(ln);
                        }
                        block_terminated = true;
                    }
                    IROp::Call(_) => {
                        let insn = di_insn;
                        if insn.dispatch_entries.is_some() {
                            if matches!(insn.dispatch_idx_reg, Some('X') | Some('Y')) {
                                for ln in
                                    ctx.emit_indirect_dispatch(insn, Some(&local_labels), outcome)
                                {
                                    lines.push(ln);
                                }
                                if insn.mnem != "JSR" {
                                    block_terminated = true;
                                }
                            } else {
                                for ln in ctx.emit_dispatch(insn, outcome) {
                                    lines.push(ln);
                                }
                                block_terminated = true;
                            }
                        } else {
                            for ln in emit_op(ctx, op, outcome) {
                                lines.push(ln);
                            }
                        }
                    }
                    _ => {
                        for ln in emit_op(ctx, op, outcome) {
                            lines.push(ln);
                        }
                    }
                }
            }
        }

        if !block_terminated {
            let succs = &block.successors;
            if succs.len() == 1 {
                let s = goto_or_return(
                    ctx, &succs[0], "", Some(blk_pc24), same_bank, &func_name, &local_labels, hle,
                    outcome,
                );
                lines.push(format!("{s} /* implicit fall-through */"));
            } else if succs.len() > 1
                && !pairs.is_empty()
                && matches!(pairs.last().unwrap().0.mnem, "JSR" | "JSL")
            {
                lines.push("switch (((cpu->m_flag & 1) << 1) | (cpu->x_flag & 1)) {".to_string());
                let mut seen_mx: HashSet<(u8, u8)> = HashSet::new();
                let mut fallback_stmt: Option<String> = None;
                for succ in succs {
                    let mx = (succ.m & 1, succ.x & 1);
                    if seen_mx.contains(&mx) {
                        continue;
                    }
                    seen_mx.insert(mx);
                    let stmt = goto_or_return(
                        ctx, succ, "", Some(blk_pc24), same_bank, &func_name, &local_labels, hle,
                        outcome,
                    );
                    if fallback_stmt.is_none() {
                        fallback_stmt = Some(stmt.clone());
                    }
                    let idx = (mx.0 << 1) | mx.1;
                    lines.push(format!(
                        "  case {idx}: {stmt} /* dynamic post-call M{}X{} */",
                        mx.0, mx.1
                    ));
                }
                if let Some(fb) = fallback_stmt {
                    lines.push(format!("  default: {fb}"));
                }
                lines.push("}".to_string());
            } else if succs.len() > 1 {
                let s = goto_or_return(
                    ctx, &succs[0], "", Some(blk_pc24), same_bank, &func_name, &local_labels, hle,
                    outcome,
                );
                lines.push(format!("{s} /* implicit fall-through */"));
            } else {
                lines.push("return RECOMP_RETURN_NORMAL; /* no terminator, no successor */".to_string());
            }
        }

        block_lines.push((key.clone(), lines));
    }

    // Compose the function source.
    let mut src: Vec<String> = Vec::new();
    src.push(format!("RecompReturn {func_name}(CpuState *cpu) {{"));
    src.push("  extern const char *g_last_recomp_func;".to_string());
    src.push(format!("  g_last_recomp_func = \"{func_name}\";"));
    src.push(format!("  RecompStackPush(\"{func_name}\");"));
    src.push(format!("  cpu_dbg_funcname(\"{func_name}\");"));
    let fn_entry_pc = (bank << 16) | (start & 0xFFFF);
    src.push(format!("  cpu_trace_func_entry(cpu, 0x{fn_entry_pc:06X}, \"{func_name}\");"));
    src.push("  RecompReturn _pending_skip = RECOMP_RETURN_NORMAL;".to_string());
    src.push("  (void)_pending_skip;  /* unused if no NLR site in this fn */".to_string());
    if entry_s_offset != 0 {
        let sign = if entry_s_offset > 0 { '+' } else { '-' };
        src.push(format!(
            "  uint16 _entry_s = (uint16)(cpu->S {sign} {}u);  /* entry_s_offset:{entry_s_offset} — caller left stack imbalanced */",
            entry_s_offset.abs()
        ));
    } else {
        src.push("  uint16 _entry_s = cpu->S;".to_string());
    }
    src.push("  uint8 _hrv = cpu->host_return_valid;".to_string());
    src.push("  if (cpu_take_tailcall_return_context(&_entry_s, &_hrv)) {".to_string());
    src.push("    cpu->host_return_valid = _hrv;".to_string());
    src.push("  }".to_string());
    src.push("  (void)_entry_s;  /* used by trampoline balance check */".to_string());
    src.push("  (void)_hrv;".to_string());
    src.push("  if (g_recomp_stack_top >= 1) g_cpu_entry_s[g_recomp_stack_top - 1] = _entry_s;".to_string());

    for (bi, key) in block_order.iter().enumerate() {
        src.push(format!("  {}:", label_for(key)));
        let block_pc24 = (bank << 16) | (key.pc & 0xFFFF);
        src.push(format!("    cpu_trace_block(cpu, 0x{block_pc24:06X});"));
        src.push("    WatchdogCheck();".to_string());
        let blk_cyc: u32 = cfg.blocks[key].insns.iter().map(|d| estimate_cycles(&d.insn)).sum();
        if blk_cyc != 0 {
            src.push(format!("    cpu_cycle_tick(cpu, {blk_cyc});"));
        }
        for ln in &block_lines[bi].1 {
            if ln.trim_start().starts_with("return") {
                src.push("    RecompStackPop();".to_string());
            }
            src.push(format!("    {ln}"));
        }
    }
    src.push("  RecompStackPop();".to_string());
    src.push("  return RECOMP_RETURN_NORMAL;".to_string());
    src.push("}".to_string());
    let mut out = src.join("\n");
    out.push('\n');
    out
}

fn unresolved_cross_bank_trap(
    src_pc24: u32,
    tgt_str: &str,
    func_name: &str,
    key: &DecodeKey,
) -> String {
    format!(
        "return cpu_trace_unresolved_goto_trap(cpu, 0x{src_pc24:06X}, {tgt_str}, \"{func_name}\", \"L_{:04X}_M{}X{}\");  /* unresolvable cross-bank goto — no named function at target */",
        key.pc & 0xFFFF,
        key.m,
        key.x
    )
}

/// Bridge to EmitCtx's private invalid-target check (re-exposed for emit.rs).
fn ctx_is_invalid(ctx: &EmitCtx, addr_24: u32) -> bool {
    ctx.is_invalid_lorom_call_target_pub(addr_24)
}

// ── emit_bank ────────────────────────────────────────────────────────────────

/// One function to emit in a bank (mirrors `emit_bank.BankEntry`).
#[derive(Debug, Clone)]
pub struct BankEntrySpec {
    pub name: Option<String>,
    pub start: u32,
    pub end: Option<u32>,
    pub entry_m: u8,
    pub entry_x: u8,
    pub tail_call_pc16: Option<u32>,
    pub entry_s_offset: i32,
}

fn default_func_name_local(bank: u32, start: u32) -> String {
    format!("bank_{bank:02X}_{start:04X}")
}

fn default_file_header(bank: u32) -> String {
    format!(
        "/* Auto-generated by snesrecomp v2 emit_bank. Do NOT hand-edit.\n \
         *\n \
         * Bank ${bank:02X}. Each function below mutates the shared CpuState\n \
         * struct via cpu->A / cpu->X / etc. Memory access goes through the\n \
         * cpu_read{{8,16}} / cpu_write{{8,16}} helpers in cpu_state.h.\n \
         */\n\
         \n\
         #include <stdio.h>\n\
         #include <stdlib.h>\n\
         \n\
         #include \"cpu_state.h\"\n\
         #include \"cpu_trace.h\"\n\
         #include \"common_cpu_infra.h\"\n\
         #include \"funcs.h\"\n"
    )
}

/// Emit one bank's C source (mirrors `emit_bank.emit_bank`). `hle` carries the
/// bank-level HLE / exclude_range directives; per-entry tail-call resolution and
/// the per-entry sibling-entry set are constructed here.
#[allow(clippy::too_many_arguments)]
pub fn emit_bank(
    ctx: &EmitCtx,
    rom: &[u8],
    bank: u32,
    entries: &[BankEntrySpec],
    env: &DecodeEnv,
    hle: &EmitHle,
    file_header: Option<&str>,
    outcome: &mut EmitOutcome,
) -> String {
    let header =
        file_header.map(|s| s.to_string()).unwrap_or_else(|| default_file_header(bank));
    let mut parts: Vec<String> = vec![header, String::new()];

    parts.push("/* Forward declarations for in-bank entries. */".to_string());
    for entry in entries {
        let base = entry
            .name
            .clone()
            .unwrap_or_else(|| default_func_name_local(bank, entry.start));
        let suffix = variant_suffix(entry.entry_m, entry.entry_x);
        parts.push(format!("RecompReturn {base}{suffix}(CpuState *cpu);"));
    }
    parts.push(String::new());

    // start_pc16 -> base name (for tail_call: sibling resolution).
    let by_start: std::collections::HashMap<u32, String> = entries
        .iter()
        .map(|e| {
            let b = e.name.clone().unwrap_or_else(|| default_func_name_local(bank, e.start));
            (e.start & 0xFFFF, b)
        })
        .collect();
    // Per-entry sibling-entry set = all entry PCs minus this entry's own.
    let all_entry_pcs: BTreeSet<u32> = entries.iter().map(|e| e.start & 0xFFFF).collect();
    let sibling_sets: Vec<BTreeSet<u32>> = entries
        .iter()
        .map(|e| {
            let mut s = all_entry_pcs.clone();
            s.remove(&(e.start & 0xFFFF));
            s
        })
        .collect();

    for (i, entry) in entries.iter().enumerate() {
        let tail_call_target_name: Option<String> = match entry.tail_call_pc16 {
            Some(tc) => {
                let tgt = tc & 0xFFFF;
                match by_start.get(&tgt) {
                    Some(n) => Some(n.clone()),
                    None => panic!(
                        "bank ${bank:02X}: func {:?} at ${:04X} declares tail_call:${tgt:04X} but no sibling func entry exists at that PC. Add the sibling as its own `func` line.",
                        entry.name, entry.start
                    ),
                }
            }
            None => None,
        };
        let mut e_env = env.clone();
        e_env.sibling_entry_pcs = Some(&sibling_sets[i]);
        let e_hle = EmitHle {
            tail_call_pc16: entry.tail_call_pc16,
            tail_call_target_name: tail_call_target_name.as_deref(),
            ..*hle
        };
        let src = emit_function(
            ctx,
            rom,
            bank,
            entry.start,
            entry.entry_m,
            entry.entry_x,
            entry.end,
            entry.name.as_deref(),
            entry.entry_s_offset,
            &e_env,
            &e_hle,
            outcome,
        );
        parts.push(src);
        parts.push(String::new());
    }

    let mut aliased: HashSet<String> = HashSet::new();
    let mut any_alias = false;
    for entry in entries {
        let name = match &entry.name {
            Some(n) => n,
            None => continue,
        };
        if aliased.contains(name) {
            continue;
        }
        let suffix = variant_suffix(entry.entry_m, entry.entry_x);
        aliased.insert(name.clone());
        any_alias = true;
        parts.push(format!(
            "void {name}(CpuState *cpu) {{\n  RecompReturn _r = {name}{suffix}(cpu);\n  if (_r != RECOMP_RETURN_NORMAL) {{\n    fprintf(stderr,\n      \"[recomp] non-local-return SKIP_%d leaked past void alias %s\\n\",\n      (int)_r, \"{name}\");\n    abort();\n  }}\n}}"
        ));
    }
    if any_alias {
        parts.push(String::new());
    }

    parts.join("\n")
}

// Test helper: emit a single function with a default ctx/env.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::DecodeEnv;

    fn rom_at_8000(bytes: &[u8]) -> Vec<u8> {
        let mut rom = bytes.to_vec();
        rom.resize(0x10000, 0);
        rom
    }

    #[test]
    fn leaf_lda_rts_emits() {
        // LDA #$01 ; RTS
        let rom = rom_at_8000(&[0xA9, 0x01, 0x60]);
        let ctx = EmitCtx::default();
        let env = DecodeEnv::default();
        let mut outcome = EmitOutcome::default();
        let hle = EmitHle::default();
        let s = emit_function(
            &ctx, &rom, 0x00, 0x8000, 1, 1, None, None, 0, &env, &hle, &mut outcome,
        );
        assert!(s.contains("RecompReturn bank_00_8000_M1X1(CpuState *cpu)"));
        assert!(s.contains("cpu_write_a_m(cpu, (uint16)(_v1));"));
        assert!(s.contains("RTS pop hardware return frame"));
    }

    #[test]
    fn label_format() {
        let k = DecodeKey::new((0x01 << 16) | 0x9234, 1, 0);
        assert_eq!(label_for(&k), "L_9234_M1X0");
    }
}
