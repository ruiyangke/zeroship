//! Registration shim — re-exports the public surface of the plugin's
//! V8 dispatch helpers from their per-concern homes. `v8_classes/*.rs`
//! and `tests/integration.rs` import everything through `callbacks::*`;
//! this module is the stable façade that lets the concerns underneath
//! move without breaking those callsites.
//!
//! | Concern | Home |
//! |---|---|
//! | V8 promise plumbing / value walker / row decoder | [`crate::v8_bridge`] |
//! | SQL execution + broker queue/drain | [`crate::exec`] |
//! | CRUD dispatch helpers (`dispatch_*`) | [`crate::crud`] |
//! | DDL orchestrator | [`crate::orchestrator::register_model`] |
//! | Explicit transaction lifecycle | [`crate::orchestrator::transaction`] |
//! | Auto-tx wrappers (`__zsBeginAutoTx` / `__zsEndAutoTx`) | [`crate::orchestrator::auto_tx`] |
//! | Replication op dispatchers + auto-spawn | [`crate::replication_ops`] |

// V8 ↔ Rust marshaling.
#[allow(unused_imports)]
pub(crate) use crate::v8_bridge::{
    fmt_db_err, get_app_id_pub, get_i64_arg, get_string_arg, read_json_arg,
    refuse_if_query_capability, row_to_json, rows_to_json, runtime_state, setup_js_promise,
    setup_promise, v8_value_to_serde_json,
};

// SQL execution + broker settle path.
#[allow(unused_imports)]
pub(crate) use crate::exec::{
    clear_pending_emits, drain_pending_emits_on_commit, ensure_pool, exec_count,
    exec_mutation_with_emit, exec_query, run_sql,
};
pub use crate::exec::exec_mutation_with_emit_for_tests;

// CRUD dispatch helpers.
#[allow(unused_imports)]
pub(crate) use crate::crud::{
    dispatch_aggregate, dispatch_count, dispatch_delete_many, dispatch_delete_one,
    dispatch_distinct, dispatch_find, dispatch_find_one, dispatch_find_or_create,
    dispatch_insert, dispatch_insert_many, dispatch_update_many, dispatch_update_one,
    dispatch_upsert,
};

// Orchestrator entry points.
pub use crate::orchestrator::auto_tx::{
    auto_begin_transaction, auto_end_transaction, install_auto_tx_globals,
};
pub use crate::orchestrator::register_model::{
    exec_register_model_with_pool, register_model_dispatch,
};
pub use crate::orchestrator::transaction::begin_transaction_dispatch;

// Replication operator + auto-spawn dispatchers.
pub use crate::replication_ops::{
    clear_consumer_registry_for_tests, is_consumer_registered_for_tests,
    replication_drop_abandoned_dispatch, replication_setup_dispatch,
    replication_watchdog_dispatch, start_replication_consumer_dispatch,
};
