//! Workspace shape: how the crates are laid out on disk, and what cargo makes of it.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.

mod every_crate_dir_matches_its_package;
