//! The bulk integration-test target for `zeroship-runtime`.
//!
//! WHY THIS FILE EXISTS
//! --------------------
//! Cargo links one executable per `tests/*.rs`, and every one of them
//! statically links this crate, V8 and the whole dependency graph. There were
//! 137 such files. Measured on this tree at `[profile.dev]
//! debug = "line-tables-only"`, 2026-08-20:
//!
//!   before  137 integration executables, 20,371,393,640 bytes (18.97 GiB)
//!   after   9 integration executables, 1,558,333,112 bytes (1.45 GiB)
//!
//! `tests/common/` was compiled once per binary that declared it (34 of them)
//! and is now compiled once here.
//!
//! No test was added, dropped or renamed. Checked by keying every test on
//! `<source file stem>::<name within the file>` - which is the old binary name
//! plus its `--list` line before the merge, and exactly the module path
//! `--list` prints after it - and diffing the two sorted sets: 1466 lines
//! either side, `diff` empty.
//!
//! HOW TO ADD A TEST FILE
//! ----------------------
//! `Cargo.toml` sets `autotests = false`, so a new `tests/<name>.rs` is
//! compiled by NOTHING until something declares it. Add `mod <name>;` below in
//! the same commit as the file, or its tests never run and nothing says so.
//!
//! HOW TO RUN A SUBSET
//! -------------------
//! `cargo test -p zeroship-runtime --test <file>` no longer resolves for any
//! file listed below - they are modules, not targets. Filter instead, and note
//! the module path is now part of the name:
//!
//!   cargo test -p zeroship-runtime --test main -- streams::
//!
//! THE OTHER TARGETS, AND WHY EACH ONE IS NOT IN HERE
//! -------------------------------------------------
//! `Cargo.toml` declares eight more `[[test]]` entries. None of them is an
//! oversight:
//!
//!   wpt              23 `wpt_*.rs` files, merged into `tests/wpt.rs`. Their
//!                    `include_str!`s read `tests/wpt/`, which is NOT tracked
//!                    in git - `tests/setup-wpt.sh` fetches ~930 MB on demand.
//!                    A checkout that has not run it cannot compile them. Kept
//!                    as its own target so that stays true of ONE target
//!                    instead of every integration test in the crate.
//!
//!   node_realworld   the six `node_*_e2e.rs` files, merged into
//!                    `tests/node_realworld.rs`. They mutate process
//!                    environment and serialise it through
//!                    `support/node_realworld.rs::lock_env()`. Each declared
//!                    that helper with its own `#[path]`, so merging them
//!                    naively would compile six independent `ENV_LOCK`s and
//!                    the mutual exclusion would be gone. The entry declares
//!                    the helper once and the six consumers `use crate::`.
//!
//!   dev_auth, node_net, node_net_security, node_pg_e2e, node_tls
//!                    one target each. Every one saves, overwrites and
//!                    RESTORES process environment (`ZEROSHIP_DEV`,
//!                    `SSL_CERT_FILE`, `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS`, ...)
//!                    behind a `static ENV_LOCK` that is private to the file.
//!                    Two of them in one process are two different mutexes
//!                    over one environment: one restores `ZEROSHIP_DEV` to
//!                    unset while the other is mid-request under it, and
//!                    `ssrf.rs` starts blocking loopback. Merging them needs
//!                    ONE shared lock, which is a change to their bodies, so
//!                    it is deliberately not done here.
//!
//!   v8_fastcall_smoke
//!                    calls `v8::V8::set_flags_from_string(
//!                    "--allow-natives-syntax --turbofan --expose-gc")` and
//!                    relies on winning the race with `init_v8()`'s `Once`.
//!                    V8 flags are process-global and are silently ignored
//!                    after `V8::initialize()`. In a merged binary any
//!                    neighbour that touches a `Runtime` first initialises V8,
//!                    the flags are dropped, and `%OptimizeFunctionOnNextCall`
//!                    stops existing. It has to be the only thing in its
//!                    process.
//!
//! WHAT THIS CONSOLIDATION DOES NOT PROTECT AGAINST
//! ------------------------------------------------
//! State that used to be per-process is now per-target. Concretely, and this
//! is a loss of isolation, not a neutral change:
//!
//!   * `static`s are shared. A counter, a `OnceLock`, a lazily-installed hook
//!     in one module is visible to all 102. Nothing in the tree does this
//!     today - the audit that produced the partition above looked for
//!     `set_var`, `set_flags_from_string`, `set_hook`, `catch_unwind`,
//!     `should_panic` and fixed listening ports and found no other case - but
//!     "no case today" is what the check licenses, not "cannot happen".
//!
//!   * V8 is initialised once for the whole target. The first module to call
//!     `init_v8()` wins the flag set and the platform flavour; every later
//!     call is a no-op. A module that needs a different V8 configuration must
//!     become its own target, exactly like `v8_fastcall_smoke`.
//!
//!   * `ZEROSHIP_DEV` is set to "1" here and never unset. `fetch_native`,
//!     `fetch_native_install`, `eventsource` and `websocket_e2e` each set it
//!     one-way, and the five files that also RESTORE it were moved out to
//!     their own targets precisely so nothing here observes it flipping back.
//!     A new module added below that asserts the NON-dev path - SSRF blocking
//!     loopback, say - will pass or fail depending on which of its neighbours
//!     ran first. That is a real hazard and it is why they are listed.
//!
//!   * Wall-clock budgets now compete with 1368 neighbours instead of the
//!     handful in their own file. `url_native::
//!     search_params_iter_is_linear_not_quadratic` asserts a 2000-entry
//!     `for-of` finishes inside 1000 ms. On the FIRST full run of this target
//!     (2026-08-20, `/proc/loadavg` 16-19 on a shared box) it read 1291 ms and
//!     failed. It then passed 10/10 filtered to itself out of this binary,
//!     10/10 out of the old `url_native` binary, 5/5 in a full run of this
//!     binary and 5/5 in a full run of the old one, so it is not
//!     deterministic. Those clean runs also sat at a LOWER load than the
//!     failing one, so they do not establish that the merge left its flake
//!     rate where it was. Read a timing assertion in here as measuring a
//!     busier machine than it used to.
//!
//!   * One process means one abort. A module that aborts (not panics) takes
//!     the other 101 down with it and the run reports no result for any of
//!     them, where before it reported 136 clean targets and one crash.
//!
//! What the empty `--list` diff proves is that no test was DROPPED. It does
//! not prove any test still tests the same thing: a test that passes only
//! because a neighbour already initialised something lists identically to one
//! that stands alone. The pass-count comparison either side of the merge is
//! the check for that, and it is a weaker instrument than it looks - it
//! cannot distinguish "still correct" from "now passing for a new reason".

mod common;

mod abort;
mod abort_runtime;
mod ai_sdk_stream;
mod async_local_storage;
mod auth_plugin;
mod base64;
mod blob_native;
mod bootstrap_install_schema_resolve;
mod call_fetch_handler;
mod capability;
mod close_event;
mod codec;
mod compression_streams;
mod console;
mod cpu_limit;
mod crypto_native;
mod crypto_node;
mod crypto;
mod custom_event;
mod dom_exception;
mod dynamic_import;
mod env;
mod eventsource;
mod event_target;
mod fetch_body_blob;
mod fetch_body;
mod fetch_native_install;
mod fetch_native;
mod fetch_request;
mod fetch_response;
mod form_data;
mod headers;
mod heap_limits;
mod idle_gc;
mod iss66_built_rpc_dispatch;
mod iss71_sequential_streams;
mod message_event;
mod modules;
mod native_source_cancel_race;
mod next_tick_ordering;
mod node_buffer;
mod node_module_registration;
mod node_os;
mod node_path;
mod node_util;
mod node_zlib;
mod pump_cleanup;
mod pump_eviction_leak;
mod request_init_enums;
mod rpc_ctx;
mod rpc_dispatch;
mod rpc_error;
mod rpc_eviction;
mod rpc;
mod rpc_superjson;
mod schema_init;
mod serve_health_and_env;
mod streams_native;
mod streams;
mod structured_clone;
mod subscription;
mod text_encoding;
mod text_encoding_streams;
mod transform_stream_strategy_errors;
mod url_native;
mod url;
mod v8_async_iterable_smoke;
mod v8_async_method_compile_fail;
mod v8_async_method_smoke;
mod v8_brand_check_smoke;
mod v8_brand_pub_smoke;
mod v8_clamp_smoke;
mod v8_class_smoke;
mod v8_const_smoke;
mod v8_fastcall_compile_fail;
mod v8_iterable_brand_check_smoke;
mod v8_iterable_live_smoke;
mod v8_iterable_smoke;
mod v8_marker_attr_compile_fail;
mod v8_must_new_smoke;
mod v8_new_object_smoke;
mod v8_paired_accessor_smoke;
mod v8_post_init_compile_fail;
mod v8_post_init_smoke;
mod v8_recover_box_smoke;
mod v8_reentrancy_smoke;
mod v8_same_object_smoke;
mod v8_state_marker_compile_fail;
mod v8_state_marker_smoke;
mod v8_static_smoke;
mod v8_webidl_convert_smoke;
mod v8_webidl_dict_compile_fail;
mod v8_webidl_dict_smoke;
mod v8_webidl_enum_smoke;
mod v8_wrap_int_smoke;
mod web_apis;
mod websocket_attrs;
mod websocket_close;
mod websocket_construct;
mod websocket_e2e;
mod ws_auth_identity;
