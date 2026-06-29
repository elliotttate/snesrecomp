//! Scratch: emit one bank's C via the Rust emit_bank, for a compile smoke test.
//! Usage: emit-one-bank --rom R --cfg-dir D --bank 0C --out /tmp/bank0C.c
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use snesrecomp_regen::cfg::load_bank_cfg;
use snesrecomp_regen::codegen::{EmitCtx, EmitOutcome};
use snesrecomp_regen::decoder::{DecodeEnv, IndirectDispatchSite};
use snesrecomp_regen::emit::{emit_bank, BankEntrySpec, EmitHle};
use snesrecomp_regen::rom::{load_rom, RelocRegion};

fn arg(args: &[String], f: &str) -> Option<String> {
    args.iter().position(|a| a == f).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rom = load_rom(arg(&args, "--rom").unwrap()).unwrap();
    let cfg_dir = arg(&args, "--cfg-dir").unwrap();
    let want_bank = u32::from_str_radix(&arg(&args, "--bank").unwrap(), 16).unwrap();
    let out = arg(&args, "--out").unwrap();

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&cfg_dir).unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str())
            .map(|n| n.starts_with("bank") && n.ends_with(".cfg")).unwrap_or(false))
        .collect();
    paths.sort();
    let cfgs: Vec<_> = paths.iter().filter_map(|p| load_bank_cfg(p).ok()).collect();

    let mut name_map: HashMap<u32, String> = HashMap::new();
    let mut data_regions = Vec::new();
    let mut reloc_regions: Vec<RelocRegion> = Vec::new();
    let mut callee_inline_skip = HashMap::new();
    let mut inline_loop_pcs: BTreeSet<u32> = BTreeSet::new();
    let mut indirect_dispatch: HashMap<u32, IndirectDispatchSite> = HashMap::new();
    for c in &cfgs {
        let bank = c.bank as u32;
        data_regions.extend(c.data_regions.iter().copied());
        reloc_regions.extend(c.reloc_regions.iter().copied());
        for e in &c.entries {
            if let Some(n) = &e.name { name_map.insert((bank << 16) | (e.start & 0xFFFF), n.clone()); }
            if let Some(s) = e.inline_skip { callee_inline_skip.insert((bank << 16) | (e.start & 0xFFFF), s); }
        }
        for nd in &c.names { name_map.insert(nd.addr_24 & 0xFFFFFF, nd.name.clone()); }
        for &p in &c.inline_dispatch_loops { inline_loop_pcs.insert((bank << 16) | p); }
        for d in &c.indirect_dispatch {
            indirect_dispatch.insert((bank << 16) | d.site_pc16, IndirectDispatchSite {
                count: d.count, idx_reg: d.idx_reg, table_bases: d.table_bases.clone() });
        }
    }
    let mut ctx = EmitCtx { rom_size: rom.len(), reloc_regions: reloc_regions.clone(), ..Default::default() };
    ctx.set_name_resolver(&name_map);
    let env = DecodeEnv {
        data_regions: Some(&data_regions), reloc_regions: Some(&reloc_regions),
        callee_inline_skip: Some(&callee_inline_skip), indirect_dispatch: Some(&indirect_dispatch),
        inline_dispatch_loop_pcs: Some(&inline_loop_pcs), ..Default::default() };

    let c = cfgs.iter().find(|c| c.bank as u32 == want_bank).expect("bank not found");
    let entries: Vec<BankEntrySpec> = c.entries.iter().map(|e| BankEntrySpec {
        name: e.name.clone(), start: e.start, end: e.end,
        entry_m: e.entry_m, entry_x: e.entry_x,
        tail_call_pc16: None, entry_s_offset: e.entry_s_offset,
    }).collect();
    let spc: BTreeSet<u32> = c.hle_spc_upload.iter().copied().collect();
    let hle = EmitHle {
        hle_spc_upload: if spc.is_empty() { None } else { Some(&spc) },
        hle_func: if c.hle_func.is_empty() { None } else { Some(&c.hle_func) },
        hle_dispatch: if c.hle_dispatch.is_empty() { None } else { Some(&c.hle_dispatch) },
        exclude_ranges: if c.exclude_ranges.is_empty() { None } else { Some(&c.exclude_ranges) },
        ..Default::default()
    };
    let mut outcome = EmitOutcome::default();
    let src = emit_bank(&ctx, &rom, want_bank, &entries, &env, &hle, None, &mut outcome);
    std::fs::write(&out, src).unwrap();
    eprintln!("wrote {} ({} entries)", out, entries.len());
}
