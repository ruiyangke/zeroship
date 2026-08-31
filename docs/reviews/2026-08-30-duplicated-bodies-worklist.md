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

DONE: group 1 (query RowDescription) and group 5 (column metadata), bound by
`25712160b`, plus the copy_in/copy_out reprepare pair.
The rest are open. Line ranges are as of 2026-08-30 and drift; locate by content.

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
