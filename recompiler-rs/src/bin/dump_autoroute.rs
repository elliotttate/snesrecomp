//! `dump-autoroute` — Rust side of the autoroute differential oracle.
//!
//! Loads the ROM + cfg dir, installs the same decode env state v2_regen does
//! pre-autoroute (aggregate reloc regions + inline-skip map), runs the five
//! cfg-mutating passes in v2_regen order (wrapper -> tail_call -> pha_rts ->
//! dispatch-helper-discovery -> exit_mx), and dumps the resulting per-bank entry
//! set + dispatch_helpers map + per-pass fix counts in the identical JSON shape
//! as `scripts/dump_autoroute.py`, for `scripts/diff_autoroute.py`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use snesrecomp_regen::autoroute::{
    discover_dispatch_helpers, exit_mx_detect_and_route, pha_rts_detect_and_route,
    tail_call_detect_and_route, wrapper_detect_and_route,
};
use snesrecomp_regen::cfg::{load_bank_cfg, BankCfg, BankEntry, IndirectDispatch, NameDecl};
use snesrecomp_regen::decoder::DecodeEnv;
use snesrecomp_regen::rom::{load_rom, RelocRegion};

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

/// Extract the bank from a `bankXX.cfg` filename (two hex digits), like the
/// `_BANK_CFG_RE` v2_regen uses.
fn filename_bank(p: &std::path::Path) -> Option<u32> {
    let name = p.file_name()?.to_str()?;
    let rest = name.strip_prefix("bank")?;
    let hex = rest.strip_suffix(".cfg")?;
    if hex.len() == 2 {
        u32::from_str_radix(hex, 16).ok()
    } else {
        None
    }
}

fn entry_obj(e: &BankEntry) -> Value {
    json!({
        "name": e.name,
        "start": e.start & 0xFFFF,
        "end": e.end.map(|v| v & 0xFFFF),
        "entry_m": e.entry_m,
        "entry_x": e.entry_x,
        "exit_mx": e.exit_mx.map(|(m, x)| json!([m, x])),
        "force_variants": e
            .force_variants
            .as_ref()
            .map(|v| Value::Array(v.iter().map(|&(m, x)| json!([m, x])).collect())),
        "inline_skip": e.inline_skip,
        "tail_call_pc16": e.tail_call_pc16.map(|v| v & 0xFFFF),
        "entry_s_offset": e.entry_s_offset,
    })
}

fn indirect_obj(d: &IndirectDispatch) -> Value {
    json!([
        d.site_pc16 & 0xFFFF,
        d.count,
        d.idx_reg.to_string(),
        d.table_bases.iter().map(|b| b & 0xFFFF).collect::<Vec<u32>>(),
    ])
}

fn main() {
    // Suppress panic spam from decode_function (caught and mapped to skips).
    std::panic::set_hook(Box::new(|_| {}));

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

    let mut parsed: Vec<BankCfg> = Vec::new();
    for p in &cfg_paths {
        match load_bank_cfg(p) {
            Ok(mut c) => {
                // v2_regen derives the bank from the `bankXX.cfg` filename, not
                // the `bank =` directive. Mirror that authority so the passes
                // (which read cfg.bank) key off the same bank Python uses.
                if let Some(b) = filename_bank(p) {
                    c.bank = b as i32;
                }
                parsed.push(c);
            }
            Err(e) => eprintln!("  parse-fail {}: {e}", p.display()),
        }
    }

    // Build name_map + cross_bank_names (mirrors v2_regen first pass).
    let mut name_map: HashMap<u32, String> = HashMap::new();
    let mut cross_bank_names: HashMap<u32, Vec<NameDecl>> = HashMap::new();
    for cfg in &parsed {
        let bank = cfg.bank as u32;
        for entry in &cfg.entries {
            if let Some(name) = &entry.name {
                name_map.insert((bank << 16) | (entry.start & 0xFFFF), name.clone());
            }
        }
        for nd in &cfg.names {
            let addr = nd.addr_24 & 0xFFFFFF;
            name_map.insert(addr, nd.name.clone());
            cross_bank_names.entry((addr >> 16) & 0xFF).or_default().push(nd.clone());
        }
    }

    // Process-global decode state: inline-skip map (`if n:`) + aggregate reloc.
    let mut inline_skip: HashMap<u32, i32> = HashMap::new();
    for cfg in &parsed {
        let bank = cfg.bank as u32;
        for entry in &cfg.entries {
            if let Some(n) = entry.inline_skip {
                if n != 0 {
                    inline_skip.insert((bank << 16) | (entry.start & 0xFFFF), n);
                }
            }
        }
    }
    let mut reloc_regions: Vec<RelocRegion> = Vec::new();
    for cfg in &parsed {
        reloc_regions.extend(cfg.reloc_regions.iter().copied());
    }

    // The env tail_call + dispatch-helper-discovery decode under (reloc +
    // global inline skip only; no dispatch_helpers / callee_exit_mx / data /
    // indirect — matching the Python passes' kwarg-less decode_function calls).
    let basic_env = DecodeEnv {
        reloc_regions: Some(&reloc_regions),
        global_inline_skip: Some(&inline_skip),
        ..Default::default()
    };

    // 1. wrapper
    let wrapper_fixes = wrapper_detect_and_route(&parsed, &mut name_map, &mut cross_bank_names, &rom);
    // 2. tail_call
    let tail_call_fixes = tail_call_detect_and_route(&mut parsed, &rom, &basic_env);
    // 3. pha_rts
    let pha_rts_fixes = pha_rts_detect_and_route(&mut parsed, &rom);
    // 4. dispatch-helper discovery
    let dispatch_helpers = discover_dispatch_helpers(&parsed, &rom, &basic_env);
    // 5. exit_mx
    let exit_mx_fixes =
        exit_mx_detect_and_route(&mut parsed, &rom, &helpers_as_hashmap(&dispatch_helpers), &reloc_regions);

    // Build output JSON.
    let mut banks = Map::new();
    for cfg in &parsed {
        let bank = cfg.bank as u32;
        let mut entries: Vec<(u32, String, Value)> = cfg
            .entries
            .iter()
            .map(|e| (e.start & 0xFFFF, e.name.clone().unwrap_or_default(), entry_obj(e)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let entries_v: Vec<Value> = entries.into_iter().map(|(_, _, v)| v).collect();

        let mut pv: Vec<[u32; 6]> = cfg
            .exit_mx_at_per_variant
            .iter()
            .map(|&(b, a, em, ex, m, x)| {
                [b as u32, a & 0xFFFF, em as u32, ex as u32, m as u32, x as u32]
            })
            .collect();
        pv.sort();
        let pv_v: Vec<Value> = pv.iter().map(|t| json!(t)).collect();

        let mut ind: Vec<(u32, u32, Value)> = cfg
            .indirect_dispatch
            .iter()
            .map(|d| (d.site_pc16 & 0xFFFF, d.count, indirect_obj(d)))
            .collect();
        ind.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let ind_v: Vec<Value> = ind.into_iter().map(|(_, _, v)| v).collect();

        banks.insert(
            bank.to_string(),
            json!({
                "entries": entries_v,
                "exit_mx_at_per_variant": pv_v,
                "indirect_dispatch": ind_v,
            }),
        );
    }

    let mut helpers_v = Map::new();
    for (k, v) in &dispatch_helpers {
        helpers_v.insert(k.to_string(), Value::String(v.clone()));
    }

    let out = json!({
        "fix_counts": {
            "wrapper": wrapper_fixes.len(),
            "tail_call": tail_call_fixes.len(),
            "pha_rts": pha_rts_fixes.len(),
            "exit_mx": exit_mx_fixes.len(),
        },
        "dispatch_helpers": Value::Object(helpers_v),
        "banks": Value::Object(banks),
    });

    let s = serde_json::to_string(&out).expect("serialize");
    std::fs::write(&out_path, s).expect("write out");
    eprintln!(
        "wrapper={} tail_call={} pha_rts={} exit_mx={} helpers={} banks={} -> {}",
        wrapper_fixes.len(),
        tail_call_fixes.len(),
        pha_rts_fixes.len(),
        exit_mx_fixes.len(),
        dispatch_helpers.len(),
        parsed.len(),
        out_path,
    );
}

/// exit_mx wants a plain `HashMap<u32,String>`; discovery returns a `BTreeMap`.
fn helpers_as_hashmap(m: &std::collections::BTreeMap<u32, String>) -> HashMap<u32, String> {
    m.iter().map(|(k, v)| (*k, v.clone())).collect()
}
