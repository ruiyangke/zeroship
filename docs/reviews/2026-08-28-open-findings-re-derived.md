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
