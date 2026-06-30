//! `dump-emit` — Rust side of the emit oracle.
//!
//! Builds the SAME controlled `EmitCtx` + `DecodeEnv` as `scripts/dump_emit.py`
//! (name_resolver from all cfg func/name decls; valid_variants empty; force_variant
//! empty; aggregate data/reloc/callee_inline_skip/indirect_dispatch/inline_loop)
//! and emits `emit_function`'s C text per cfg entry to JSON in the identical shape
//! (key "BB:PCPC:m:x" -> C text) so `scripts/diff_emit.py` can diff fresh-vs-fresh.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use serde_json::Value;

use snesrecomp_regen::cfg::load_bank_cfg;
use snesrecomp_regen::codegen::{EmitCtx, EmitOutcome};
use snesrecomp_regen::decoder::{DecodeEnv, IndirectDispatchSite};
use snesrecomp_regen::emit::{emit_function, EmitHle};
use snesrecomp_regen::rom::{load_rom, RelocRegion};

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rom_path = arg_value(&args, "--rom").expect("--rom required");
    let cfg_dir = arg_value(&args, "--cfg-dir").expect("--cfg-dir required");
    let out_path = arg_value(&args, "--out").expect("--out required");

    let rom = load_rom(&rom_path).expect("load rom");

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
    let mut name_map: HashMap<u32, String> = HashMap::new();
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
            if let Some(name) = &e.name {
                name_map.insert((bank << 16) | (e.start & 0xFFFF), name.clone());
            }
            if let Some(skip) = e.inline_skip {
                callee_inline_skip.insert((bank << 16) | (e.start & 0xFFFF), skip);
            }
        }
        for nd in &c.names {
            name_map.insert(nd.addr_24 & 0xFFFFFF, nd.name.clone());
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

    // Controlled EmitCtx (mirrors set_rom_size / set_name_resolver /
    // set_reloc_regions / set_valid_variants(None) / set_force_variant_at({})).
    let mut ctx = EmitCtx {
        rom_size: rom.len(),
        reloc_regions: reloc_regions.clone(),
        ..Default::default()
    };
    ctx.set_name_resolver(&name_map);

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
                let mut outcome = EmitOutcome::default();
                let hle = EmitHle::default();
                emit_function(
                    &ctx,
                    &rom,
                    bank,
                    e.start,
                    e.entry_m,
                    e.entry_x,
                    e.end,
                    e.name.as_deref(),
                    e.entry_s_offset,
                    &env,
                    &hle,
                    None,
                    &mut outcome,
                )
            }));
            match res {
                Ok(txt) => {
                    n_ok += 1;
                    out.insert(key, Value::String(txt));
                }
                Err(_) => {
                    n_err += 1;
                    out.insert(key, Value::String("__ERROR__ panic".to_string()));
                }
            }
        }
    }

    let val = Value::Object(out.into_iter().collect());
    let s = serde_json::to_string(&val).expect("serialize");
    std::fs::write(&out_path, s).expect("write out");
    eprintln!("emitted {n_ok} functions ({n_err} errors) -> {out_path}");
}
