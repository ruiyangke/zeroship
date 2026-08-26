//! Name claims: the namespaces an object occupies, and the identifiers it may not claim twice.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod authored_identifier_lengths;
mod comment_schema_canonicalization;
mod duplicate_constraint_name_on_one_table;
mod duplicate_index_names_in_one_table;
mod duplicate_trigger_names_pg;
mod function_signatures_claimed_twice;
mod hostile_identifiers;
mod index_shares_the_relation_namespace;
mod name_claimed_twice_in_one_migration;
mod partition_claims_the_relation_namespace_pg;
mod privileged_names_claimed_twice;
mod relation_namespace_is_shared;
mod type_namespace_is_shared_pg;
mod unique_constraint_duplicate_columns;
