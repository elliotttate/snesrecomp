#!/usr/bin/env bash
# Diff Rust regen output against the Python golden snapshot (dev aid).
# We target FUNCTIONAL equivalence, not byte-identity, so divergences are
# triage signal, not hard failures: benign (whitespace/comment/order) → ignore;
# semantic → bug. Summarizes per-file diff line counts.
#
# Usage: diff-golden.sh <rust-out-dir>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
GOLDEN="$RS_ROOT/golden/src_gen"
RUST_OUT="${1:?usage: diff-golden.sh <rust-out-dir>}"

[ -d "$GOLDEN" ] || { echo "diff-golden: no golden snapshot at $GOLDEN (run make-golden.sh)" >&2; exit 1; }

total_diff=0
for g in "$GOLDEN"/*.c; do
  f="$(basename "$g")"
  r="$RUST_OUT/$f"
  if [ ! -f "$r" ]; then
    printf '%-22s MISSING in rust output\n' "$f"
    continue
  fi
  n="$(diff -u "$g" "$r" | grep -cE '^[+-]' || true)"
  total_diff=$((total_diff + n))
  printf '%-22s %6d diff lines\n' "$f" "$n"
done
echo "---"
echo "total diff lines: $total_diff"
