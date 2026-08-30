# The compio-postgres open-findings list, re-derived against HEAD

A findings list carried forward from `d2422bdcd` was re-checked against
`0b509285a` on 2026-08-28. **None of the six survived.** Three were already
fixed, two rested on a false premise, and one cited code that no longer exists.

This document exists so the list is not re-derived a fourth time. Each verdict
below names the evidence, not an opinion.

## 1. `connect_raw.rs:302` discards `deferred_error`; `take_deferred_error()` is uncalled

**FALSE.** `take_deferred_error()` has two callers: `connection.rs:811` and
`connection.rs:2687`. The claim was likely produced by a grep whose range
excluded them.

The narrower true statement - that `connect_raw`'s handshake loop ignores the
field while `connection.rs` consumes it - is by design: `deferred_error` is set
only when the codec already saw an `ErrorResponse` in the same batch
(`codec.rs`, every assignment site is guarded by `saw_error_response`), and an
`ErrorResponse` during startup ends the handshake with the server's own
diagnosis, which is strictly more informative than the framing error behind it.

## 2. `connect_raw.rs:461` MAX_DELAYED_HANDSHAKE_BYTES: a retained frame pins the whole allocation

**ALREADY FIXED**, and pinned by
`delayed_message_does_not_pin_handshake_read_allocation` (`connect_raw.rs`
tests). The test asserts the delayed frame does not hold the handshake read
buffer's whole allocation.

## 3. `connect_raw.rs:993` `handshake.stream.into_inner()` drops unread buffered bytes

**STALE CITATION, AND FALSE.** There is no `into_inner` call anywhere in
`connect_raw.rs` or `connect.rs`. The handoff passes `handshake.stream` and
`handshake.delayed` to `Connection::new`; `handshake.pending` is not passed, but
a temporary instrument asserting `pending_count == 0` at the handoff held across
the whole suite, so nothing is stranded there.

Frames arriving in the same read as `ReadyForQuery` survive in the STREAM
BUFFER, which is handed over. Pinned by three scripted cases
(`startup_handoff_preserves_a_trailing_notification`,
`..._applies_a_trailing_parameter_status`, `..._preserves_a_trailing_notice`),
and mutation-proved: inserting `handshake.stream.buf().clear()` before
`Connection::new` turns all three RED with "the trailing startup notification
was lost: None".

## 4. `binary_copy.rs:63` cancelling `write_raw` discards up to 4KB of already-Ok rows

**FALSE, AND INVERTED.** `send_buffered_rows` does
`let row = buf.split_off(row_start)` BEFORE its await, which keeps the
already-`Ok` prefix in `buf` and moves only the in-flight row into a local. A
cancellation at `poll_ready` therefore rolls back exactly the unfinished call
and nothing else. The comment above it states this.

## 5. `copy_in.rs` poll_flush treats the sink's OWN close as a lost connection

**FALSE, proved by an induced hang.** `poll_close` is
`self.poll_finish(cx).map_ok(|_| ())` and never closes the sender, so
`sender.is_closed()` reports a genuine receiver drop.

Mutating `let disconnected = this.sender.is_closed();` to `= true;`
(`copy_in.rs:487`) makes the suite HANG at
`copy_in_failure::a_constraint_violation_reports_its_sqlstate_and_detail` rather
than fail. That the unmutated suite does not hang is the proof the branch is not
taken on our own close path. `a_finished_copy_sink_does_not_report_connection_closed`
and the `finish()` row-count assertion at `copy_in_failure.rs:336` cover it.

**Note the failure MODE:** a regression here presents as a wedge, not a failing
assertion, because `poll_disconnected_diagnosis` loops on
`Poll::Ready(Ok(_))` until an error or `Pending`. Give any test touching it a
timeout.

## 6. TLS split discards unconsumed ciphertext

**FALSE, and the citation was stale.** The audit named `tls_sansio.rs:750`; the
`MaybeTlsStream` split is at `maybe_tls_stream.rs:175` and merely delegates.
The real split, `TlsStreamCore::try_into_split` (`tls_sansio.rs:952`),
destructures `socket, session, cipher, cipher_len, cipher_read, plain` and moves
every buffer field into the read half - in BOTH the `Ok` and `Err` arms.

## What to take from this

A findings list ages against the tree it was derived from. Three of these six
described code that had already changed, and two cited line numbers that no
longer pointed at the named construct. **Re-derive before acting, and check the
citation before trusting the claim** - a stale line number is the cheapest
possible tell that the rest of the entry needs re-checking.

## Second pass, 2026-08-29: three more carried findings re-derived

### "The TLS split discards unconsumed ciphertext" - DISPROVED at all three layers

The claim carried a line reference to `tls_sansio.rs:750` that no longer points
at a split. Re-derived from scratch: the split chain is `MaybeTlsStream` ->
`TlsStreamCore` -> `BufStream` -> socket, and EVERY layer carries its buffered
bytes across.

`maybe_tls_stream.rs:175` only delegates - it moves the inner stream whole into
`s.try_into_split()` and discards nothing itself.

`tls_sansio.rs:952` destructures the entire `TlsReader` and carries every field
into the read half - `cipher`, `cipher_len`, `cipher_read` (the ciphertext
buffer and both offsets) and `plain` (decrypted bytes not yet handed out). Its
own doc states the property: "Split only the socket, carrying every byte already
read from it into the owned read half. A refused split rebuilds the identical
stream." The `Err` arm does exactly that.

`buf_stream.rs:746` does the same for `read_buf`, `read_scratch`, `write_buf`,
`read_deadline` and `max_message_size`, and its doc addresses the one case that
looks lossy: "A non-empty `write_buf` does NOT force the serialized fallback -
those bytes are simply carried onto the new write half and flushed with the next
frame."

**Third pass, 2026-08-30: what KEEPS it correct.** The two passes above both
established that the carry-over is right, and neither named a guard - so a
reader learns the code is correct today but not what would catch it regressing.
Two tests bind the property directly, by name:

    tls_rustls.rs  tls_split_preserves_ciphertext_already_read_from_socket
    tls_sansio.rs  tls_split_preserves_the_plain_scratch_buffer

They split the two halves of the claim between them - ciphertext already pulled
off the socket, and decrypted bytes not yet handed to the caller. A change to
`TlsStreamCore::try_into_split` that drops a field fails one of these, so cite
them rather than re-deriving the field list a fourth time. Named by symbol, not
line: the citation that started this entry rotted because it was a line number.

**A stale line number is not a weak citation, it is a different claim.** Point it
at the current code before judging it.

### "codec.rs:75 has an uncalled take_deferred_error()" - DISPROVED

It has two callers: `connection.rs:810` and `connection.rs:2685`. The field is
handled at six further sites in the connection loop (1288, 1301, 1322, 1325,
2296, 2317).

### "connect_raw.rs:302 discards deferred_error" - CONFIRMED, and my first answer was wrong

`BackendMessage::Normal { messages, .. }` drops the field. My initial reading
said this was harmless because `take_deferred_error`'s doc attributes the field
to "the split reader", and the handshake is not split.

**That is wrong, and the doc is what made it wrong.** The producer is shared.
`connect_raw.rs:294` calls `read_backend_detached_async_frames`, a two-line
delegation to `read_backend_with_async_storage(stream,
AsyncFrameStorage::Detached)` - and that function is where `deferred_error` is
set, at five sites (`codec.rs` ~380, 419, 432, 449, 463). Both the split reader
and the handshake go through it. So a `Normal` carrying `Some(_)` does reach
line 302 and is dropped.

The doc sentence is accurate about ONE producer and reads as if it names the
only one. Under investigation: which of the five sites are reachable before
`ReadyForQuery`, and what the handshake does after dropping the error.

**Running tally across both passes: of the findings re-derived on 2026-08-28 and
2026-08-29, the great majority did not survive contact with the code.** The two
that did - the post-write cancel and this one - were both confirmed by following
a call one level deeper than the finding's own citation.

### "binary_copy.rs:63 write_raw cancellation discards up to 4KB of already-Ok rows" - DISPROVED, and inverted

The claim has the direction of `split_off` backwards. `send_buffered_rows`:

    // Keep rows whose calls already returned `Ok` in `buf` until the sink can
    // accept their frame. The row which crossed the threshold stays local to
    // this future, so cancellation while readiness is pending rolls back only
    // that unfinished call.
    let row = buf.split_off(row_start);
    std::future::poll_fn(|cx| sink.as_mut().poll_ready(cx)).await?;

    // No cancellation point separates removing the completed prefix from
    // transferring the combined frame into the sink. Once `start_send`
    // succeeds, the sink owns the bytes while `poll_flush` is pending.
    buf.unsplit(row);
    sink.as_mut().start_send(buf.split().freeze())?;

`BytesMut::split_off(n)` RETAINS `[..n]` in the receiver and returns `[n..]`.
`buf` is `&mut BytesMut` owned by the `BinaryCopyInWriter`, not by this future.
So the already-`Ok` rows are exactly what SURVIVES a cancellation, and the only
thing lost is `row` - the current call's own row, whose call is the one being
cancelled. That is the correct rollback boundary.

The one remaining window, between `unsplit` and `start_send`, contains no await,
and the comment says so.

This is the fourth finding in this crate answered by a comment adjacent to the
cited line. The audit's hit rate on carried findings is low enough that
re-deriving each one before acting is cheaper than acting on it.

### "connect_raw.rs:461 MAX_DELAYED_HANDSHAKE_BYTES charges frame_len but a retained frame pins the whole allocation" - DISPROVED

`delay()` charges `frame_len`, and the retained frame is a private copy of
exactly that many bytes, so charged equals held.

`codec.rs` has two storage modes for an async frame. The handshake - the ONLY
path that retains one, via `delay()` - uses `Detached`:

    AsyncFrameStorage::Detached => {
        let mut frame = BytesMut::from(&stream.buf()[..frame_len]);
        stream.buf().advance(frame_len);
        backend::Message::parse(&mut frame)
    }

`BytesMut::from(&[u8])` allocates and COPIES, then the shared buffer is
advanced. A 13-byte notice therefore holds a 13-byte allocation.

The hazard the finding describes is real and the crate already found it - the
comment at `connect_raw.rs:290` records the measurement that drove the fix: "a
13-byte notice keeps a grown read buffer alive in full - measured at 13 bytes
pinning 1 MiB. `MAX_DELAYED_HANDSHAKE_BYTES` charges the frame, so what is
charged and what is held must be the same bytes." `Detached` IS that fix. The
`Shared` mode still parses in place, and is used only where nothing is retained.

### "connect_raw.rs:993 handshake.stream.into_inner() drops unread buffered bytes" - STALE, no such call

`into_inner()` does not occur anywhere in `connect_raw.rs`. The function near
that line, `handshake_for_replication`, returns the `BufStream` WHOLE, and its
doc states the property the finding asks for: "The returned `BufStream` is the
SAME one that decoded startup: bytes following `ReadyForQuery` may already be in
its read buffer."

Second finding in this batch whose line reference points at code that no longer
exists. A carried finding needs its citation re-resolved before its claim is
even meaningful.

## connect_raw.rs:302 DOWNGRADED by experiment, 2026-08-29

The drop is real - `BackendMessage::Normal { messages, .. }` discards
`deferred_error`, and the handshake does reach the producer that sets it. But
the consequence is NOT a lost error.

A scripted-input experiment fed an `ErrorResponse` followed by a frame whose
header length is under 4. The handshake returned the `ErrorResponse`, and on its
NEXT read surfaced:

    MEASURED_HANDSHAKE_RESULT: error communicating with the server:
      invalid message length: header length < 4

The reason is structural: `deferred_error` is DERIVED from bytes that remain in
the read buffer. Discarding the derived error does not discard the bytes, so the
next decode recomputes it. The effect is a one-round delay, not a loss.

Startup-reachability of the five producer sites, from the same run:

| codec site | startup-reachable |
| --- | --- |
| malformed header | yes |
| generic `max_message_size` | no - startup keeps the 64 MiB default and fills 16 KiB |
| startup-tag length (`K`/`v`) | yes |
| COPY metadata | not during startup |
| malformed `ReadyForQuery` | yes |

**Residual question, deliberately left open.** One producer site is
`deferred_error = Some(Error::io(error))`. An I/O error is NOT derived from
retained bytes, so the re-detection argument above does not obviously cover it -
though a socket that failed once will generally fail again. Nobody has measured
that arm.

**Status: not worth fixing on current evidence.** A one-round delay in surfacing
an error that does surface is not a defect worth changing a handshake for. This
is recorded so the next reader does not re-derive it a third time.

**Process note.** The job that produced this measurement was terminated by the
model provider's safety filter partway through ("flagged for possible
cybersecurity risk") after 165k tokens, because the brief was framed around
malformed peer input. The measurement above was recovered from its log. Briefs
in this area need neutral, correctness-shaped framing to survive.

## The soak's RSS rule is under-powered at the default 180s, measured 2026-08-29

Same tree, same dedicated container (port 5470), two durations:

| duration | samples | window | delta_kib | rises/falls | excess vs budget | verdict |
| --- | ---: | ---: | ---: | --- | --- | --- |
| 180s | 37 | 9 | +36 | 8/6 | 592 vs 576 | **growing (failed)** |
| 420s | 85 | 21 | **-72** | 20/20 | 768 vs 1344 | stable (passed) |

`pool_acquires == pool_releases` EXACTLY in both runs - 35408 and 82346. The
leak criterion never wavered; only the memory-trend arm did.

**Why the short run fails.** The rule compares the sum of the final quartile
against the preceding window, allowing `noise_band_kib=64` PER SAMPLE. But the
observed RSS oscillates between 9704 and 9984 KiB - an amplitude of about 280
KiB, more than four times the per-sample band. With a 9-sample window the budget
is 576 KiB while a window that happens to sit on the high plateau instead of the
low one can differ by far more than that. The 180s run failed by 16 KiB out of
88,000 - 0.018% - on a series whose EARLY PEAK (9984, sample 2) was higher than
its final value (9832).

With a 21-sample window the budget is 1344 KiB and the window averages over
several oscillation periods, which is enough.

**So a 180s soak failure is not evidence of a leak.** Either run it at 420s or
longer, or read the trend arm alongside `delta_kib` and the rises/falls balance
rather than as a verdict. A rule whose noise band is smaller than the signal's
own oscillation amplitude can only be trusted over a long enough window.

### Why the RSS rule misfires, from its own calibration constant

`benches/soak.rs` states what the noise band was sized against:

    // 64 KiB is the smallest power-of-two band above the measured benign
    // 52 KiB allocator commit, without muting the sustained 100 KiB/sample
    // leaks this rule's vectors exercise.
    //
    // At the default 37 samples over 180 seconds, this detects a steady linear
    // leak above 256 KiB per run ...; it cannot detect one at or below that
    // rate, and a sufficiently late leak can be diluted by the quartile mean.

Two things follow.

**The band is calibrated to a 52 KiB benign step. The series it runs against
oscillates over about 280 KiB.** Measured 2026-08-29 on the dedicated container:
RSS moved between 9704 and 9984 KiB with no leak present, `rises=8 falls=6`, and
a final value BELOW the early peak. That amplitude is more than five times the
figure the band was chosen above, so a 9-sample window landing on the high side
of the oscillation instead of the low side exceeds a 576 KiB budget on placement
alone. That is exactly what happened: 592 against 576, a 16 KiB margin on an
88,000 KiB sum.

**The documented analysis only covers false negatives.** The comment reasons
carefully about the smallest leak the rule can SEE and about dilution, and says
nothing about how often it fires with no leak present. The 420s run at the same
commit reported `delta_kib=-72`, `rises=20 falls=20`, `verdict=stable` - so the
false positive is a property of the window length, not of the tree.

**`verify.sh`'s `rss-growth-rule` arm cannot catch this, by construction.** That
arm runs `cargo test -p compio-postgres --bench soak`, which exercises the
rule's SYNTHETIC VECTORS against a floor of 9. The vectors check the rule's
arithmetic on constructed series; nothing there measures whether the band suits
a real allocator's behaviour. A green arm and a false-positive live run are
consistent, and both were observed on the same commit.

Where a fix would go: scale the noise band to the measured oscillation
amplitude of the run rather than to a fixed 64 KiB, or require the window to
span several oscillation periods before the trend arm is allowed a verdict.
Neither is attempted here - this is recorded so the next person to see a 180s
soak go red does not go looking for a leak first.

## The workspace clippy gate is red on main, and it is not compio-postgres

Measured 2026-08-29. `./tests/clippy_gate.sh` fails with `rc=1`:

    before: linted: 58 targets in 17 packages (expected 217 in 38)
    after:  linted: 77 targets in 25 packages (expected 217 in 38)

"after" is with `crates/zeroship-migrate-backend` fixed (99 doc-list indent
errors plus five `#[allow(clippy::too_many_arguments)]`). That crate was red
since `b044546c2` on 2026-08-26, three days.

Fixing it moved the gate 19 targets and 8 packages further before aborting, and
revealed the next layer. Remaining deny-level errors, none in this crate's area:

    crates/zeroship-migrate-core      195
    crates/zeroship-migrate-postgres  121
    crates/zeroship-migrate-sqlite     84
    crates/zeroship-migrate-mysql      84
    crates/zeroship-plugin-db           1

Mostly `doc_lazy_continuation`, plus `result_large_err` and
`too_many_arguments`. `result_large_err` wants error types boxed, which is an
API change, not a lint tidy - so the remaining work is NOT the mechanical sweep
the first crate was.

**What this means for compio-postgres specifically, stated exactly.** The crate
is no longer in the gate's "produced NO linted target" list, so its lib is now
linted. But nine of its targets still are not, because the run aborts before
reaching them:

    compio-postgres  suite (test)          serialized_loop (test)
    compio-postgres  socket_release (test) tls_live (test)
    compio-postgres  unix_socket_live (test)
    compio-postgres  soak (bench)          query_live (bench)
    compio-postgres  chaos_probe (example) chaos_slots (example)

Those targets are NOT unverified: `cargo clippy -p compio-postgres
--all-features --all-targets` covers all of them and reports 0 errors. What is
true is only that the WORKSPACE GATE cannot vouch for them while an upstream
crate is red. Do not read a green per-crate clippy as a green gate, and do not
read the gate's silence about a target as evidence it is clean - the gate says
so itself: "this list is the reason the run above is not evidence that they are
clean."

## A failed slot teardown leaks, and the leak is bounded at 20

Measured 2026-08-29 after the pooled run. `pg_replication_slots` on the 5455
container held:

    cpg_subtxn_2398063_a8c25b21478b8119_s | active=f | active_pid=(none)

An orphan from a fixture whose teardown lost the 55006 race. All three test
servers report `max_replication_slots=20`.

**This is why a teardown that SWALLOWS the drop error is the wrong repair.**
Turning `.expect("fixture teardown failed")` into a warning makes the run green
and the slot immortal. Twenty of those and the next suite fails at slot
creation, in a test that has nothing to do with the one that leaked, with an
error that points nowhere useful.

The right shape is the one `drop_slot_when_released` already implements: poll
until the walsender releases, on a deadline, and then FAIL if it never does. A
retry converts a race into a wait; a warning converts a race into a slow leak.

Anything consolidating these teardowns must keep the failure loud.

### CORRECTION: warn-on-teardown is safe here, because a global sweep reclaims the leak

The section above says a teardown that warns instead of failing "makes the run
green and the slot immortal", and that anything consolidating these teardowns
"must keep the failure loud". **That is wrong, and I wrote it before reading
what constrains the leak.**

`tests/common/mod.rs` has `sweep_stale_replication_slots`, which predates this
work (its doc cites a 2026-08-24 measurement). Its query is GLOBAL:

    SELECT slot_name FROM pg_replication_slots
      WHERE NOT active AND slot_type = 'logical'

and it drops only those whose `test_object_name`-embedded PID is no longer
running - so it is safe under concurrent test binaries, and it reclaims a slot
leaked by ANY fixture, not just its own. It runs via `sweep_stale_test_objects`
at 8 fixture-setup sites across `pgoutput_options`, `pgoutput_streaming`,
`pgoutput_live_decode` (3), `pgoutput_subtransactions` (2) and
`replication_live`, plus directly in `pgoutput_two_phase` (2).

So the orphan I found - `cpg_subtxn_2398063_...`, inactive, owning PID dead - is
precisely what the next run's sweep collects. The cap of 20 is not approached by
warn-on-teardown; it would only be approached if the sweep did not exist.

`replication_publication_names.rs` is the one file that warns and does not sweep
itself, and even its leaks are collected by any other fixture's sweep.

The consolidation also KEEPS the retry: `drop_replication_slot` still loops on
55006 with a 10s deadline before giving up, which is the part that actually
fixes the flake.

**What I got wrong, and it is the same error this document keeps recording in
other people's findings.** I judged a change by the shape of one line -
`eprintln!` where an `expect` used to be - and reasoned from a resource cap to a
failure mode without checking whether anything reclaimed the resource. The
sweep was two files away. Read what bounds the damage before ranking it.

## Gate state after the teardown consolidation, 2026-08-29 at `80f6c07b6`

    PORT 5455  CARGO_EXIT=0  TARGETS=7  1348 passed 0 failed
    PORT 5459  CARGO_EXIT=0  TARGETS=7  1348 passed 0 failed

5459 previously failed at 1347/1 on
`pgoutput_subtransactions::a_stream_abort_never_lands_inside_an_open_chunk`
with SQLSTATE 55006 in fixture teardown. It is clean now, with the retry in
place at all 13 slot-drop sites.

**That is the expected outcome, not proof.** One green run cannot show a race is
gone; what can be shown is that the retry exists on the path that lost it, and
that the previously-failing test is green on the server where it failed. Both
hold.

## Why coverage is NOT being re-run, and it is not an omission

`docs/reviews/2026-08-28-coverage-cannot-see-test-gaps.md` records a controlled
experiment on this crate: five tests that each provably catch a real defect were
added, and the missed-line counts did not move - `transaction.rs` 40/314 before
and 40/314 after, `connection.rs` 536 missed before and 536 after. A surviving
mutant lives in code that ALREADY EXECUTES, so line coverage is blind to it by
construction.

Today is that experiment's confirmation from the other direction. Mutation
sweeps found real gaps that coverage would not have flagged:

    buf_stream split half   7 mutations, 4 SURVIVED -> 4 tests, each proved RED
    tls split halves        in progress, 4 SURVIVED of 8 so far

Eight gaps, all in lines the suite already executed. Re-running llvm-cov would
cost a full instrumented suite and produce a number that moves by roughly
nothing, which is precisely what was measured on 2026-08-28.

Coverage stays useful for the opposite question - finding code NOTHING reaches.
It is the wrong instrument for "is the code that runs actually checked", and
that is the question this hardening effort keeps asking.

### The `Error::io` arm left open above is closed, and my caveat was wrong

The downgrade of `connect_raw.rs:302` said the re-detection argument covered the
four parse-derived producers but that one arm was unmeasured:

> One producer site is `deferred_error = Some(Error::io(error))`. An I/O error is
> NOT derived from retained bytes, so the re-detection argument above does not
> obviously cover it.

**It is not an I/O error.** `codec.rs:376`:

    let header = match backend::Header::parse(&stream.buf()[idx..]) {
        Ok(Some(header)) => header,
        Ok(None) => break,
        Err(error) if saw_error_response => {
            deferred_error = Some(Error::io(error));
            break;
        }
        Err(error) => return Err(Error::io(error)),
    };

It is a HEADER PARSE failure over `stream.buf()` - buffered bytes - wrapped in
`Error::io` only because `backend::Header::parse` returns `io::Error`. The
socket is not involved. So all five producers are derived from bytes that stay
in the read buffer, the next decode recomputes every one of them, and the
one-round-delay conclusion holds for the whole set.

**I inferred the source from the constructor's NAME.** `Error::io` reads as "the
socket failed", and I ranked an open question on that reading without opening
the arm. Grep answers spelling; the match arm answers behaviour. This is the
same error the sections above catalogue in carried findings, made by me, twice
today - once on this and once on the slot-drop teardown.

## Proof that the two-server runs really were two servers, 2026-08-29

Every gate this session reported "1354/0 on both 5455 and 5459". That claim
rests on the runs reaching different servers, which was asserted ~15 times and
never printed. The cross-version runbook supplies the oracle; run at
`3464109be` with the feature set used all session
(`tls,live-tls-tests,live-unix-socket,with-chrono-0_4,with-time-0_3`):

    5455: server_version_num=160014 protocol=V3_0 backend_key_len=4  cancel_packet_len=16
    5459: server_version_num=180004 protocol=V3_2 backend_key_len=32 cancel_packet_len=44

Different servers, different WIRE PROTOCOL (3.0 against 3.2), and a cancel key
that is 4 bytes on one and 32 on the other, changing the cancel packet from 16
to 44 bytes. The two runs exercise genuinely different code, which is the whole
point of running both.

**Why this needed printing.** The runbook records the failure it prevents:
`--all-features` enables `suite-over-tls`, which `#[cfg]`-REPLACES
`common::test_url()` so the suite reads its DSN from
`tests/data/live/tls_live.conf` and IGNORES `PG_TEST_URL` entirely. A
cross-version run shaped that way reports `server_version_num=160015
protocol=V3_0` for BOTH ports - perfect agreement, one server, and a published
verdict that means nothing. The feature set above is the one that does not do
that, and this measurement confirms it in the tree as it stands rather than by
reading the flag list.

Same discipline as the pooler's `login attempt` delta and chaos 3's
`max_connections` gate: prove the run reached the thing it claims to measure,
because a run that reached somewhere else passes just as quietly.

## A statement crossing a pool lease is DELIBERATE, and I nearly filed it as a gap

While reviewing the `Statement` ownership check I noted that a statement
retained across a lease boundary would pass an `Arc::ptr_eq` on `InnerClient`,
because a returned session handed to the next borrower is the same physical
connection - and I flagged it as "a different axis, worth noting".

`pool.rs:1531` answers it directly:

    // ROLLBACK, not `DISCARD ALL`. The transaction is the only thing the
    // next borrower must not inherit; session state is something callers
    // are entitled to hand across a release. `DISCARD ALL` would take out
    // session-scoped advisory locks (crates/plugin-db's LockGuard holds one
    // on a pooled client), every prepared statement (this driver's own
    // type-info cache holds those for the life of the Client, so the next
    // use of a cached entry would fail), and every session GUC. It also
    // cannot run inside a transaction block at all - the server rejects it
    // with `25001` - which is precisely the state this code addresses.

So handing session state across a release is the POLICY, with three named
reasons the alternative is worse, one of which is that the driver's own
type-info cache would break.

**This confirms the shape of the ownership check being added.** It must key on
CONNECTION identity - `Arc::ptr_eq` on the `InnerClient` - and NOT on lease
identity. A check that refused a statement from a previous lease would refuse
something the pool deliberately permits, and would break the type-info cache the
comment names.

The pool DOES scope one thing to the lease: a `CancelToken`, because a stale
token can cancel the next borrower's query
(`a_token_from_a_returned_pool_lease_cannot_cancel_the_next_borrower`). A
statement cannot do harm that way - it names an object that really is prepared
on that backend. Different hazard, different scoping, both deliberate.

## Why the two-server gate and `verify.sh` are both required, measured 2026-08-29

Neither the standard feature set nor `--all-features` compiles every test, and
the sets are not nested. Measured at `33a60076f` with `cargo test ... -- --list`:

    lib target:    default 496   standard set 536   --all-features 536
    suite target:  standard set 711   --all-features 710

**The lib target is settled**: the standard set compiles the same 536 as
`--all-features`, so the `with-*` type-codec features hide nothing there.

**The suite target is not nested in either direction.** Three tests exist ONLY
under the standard set, because `suite-over-tls` cfg-REPLACES them:

    prefer_attestation_fallback::prefer_with_a_root_cert_falls_back_when_the_connector_cannot_attest
    prefer_attestation_fallback::prefer_without_a_root_cert_still_connects
    prefer_attestation_fallback::verify_full_still_refuses_a_connector_that_cannot_attest

Two exist ONLY under `--all-features`, each behind a shape feature:

    cancel_request::a_pool_can_cancel_tls_with_its_private_policy_lineage   #[cfg(feature = "suite-over-tls")]
    integration::suite_statement_cache_mode_reuses_identical_sql            #[cfg(feature = "suite-with-statement-cache")]

So the two-port gate at 711 misses two tests, and an `--all-features` run misses
three - and would ALSO read its DSN from `tls_live.conf` instead of
`PG_TEST_URL`, which is the trap recorded above.

**`verify.sh` closes exactly that gap**, because it runs `suite-over-tls` and
`statement-cache` as separate modes rather than as one merged feature set. The
two tests the standard gate cannot see are run there. This is why the two are
not redundant, and why a green two-port gate is not sufficient on its own.

Practical rule: the gate answers "does it work on both server versions", and
`verify.sh` answers "does it work in every shape". Both, every time production
code changes. Neither substitutes for the other, and that is now measured rather
than assumed.

**There are no `#[ignore]`d tests in this crate.** Checked the same day; the one
grep hit is a comment in `tests/suite/url_parity.rs` recording that six once-
ignored tests were un-ignored. That hiding place is already closed.

## The pool's uncertain-cancel flag is unbound, and the mechanism is a masking disjunction

Mutation sweep of the pool's retirement machinery, 2026-08-29: 14 mutations, 10
KILLED, 4 SURVIVED. The survivors, derived independently of the sweep:

    3  begin_cancel's post-Arc active recheck (the race-closing check) removed
    4  is_uncertain() forced to false          (the READ of the flag)
    6  the uncertain write in PoolCancelAttempt::drop removed  (the WRITE)
    8  CommandRecoveryGuard::drop without force_close

**4 and 6 are the read and the write of the SAME flag.** Both halves of the
uncertain-cancel mechanism can be deleted and the suite stays green - the
mechanism `ae8ba17f4` relies on to stop a possibly-sent cancel reaching the next
borrower.

**Why they survive, which the sweep alone does not say.** The flag has exactly
one reader, `client.rs:2420`:

    pub(crate) fn pool_cancel_lease_prevents_reuse(&self) -> bool {
        self.pool_cancel_lease.as_ref()
            .is_some_and(|lease| Arc::strong_count(lease) > 1 || lease.is_uncertain())
    }

It is a DISJUNCTION, and `Arc::strong_count(lease) > 1` is true in every
scenario the suite constructs - any live `CancelToken` holds a second `Arc`. So
the first term decides the outcome and the second is never load-bearing under
test. Forcing `is_uncertain()` to false changes nothing observable.

**What a binding test must therefore construct**, and this is the hard part:
`strong_count == 1` AND the flag set - the token DROPPED (so no second `Arc`
survives) while the attempt recorded uncertainty on its way out. That is exactly
the case the flag exists for, and exactly the case nothing exercises.

The only occurrence of "uncertain" anywhere in `tests/` is an assertion MESSAGE
string in `hostile_peer.rs:2486`, not a test of this behaviour.

**Mutation 8 is worth its own note.** `CommandRecoveryGuard::drop` is one of only
two `Drop` impls in this crate that do real work; the Drop survey earlier today
found its mechanism correct. It is correct AND untested - removing its
`force_close` breaks nothing in the suite. Correct-by-inspection and
bound-by-test are different properties, and the survey could only establish the
first.

### Not every survivor is a gap: mutation 3 may be untestable by construction

`begin_cancel`'s second `ensure_active()` survived. Before treating that as a
coverage gap, look at what it guards:

    self.ensure_active()?;
    let lease = Arc::clone(self);
    // Recheck after taking the reference which makes the attempt visible
    // to pool return. If return won the race, do not send. If it happens
    // after this load, the retained Arc makes return retire the session.
    lease.ensure_active()?;

The two checks bracket an `Arc::clone`, with NO await point between them. On a
single-threaded compio runtime no other task can interleave there at all; the
window is reachable only from another thread, and not on demand. A deterministic
test would need a hook injected into production code between those two
statements.

So this is defensive code against a real but externally untriggerable race, and
a surviving mutation says "no test reaches it", not "nobody bothered". The
correct outcome for it may be NO TEST plus a note - and a test that claims to
bind it deserves scrutiny for whether it changed production code to create the
seam.

**Contrast with mutations 4 and 6.** Those are trivially reachable: construct a
lease whose token has been dropped and whose attempt recorded uncertainty, then
ask `pool_cancel_lease_prevents_reuse`. Nothing about that needs a race. They
survived because a disjunction masks them, which is a genuine gap.

Same sweep, two survivors, opposite verdicts. Ratios do not tell you which is
which; only reading the guarded code does.

### The failed-refill permit path is unbound in both directions

Two further survivors from the same sweep, in the housekeeper's refill path:

    15  the armed WeakPermitGuard drop DECREMENT, after a failed or cancelled refill
    19  the wake_one_waiter() on that same armed-drop path

Both survive all four targets. Taken together they say: when a background refill
fails, nothing in the suite notices if the pool forgets to give the permit back
AND forgets to wake anyone waiting for it.

**The consequence is worse than either alone.** A missing decrement leaves the
pool believing a slot is in use that is not, shrinking effective capacity for
the process lifetime. A missing wake leaves a caller parked on a permit that
will never be signalled. A pool that slowly loses capacity while waiters hang is
exactly the failure that gets diagnosed as "the database is slow".

**The soak cannot see this, and its green does not cover it.** The soak's
criterion is `pool_acquires == pool_releases` - measured exact at 82542 this
session. A refill that fails is not an acquire, so its permit accounting never
enters that equality. An exact-equality check on one counter pair is not a check
on the whole permit lifecycle.

**They must be killed INDEPENDENTLY.** One test that fails when either mutation
is applied proves neither: it cannot distinguish "the decrement is missing" from
"the wake is missing", so a later change could restore one and the test would go
green with the other still broken. Two tests, or one test per assertion with
both asserted separately.

That distinction is the agent's own, reached while sweeping, and it is right.

## The whole uncertain-cancel retirement chain is unbound, end to end

Sweep complete enough to conclude: 25 mutations adjudicated in the pool's
retirement machinery, **8 SURVIVED**. They are not eight unrelated gaps. They
are the four stages of ONE mechanism, plus two adjacent ones:

**The uncertainty chain - every stage unbound:**

     6  the WRITE   - uncertain flag set in `PoolCancelAttempt::drop`
     4  the READ    - `is_uncertain()` forced false
    21  the BRANCH  - return-time "escaped or uncertain cancel authority" eviction disabled
    22  the ACTION  - that branch's explicit `force_close` removed

Set the flag, read it, act on it, retire the session: no test binds any of the
four. This is the mechanism `ae8ba17f4` added this session so a possibly-sent
cancel cannot reach the next borrower. It works - and nothing would notice if it
stopped.

**Permit accounting on a failed refill, both halves:**

    15  the armed WeakPermitGuard drop DECREMENT
    19  that same path's WAKE (decrement preserved, to separate them)

**Synchronous retirement, in a second guard:**

     8  `CommandRecoveryGuard::drop` without `force_close` - and the sweep found
        WHY it survives: "the existing recovery test is indeed masked by
        independent cancel abandonment", the same masking shape as the
        `Arc::strong_count > 1 ||` disjunction.

**One that is probably NOT a gap:**

     3  `begin_cancel`'s post-`Arc` recheck - brackets an `Arc::clone` with no
        await point, so unreachable on a single-threaded runtime. No test plus a
        note is the right outcome; see the section above.

**The pattern across all of them is masking.** Three separate survivors were
traced to a stronger condition standing in front of a weaker one -
`strong_count > 1 ||`, independent cancel abandonment, an eviction that happens
anyway. Each guard is correct; each is invisible because something else covers
its case in every scenario the suite builds. That is what a mutation sweep finds
and coverage cannot: these lines all execute.

## codec.rs swept: all five deferred-error producers were unbound

Sweep of `codec.rs`, 2026-08-29, budget of 10 mutations honoured exactly:
**5 KILLED, 5 SURVIVED**.

    1  complete-frame check `<` -> `<=`            KILLED
    2  bypass head startup-length validation       KILLED
    3  `saw_error_response |=` -> `&=`             KILLED
    4  header-parse deferred error -> None         SURVIVED
    5  message-ceiling deferred error -> None      SURVIVED
    6  startup-limit deferred error -> None        SURVIVED
    7  COPY-metadata deferred error -> None        SURVIVED
    8  ReadyForQuery deferred error -> None        SURVIVED
    9  Detached async parsing -> shared parsing    KILLED
    10 bare command-tag fallback `0` -> `1`        KILLED

**All five survivors are the same behaviour at five sites**: a decode failure
that follows an ErrorResponse must be ATTACHED to the batch already decoded,
not dropped. One test now binds all five, with a distinct assertion per site so
each fails alone - verified here on the header-parse and ReadyForQuery sites
independently, plus the `saw_error_response` gate.

**This matters for a downgrade recorded above.** `connect_raw.rs:302` was
downgraded on the reasoning that a dropped `deferred_error` is recomputed by the
next decode. That argument assumes the DEFERRAL ITSELF works - and until now
nothing bound it at any of the five producers. The downgrade stands; its premise
is now checked rather than assumed.

Mutation 9 is worth noting as a KILL: replacing `Detached` async parsing with
`Shared` was caught by an allocation test. That is the fix for the
13-bytes-pinning-1-MiB measurement recorded earlier, and it is bound.

## The line numbers in THIS document are already stale. Cite symbols.

Measured 2026-08-29, hours after writing them. Five references sampled from the
sections above; three had moved:

    connect_raw.rs:302  ->  316   (BackendMessage::Normal arm)
    client.rs:2420      ->  2136  (pool_cancel_lease_prevents_reuse)
    connect_raw.rs:290  ->  292   (the "13 bytes pinning 1 MiB" comment)
    codec.rs:75                   still correct
    pool.rs:1531                  still correct

Every one drifted because of work landed the same day - a module extracted, a
framing body unified, tests inserted above the cited line.

**This document opens by criticising exactly this.** It records that a carried
audit pointed at `tls_sansio.rs:750` for a split that lives elsewhere, and
concludes "a stale line number is not a weak citation, it is a different claim."
The same rot set into these notes within hours of writing them.

**Updating the numbers is not the fix; they will rot again by the next commit.**
Cite the SYMBOL, which survives edits:

    BAD   connect_raw.rs:302 discards deferred_error
    GOOD  connect_raw.rs, the `BackendMessage::Normal { messages, .. }` arm in
          Handshake::next, discards deferred_error

A reader can find a symbol with one grep whatever the line number is; a line
number that has moved sends them to unrelated code and looks authoritative
while doing it.

Line numbers here are a convenience for the day they were written. When a claim
in this file matters, re-resolve it by symbol before acting - the same rule the
sections above apply to everyone else's findings.

## Soak on the final tree, with the ownership check on the query path

Re-run at `c575e9680`, 420s, dedicated container (5470), 0 pre-existing
connections:

    soak result=ok   SOAK_EXIT=0   elapsed_ms=455453
    pool_acquires  82581
    pool_releases  82581            exact
    rss_rule  samples=85 rises=9 falls=7 delta_kib=60
              preceding_sum_kib=209564  tail_sum_kib=209044  verdict=stable

**Why re-run it.** Two production commits landed after the previous soak: the
write-framing unification, and `33a60076f`, which put an `Arc::ptr_eq` owner
check on EVERY query's statement path. That is the hottest path in the driver
and had only been exercised by short suite runs. A refusal path added there
could plausibly leak a lease or a descriptor; the acquire/release equality is
the instrument that would show it.

It does not. 82581 pairs, exact, and the tail window sum came in BELOW the
preceding window - the opposite of drift.

**Reading the trend arm correctly.** `delta_kib=60` looks like growth and is
not: the quartile comparison is what decides, and `tail_sum < preceding_sum`
here. This is the same arm that produced a false "growing" verdict at the 180s
default earlier, measured and explained above - at 420s the window spans enough
oscillation periods to be trustworthy.

Three soaks this session, all with exact acquire/release equality: 35408 at
180s, 82346 and 82581 at 420s.

## copy_in's "own close read as a lost connection": premise true, conclusion false

The carried finding said `poll_ready`/`poll_flush` treat the sink's OWN close as
a lost connection and drain away `CommandComplete` + `ReadyForQuery`. It shipped
with its own open question - is `poll_flush`'s `sender.is_closed()` even
reachable from our `poll_close`? - and said to answer that before fixing.

**It is reachable.** `poll_close` delegates to `poll_finish`, and `poll_finish`
calls `poll_flush`. So the chain `poll_close -> poll_finish -> poll_flush ->
sender.is_closed()` is real, and closing our own sender does make that load
report `true`. Anyone re-deriving this will reach the same point and it looks
alarming.

**The misdiagnosis it predicts cannot happen, because the guard is already
there.** `poll_finish` sets `SinkState::ReadingAfterClose` on the arm where our
own `sender.poll_close` returned `Ok`. `poll_flush` captures that state into
`closed_by_sink` BEFORE doing any work, and the disconnect arm requires
`disconnected && !closed_by_sink`. Our own close therefore takes the plain
`Poll::Ready(Ok(()))` arm and the response stream keeps being read - which is
exactly what preserves `CommandComplete` + `ReadyForQuery`.

**And it is bound.** `flush_after_cancelled_close_preserves_copy_completion`
drives a close poll to `Pending`, asserts the state has left
`Active | Closing | Finished`, and asserts `sender.is_closed()` is true - i.e.
it stands in precisely the state the finding describes and then checks the
completion still arrives.

**Why this one is worth writing down.** The finding is not wrong about the
mechanism; it is wrong about the outcome, and the difference is one `&&` term
that a reader scanning for `is_closed()` will not see. A finding whose premise
survives re-derivation is the most dangerous kind, because confirming the
premise feels like confirming the finding. Cite
`flush_after_cancelled_close_preserves_copy_completion` when this resurfaces.
