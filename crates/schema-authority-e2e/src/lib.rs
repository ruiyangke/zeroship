//! `zeroship-schema-authority-e2e` — the **P7 capstone** crate.
//!
//! This crate carries no library code on purpose. It exists so a single test
//! target can dev-depend on the WHOLE schema-authority stack at once —
//! `zeroship-migrate-js` (P3), `zeroship-bundle` + `zeroship-control` (P6),
//! and `zeroship-plugin-db` (P4/P5) — and chain their REAL component functions
//! into one end-to-end pipeline. No other workspace crate depends on all four,
//! and nothing depends on THIS crate, so the dependency graph stays acyclic.
//!
//! The substance lives in `tests/capstone_pipeline_pg.rs`. See the design
//! `docs/proposals/2026-06-18-schema-authority-drizzle-model-design.md` §12 P7.
