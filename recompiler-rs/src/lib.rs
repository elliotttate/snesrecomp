//! Rust port of the snesrecomp v2 static recompiler ("regen") pipeline.
//!
//! Ports `recompiler/v2/*` plus `tools/v2_regen.py` from Python. The design
//! replaces the Python pipeline's ~15 process-global mutable singletons with
//! two threaded, immutable-after-build context structs (`DecodeEnv`, `EmitCtx`)
//! and makes per-bank emit return data (`EmitOutcome`) rather than mutate
//! globals, so `rayon`-parallel emit stays data-race-free.
//!
//! Target is *functional* equivalence with the Python output — generated C must
//! compile and the game must boot/render/test the same — not byte-identity.
//! Ordering that reaches output is normalized via sorting / `BTreeMap` rather
//! than replicating CPython's `set`-iteration quirks.

// Phase 1 — foundations.
pub mod cfg;
pub mod cycles;
pub mod insn;
pub mod ir;
pub mod rom;
pub mod widths;

// Phase 2+ modules land here as they are ported.
