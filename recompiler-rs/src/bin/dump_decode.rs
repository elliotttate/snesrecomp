//! `dump-decode` — Rust side of the differential oracle.
//!
//! Loads the ROM + cfg dir, builds the SAME controlled `DecodeEnv` as
//! `scripts/dump_decode.py` (aggregate data_regions / reloc_regions /
//! callee_inline_skip / indirect_dispatch / inline_dispatch_loop_pcs; everything
//! else None), decodes every cfg entry, and emits JSON in the identical shape so
//! `scripts/diff_decode.py` can compare against `golden/decode.json`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use snesrecomp_regen::cfg::load_bank_cfg;
use snesrecomp_regen::decoder::{
    decode_function, DecodeEnv, DecodeKey, DecodedInsn, FunctionDecodeGraph, IndirectDispatchSite,
};
use snesrecomp_regen::rom::{load_rom, RelocRegion};

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn key_str(k: &DecodeKey) -> String {
    let ps: Vec<String> = k.p_stack.iter().map(|(m, x)| format!("{m}{x}")).collect();
    format!("{:06X}:{}:{}:[{}]", k.pc, k.m, k.x, ps.join(";"))
}

fn insn_obj(di: &DecodedInsn) -> Value {
    let ins = &di.insn;
    let mut succ: Vec<String> = di.successors.iter().map(key_str).collect();
    succ.sort();
    json!({
        "addr": ins.addr,
        "op": ins.opcode,
        "mnem": ins.mnem,
        "mode": ins.mode.index(),
        "operand": ins.operand,
        "length": ins.length,
        "m": ins.m_flag,
        "x": ins.x_flag,
        "succ": succ,
    })
}

fn graph_obj(g: &FunctionDecodeGraph) -> Value {
    let mut insns = Map::new();
    for di in g.insns() {
        insns.insert(key_str(&di.key), insn_obj(di));
    }
    let mut unresolved: Vec<u32> = g.unresolved_indirects.iter().map(|u| u.site_pc24).collect();
    unresolved.sort();
    let mut suppressed: Vec<u32> =
        g.suppressed_indirect_calls.iter().map(|s| s.site_pc24).collect();
    suppressed.sort();
    let mut dsupp: Vec<(u32, u32)> = g
        .dispatch_targets_suppressed
        .iter()
        .map(|d| (d.site_pc24, d.target_pc24))
        .collect();
    dsupp.sort();
    let dsupp_v: Vec<Value> = dsupp.iter().map(|(a, b)| json!([a, b])).collect();

    json!({
        "entry": g.entry.as_ref().map(key_str).unwrap_or_default(),
        "insns": Value::Object(insns),
        "unresolved": unresolved,
        "suppressed_calls": suppressed,
        "dispatch_suppressed": dsupp_v,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rom_path = arg_value(&args, "--rom").expect("--rom required");
    let cfg_dir = arg_value(&args, "--cfg-dir").expect("--cfg-dir required");
    let out_path = arg_value(&args, "--out").expect("--out required");

    let rom = load_rom(&rom_path).expect("load rom");

    // Glob bank*.cfg sorted (matches Python sorted(glob)).
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

    let mut cfgs = Vec::new();
    for p in &cfg_paths {
        match load_bank_cfg(p) {
            Ok(c) => cfgs.push(c),
            Err(e) => eprintln!("  parse-fail {}: {e}", p.display()),
        }
    }

    // Aggregate the controlled env across all cfgs.
    let mut data_regions: Vec<(u32, u32, u32)> = Vec::new();
    let mut reloc_regions: Vec<RelocRegion> = Vec::new();
    let mut callee_inline_skip: HashMap<u32, i32> = HashMap::new();
    let mut inline_loop_pcs: BTreeSet<u32> = BTreeSet::new();
    let mut indirect_dispatch: HashMap<u32, IndirectDispatchSite> = HashMap::new();

    for c in &cfgs {
        let bank = c.bank as u32;
        data_regions.extend(c.data_regions.iter().copied());
        reloc_regions.extend(c.reloc_regions.iter().copied());
        for e in &c.entries {
            if let Some(skip) = e.inline_skip {
                callee_inline_skip.insert((bank << 16) | (e.start & 0xFFFF), skip);
            }
        }
        for &pc16 in &c.inline_dispatch_loops {
            inline_loop_pcs.insert((bank << 16) | pc16);
        }
        for d in &c.indirect_dispatch {
            let site = (bank << 16) | d.site_pc16;
            indirect_dispatch.insert(
                site,
                IndirectDispatchSite {
                    count: d.count,
                    idx_reg: d.idx_reg,
                    table_bases: d.table_bases.clone(),
                },
            );
        }
    }

    let env = DecodeEnv {
        data_regions: Some(&data_regions),
        reloc_regions: Some(&reloc_regions),
        callee_inline_skip: Some(&callee_inline_skip),
        indirect_dispatch: Some(&indirect_dispatch),
        inline_dispatch_loop_pcs: Some(&inline_loop_pcs),
        ..Default::default()
    };

    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let mut n_ok = 0usize;
    let mut n_err = 0usize;
    for c in &cfgs {
        let bank = c.bank as u32;
        for e in &c.entries {
            let key = format!("{:02X}:{:04X}:{}:{}", c.bank, e.start, e.entry_m, e.entry_x);
            let res = catch_unwind(AssertUnwindSafe(|| {
                decode_function(&rom, bank, e.start, e.entry_m, e.entry_x, e.end, &env)
            }));
            match res {
                Ok(g) => {
                    n_ok += 1;
                    out.insert(key, graph_obj(&g));
                }
                Err(_) => {
                    n_err += 1;
                    out.insert(key, json!({ "error": "panic" }));
                }
            }
        }
    }

    let val = Value::Object(out.into_iter().collect());
    let s = serde_json::to_string(&val).expect("serialize");
    std::fs::write(&out_path, s).expect("write out");
    eprintln!("dumped {n_ok} graphs ({n_err} errors) -> {out_path}");
}
