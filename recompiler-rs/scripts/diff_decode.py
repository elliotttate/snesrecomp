#!/usr/bin/env python3
"""Compare two decode-dump JSONs (golden vs rust) for functional equivalence.

Reports: graphs identical, graphs differing, missing/extra top-level entries,
and the first few divergences per category (insn-set mismatch, successor
mismatch, bookkeeping mismatch).

Usage:
  diff_decode.py golden/decode.json /tmp/rust_decode.json
"""
import json
import sys


def load(p):
    with open(p) as f:
        return json.load(f)


def main():
    a_path, b_path = sys.argv[1], sys.argv[2]
    a = load(a_path)  # golden
    b = load(b_path)  # rust

    a_keys = set(a)
    b_keys = set(b)
    missing = sorted(a_keys - b_keys)  # in golden, not in rust
    extra = sorted(b_keys - a_keys)    # in rust, not in golden
    common = sorted(a_keys & b_keys)

    identical = 0
    differing = []
    cat = {"insns": [], "succ": [], "entry": [], "bookkeeping": []}

    for k in common:
        ga, gb = a[k], b[k]
        if ga == gb:
            identical += 1
            continue
        differing.append(k)
        # Categorize.
        ia = ga.get("insns", {})
        ib = gb.get("insns", {})
        ka, kb = set(ia), set(ib)
        if ka != kb:
            cat["insns"].append((k, sorted(ka - kb)[:4], sorted(kb - ka)[:4]))
            continue
        # same insn key-set: check per-insn content / successors
        succ_diff = None
        body_diff = None
        for ik in ia:
            if ia[ik] != ib[ik]:
                if ia[ik].get("succ") != ib[ik].get("succ"):
                    succ_diff = (ik, ia[ik].get("succ"), ib[ik].get("succ"))
                else:
                    body_diff = (ik, ia[ik], ib[ik])
                break
        if ga.get("entry") != gb.get("entry"):
            cat["entry"].append((k, ga.get("entry"), gb.get("entry")))
        elif succ_diff is not None:
            cat["succ"].append((k, succ_diff))
        elif body_diff is not None:
            cat["insns"].append((k, [body_diff[0]], []))
        else:
            # bookkeeping fields differ
            bk = {}
            for f in ("unresolved", "suppressed_calls", "dispatch_suppressed"):
                if ga.get(f) != gb.get(f):
                    bk[f] = (ga.get(f), gb.get(f))
            cat["bookkeeping"].append((k, bk))

    total_golden = len(a)
    print(f"golden entries:   {total_golden}")
    print(f"rust entries:     {len(b)}")
    print(f"common entries:   {len(common)}")
    print(f"missing (in golden, not rust): {len(missing)}")
    print(f"extra   (in rust, not golden): {len(extra)}")
    print(f"identical graphs: {identical}/{total_golden}")
    print(f"differing graphs: {len(differing)}")
    print()
    if missing:
        print("first missing keys:", missing[:12])
    if extra:
        print("first extra keys:  ", extra[:12])
    print()
    for name in ("insns", "succ", "entry", "bookkeeping"):
        items = cat[name]
        if not items:
            continue
        print(f"=== {name} mismatches: {len(items)} ===")
        for it in items[:5]:
            print("  ", it)
        print()

    ok = (not missing) and (not extra) and (identical == total_golden)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
