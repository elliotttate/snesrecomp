# snesrecomp native analysis (Rust)

## Current LLE-first analyzer

`src/bin/analyze.rs` is the supported experiment on the current branch. It
ports the hot whole-program variant/exit-MX fixed point while keeping the
current Python emitter as the output authority. Its JSON uses
`ProgramManifest` format 3, so `tools/v2_emit.py --analysis-backend native`
can consume it directly. `auto` mode falls back to Python when a release
binary is unavailable or fails.

Build and use it with:

```bash
cd recompiler-rs
cargo build --release --bin analyze
cd ..
python tools/v2_emit.py --rom game.sfc --cfg-dir recomp \
  --out-dir src/gen --cfg-roots --analysis-backend native
```

Mega Man X static-coverage benchmark on 2026-07-18 (same machine):

| Analyzer | Time | Variants | Exact exits | Mode sets |
|---|---:|---:|---:|---:|
| Python baseline | 402.542 s | 4,561 | 4,032 | 558 |
| Rust native | 14.598-15.693 s | 4,561 | 4,032 | 558 |

That is 25.7-27.6x faster and clears the 25x target (16.1 seconds). The AOT/LLE
split also matches (4,551/10). `tools/v2_compare_analysis.py` checks this
emission compatibility boundary. Diagnostic graph summaries are not yet
byte-identical: the historical Rust decoder statically classifies some
indirect dispatches that current Python records as runtime/LLE edges. Pass
`--strict-summaries` to expose that remaining porting work.

Using the Python baseline manifest and the native manifest to drive the same
current Python emitter produced byte-identical generated C; only cache
metadata and `program_manifest.json` diagnostics differed.

## Historical full-regenerator port

Rust port of the snesrecomp v2 static recompiler ("regen") pipeline — the thing
that turns a ROM + per-bank `.cfg` files into one generated C file per bank, a
`dispatch_v2.c`, and `recomp/funcs.h`. Replaces `tools/v2_regen.py` and the
`recompiler/v2/*` package.

**Why:** speed (the Python regen runs against a fixed 1.5 h watchdog; decode cache
is disabled) and a clean typed core (the Python pipeline threads ~15 process-global
mutable singletons). See `docs/` and the plan in the StarFoxRecomp repo.

**Fidelity target:** *functional* equivalence with the Python output — generated C
must compile and the game must boot/render/test the same — not byte-identity.

## Layout

- `src/` — library modules (`rom`, `cfg`, `ir`, `widths`, `cycles`, `decoder`,
  `cfgbuild`, `lowering`, `codegen`/`emit`, `autoroute`).
- `src/bin/regen.rs` — replaces `v2_regen.py`.
- `src/bin/sync_funcs_h.rs` — replaces `v2_sync_funcs_h.py`.
- `scripts/make-golden.sh` — run the *Python* regen to produce a reference snapshot
  under `golden/` (gitignored; leaked-source derived).
- `scripts/diff-golden.sh` — diff Rust output against the golden snapshot (dev aid).

## Build / test

```bash
cargo build --release
cargo test
```

## Historical status

Porting in phases (bottom-up, each gated by ported tests):

- [x] Phase 0 — workspace scaffold + golden oracle
- [x] Phase 1 — foundations (rom, insn, ir, widths, cycles, cfg) — parses all 43
  SF cfgs, matches the Python loader exactly (1486 entries)
- [~] Phase 2 — decoder + CFG build. Type contract + `DecodeEnv` fixed; the
  differential oracle (`scripts/dump_decode.py` → `golden/decode.json`, 1486
  reference graphs) is built. The `decode_function` body is in progress, gated
  on exact graph equality vs the oracle.
- [ ] Phase 3 — lowering + codegen + emit
- [ ] Phase 4 — autoroute passes
- [ ] Phase 5 — orchestrator + funcs.h
- [ ] Phase 6 — cutover
