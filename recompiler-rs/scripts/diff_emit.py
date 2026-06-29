#!/usr/bin/env python3
"""Diff two emit-oracle JSONs (Python vs Rust) keyed "BB:PCPC:m:x" -> C text.

Reports: identical count, differing count, present-only-in-one, and a short
unified diff for the first ~10 differing functions. Treats two values that both
start with "__ERROR__" as identical (both sides errored on that function).

Usage:
  diff_emit.py /tmp/py_emit.json /tmp/rust_emit.json [--max-diffs N]
"""
import argparse
import difflib
import json
import sys


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("py_json")
    ap.add_argument("rust_json")
    ap.add_argument("--max-diffs", type=int, default=10)
    ap.add_argument("--context", type=int, default=3)
    args = ap.parse_args()

    with open(args.py_json) as f:
        py = json.load(f)
    with open(args.rust_json) as f:
        rust = json.load(f)

    py_keys = set(py)
    rust_keys = set(rust)
    only_py = sorted(py_keys - rust_keys)
    only_rust = sorted(rust_keys - py_keys)
    common = sorted(py_keys & rust_keys)

    identical = 0
    both_err = 0
    differing = []
    for k in common:
        a, b = py[k], rust[k]
        if a == b:
            identical += 1
            continue
        if a.startswith("__ERROR__") and b.startswith("__ERROR__"):
            both_err += 1
            identical += 1
            continue
        differing.append(k)

    total = len(common)
    print(f"py functions:    {len(py_keys)}")
    print(f"rust functions:  {len(rust_keys)}")
    print(f"common:          {total}")
    print(f"identical:       {identical}  ({both_err} both-errored)")
    print(f"differing:       {len(differing)}")
    print(f"only in py:      {len(only_py)}")
    print(f"only in rust:    {len(only_rust)}")
    if total:
        print(f"match rate:      {identical}/{total} = {identical/total*100:.2f}%")

    if only_py[:10]:
        print("\n-- only in py (first 10):", only_py[:10])
    if only_rust[:10]:
        print("\n-- only in rust (first 10):", only_rust[:10])

    for k in differing[: args.max_diffs]:
        a = py[k].splitlines()
        b = rust[k].splitlines()
        print(f"\n===== DIFF {k} (py:{len(a)} lines vs rust:{len(b)} lines) =====")
        diff = difflib.unified_diff(
            a, b, fromfile=f"py:{k}", tofile=f"rust:{k}",
            lineterm="", n=args.context,
        )
        shown = 0
        for line in diff:
            print(line)
            shown += 1
            if shown > 60:
                print("... (diff truncated) ...")
                break

    if differing or only_py or only_rust:
        sys.exit(1)


if __name__ == "__main__":
    main()
