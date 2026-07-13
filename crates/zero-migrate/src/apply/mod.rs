pub mod backend;
pub mod baseline;
pub mod drift;
pub mod executor;
// The bundle IR envelope apply entry points (moved out of the retired `command/`
// module — the CLI half of `command/` was retired into the
// `zero-migrate-engine` TS CLI; this non-CLI engine logic now lives under `apply::`).
pub mod ir_apply;
pub mod journal;
pub mod precondition;
pub mod role;
