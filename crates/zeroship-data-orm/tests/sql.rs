#[path = "sql/determinism.rs"]
mod determinism;
#[path = "sql/ident_refusals.rs"]
mod ident_refusals;
#[path = "sql/joins.rs"]
mod joins;
#[path = "sql/no_sql_text_escape_hatch.rs"]
mod no_sql_text_escape_hatch;
#[path = "sql/null_is_a_node.rs"]
mod null_is_a_node;
#[path = "sql/parameters_never_carry_values.rs"]
mod parameters_never_carry_values;
#[path = "sql/plan_invariants.rs"]
mod plan_invariants;
#[path = "sql/predicate_depth.rs"]
mod predicate_depth;
#[path = "sql/projection_rules.rs"]
mod projection_rules;
#[path = "sql/public_contract.rs"]
mod public_contract;
#[path = "sql/search_family.rs"]
mod search_family;
#[path = "sql/write_family.rs"]
mod write_family;
