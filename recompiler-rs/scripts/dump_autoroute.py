#!/usr/bin/env python3
"""Differential-oracle shim: run the cfg-mutating autoroute passes in the SAME
order `tools/v2_regen.py` does (wrapper -> tail_call -> pha_rts ->
dispatch-helper-discovery -> exit_mx) over the live cfg corpus, then dump the
resulting per-bank entry set + dispatch_helpers map + each pass's fix count as
JSON so the Rust `dump-autoroute` bin can be diffed against it.

The process-global decode state (inline-skip map + reloc regions) is installed
exactly as v2_regen installs it before the autoroute block, so the passes decode
under identical inputs on both sides.

Usage:
  dump_autoroute.py --rom sf.sfc --cfg-dir recomp --out golden/autoroute.json
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
from v2.decoder import (  # noqa: E402
    classify_dispatch_helper, decode_function, set_global_inline_skip,
)
from v2.wrapper_autoroute import detect_and_route as autoroute_wrappers  # noqa: E402
from v2.tail_call_autoroute import detect_and_route as autoroute_tail_calls  # noqa: E402
from v2.pha_rts_autoroute import detect_and_route as autoroute_pha_rts  # noqa: E402
from v2.exit_mx_autoroute import detect_and_route as autoroute_exit_mx  # noqa: E402

_BANK_RE = __import__("re").compile(r"bank([0-9A-Fa-f]{2})\.cfg$")


def entry_obj(e):
    # exit_mx / inline_skip / force_variants are non-default dynamic attrs the
    # cfg loader only sets when the func line carries them.
    exit_mx = getattr(e, "exit_mx", None)
    inline_skip = getattr(e, "inline_skip", None)
    force_variants = getattr(e, "force_variants", None)
    return {
        "name": e.name,
        "start": e.start & 0xFFFF,
        "end": (e.end & 0xFFFF) if e.end is not None else None,
        "entry_m": int(e.entry_m),
        "entry_x": int(e.entry_x),
        "exit_mx": [int(exit_mx[0]), int(exit_mx[1])] if exit_mx is not None else None,
        "force_variants": (
            [[int(p[0]), int(p[1])] for p in force_variants]
            if force_variants else None
        ),
        "inline_skip": (int(inline_skip) if inline_skip is not None else None),
        "tail_call_pc16": (e.tail_call_pc16 & 0xFFFF) if e.tail_call_pc16 is not None else None,
        "entry_s_offset": int(e.entry_s_offset),
    }


def indirect_obj(d):
    return [
        d["site_pc16"] & 0xFFFF,
        int(d["count"]),
        str(d["idx_reg"]),
        [b & 0xFFFF for b in d["table_bases"]],
    ]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rom", required=True)
    ap.add_argument("--cfg-dir", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    rom = load_rom(args.rom)

    # Load cfgs in sorted-glob (== bank-ascending) order, mirroring v2_regen.
    parsed = []  # (bank, path, cfg)
    name_map = {}
    cross_bank_names = {}
    for p in sorted(glob.glob(os.path.join(args.cfg_dir, "bank*.cfg"))):
        m = _BANK_RE.search(os.path.basename(p))
        if not m:
            continue
        bank = int(m.group(1), 16)
        try:
            cfg = load_bank_cfg(p)
        except Exception as e:
            print(f"  parse-fail {p}: {e}", file=sys.stderr)
            continue
        parsed.append((bank, p, cfg))
        for entry in cfg.entries:
            if entry.name:
                name_map[(bank << 16) | (entry.start & 0xFFFF)] = entry.name
        for nd in cfg.names:
            addr = nd.addr_24 & 0xFFFFFF
            name_map[addr] = nd.name
            cross_bank_names.setdefault((addr >> 16) & 0xFF, []).append(nd)

    # Install process-global decode state as v2_regen does pre-autoroute.
    callee_inline_skip = {}
    for bank, _p, cfg in parsed:
        for entry in cfg.entries:
            n = getattr(entry, "inline_skip", None)
            if n:
                callee_inline_skip[(bank << 16) | (entry.start & 0xFFFF)] = n
    set_global_inline_skip(callee_inline_skip)

    reloc_regions = []
    for _bank, _p, cfg in parsed:
        if getattr(cfg, "reloc_regions", None):
            reloc_regions.extend(cfg.reloc_regions)
    set_active_reloc_regions(reloc_regions or None)

    # 1. wrapper
    wrapper_fixes = autoroute_wrappers(parsed, name_map, cross_bank_names, rom)
    # 2. tail_call
    tail_call_fixes = autoroute_tail_calls(parsed, rom)
    # 3. pha_rts
    pha_rts_fixes = autoroute_pha_rts(parsed, rom)
    # 4. dispatch-helper discovery
    dispatch_helpers = {}
    jsl_targets = set()
    for bank, _p, cfg in parsed:
        for entry in cfg.entries:
            try:
                g = decode_function(rom, bank, entry.start,
                                    entry_m=entry.entry_m, entry_x=entry.entry_x,
                                    end=entry.end)
            except Exception:
                continue
            for di in g.insns.values():
                ins = di.insn
                if ins.mnem == "JSL":
                    jsl_targets.add(ins.operand & 0xFFFFFF)
                elif ins.mnem == "JMP" and ins.length == 4:
                    jsl_targets.add(ins.operand & 0xFFFFFF)
    for tgt in jsl_targets:
        kind = classify_dispatch_helper(rom, (tgt >> 16) & 0xFF, tgt & 0xFFFF)
        if kind:
            dispatch_helpers[tgt] = kind
    # 5. exit_mx
    exit_mx_fixes = autoroute_exit_mx(parsed, rom, dispatch_helpers=dispatch_helpers)

    banks = {}
    for bank, _p, cfg in parsed:
        entries = sorted(
            (entry_obj(e) for e in cfg.entries),
            key=lambda o: (o["start"], o["name"] or ""),
        )
        per_variant = sorted(
            [int(t[0]), int(t[1]) & 0xFFFF, int(t[2]), int(t[3]), int(t[4]), int(t[5])]
            for t in cfg.exit_mx_at_per_variant
        )
        ind = sorted((indirect_obj(d) for d in cfg.indirect_dispatch),
                     key=lambda o: (o[0], o[1]))
        banks[str(bank)] = {
            "entries": entries,
            "exit_mx_at_per_variant": per_variant,
            "indirect_dispatch": ind,
        }

    out = {
        "fix_counts": {
            "wrapper": len(wrapper_fixes),
            "tail_call": len(tail_call_fixes),
            "pha_rts": len(pha_rts_fixes),
            "exit_mx": len(exit_mx_fixes),
        },
        "dispatch_helpers": {str(k): v for k, v in dispatch_helpers.items()},
        "banks": banks,
    }
    with open(args.out, "w") as f:
        json.dump(out, f, sort_keys=True, separators=(",", ":"))
    print(f"wrapper={len(wrapper_fixes)} tail_call={len(tail_call_fixes)} "
          f"pha_rts={len(pha_rts_fixes)} exit_mx={len(exit_mx_fixes)} "
          f"helpers={len(dispatch_helpers)} banks={len(banks)} -> {args.out}")


if __name__ == "__main__":
    main()
