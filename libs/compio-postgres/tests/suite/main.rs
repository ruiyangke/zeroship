// Hoisted from the three modules that used to carry it as crate roots
// (`pool_session_state_carry`, `timeout_interaction`,
// `pool_transaction_isolation`). A `#![recursion_limit]` inside a MODULE is
// IGNORED - rustc warns, and the build still succeeds on the default limit, so
// folding those files in dropped it in a way only the warning showed.
#![recursion_limit = "256"]

//! The crate's integration suite, as ONE test binary.
//!
//! Every `.rs` directly under `tests/` is a separate Cargo test target, and each
//! one statically links the whole driver. Folding 74 of them into modules here
//! took the crate from 79 test binaries to 5, and the clippy gate from 229
//! targets to 155. MEASURED 2026-08-26 in the debug profile: a single-file
//! target costs 12-13 MB (`serialized_loop`, `socket_release`), so the old
//! layout spent roughly 960 MB of linking per feature configuration and the
//! verification matrix builds four of them; this suite binary is 58 MB.
//!
//! AND THEY STILL BITE. A name-set diff proves the cases are PRESENT, never
//! that they still catch anything, so two guards were mutated after the fold
//! and both killed exactly the right test (2026-08-26, each mutation confirmed
//! applied by `git diff` before the run and restored to an empty diff after):
//! flipping the pool's release-time rollback condition in `pool.rs` to `==`
//! fails `integration::released_open_transaction_is_not_inherited_by_the
//! _next_borrower`, and raising `DEFAULT_MAX_MESSAGE_SIZE` 64x fails
//! `message_size_limit::a_message_over_the_limit_names_the_limit` while its
//! three siblings correctly survive, since they do not depend on the default.
//!
//! THE CASES ARE UNCHANGED. Verified by diffing the test-name sets before and
//! after: 869 distinct names on each side, empty difference both ways. The
//! headline count falls 1779 -> 895 because `common`'s 13 tests were being
//! compiled and RE-EXECUTED in 70 separate binaries; 1779 - 895 is exactly
//! 69 x 13. A test that was `foo.rs::bar` is now reported as `foo::bar`.
//!
//! Four targets stay separate, each for a reason that would break if folded:
//!   - `tls_live`, `unix_socket_live` - declared with `required-features`, so
//!     they must be able to not build at all
//!   - `serialized_loop` - already an explicit target
//!   - `socket_release` - installs a process-global `log::set_logger`, and so
//!     does `transaction_claims` in here. Only the first call in a process
//!     succeeds, so one of the two has to own its own process.
//!
//! Helper names collide freely across these modules (`connected`, `client`,
//! `fixture` are each defined many times) because each file is its own module
//! namespace. That is why the files could be folded without renaming anything.
//!
//! What DOES need care when adding a file here: a crate-level inner attribute.
//! `#![cfg(...)]` keeps working (it applies to the module), but anything that is
//! meaningful only at a crate root - `recursion_limit` above, and any test that
//! re-executes itself by test NAME, which now needs the module prefix - does
//! not.

#[allow(dead_code)]
#[path = "../common/mod.rs"]
pub mod common;

mod backend_termination;
mod batch_atomicity;
mod cancel_request;
mod client_encoding;
mod column_lookup;
mod column_metadata;
mod command_timeout;
mod concurrent_routing;
mod config_fuzz;
mod connect_failure_diagnosis;
mod connection_churn;
mod copy_in_failure;
mod copy_interleaving;
mod copy_out_abandonment;
mod copy_out_copy_in_resync;
mod copy_refusal;
mod differential_tokio;
mod domain_parameters;
mod error_fields;
mod execute_row_counts;
mod extended_query_copy_resync;
mod frame_fuzz;
mod generic_client;
mod hostile_peer;
mod hostile_session_gucs;
mod integration;
mod keyword_quoting;
mod keyword_whitespace;
mod libpq_parameter_parity;
mod lsn_server_parity;
mod message_size_limit;
mod nested_transaction;
mod notice_delivery;
mod notification_identity;
mod parameter_status;
mod passfile_live;
mod pgoutput_allocation;
mod pgoutput_fuzz;
mod pgoutput_live_decode;
mod pgoutput_options;
mod pgoutput_streaming;
mod pgoutput_subtransactions;
mod pgoutput_two_phase;
mod pool_close;
mod pool_fairness;
mod pool_hooks;
mod pool_lifetime;
mod pool_session_state_carry;
mod pool_transaction_isolation;
mod portal_abandonment;
mod portal_name_collision;
mod portal_paging;
mod prefer_attestation_fallback;
mod protocol_version_live;
mod query_backpressure;
mod query_claims;
mod query_observer;
mod raw_value_column_identity;
mod read_timeout;
mod record_array;
mod replication_live;
mod replication_publication_names;
mod require_auth_enforcement;
mod row_count_resync;
mod service_live;
mod simple_query_copy_chain_resync;
mod simple_query_copy_resync;
mod simple_query_protocol;
mod sqlstate_identity;
mod startup_options;
mod target_session_attrs_live;
mod temporal_edge_values;
mod timeout_interaction;
mod transaction_builder;
mod transaction_claims;
mod type_cache_residue;
mod type_edge_values;
mod unix_socket_path_limit;
mod url_credentials;
mod url_parity;
mod value_round_trip;
