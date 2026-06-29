# snesrecomp-regen (Rust)

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

## Status

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
