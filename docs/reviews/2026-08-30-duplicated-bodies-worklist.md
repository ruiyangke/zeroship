# Duplicated logic bodies in compio-postgres, and which copies are bound

Measured 2026-08-30 at `b3dc8a5e6` by a mechanical sweep: **621 maximal
duplicate candidates, filtered to 50 behavioural groups.** Filtering dropped
matches that carry no behaviour - closing braces, `use` lines, derive lists, and
single-call delegations with no branch.

## Why this list exists

When a body is written out N times, a test typically binds ONE copy and the
other N-1 are unprotected while looking covered. Four instances of this were
found and fixed here in one session:

    buf_stream.rs   26 character-identical lines, whole-stream vs split read
                    -> a mutation sweep found 4 unbound survivors behind it
    generic_client  `self.query_opt(...)` in the Client impl and the Transaction impl
    config.rs       the backslash-escape block in the quoted and unquoted lexers
    bind/prepare/query  `table_oid: Some(...).filter(|n| *n != 0)` in FOUR places

## The acceptance test for "this group is bound"

Not "a test exercises this logic". The check is: **mutate ONE copy and exactly
ONE test fails.** Demonstrated for the `table_oid` group - mutating `bind.rs:160`
alone produced

    bind_unnamed_portal_row_description_preserves_table_oid ... FAILED
    query_text_params_row_description_preserves_table_oid   ... ok
    query_typed_row_description_preserves_table_oid         ... ok

If mutating one copy fails several tests, the tests are not isolating the copies.

## Do NOT deduplicate these

Collapsing a group into one function is a refactor, is out of scope for coverage
work, and destroys the structure being measured. Several of these duplications
are deliberate - `pool.rs:1531` documents why release runs ROLLBACK rather than
`DISCARD ALL`, and the neighbouring arms differ in exactly that kind of detail.
Read each group before assuming the copies are interchangeable.

## Status

Line ranges are as of 2026-08-30 and drift; locate by content. Every verdict
below was re-run by the pilot against the FULL seven-target gate
(`--features tls,live-tls-tests,live-unix-socket,with-chrono-0_4,with-time-0_3
--no-fail-fast`), not taken from an agent's report.

| Group | Copies | Verdict | Binding test(s) |
| ---: | ---: | --- | --- |
| 1 Query RowDescription | 2 | BOUND (`25712160b`) | pre-existing |
| 5 Column metadata | 4 | BOUND (`25712160b`) | pre-existing |
| 6 Cached statement replay family | 6 | BOUND, each independently | agent table, one distinct failure per copy |
| 8 Flush/read terminal polling | 3 | 1 BOUND; **2 EXECUTED BY NOTHING** - see the open item below | `flush_retirement_terminal_arm_preserves_the_read_error` (the bound one) |
| 9 Optional-row cardinality | 2 | BOUND | typed and untyped `query_opt` early-return tests |
| 10 Buffered ErrorResponse scanner | 2 | SPLIT: replication live and covered by 4; **connection copy is DEAD** | see below |
| 14 Close plus Sync | 2 | BOUND | `dropping_armed_portal_cleanup_enqueues_close` |
| 17 Cancel confirmation | **3**, not 2 | 2 pre-bound, 1 was UNBOUND | `raw_cancel_success_keeps_pool_lease_reusable` (new) |
| 15 Scalar row arity | 3 | BOUND | `query_scalar` / `query_one_scalar` / `query_opt_scalar` arity tests |
| 18 Statement-cache LRU updates | **4**, not 3 | BOUND, including the uncounted candidate-LRU copy | LRU eviction tests |
| 19 Config value lexer | 2 | 1 pre-bound, 1 was UNBOUND | `unquoted_conninfo_backslash_escapes_the_next_character` |
| 21 Serialized terminal handling | 2 (+2 siblings elsewhere) | SPLIT: step B covered by 24; **step C is DEAD** | see below |
| 22/27 Housekeeping close (EOF clean-close family) | 4 across the file | 2 covered by 24 each; step C DEAD; **step D was UNBOUND** | `serialized_eof_with_only_housekeeping_in_flight_closes_cleanly` (new) |
| 23 COPY refusal drain | 2 | BOUND (visible only in the full suite) | agent-reported |
| 25 Bind cache invalidation | 2 | 1 pre-bound, 1 was UNBOUND | `unnamed_bind_parse_error_invalidates_cached_statement` (new) |
| 26 Plaintext TLS shortcut | 2 | covered, but by a BLUNT probe - see note | 42 and 216 failures respectively |
| 28 Terminal classification | **5**, not 2 | flush-path copy was UNBOUND | `eof_during_write_classifies_the_captured_terminal` (new) |
| 29 COPY encoding selection | 2 | BOUND | `probationary_copy_in_reparses_immediately_before_bind` |
| 30 COPY IN pre-Bind abort | 2 | BOUND, each independently | `unexpected_{parse,bind}_slot_message_suppresses_copy_terminal` |
| 34 Row-range decoding | 2 | NOT independently bound (3 overlap on one copy) | see agent table |
| 35 Simple-query column scanning | 2 | BOUND; one sub-branch **unbindable** | `copy_in_classifier_scans_past_doubled_quoted_identifier_delimiters` |
| 36 Binary COPY rejection | 2 | BOUND | `bytes_after_the_binary_copy_trailer_are_refused` + sibling |
| 37 Frame-offset guard | 2 | was UNBOUND, now BOUND each | `{first_matching_tag,error_response_before}_advances_past_the_entire_leading_frame` |
| 38 Deferred codec error | 4 (+1 uncounted guard arm) | covered, NOT independently bound - and correctly so, see below | one specific test per copy + a deliberate aggregate |
| 39 TLS release attachment | 2 | connect_raw covered by 4; **replication copy was UNBOUND** | `dropping_a_tls_replication_connection_sends_close_notify` (new) |
| 42 Weak pool callbacks | 6 | BOUND | agent table |
| 43 Weak pool metrics | 2 | BOTH were UNBOUND | `housekeeping_after_connect_{failure,ineligibility}_records_an_eviction` |
| 45 Streaming COPY refusal | 2 | BOUND each | agent table |
| 46 Simple-query COPY collection | 2 | 1 pre-bound, 1 was UNBOUND | `unsupported_copy_out_is_refused_by_name_at_every_public_entry_point` |
| 47 TLS panic poisoning | 2 | `with` pre-bound; **`try_with` was UNBOUND** | `a_panicking_callback_under_try_with_poisons_the_shared_session` (new) |
| 48 Transaction drop | 2 | BOUND | agent table |
| 49 Row panic mapping | 2 | BOUND | `a_get_panic_distinguishes_a_null_from_a_type_mismatch` + sibling |
| 50 COPY format validation | 2 | NOT independently bound (2 overlap on one copy) | agent table |

**The worklist's copy COUNTS are unreliable, and that is the most reusable
finding here.** Group 17 said two and had three; group 28 said two and had five;
group 18 said three and had four; `remember_server_error` said two and had
eight. In group 17 and group 28 the
UNCOUNTED copy was the only unbound one. Always enumerate by content first.

### "More than one failure" is not automatically a gap

Group 38 is the worked example. Mutating each of the four
`if saw_error_response {` copies in `codec.rs` fails 3, 3, 3 and 2 tests -
never exactly one, so the acceptance rule flags all four. They are fine.
Each copy has its OWN named test:

    421 message ceiling  -> a_message_ceiling_failure_after_error_response_is_deferred
    434 startup limit    -> a_startup_limit_failure_after_error_response_is_deferred
    451 copy metadata    -> a_copy_metadata_failure_after_error_response_is_deferred
    465 ready for query  -> a_ready_for_query_failure_after_error_response_is_deferred

The extra failures are `a_leading_error_response_survives_every_later_validation_failure`,
a DELIBERATE cross-cutting test cited in a source comment beside these arms,
plus each validator's own non-deferred test. That is stronger coverage than
one-test-per-copy, not weaker.

So read a multi-failure result by NAME, never by count: if one failing test is
specific to the mutated copy, the copy is bound and the rest are aggregates
doing their job. Only when no failing test is specific to the copy - or when
zero fail - is there anything to fix.

The worklist also under-counted here: the `Err(error) if saw_error_response =>`
guard arm on `Header::parse` is a fifth copy of the same decision in a different
syntactic shape, and the group entry lists four.

### A mutation that breaks 216 tests has not isolated anything

Group 26 is the counter-example to reading big failure counts as strong
coverage. Inverting `if encryption == Encryption::Plaintext { return Ok(()) }`
in `connect_raw.rs` fails **216** tests, because every plaintext connection then
runs TLS validation and the driver stops working at all. That says the line is
load-bearing; it says almost nothing about whether the specific decision - skip
validation when the transport is plaintext - is checked by anything.

The `cancel_query.rs` twin fails 42, including the four cancel TLS-policy tests
that ARE specific to it, so that copy is genuinely covered.

Treat a three-figure failure count as "the mutation was too coarse to be
informative", not as "well covered". The useful probe changes one decision, not
one precondition the whole driver rests on.

### OPEN: two mid-flush terminal recorders that no test executes

Unlike the unbindable copies below, these are NOT provably unreachable - they
are simply untested, and each names a real scenario. Measured 2026-08-31:
replacing either with `panic!` leaves all 1449 tests green, so nothing runs
them.

- **`flush_with_read_draining`'s ReadTimeout poll** (`read_terminal = Some(terminal)`
  under the comment "ReadTimeout is out-of-band and outranks every FIFO gate").
  Needs a read timeout to fire WHILE a flush is parked. The existing
  read-timeout tests never have a flush in flight, and the existing
  backpressured-flush tests never arm a timeout.
- **The nested drain loop's `ReadEvent::Terminal` arm** (`read_terminal =
  Some(Some(terminal))` followed by `break`). The backpressured-flush tests
  reach the OUTER poll's terminal arm instead, which is the copy that IS bound.
  This one needs a terminal read to arrive during the nested read-draining pass.

Both discard the diagnosis silently when broken: swapping either for
`Some(None)` turns a specific error into "channel closed", which downstream
becomes a causeless `Error::closed()`. That is the same class of defect the
terminal-error plumbing exists to prevent.

### Two copies are unbindable by construction, and no test should be written

- **`connection.rs`'s `take_buffered_server_error`** cannot return `Some`. Both
  callers run `drain_buffered_backend_frames` first, which returns only when the
  head frame is incomplete - which this scanner then refuses. Panicking on its
  `Some` arm left all 1445 tests green; the same mutation on its live
  replication twin fails exactly four.
- **Step C's clean-close arm in the serialized loop.** Step B already returns
  `Ok(())` for `terminating && !has_awaited_response`, so reaching step C means
  an awaited response EXISTS; `publish_terminal_error` then returns early on an
  EOF instead of draining, so on the `is_eof` branch the deque is provably
  unchanged and the second conjunct stays false. A non-EOF error may drain, but
  then the first conjunct is false. `panic!` there leaves all 1447 tests green.
  **Its step D twin is spelled identically and IS reachable** - that one was a
  real gap and now has a test. Same source text, opposite verdicts, which is why
  these have to be judged per copy and never per group.
- **The doubled-quote arm of `may_enter_copy_in`'s identifier scanner.**
  Deleting it exposes no byte: the first quote closes the identifier and the
  second reopens it, so the same span stays quoted. Checked over every string of
  length <= 12 in `{quote, x, semicolon}` - 797,161 inputs, zero disagreements.

Both carry source comments saying so. A mutation report calling either unbound
is CORRECT; inventing a test to satisfy it would bind a path the system cannot
take.

## The 50 groups

1. Query RowDescription parser  -  `query.rs:201-237`, `query.rs:348-384`
2. Text/typed Bind encoding and errors  -  `query.rs:173-201`, `query.rs:265-292`
3. Typed query/execute frontend batch  -  `query.rs:329-348`, `query.rs:394-413`
4. Execute response/COPY refusal  -  `query.rs:292-316`, `query.rs:413-441`, `query.rs:547-552`
5. Column metadata construction  -  `bind.rs:153-166`, `prepare.rs:294-308`, `query.rs:215-235`, `query.rs:362-382`
6. Cached statement replay family  -  `client.rs:2664-2728`, `2723-2757`, `2900-2954`, `2958-2996`, `3010-3039`, `3051-3080`
7. Cancel-query TLS setup  -  `cancel_query.rs:114-160`, `187-224`
8. Flush/read terminal polling  -  `connection.rs:2210-2224`, `2348-2352`, `2408-2423`
9. Optional-row cardinality  -  `client.rs:2614-2627`, `2838-2851`
10. Buffered ErrorResponse scanner  -  `connection.rs:1160-1174`, `replication.rs:1633-1647`
11. Debug/non-debug query encoding  -  `query.rs:79-89`, `511-521`
12. TLS prewrite poison/close  -  `tls_sansio.rs:465-477`, `503-510`, `516-528`
13. Startup/auth handshake  -  `connect_raw.rs:594-605`, `1002-1013`
14. Close plus Sync  -  `portal.rs:59-68`, `statement.rs:32-41`
15. Scalar row arity  -  `client.rs:2522-2539`, `2567-2589`, `2637-2648`
16. Terminal server-error recording  -  `connection.rs:1218-1229`, `1248-1256`
17. Cancel confirmation  -  `cancel_token.rs:292-299`, `367-374`
18. Statement-cache LRU updates  -  `client.rs:1760-1767`, `1825-1828`, `1868-1875`
19. Config value lexer  -  `config.rs:2849-2855`, `2875-2881`
20. Authentication exchange  -  `connect_raw.rs:1337-1345`, `1355-1363`
21. Serialized terminal handling  -  `connection.rs:688-695`, `713-720`
22. Pending-response delivery  -  `connection.rs:2252-2258`, `3001-3009`
23. COPY refusal drain  -  `copy_in.rs:593-598`, `copy_out.rs:118-123`
24. Replication poison/release  -  `replication.rs:511-520`, `528-532`, `1126-1133`, `1270-1274`, `1331-1335`, `1499-1505`
25. Bind cache invalidation  -  `bind.rs:109-113`, `121-125`
26. Plaintext TLS shortcut  -  `cancel_query.rs:40-45`, `connect_raw.rs:679-684`
27. Housekeeping close  -  `connection.rs:726-732`, `748-754`
28. Terminal classification  -  `connection.rs:3097-3101`, `3172-3176`
29. COPY encoding selection  -  `copy_in.rs:566-570`, `copy_out.rs:44-48`
30. COPY IN pre-Bind abort  -  `copy_in.rs:606-612`, `621-627`
31. Pool acquisition arms  -  `pool.rs:1335-1359`, `1386-1425`
32. Pool return/handoff  -  `pool.rs:1473-1493`, `1504-1516`
33. Replication async/error handling  -  `replication.rs:602-612`, `1421-1425`
34. Row-range decoding  -  `row.rs:155-159`, `358-362`
35. Simple-query column scanning  -  `simple_query.rs:359-364`, `375-380`
36. Binary COPY rejection  -  `binary_copy.rs:336-339`, `380-383`
37. Frame-offset guard  -  `codec.rs:174-177`, `211-214`
38. Deferred codec error  -  `codec.rs:418-422`, `431-435`, `448-452`, `462-466`
39. TLS release attachment  -  `connect_raw.rs:583-587`, `replication.rs:285-289`
40. Request COPY flags  -  `connection.rs:974-977`, `3026-3029`
41. COPY state reset  -  `connection.rs:2781-2784`, `3183-3186`
42. Weak pool callbacks  -  `pool.rs:1720-1724`, `1817-1821`, `1844-1848`, `1855-1859`, `1894-1898`, `1921-1925`
43. Weak pool metrics  -  `pool.rs:1863-1868`, `1880-1885`
44. Pool permit guards  -  `pool.rs:2076-2079`, `2106-2109`
45. Streaming COPY refusal  -  `query.rs:847-852`, `simple_query.rs:534-542`
46. Simple-query COPY collection  -  `simple_query.rs:223-239`, `269-276`
47. TLS panic poisoning  -  `tls_sansio.rs:559-562`, `576-579`
48. Transaction drop  -  `transaction.rs:88-103`, `transaction_builder.rs:117-125`
49. Row panic mapping  -  `row.rs:219-224`, `415-420`
50. COPY format validation  -  `copy_format.rs:56-59`, `112-115`

## Group B (replication `release.shutdown()`): 5 of 6 bound, and the 6th is not exotic

Measured by mutating each copy to `/* mutated */` individually on a quiescent
tree and counting which named tests fail. State at `a1ff36efe`:

    line  542  bound   identify_system_write_failure_shuts_down_its_release_handle
    line  588  bound   identify_system_response_read_failure_shuts_down_its_release_handle
    line 1154  bound   next_read_timeout_shuts_down_its_release_handle
    line 1297  bound   automatic_keepalive_write_failure_shuts_down_its_release_handle
    line 1322  bound   copy_done_with_a_body_shuts_down_its_release_handle
    line  528  UNBOUND

Every bound copy fails exactly ONE test, which is the acceptance bar.

**Why 528 survived the others.** It sits in `identify_system`, in a branch
guarded by `result.as_ref().is_err_and(Error::is_read_timeout)` - so a plain
read FAILURE does not reach it, and neither does a write failure. It needs a
genuine `Kind::ReadTimeout`, which is why the two identify-system tests next to
it both miss.

**It is bindable; the technique already exists in the tree.**
`tests/serialized_loop.rs:400-451` drives a real read timeout and asserts
`error.is_read_timeout()`. A test that gives `identify_system` a transport which
accepts the query and then never answers, under a short command timeout, reaches
line 528. That is the last copy in this group.

**Do not "simplify" 528 and 542 into one arm.** They are textually similar and
behaviourally different: 542 is a send failure that may have left PostgreSQL
holding a frontend fragment, 528 is a timeout that leaves the read boundary
unknown. Both poison and shut down, for unrelated reasons, and the comments
above each say so.

## Line 528 is UNBINDABLE by peer observation, and here is what masks it

2026-08-31. An agent wrote `identify_system_read_timeout_shuts_down_its_release_handle`
for the last unbound copy. It compiles, it passes, it is named correctly - and
it binds NOTHING. Mutating each of the six `release.shutdown()` call sites to
`/* mutated */` in turn leaves it green every time:

    line 528 -> 0 of 1 failed      line 1154 -> 0
    line 542 -> 0                  line 1297 -> 0
    line 588 -> 0                  line 1322 -> 0

**The mask is `Drop`.** `release.rs:362`:

    impl Drop for ConnectionDropRelease {
        fn drop(&mut self) {
            // Drop cannot report an error, and a concurrent client release or
            // peer close can legitimately win this shutdown race.
            self.shutdown();
        }
    }

The handle shuts down unconditionally when it goes out of scope, and
`ConnectionDropRelease::shutdown`'s own doc adds that a socket-read timeout may
not promptly release the descriptor. So "the peer saw a shutdown" is reachable
on the timeout path without the explicit call ever running. A test asserting on
the peer cannot separate the two.

**Why the other five copies ARE bindable.** Their tests observe the shutdown at
a point where the explicit call is the only thing that could have produced it
yet - the write-failure and read-failure arms shut down and then keep the
connection alive, so the peer observation happens strictly before Drop. The
timeout arm does not offer that window.

**So the correct outcome for 528 is option (b): a comment, not a test.** Name
the mask at the call site. Do not merge a test whose name claims a binding it
does not have - that is worse than no test, because the next person reading the
group will believe all six are covered.

**Group B final: 5 of 6 bound, 1 unbindable-by-construction with the mask
named.** That is a complete result, not a partial one.

## CORRECTION: group A (client.rs replay family) was ALREADY fully bound

This document said only the probationary-reprepare pair was bound, and a job was
dispatched on that premise. **The premise was wrong.** Measured 2026-08-31 by
mutating each of the six copies separately and running the FULL suite each time:
every copy is killed by an existing test. No work was needed and none was done.

Independently re-verified on main. Mutating the replay guard at `client.rs:3010`
(`if !before_bind_complete` -> `if true`, which refuses the replay at that copy
only and still compiles):

    test result: FAILED. 714 passed; 1 failed
    integration::execute_raw_probationary_cache_winner_reprepares_after_deallocate

One copy, one failure. The group meets the acceptance bar already.

### Two ways my own spot-check went wrong first, both instructive

**A mutation that does not COMPILE proves nothing.** My first attempt renamed
the call to `reprepare_cached_statement_once_DISABLED`. That is a build error,
not a behaviour change - no test ran, so no verdict was possible. A mutation
must compile and change behaviour.

**A filtered run cannot refute a full one.** My second attempt compiled, and a
run filtered to `statement_cache stale_cached` reported 38 passed, 0 failed -
which looked like it contradicted the agent. It did not: the binding test is
`execute_raw_probationary_cache_winner_reprepares_after_deallocate`, which
matches neither filter word. The full suite found it immediately. When checking
whether a mutation is caught, filter by nothing.

## `remember_server_error` is EIGHT copies, not two, and five are unbound

The entry above recorded "terminal server-error recording - connection.rs
1218-1229, 1248-1256" as a two-copy group. Re-derived 2026-08-31: there are
EIGHT call sites. Each was mutated alone (the call replaced by a tuple binding
that compiles and skips it), compile-checked, then measured with a FULL suite
run:

    line  979  SURVIVED
    line  987  SURVIVED
    line 1257  SURVIVED
    line 1263  SURVIVED
    line 1284  SURVIVED
    line 1290  KILLED  client_encoding::encoding_retirement_preserves_a_pipelined_server_error
    line 1447  KILLED  type_cache_residue::query_text_params_preserves_the_outer_error_when_type_resolution_fails
                       type_cache_residue::query_typed_preserves_the_outer_error_when_type_resolution_fails
    line 1605  KILLED  backend_termination::a_checked_out_idle_pool_backend_is_discarded_after_termination
                       pool_hooks::{current,final,fresh_after_connect}_warmup_eligibility_preserves_idle_fatal

Three bound, five not.

### Why the five survive: the tests INJECT the stored error

The read side is well covered - `client.rs:779` takes the request-local error to
produce a better diagnosis, `connection.rs:561` gates on the terminal one. But
the tests that exercise those readers set the slot directly rather than driving
the code that fills it:

    #[test]
    fn enqueue_failure_preserves_terminal_server_diagnosis() {
        let client = client(sender);
        *client.inner.terminal_server_error.lock() = Some(admin_shutdown());

So a regression that silently stopped RECORDING the server's FATAL would leave
every injection-based test green. This is not "error handling is untested" - it
is "the write side of a well-tested read side has no coverage", which is a
narrower and more actionable statement.

### Note the shape of the three that ARE bound

They are the paths where the stored error changes an observable outcome:
encoding retirement, type-resolution failure, and pool warmup eligibility.
Line 1605 fails FOUR tests, which is weaker isolation than the one-copy-one-
failure bar but still a genuine binding.

**Do not treat the eight as interchangeable.** 1257 and 1263 are a pair writing
to DIFFERENT slots (terminal vs request-local) in the same branch, as are 1284
and 1290 - and within that second pair the request-local write is bound while
the terminal one is not. Whatever fills these gaps has to distinguish the slot,
not just the call site.
