#!/usr/bin/env bash
# Produce the golden reference output by running the *Python* regen, so the
# Rust port can be diffed against it during development. Output is leaked-source
# derived → lands under recompiler-rs/golden/ which is gitignored.
#
# Usage: make-golden.sh [<game-root>]
#   <game-root> defaults to the StarFoxRecomp checkout that owns sf.sfc.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SNESRECOMP="$(cd "$RS_ROOT/.." && pwd)"

GAME_ROOT="${1:-/home/shoes/2026-glm-pi/StarFoxRecomp}"
ROM="$GAME_ROOT/sf.sfc"
CFG_DIR="$GAME_ROOT/recomp"
GOLDEN="$RS_ROOT/golden"

[ -f "$ROM" ] || { echo "make-golden: ROM not found: $ROM" >&2; exit 1; }
[ -d "$CFG_DIR" ] || { echo "make-golden: cfg dir not found: $CFG_DIR" >&2; exit 1; }

OUT="$GOLDEN/src_gen"
mkdir -p "$OUT"
echo "=== Python regen → $OUT ==="
python3 "$SNESRECOMP/tools/v2_regen.py" --rom "$ROM" --cfg-dir "$CFG_DIR" --out-dir "$OUT"

echo "=== Python funcs.h → $GOLDEN/funcs.h ==="
python3 "$SNESRECOMP/tools/v2_sync_funcs_h.py" --cfg-dir "$CFG_DIR" --out "$GOLDEN/funcs.h"

echo "=== golden snapshot complete ==="
ls -la "$OUT" | head
