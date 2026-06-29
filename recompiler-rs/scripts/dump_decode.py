#!/usr/bin/env python3
"""Differential-oracle shim: dump the Python `decode_function` output for every
cfg entry to JSON, so the Rust decoder port can be diffed against it.

The env is CONTROLLED and explicit so both sides decode under identical inputs:
the aggregate data_regions / reloc_regions / indirect_dispatch / inline-dispatch
-loop / callee_inline_skip drawn from the cfgs, and NO dispatch_helpers /
callee_exit_mx (those come from later discovery passes; layered in once the core
walk matches). Output is one JSON object per entry, keyed by "bank:pc:m:x".

Usage:
  dump_decode.py --rom sf.sfc --cfg-dir recomp --out golden/decode.json
"""
import argparse
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
# HERE = .../recompiler-rs/scripts → repo root is two levels up.
SNESRECOMP = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(SNESRECOMP, "recompiler"))
sys.path.insert(0, os.path.join(SNESRECOMP, "recompiler", "v2"))

from snes65816 import load_rom, set_active_reloc_regions  # noqa: E402
from v2.cfg_loader import load_bank_cfg  # noqa: E402
from v2.decoder import decode_function  # noqa: E402
import glob  # noqa: E402


def key_str(k):
    ps = ";".join(f"{m}{x}" for (m, x) in k.p_stack)
    return f"{k.pc:06X}:{k.m}:{k.x}:[{ps}]"


def insn_obj(di):
    ins = di.insn
    return {
        "addr": ins.addr,
        "op": ins.opcode,
        "mnem": ins.mnem,
        "mode": ins.mode,
        "operand": ins.operand,
        "length": ins.length,
        "m": ins.m_flag,
        "x": ins.x_flag,
        "succ": sorted(key_str(s) for s in di.successors),
    }


def graph_obj(g):
    return {
        "entry": key_str(g.entry),
        "insns": {key_str(k): insn_obj(di) for k, di in g.insns.items()},
        "unresolved": sorted(u.site_pc24 for u in g.unresolved_indirects),
        "suppressed_calls": sorted(s.site_pc24 for s in g.suppressed_indirect_calls),
        "dispatch_suppressed": sorted(
            (d.site_pc24, d.target_pc24) for d in g.dispatch_targets_suppressed
        ),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rom", required=True)
    ap.add_argument("--cfg-dir", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    rom = load_rom(args.rom)
    cfgs = []
    for p in sorted(glob.glob(os.path.join(args.cfg_dir, "bank*.cfg"))):
        try:
            cfgs.append((p, load_bank_cfg(p)))
        except Exception as e:
            print(f"  parse-fail {p}: {e}", file=sys.stderr)

    # Aggregate the controlled env across all cfgs.
    data_regions = []
    reloc_regions = []
    callee_inline_skip = {}
    inline_loop_pcs = set()
    indirect_dispatch = {}  # site_pc24 -> {count, idx_reg, table_bases}
    for _p, c in cfgs:
        data_regions.extend(c.data_regions)
        reloc_regions.extend(c.reloc_regions)
        for e in c.entries:
            if getattr(e, "inline_skip", None) is not None:
                callee_inline_skip[(c.bank << 16) | (e.start & 0xFFFF)] = e.inline_skip
        for pc16 in c.inline_dispatch_loops:
            inline_loop_pcs.add((c.bank << 16) | pc16)
        for d in c.indirect_dispatch:
            site = (c.bank << 16) | d["site_pc16"]
            indirect_dispatch[site] = {
                "count": d["count"],
                "idx_reg": d["idx_reg"],
                "table_bases": list(d["table_bases"]),
            }

    set_active_reloc_regions(reloc_regions or None)

    out = {}
    n_ok = n_err = 0
    for _p, c in cfgs:
        for e in c.entries:
            try:
                g = decode_function(
                    rom, c.bank, e.start,
                    entry_m=e.entry_m, entry_x=e.entry_x, end=e.end,
                    data_regions=data_regions or None,
                    reloc_regions=reloc_regions or None,
                    callee_inline_skip=callee_inline_skip or None,
                    indirect_dispatch=indirect_dispatch or None,
                    inline_dispatch_loop_pcs=inline_loop_pcs or None,
                )
            except Exception as ex:
                n_err += 1
                out[f"{c.bank:02X}:{e.start:04X}:{e.entry_m}:{e.entry_x}"] = {
                    "error": f"{type(ex).__name__}: {ex}"
                }
                continue
            n_ok += 1
            out[f"{c.bank:02X}:{e.start:04X}:{e.entry_m}:{e.entry_x}"] = graph_obj(g)

    with open(args.out, "w") as f:
        json.dump(out, f, sort_keys=True, separators=(",", ":"))
    print(f"dumped {n_ok} graphs ({n_err} errors) -> {args.out}")


if __name__ == "__main__":
    main()
