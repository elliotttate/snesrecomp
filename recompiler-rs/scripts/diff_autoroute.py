#!/usr/bin/env python3
"""Compare two autoroute dumps (Python oracle vs Rust port) and report the match
rate: banks identical / total, total entry-field diffs, dispatch_helpers diff,
and fix-count diffs. Exit 0 iff byte-identical (modulo JSON key ordering).

Usage:
  diff_autoroute.py <py.json> <rs.json>
"""
import json
import sys


def load(p):
    with open(p) as f:
        return json.load(f)


def main():
    if len(sys.argv) != 3:
        print("usage: diff_autoroute.py <py.json> <rs.json>", file=sys.stderr)
        return 2
    a = load(sys.argv[1])
    b = load(sys.argv[2])

    ok = True

    # Fix counts.
    fc_a, fc_b = a.get("fix_counts", {}), b.get("fix_counts", {})
    for k in sorted(set(fc_a) | set(fc_b)):
        if fc_a.get(k) != fc_b.get(k):
            ok = False
            print(f"FIX-COUNT diff [{k}]: py={fc_a.get(k)} rs={fc_b.get(k)}")
    if fc_a == fc_b:
        print(f"fix_counts identical: {fc_a}")

    # Dispatch helpers.
    dh_a, dh_b = a.get("dispatch_helpers", {}), b.get("dispatch_helpers", {})
    dh_keys = set(dh_a) | set(dh_b)
    dh_diffs = 0
    for k in sorted(dh_keys, key=lambda s: int(s)):
        if dh_a.get(k) != dh_b.get(k):
            dh_diffs += 1
            if dh_diffs <= 20:
                print(f"HELPER diff pc24={int(k):06X}: py={dh_a.get(k)} rs={dh_b.get(k)}")
    if dh_diffs:
        ok = False
        print(f"dispatch_helpers: {dh_diffs} diffs (py={len(dh_a)} rs={len(dh_b)})")
    else:
        print(f"dispatch_helpers identical: {len(dh_a)} entries")

    # Per-bank entry sets.
    ba, bb = a.get("banks", {}), b.get("banks", {})
    all_banks = sorted(set(ba) | set(bb), key=lambda s: int(s))
    identical = 0
    total = len(all_banks)
    field_diffs = 0
    pv_diffs = 0
    ind_diffs = 0
    examples = []
    for bk in all_banks:
        eb_a = ba.get(bk, {})
        eb_b = bb.get(bk, {})
        bank_ok = True

        ea = eb_a.get("entries", [])
        ee = eb_b.get("entries", [])
        if len(ea) != len(ee):
            bank_ok = False
            if len(examples) < 30:
                examples.append(f"bank {int(bk):02X}: entry count py={len(ea)} rs={len(ee)}")
        for i in range(min(len(ea), len(ee))):
            if ea[i] != ee[i]:
                # Per-field diff count.
                keys = set(ea[i]) | set(ee[i])
                for fk in sorted(keys):
                    if ea[i].get(fk) != ee[i].get(fk):
                        field_diffs += 1
                        if len(examples) < 30:
                            examples.append(
                                f"bank {int(bk):02X} entry[{i}] "
                                f"start={ea[i].get('start'):#06x} field {fk}: "
                                f"py={ea[i].get(fk)} rs={ee[i].get(fk)}"
                            )
                bank_ok = False

        pa = eb_a.get("exit_mx_at_per_variant", [])
        pp = eb_b.get("exit_mx_at_per_variant", [])
        if pa != pp:
            bank_ok = False
            d = len([1 for x in pa if x not in pp]) + len([1 for x in pp if x not in pa])
            pv_diffs += d
            if len(examples) < 30:
                examples.append(
                    f"bank {int(bk):02X}: per_variant py={len(pa)} rs={len(pp)} symdiff~{d}")

        ia = eb_a.get("indirect_dispatch", [])
        ii = eb_b.get("indirect_dispatch", [])
        if ia != ii:
            bank_ok = False
            d = len([1 for x in ia if x not in ii]) + len([1 for x in ii if x not in ia])
            ind_diffs += d
            if len(examples) < 30:
                examples.append(
                    f"bank {int(bk):02X}: indirect_dispatch py={len(ia)} rs={len(ii)} symdiff~{d}")

        if bank_ok:
            identical += 1
        else:
            ok = False

    print()
    print(f"banks identical: {identical}/{total}")
    print(f"entry-field diffs: {field_diffs}")
    print(f"per-variant diffs: {pv_diffs}")
    print(f"indirect_dispatch diffs: {ind_diffs}")
    if examples:
        print("--- examples ---")
        for e in examples:
            print(" ", e)

    print()
    print("RESULT:", "IDENTICAL" if ok else "DIVERGENT")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
