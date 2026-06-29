#!/usr/bin/env python3
"""Emit oracle: dump the Python `emit_function` C text for every cfg entry under
a CONTROLLED env, so the Rust codegen/emit port can be diffed against it.

Controlled env (must match the Rust dump-emit): name_resolver = the global
name map built from all cfg `func`/`name` decls; valid_variants = None (single
variant per function); force_variant_at = {}; aggregate data_regions /
reloc_regions / callee_inline_skip / indirect_dispatch / inline_dispatch_loop
from the cfgs; dispatch_helpers / callee_exit_mx / _modes = None. Output: one
JSON object mapping "BB:PCPC:m:x" -> emitted C text.

Usage:
  dump_emit.py --rom sf.sfc --cfg-dir recomp --out golden/emit.json
"""
import argparse
import glob
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SNESRECOMP = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(SNESRECOMP, "recompiler"))
sys.path.insert(0, os.path.join(SNESRECOMP, "recompiler", "v2"))

from snes65816 import load_rom, set_active_reloc_regions  # noqa: E402
from v2.cfg_loader import load_bank_cfg  # noqa: E402
from v2.emit_function import emit_function  # noqa: E402
from v2 import codegen  # noqa: E402


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rom", required=True)
    ap.add_argument("--cfg-dir", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    rom = load_rom(args.rom)
    codegen.set_rom_size(len(rom))

    cfgs = []
    for p in sorted(glob.glob(os.path.join(args.cfg_dir, "bank*.cfg"))):
        try:
            cfgs.append(load_bank_cfg(p))
        except Exception as e:
            print(f"  parse-fail {p}: {e}", file=sys.stderr)

    name_map = {}
    data_regions, reloc_regions = [], []
    callee_inline_skip, inline_loop_pcs, indirect_dispatch = {}, set(), {}
    for c in cfgs:
        data_regions.extend(c.data_regions)
        reloc_regions.extend(c.reloc_regions)
        for e in c.entries:
            if e.name:
                name_map[(c.bank << 16) | (e.start & 0xFFFF)] = e.name
            if getattr(e, "inline_skip", None) is not None:
                callee_inline_skip[(c.bank << 16) | (e.start & 0xFFFF)] = e.inline_skip
        for nd in c.names:
            name_map[nd.addr_24 & 0xFFFFFF] = nd.name
        for pc16 in c.inline_dispatch_loops:
            inline_loop_pcs.add((c.bank << 16) | pc16)
        for d in c.indirect_dispatch:
            indirect_dispatch[(c.bank << 16) | d["site_pc16"]] = {
                "count": d["count"], "idx_reg": d["idx_reg"],
                "table_bases": list(d["table_bases"]),
            }

    codegen.set_name_resolver(name_map)
    codegen.set_reloc_regions(reloc_regions or None)
    codegen.set_valid_variants(None)
    codegen.set_force_variant_at({})
    set_active_reloc_regions(reloc_regions or None)

    out = {}
    n_ok = n_err = 0
    for c in cfgs:
        for e in c.entries:
            key = f"{c.bank:02X}:{e.start:04X}:{e.entry_m}:{e.entry_x}"
            try:
                txt = emit_function(
                    rom, c.bank, e.start, e.entry_m, e.entry_x,
                    end=e.end, func_name=e.name,
                    data_regions=data_regions or None,
                    reloc_regions=reloc_regions or None,
                    callee_inline_skip=callee_inline_skip or None,
                    indirect_dispatch=indirect_dispatch or None,
                    inline_dispatch_loop_pcs=inline_loop_pcs or None,
                    tail_call_pc16=getattr(e, "tail_call_pc16", None),
                    entry_s_offset=getattr(e, "entry_s_offset", 0),
                )
                out[key] = txt
                n_ok += 1
            except Exception as ex:
                out[key] = f"__ERROR__ {type(ex).__name__}: {ex}"
                n_err += 1

    with open(args.out, "w") as f:
        json.dump(out, f, sort_keys=True)
    print(f"emitted {n_ok} functions ({n_err} errors) -> {args.out}")


if __name__ == "__main__":
    main()
