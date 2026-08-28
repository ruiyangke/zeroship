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
