# The six standing compio-postgres findings, re-derived line by line

Every cycle of this pilot has carried the same six open findings forward. They
were re-derived against `ef05cc94e` by reading the code they name, not by
re-reading the list. **Five of the six are no longer true.** Four were fixed by
work that landed since the list was written; one was never true as stated.

The list also carried stale line numbers - it pointed at `connect_raw.rs:993`
and `tls_sansio.rs:750`, neither of which is the code described. A finding whose
line reference has drifted cannot be judged by looking where it points, and a
finding nobody re-derives is indistinguishable from a finding nobody fixed.

## 1. `deferred_error` discarded at the handshake; `take_deferred_error` uncalled

**WRONG on the second half, deliberate on the first.**

`take_deferred_error` has two production callers, one per read path:

    connection.rs:859    the serialized loop
    connection.rs:2774   the split path

and two tests that name the behaviour, `serialized_deferred_error_poisons_before
_its_prefix_wakes` and `split_deferred_error_poisons_before_its_acknowledged
_prefix_wakes`.

The handshake at `connect_raw.rs:319` does destructure `BackendMessage::Normal
{ messages, .. }` and drop the field, and `:320-322` argues that is safe because
`deferred_error` is only ever set when the batch already contains an
ErrorResponse - so startup fails regardless, on the better diagnostic.

**That claim is load-bearing, so it was checked rather than believed.** All five
assignment sites in `codec.rs` are gated on `saw_error_response`, and each has an
ungated `return Err(error)` beside it for the case where no ErrorResponse was
seen:

    codec.rs:390   Err(error) if saw_error_response      header parse
    codec.rs:429   if saw_error_response                 validate_length
    codec.rs:442   if saw_error_response                 startup message length
    codec.rs:459   if saw_error_response                 COPY wire format
    codec.rs:473   if saw_error_response                 ReadyForQuery

The gate holds at every site. The handshake discard is sound.

## 2. `MAX_DELAYED_HANDSHAKE_BYTES` charges the frame but pins the allocation

**FIXED.** The constant now lives at `connect_raw.rs:84` and is enforced at
`:503`. Delayed async frames are read through
`read_backend_detached_async_frames`, and `:288-296` records the measurement
that motivated it - a 13-byte notice pinning a grown 1 MiB read buffer - and
states the resulting invariant: "what is charged and what is held must be the
same bytes." The detach is in the code, not only in the comment.

## 3. `handshake.stream.into_inner()` drops unread buffered bytes

**OBSOLETE - there is no such call site.** `BufStream::into_inner` still exists
but carries `#[allow(dead_code)]` and has zero callers; so does its neighbour
`get_mut`. Nothing can lose bytes through a function nothing calls.

Both are dead code the no-back-compat rule would delete. Filed as cleanup, not
as a defect.

## 4. `write_raw` splits the row buffer before an await, losing up to 4 KB

**FIXED, and the fix is the opposite arrangement.** `send_buffered_rows`
(`binary_copy.rs:159-180`) splits off only the row that crossed the threshold
and leaves every already-`Ok` row in `buf` until the sink is ready:

    let row = buf.split_off(row_start);
    poll_fn(|cx| sink.as_mut().poll_ready(cx)).await?;
    buf.unsplit(row);
    sink.as_mut().start_send(buf.split().freeze())?;

Cancellation while readiness is pending therefore rolls back only the unfinished
call. `:174-176` also notes that no cancellation point separates `unsplit` from
`start_send`.

## 5. `copy_in` poll_ready/poll_flush treat the sink's own close as a lost connection

**STILL OPEN, and still unverified.** This is the one item that survives. It was
recorded as needing a reachability decision first - whether `poll_flush`'s
`sender.is_closed()` is reachable from our own `poll_close` - and that question
has not been answered. Answer it before writing a fix; a fix for an unreachable
branch is churn.

## 6. The TLS split discards unconsumed ciphertext

**WRONG.** Every buffer travels to the read half. `TlsStreamCore::try_into_split`
(`tls_sansio.rs:963-999`) moves `cipher`, `cipher_len`, `cipher_read` and `plain`
into the new `TlsReadHalf`, and reconstructs all four on the `Err` path.
`MaybeTlsStream::try_into_split` (`maybe_tls_stream.rs:207`) only dispatches to
it, and `BufStream::try_into_split` (`buf_stream.rs:724`) carries `read_buf` and
`write_buf` across in both arms.

## What this costs and what to change

Re-deriving all six took about half an hour of reading. Carrying them unverified
cost more than that: each cycle re-asserted them as live, and one of them named a
function that no longer has a caller.

**A findings list needs the commit it was true at.** These carried none, so
there was no cheap way to tell "fixed" from "still broken" short of re-deriving
every entry - which is what finally happened. Any finding that survives a cycle
should carry the commit it was last confirmed at, and any finding whose line
reference no longer resolves should be re-derived or dropped, never forwarded.
