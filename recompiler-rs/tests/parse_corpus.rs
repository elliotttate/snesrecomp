//! Phase 1 gate: parse every real `bank*.cfg` in a cfg dir without error.
//!
//! Opt-in (leaked-source paths stay out of the normal test run):
//!   SNESRECOMP_CFG_DIR=/path/to/recomp cargo test --test parse_corpus -- --ignored --nocapture

use snesrecomp_regen::cfg::load_bank_cfg;

#[test]
#[ignore]
fn parse_all_cfgs() {
    let dir = std::env::var("SNESRECOMP_CFG_DIR")
        .expect("set SNESRECOMP_CFG_DIR to the recomp/ dir");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("bank") && n.ends_with(".cfg"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no bank*.cfg under {dir}");

    let mut total_entries = 0usize;
    let mut total_names = 0usize;
    let mut failures = Vec::new();
    for p in &paths {
        match load_bank_cfg(p) {
            Ok(cfg) => {
                total_entries += cfg.entries.len();
                total_names += cfg.names.len();
                println!(
                    "{:<16} bank=${:02X} entries={} names={} reloc={} indirect={} hle_func={}",
                    p.file_name().unwrap().to_string_lossy(),
                    cfg.bank,
                    cfg.entries.len(),
                    cfg.names.len(),
                    cfg.reloc_regions.len(),
                    cfg.indirect_dispatch.len(),
                    cfg.hle_func.len(),
                );
            }
            Err(e) => failures.push(format!("{}: {e}", p.display())),
        }
    }
    println!(
        "--- parsed {} cfgs, {} entries, {} names ---",
        paths.len(),
        total_entries,
        total_names
    );
    assert!(failures.is_empty(), "parse failures:\n{}", failures.join("\n"));
}
