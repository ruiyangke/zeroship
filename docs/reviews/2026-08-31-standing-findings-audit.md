# The six standing compio-postgres findings, re-derived line by line

Every cycle of this pilot has carried the same six open findings forward. They
were re-derived against `ef05cc94e` by reading the code they name, not by
re-reading the list. **None of the six is still true.** Four were fixed by work
that landed since the list was written, two were never true as stated, and the
one carried as "unverified" turns out to be guarded on both arms with a test
naming the behaviour.

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

**HANDLED on both arms, and one of them is bound by a test.**

The finding asked for a reachability decision first - is `poll_flush`'s
`sender.is_closed()` reachable from our own `poll_close`? It is, and the code
already distinguishes the two ways of getting there.

`poll_flush` does not test `is_closed()` alone. It carries the sink's own state
into the decision (`copy_in.rs:537, 569`):

    let closed_by_sink = matches!(self.state, SinkState::ReadingAfterClose);
    ...
    Poll::Ready(Ok(())) if disconnected && !closed_by_sink =>
        self.poll_disconnected_diagnosis(cx),

So a disconnect that the sink caused by closing its own producer is not
diagnosed as a lost connection. The other order - the sender closing while the
state is still `Active`, before `ReadingAfterClose` is set - is caught one level
up in `poll_finish` (`:362-369`), which converts a `closed` error into
`SinkState::Reading` and continues, with the reason stated: the connection can
close the COPY producer only after publishing any decoded backend messages, so
a queued ErrorResponse is the better diagnosis than the local symptom.

Neither arm drains away `CommandComplete` + `ReadyForQuery`, which was the
consequence the finding predicted.

`flush_after_cancelled_close_preserves_copy_completion` (`:1192`) binds exactly
this: it asserts the first `poll_close` returns `Pending`, that the sink has
left `Active`/`Closing`/`Finished` for response reading, that it has closed its
own sender, and that CopyDone + Sync still reached the wire as
`[b'c',0,0,0,4,b'S',0,0,0,4]`.

**What is verified here is the state machine and the test, by reading.** No
mutation was run against this arm on this cycle, so "the guard exists and a test
names the behaviour" is the claim - not "the test would fail if the guard were
removed."

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

The failure is not that the findings were wrong when written - most were right,
and the work that fixed them was real. It is that **nothing retired them**. A
finding is created with evidence and then survives on repetition alone, so the
list drifts from a description of the code into a description of the past.
That is the same defect as a stale comment, with the same cause and the same
fix: re-derive, or delete.

**A findings list needs the commit it was true at.** These carried none, so
there was no cheap way to tell "fixed" from "still broken" short of re-deriving
every entry - which is what finally happened. Any finding that survives a cycle
should carry the commit it was last confirmed at, and any finding whose line
reference no longer resolves should be re-derived or dropped, never forwarded.

## Re-derived 2026-09-01 at 07a05fb20

The recurring prompt still carries these six as "all re-verified live at
d2422bdcd". That commit is real, and it is **688 commits behind HEAD**. Every
item was re-checked against the current tree; none is true, and the specific
line numbers no longer point at the code described.

| Finding as stated | At HEAD |
| --- | --- |
| `codec.rs:75` has an uncalled `take_deferred_error()` | **Three live callers**: `codec.rs:867`, `connection.rs:804`, `connection.rs:2485`. |
| `connect_raw.rs:302` `Normal { messages, .. }` discards `deferred_error` | The arm is at `:319`. Every assignment site is gated on `saw_error_response`, so nothing is discarded that was set. |
| `connect_raw.rs:461` charges `frame_len` but a retained frame pins the whole allocation | This is the fix, not the bug. The constant is at `:84` and the check at `:503`; the read is `read_backend_detached_async_frames`, chosen so "what is charged and what is held must be the same bytes". The comment at `:288-297` records the original measurement, 13 bytes pinning 1 MiB. |
| `connect_raw.rs:993` `into_inner()` drops unread buffered bytes | **No `into_inner()` call exists anywhere in `connect_raw.rs`.** |
| `binary_copy.rs:63` `write_raw` splits the row buffer before an await | `write_raw` is at `:72` and takes a `checkpoint`, truncating to it on error; the only await is a `send_buffered_rows(.., checkpoint)` past 4096 bytes. There is no pre-await split to lose rows at. |
| TLS split discards unconsumed ciphertext, at `tls_sansio.rs:750` | The split is `try_into_split`, `maybe_tls_stream.rs:207` and `:269`. The cited file and line describe nothing. |

Treat the list in the prompt as a fixed string, not as a live finding set. It
has now been re-derived twice, on 2026-08-31 and 2026-09-01, with the same
result both times.
