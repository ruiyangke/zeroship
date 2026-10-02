//! Name claims: the namespaces an object occupies, and the identifiers it may not claim twice.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


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
