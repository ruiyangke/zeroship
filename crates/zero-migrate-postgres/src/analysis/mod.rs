pub mod analyze;
pub mod classify;
// `pub` (not `pub(crate)`): the engine's precondition shape-gate imports
// `first_dml_node` across the crate boundary.
pub mod tree_walk;
