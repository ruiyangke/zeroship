# PgBouncer residue classification

Date: 2026-08-28

## Scope and method

Each of the 65 names in `RESIDUE_NAMES.txt` was run as an exact libtest filter
through PgBouncer on port 6548 and directly on port 5473, always with
`--test-threads=1` and the requested feature set. Every invocation reported
`running 1 test`; no zero-match result was accepted. The result was:

- PgBouncer: 0 passed, 65 failed.
- Direct: 65 passed, 0 failed.
- Failed both ways: none.

The live pooler was PgBouncer 1.25.2. `SHOW CONFIG` reported
`pool_mode=transaction`, `default_pool_size=20`,
`max_prepared_statements=200`, and
`server_reset_query_always=0`. The generated configuration ignores
`search_path`, `default_transaction_isolation`,
`default_transaction_read_only`, `extra_float_digits`, `options`, and
`application_name`. Although `SHOW CONFIG` displays the default
`server_reset_query=DISCARD ALL`, it is not run on transaction-pool handoff
when `server_reset_query_always=0`; the file has no explicit reset setting.

PgBouncer was restarted before the sweep. It was also restarted immediately
before isolated reruns of the ambiguous COPY observer, prepared-statement,
open-transaction, schema-isolation, pool-hook, terminated-backend, and
type-cache cases. Those reruns reproduced the same failure mechanisms. One
portal-ownership case was clean-rerun a second time. The `42P07` and `42710`
fixture collisions persisted across a restart because ignored `search_path`
had put `tx_leak` and `cpg_cache_enum` in `public`; a read-only catalog query
confirmed both objects there.

The categories are applied as follows:

- **A**: the assertion needs backend/session affinity or another behavior that
  transaction pooling does not promise.
- **B**: the driver is wrong and the pooler merely exposes it.
- **C**: the driver behavior under test completed correctly, and only an
  incidental direct-connection oracle or endpoint-specific diagnostic made the
  test fail.

## Classification

| # | Test | Category | Concrete failure and mechanism |
|---:|---|:---:|---|
| 1 | `backend_termination::a_checked_out_idle_pool_backend_is_discarded_after_termination` | A | The `application_name` selector found `[]`, not PID `271701`. `application_name` is ignored and an idle PgBouncer frontend owns no fixed PostgreSQL backend, so the test cannot target the driver's physical pool entry. |
| 2 | `backend_termination::a_checked_out_pool_backend_killed_mid_query_is_discarded` | A | The selector again found no tagged backend. A PID sampled in one transaction does not identify the backend serving the later query, and killing a server connection does not necessarily close the driver-to-PgBouncer connection. |
| 3 | `backend_termination::all_idle_pool_backends_are_replaced_after_mass_termination` | A | Holding three driver leases exposed one PostgreSQL PID, not three. Three logical frontends do not imply three persistent server backends in transaction mode. |
| 4 | `cancel_request::a_cancel_request_with_the_wrong_secret_key_is_inert` | A | The 5 s watchdog expired before the positive-control cancel. PgBouncer synthesizes `BackendKeyData`; its cancel key/router, not a stable PostgreSQL PID/key pair, is the endpoint contract being exercised. |
| 5 | `cancel_request::a_token_from_a_returned_pool_lease_cannot_cancel_the_next_borrower` | A | The 5 s watchdog expired in the readiness observer, which searched `pg_stat_activity` with the next frontend's synthetic process ID. Transaction pooling cannot provide the stable PostgreSQL PID precondition this test requires; the local ended-lease assertion was never reached. |
| 6 | `cancel_request::cancel_during_copy_in_surfaces_57014_and_preserves_session` | A | `COPY progress for backend 1389498766 did not become visible`. That is PgBouncer's synthetic key, not a PostgreSQL PID. The temp table, COPY, and follow-up also require one backend across transaction boundaries. |
| 7 | `cancel_request::cancel_during_copy_out_surfaces_57014_and_preserves_session` | A | The same progress lookup failed for synthetic ID `1451150439`. Active-query cancellation and the progress PID are owned/routed by PgBouncer, not exposed as one stable PostgreSQL session. |
| 8 | `cancel_request::cancel_inside_transaction_requires_rollback_then_preserves_session` | A | The transaction-cancel watchdog expired. `BEGIN` pins a backend, but the observer still queried `pg_stat_activity` with PgBouncer's synthetic process ID, and cancel routing remains a pooler endpoint semantic. |
| 9 | `cancel_request::raw_cancel_interrupts_running_query_and_preserves_session` | A | The raw-cancel watchdog expired. The packet went to PgBouncer, whose negotiated protocol and synthetic cancel key are not PostgreSQL's direct `BackendKeyData` contract. |
| 10 | `cancel_request::running_query_cancel_returns_57014_and_preserves_session` | A | The 5 s cancel watchdog expired after the direct-PID readiness assumption failed. Transaction pooling does not expose a stable PostgreSQL cancel target for this oracle. |
| 11 | `cancel_request::two_cancels_for_one_running_query_leave_the_session_usable` | A | The two-cancel watchdog expired. Both packets address PgBouncer's transient active-query mapping, not the stable PostgreSQL backend/key pair the test assumes. |
| 12 | `client_encoding::the_startup_packet_announces_utf8_rather_than_inheriting_it` | C | The setting was UTF8, but `pg_settings.source` was `default`, not `client`. The driver announced UTF8 to PgBouncer; PostgreSQL sees PgBouncer's backend startup, so only the direct-server provenance oracle differs. |
| 13 | `command_timeout::command_inside_the_deadline_is_untouched` | C | The in-budget command succeeded and returned 7; only PID equality failed (`271729` versus synthetic `664614590`). The timeout behavior was correct. |
| 14 | `command_timeout::direct_pooled_client_query_does_not_enter_command_scope` | C | The 200 ms direct query completed; only PostgreSQL PID versus synthetic frontend ID failed (`271729` versus `72940389`). The command-scope claim passed. |
| 15 | `command_timeout::overrun_is_cancelled_and_the_same_client_remains_usable` | C | Timeout classification, cancellation, draining, and the follow-up query all completed; the first failure was only PID equality (`271729` versus `587568137`). |
| 16 | `command_timeout::timeout_inside_raw_transaction_rolls_back_before_same_client_reuse` | C | Recovery reached a successful follow-up returning 42; only PID equality failed (`271733` versus `70694744`). A post-rollback transaction pool is free to choose another backend. |
| 17 | `connect_failure_diagnosis::a_missing_database_is_named` | C | PgBouncer returned `FATAL: no such database: zz_no_such_database`, which names the database but not PostgreSQL's hardcoded phrase `does not exist`. This is a correct endpoint-specific diagnostic. |
| 18 | `connection_churn::sustained_connection_and_pool_churn_leaves_nothing_behind` | A | `pg_stat_activity` saw zero tagged clients where the test required one. Ignored `application_name`, decoupled frontend/backend lifetimes, session locks, and no effective reset make the direct-backend census unavailable. |
| 19 | `differential_tokio::both_drivers_agree_on_listen_notify_routing` | A | The two logical clients were assigned the same PID (`271733`) before the notification assertions. `LISTEN` registration is backend-session state and is detached from the logical listener at transaction handoff. |
| 20 | `differential_tokio::the_implicit_cache_retries_a_stale_plan_outside_a_transaction` | A | The third outcome was SQLSTATE `0A000`, not rows. Named prepared plans are backend-local; the crate itself documents that the implicit cache must be disabled behind transaction poolers. |
| 21 | `integration::abandoned_copy_in_startup_rejection_does_not_poison_the_next_operation` | A | The 10 s watchdog reproduced after a clean restart. Its loop searched `pg_stat_activity` with `target.process_id()`, which is synthetic behind PgBouncer. The required stable backend identity is unavailable, and the protocol-recovery assertion was never reached. |
| 22 | `integration::a_template_clone_is_not_blocked_by_the_previous_runtime` | A | Fixture cleanup failed immediately with PgBouncer's `08P01 FATAL: no such database: postgres`; this pooler maps only `zeroship`. Even with a mapping, closing the driver frontend need not close PgBouncer's server session to a template database, which is the direct-session property the test requires. |
| 23 | `integration::cancelled_cached_plan_error_is_not_handed_to_the_next_borrower` | A | The next borrower received stale-plan SQLSTATE `0A000`. A one-entry driver pool is still a frontend pool; it does not preserve the temp table and cached plan on one server backend. |
| 24 | `integration::cancelled_name_collision_preserves_existing_statement` | A | The test could not find a driver name shaped as `sN`. PgBouncer rewrites and virtualizes protocol-level prepared names, so raw server-name collision introspection is not the driver's namespace. |
| 25 | `integration::cancelled_prepare_does_not_leak_a_server_statement` | A | A clean rerun changed `pg_prepared_statements` from 3 to 4 after a dropped prepare. With `max_prepared_statements=200`, PgBouncer may retain/deduplicate a physical plan after frontend `Close`; this count cannot prove a driver leak. |
| 26 | `integration::clear_type_cache_refreshes_implicitly_cached_statement_metadata` | A | SQLSTATE `42710`: `type cpg_cache_enum already exists`. Ignored startup `search_path` put the fixed type in `public` (confirmed in the catalog); the test also relies on named cache state across transactions. |
| 27 | `integration::generated_name_collision_preserves_existing_statement` | A | The test could not parse an `sN` server name. PgBouncer's rewritten names and backend handoff invalidate the raw prepared-name reservation/collision oracle. |
| 28 | `integration::identical_sql_has_distinct_names_and_each_drop_closes_one` | A | The clean rerun saw one server plan, not two: PgBouncer deduplicated identical SQL under its prepared-statement virtualization. Physical server plan count is not frontend `Statement` identity. |
| 29 | `integration::notify_delivered_on_idle_listener` | A | The idle listener received nothing within 5 s. `LISTEN` remains registered on the released server backend, not on the idle PgBouncer frontend. |
| 30 | `integration::released_open_transaction_is_not_inherited_by_the_next_borrower` | A | SQLSTATE `42P07`: `relation tx_leak already exists`, reproduced after restart before the release assertion. This observed relation was not temporary: ignored `search_path` left persistent `public.tx_leak` (catalog-confirmed). The driver rollback path was not reached. |
| 31 | `integration::statement_cache_bypass_is_one_shot` | A | The server-plan count was one, not two. PgBouncer deduplicated/virtualized the cached and one-shot prepares, so `pg_prepared_statements` cannot witness driver cache bypass. |
| 32 | `integration::statement_cache_capacity_zero_ignores_the_execution_threshold` | A | The plan count was one, not two. Separate raw prepares can be deduplicated or observed on different server backends. |
| 33 | `integration::statement_cache_capacity_zero_prepares_every_call` | A | The plan count was one, not four. PgBouncer's physical prepared-plan cache is not a count of driver prepare calls. |
| 34 | `integration::statement_cache_concurrent_stale_callers_share_one_replacement` | A | Clean rerun returned pooler SQLSTATE `08P01`, `prepared statement did not exist`; PgBouncer logs named this a pooler error. SQL `DEALLOCATE ALL` and frontend cached names desynchronize its virtual name map. |
| 35 | `integration::statement_cache_does_not_reprepare_an_explicit_statement` | A | The intended explicit use returned `0A000`, but a fresh replacement `prepare` then returned the same stale-plan error. The temp relation and prepared plan are backend-local, while PgBouncer may reuse/deduplicate its physical plan across logical prepares. |
| 36 | `integration::statement_cache_does_not_retry_0a000_after_parameter_input` | A | The intended `0A000` occurred, but the physical-plan probe said the unsafe entry remained. PgBouncer's retained/deduplicated plan is not the driver's logical cache entry; all fixtures are also session-local. |
| 37 | `integration::statement_cache_does_not_retry_0a000_inside_a_transaction` | A | After rollback, refresh still returned stale-plan `0A000`, reproduced cleanly. The plan was warmed before `BEGIN`; transaction pinning cannot restore the earlier backend-local temp table/plan provenance. |
| 38 | `integration::statement_cache_does_not_retry_after_a_savepoint` | A | Expected PostgreSQL `26000`, received PgBouncer `08P01`. The pre-transaction cached name and in-transaction `DEALLOCATE ALL` do not establish one physical prepared-statement namespace through the pooler. |
| 39 | `integration::statement_cache_eviction_closes_after_outstanding_clones` | A | The evicted SQL remained visible in `pg_prepared_statements`. PgBouncer may retain a physical plan after the frontend logical statement is closed. |
| 40 | `integration::statement_cache_eviction_waits_for_a_bound_portal` | A | Even in one pinned transaction, the final physical-plan emptiness assertion failed on two clean reruns. The driver `Close` passes through PgBouncer's `max_prepared_statements=200` virtualization, which may retain the server plan; direct execution passes. |
| 41 | `integration::statement_cache_evicts_the_least_recently_used_sql` | A | The supposedly evicted SQL remained physically prepared. Backend-local/deduplicated PgBouncer plans cannot expose the driver's logical LRU contents. |
| 42 | `integration::statement_cache_execution_count_resets_after_prepared_eviction` | A | SQL A remained in `pg_prepared_statements` after logical eviction. The physical pooler plan is not driver admission history. |
| 43 | `integration::statement_cache_retries_a_statement_missing_after_deallocate_all` | A | PgBouncer returned fatal `08P01 prepared statement did not exist`, not PostgreSQL `26000`; its own log called this a pooler error. Raw `DEALLOCATE ALL` invalidated the pooler's tracked physical name. |
| 44 | `integration::statement_cache_retries_a_statement_missing_after_discard_all` | A | The same PgBouncer `08P01` occurred after `DISCARD ALL`. This SQL clears a server backend behind PgBouncer's virtual prepared-name map. |
| 45 | `integration::statement_cache_retries_execute_without_double_applying` | A | The retry stopped at PgBouncer fatal `08P01 prepared statement did not exist`. The temp table, cached UPDATE, and `DEALLOCATE ALL` require one server session. |
| 46 | `integration::statement_cache_retries_stale_result_shape_once_after_0a000` | A | PostgreSQL `0A000 cached plan must not change result type` escaped instead of being healed. The temp relation and named plan were established across transactions without backend affinity. |
| 47 | `integration::statement_cache_retries_stale_result_shape_under_sustained_concurrency` | A | `0A000` escaped during the concurrent rounds. Repeated temp-table ALTERs and named-plan reuse are session-local and unsupported by transaction handoff. |
| 48 | `integration::statement_cache_retries_stale_result_shape_while_a_peer_request_is_in_flight` | A | The busy request received `0A000`. The cache's stale-plan premise depends on a temp relation and one backend-local prepared plan. |
| 49 | `integration::statement_cache_stale_reprepare_keeps_admission` | A | PgBouncer returned fatal `08P01 prepared statement did not exist`. `DEALLOCATE ALL` and the admitted cached name do not share a stable physical statement namespace. |
| 50 | `integration::test_isolation_is_per_schema` | A | `current_schema()` was `public`, not the unique test schema, including after a clean restart. The pooler explicitly ignores the startup `options=-c search_path=...`; session schema cannot follow transaction handoff. |
| 51 | `notification_identity::a_notification_carries_the_notifying_backends_process_id` | A | Two logical clients received the same PostgreSQL PID (`271790`), invalidating the test's distinct-backend precondition. LISTEN registration and notification-source identity are server-session semantics. |
| 52 | `notification_identity::a_notification_without_a_payload_delivers_an_empty_string` | A | No notification arrived within 10 s. The LISTEN registration stayed on a released backend and was not attached to the logical frontend. |
| 53 | `pool_hooks::after_connect_failure_discards_the_connection` | A | A clean rerun said the replacement inherited the failed hook's GUC. Discarding a driver frontend does not discard/reset its PgBouncer server backend; transaction handoff runs no reset (`server_reset_query_always=0`). |
| 54 | `pool_hooks::after_release_false_discards_the_dirty_session` | A | The next borrower inherited the dirty GUC. This is the expected missing-reset session-state leak, not proof the driver reused its discarded frontend. |
| 55 | `pool_hooks::before_acquire_false_discards_and_retries` | A | The accepted borrower received the rejected session's marker. Multiple driver frontends can reuse the same unreset server backend. |
| 56 | `pool_lifetime::max_lifetime_rotates_without_a_housekeeper` | A | Before and after max lifetime, the test saw PostgreSQL PID `271791`. The test defines that PID as physical-connection identity, but replacing a driver-to-PgBouncer socket does not require PgBouncer to replace its reusable server backend; no pooler-valid rotation oracle is present. |
| 57 | `pool_transaction_isolation::a_terminated_backend_is_not_handed_to_the_next_borrower` | A | The would-be killer received fatal `57P01 terminating connection due to administrator command`, reproduced cleanly: PgBouncer assigned it the sampled backend, so it killed itself. A prior transaction's PID is not owned by the next frontend operation. |
| 58 | `query_claims::copy_in_close_commits_input_and_keeps_the_client_usable` | A | COPY and the follow-up query completed, but PostgreSQL PID `271790` did not equal synthetic frontend ID `704714174`. The test also uses a temp table across transactions, so its fixture is not guaranteed even though this assignment happened to retain it. |
| 59 | `query_observer::observer_reports_dropped_in_flight_future_cancelled_once` | A | The 5 s readiness loop never found the executing statement because it used a synthetic process ID. Its session advisory-lock fixture also cannot be acquired and later released reliably across transaction handoffs. |
| 60 | `startup_options::options_reaches_the_backend_in_both_connection_string_forms` | A | Backend `search_path` stayed `"$user", public`, not `pg_catalog`. The configured pooler explicitly accepts then ignores `options`, so a per-frontend startup GUC does not become stable server-session state. |
| 61 | `target_session_attrs_live::primary_accepts_a_read_only_session` | A | `default_transaction_read_only` was `off`, not `on`. PgBouncer ignores that startup parameter and does not preserve the requested session GUC. |
| 62 | `target_session_attrs_live::read_only_accepts_a_session_configured_read_only` | A | The target probe rejected with `database is not read only`. The configured read-only startup option was ignored before the probe reached a server backend. |
| 63 | `timeout_interaction::command_timeout_recovers_without_a_read_timeout` | C | Command timeout recovery and the follow-up query completed; only PID equality failed (`271802` versus synthetic `622467307`). The driver recovery behavior was correct. |
| 64 | `type_cache_residue::abandoned_stale_typeinfo_lookup_cleans_the_cache_for_the_next_lookup` | A | Clean rerun received PgBouncer fatal `08P01 prepared statement did not exist`. The `pg_temp` type, internal helper, and `DEALLOCATE ALL` require one backend-local namespace. |
| 65 | `type_cache_residue::stale_typeinfo_statement_failure_cleans_the_cache_for_the_next_lookup` | A | Clean rerun expected PostgreSQL `26000` but received PgBouncer `08P01`. The pooler's virtual prepared-name map, not driver cache cleanup, produced the error. |

## Summary

| Category | Count |
|---|---:|
| A. Pooler limitation, correct behavior | 58 |
| B. Our defect, visible only through a pooler | 0 |
| C. Test needs mode-awareness | 7 |
| **Total** | **65** |

There are no category B findings, so there is no asserted B to argue against.
The strongest near-B cases were nevertheless checked against the opposite
case:

- `released_open_transaction_is_not_inherited_by_the_next_borrower` should, in
  principle, work because PgBouncer pins an open transaction and the driver
  queues rollback on release. Against B, the observed and clean-rerun failure
  is earlier: SQLSTATE `42P07` on fixture creation. The catalog showed
  `public.tx_leak`, placed there because startup `search_path` is ignored. No
  rollback/reborrow assertion ran.
- `statement_cache_eviction_waits_for_a_bound_portal` keeps its measured work
  inside one explicit transaction, so backend switching alone is not an
  answer. Against B, the failed assertion observes physical
  `pg_prepared_statements`, while PgBouncer is configured to virtualize and
  retain physical prepared plans (`max_prepared_statements=200`). It reproduced
  twice after restarts and passes directly; the driver/frontend `Close` and the
  pooler's physical plan lifetime are different contracts.
- `abandoned_copy_in_startup_rejection_does_not_poison_the_next_operation`
  looks exactly like a stranded protocol response because it hits a 10 s
  watchdog. Against B, it is spinning in a `pg_stat_activity` lookup keyed by
  PgBouncer's synthetic `process_id()`. The recovery query is never reached.
- The fatal prepared-statement `08P01` cases could look like driver protocol
  corruption. Against B, PgBouncer's own logs say `pooler error: prepared
  statement did not exist`, and the cases deliberately issue SQL
  `DEALLOCATE ALL`/`DISCARD ALL` behind its virtual prepared-name map.

SQLSTATE `42P07` from a temp table surviving on a pooled backend is category A
as required. The observed `tx_leak` `42P07` was a separate A mechanism: the
ignored schema startup option left a persistent relation in `public`.
SQLSTATE `57014` from a leaked `statement_timeout` is also category A:
transaction mode with `server_reset_query_always=0` can hand that GUC to
another frontend. No isolated exact-name run in this sweep had that leaked
`57014` as its first failure, so it was not used to relabel a different
observed mechanism.

## The count is a stable core plus a flaky fringe, measured 2026-08-28

The 58 in this document is not a single reading. Three full pooled runs of the
`suite` target were taken at `85bf15fbe` and `0b509285a`:

    run at 85bf15fbe   58 failed
    run 1 at 0b509285a 59 failed
    run 2 at 0b509285a 63 failed

Set-differencing them, rather than comparing totals:

- **58 tests failed in ALL THREE runs.** That intersection is the real
  category-A residue this document classifies.
- Run 1 added `read_timeout::copy_input_time_is_not_charged_as_server_read_silence`.
  **It is category A, and calling it timing-sensitive here was wrong.**
  Measured 2026-08-28 by repetition rather than by a single isolated run:
  5 consecutive runs against 5455 all passed at 10.08s, while against 6548 the
  FIRST passed at 10.02s and runs 2-5 failed in 0.05s each. A ten-second pass
  followed by four fifty-millisecond failures is not jitter; it is state.

  The cause is exact: `read_timeout.rs:939` creates a fixture named
  `cpg_read_timeout_copy` - a FIXED name, not `common::test_object_name` - and
  the second run dies on
  `SQLSTATE 42P07: relation "cpg_read_timeout_copy" already exists`. The temp
  table survives on the pooled backend, which is the same category-A mechanism
  this document already records for 42P07.

  **So the residue total depends on execution history, not only on the code.**
  Whether this test fails depends on whether an earlier run left its table on
  the backend the pooler hands out. That, not jitter, is what moved the total
  between runs. One isolated run is not enough to classify a pooled failure;
  repeat it, and read a fast failure after a slow pass as leaked state.
- Run 2 added four `differential_tokio::*` COPY cases
  (`copy_out_bytes`, `copy_in_results`, `binary_copy_roundtrip`,
  `a_copy_without_a_producer_costs_tokio_the_connection_and_not_this_one`).

**Do not read a total as a regression signal.** A run reporting 59 or 63 has not
regressed against 58; it has picked up members of the fringe. Compare the FAILURE
SET against the 58-name core, the way this section was derived - a total moved by
five while the core was byte-identical.

The six tests added between those two commits (three startup-handoff cases and
the `differential_server_errors`, `differential_config_parsing` and
`differential_copy` oracles) appear in NEITHER fringe, so the new suites are
pooler-stable.

## After fixture isolation (`4255a2e42` + `1d8bbed27`), measured 2026-08-28

54 literal-named relation fixtures were given process-scoped names across the
two sweeps; a `grep` for a literal `CREATE ... TABLE <name>` in the suite now
matches only a string inside an assertion message. Measured against 6548:

    42P07 matches, two consecutive full runs   0 and 0
    failure totals, two consecutive full runs  61 and 62

**The self-collision is gone.** `read_timeout::copy_input_time_is_not_charged_as_server_read_silence`
now passes TWICE in isolation on the pooler (10.01s each); before the sweep it
passed once at 10.02s and then failed four times in 0.05s each.

Set-differenced against the 58-name core rather than compared by total:

- **One core member now PASSES**:
  `integration::released_open_transaction_is_not_inherited_by_the_next_borrower`.
  It had been failing on a fixture collision, not on pooler semantics.
- **Five sit above the core**, all COPY-related: the four
  `differential_tokio::*` copy cases and the `read_timeout` case above. Each
  passes in isolation and fails only inside a full serial run, so the remaining
  mechanism is session state left by an EARLIER TEST, not by a previous run of
  itself. That is category A and outside the driver.

**A rising total after this change is not a regression.** A test that used to die
early on 42P07 never reached the assertions that could fail for a genuine pooled
reason; removing the collision lets it run further. Compare the SET.

## The residue is version-independent except for one entry, measured 2026-08-28

Every measurement above used `6548`, which fronts the PG **16.14** server on
5455. The suite was run for the first time against `6549`, which fronts the PG
**18.4** server on 5459 (confirmed by container IP 172.17.0.15, not by name).

    pooled vs PG 16.14   58 core
    pooled vs PG 18.4    62 failed / 622 passed

Set-differenced rather than compared by total:

- **57 of the 58 core members fail identically on both server versions.** The
  category-A classification is a property of transaction pooling, not of the
  server behind it.
- The one core member that PASSES on 18.4,
  `integration::released_open_transaction_is_not_inherited_by_the_next_borrower`,
  is the entry already reclassified above as a fixture collision rather than
  pooler semantics. It now passes on both.
- Four `differential_tokio::*` COPY cases appear on both and are the known
  serial-run contamination set: each passes in isolation.

**One failure exists ONLY against the newer server**, and it is a new shape of
category A:

    protocol_version_live::the_negotiated_version_matches_what_the_server_can_speak
    assertion failed: server_version_num=180004 speaks V3_2,
    but the session settled on V3_0     left: V3_0   right: V3_2

Direct against 5459 the same test passes. PgBouncer 1.25.2 is the protocol
endpoint and speaks 3.0; the backend speaks 3.2. The test's oracle reads
`server_version_num` from the SERVER and compares it against a version the
driver negotiated with the POOLER. That is the same mistake as the synthetic-PID
cases in the table above, in a new place.

**It is invisible on PG 16**, where pooler and server both speak 3.0. So a
pooled residue measured against one server version cannot be assumed complete:
any entry whose oracle reads a server capability the pooler does not forward
will only appear once the backend outgrows the pooler. Re-measure against the
newest server available, not just the default fixture.
