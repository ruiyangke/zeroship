# The carried-forward compio-postgres findings list, re-derived and retired

Date: 2026-08-31. Tree: `2a916b1b3`.

A six-item "open findings" list had been re-quoted verbatim at the top of every
pilot cycle for roughly thirty-five cycles. It was re-derived item by item
against the tree on this date. **Five of the six are dead** - four fixed by
earlier work in this same effort, one describing a symbol that does not exist.
Only one survives, and it needs a judgement rather than a patch.

This is the failure mode in
`feedback_numbers_carried_forward_go_stale`: a list is cheap to re-quote and
expensive to re-derive, so it gets re-quoted. Line numbers rot first, then the
claims behind them. Every item below carries the evidence that closed it, keyed
to a **symbol** rather than a line, because the line numbers in the original
list were wrong in every single case.

## Verdicts

| # | Claim as carried | Verdict | Evidence |
| --- | --- | --- | --- |
| 1 | `connect_raw.rs:302` discards `deferred_error`; `codec.rs:75 take_deferred_error()` is uncalled | **PARTLY REAL** - discard is real (now at the `BackendMessage::Normal { messages, .. }` arm), "uncalled" is false | `take_deferred_error` has three callers: `codec.rs` (one), `connection.rs` (two). Open question dispatched separately. |
| 2 | `MAX_DELAYED_HANDSHAKE_BYTES` charges `frame_len` but a retained frame pins the whole allocation | **FIXED** | `a228a10aa fix(postgres): stop a delayed handshake message pinning the read allocation`, with `delayed_message_does_not_pin_handshake_read_allocation` |
| 3 | `handshake.stream.into_inner()` drops unread buffered bytes | **FALSE - SYMBOL DOES NOT EXIST** | zero occurrences of `into_inner` in `connect_raw.rs`. `handshake.stream` is a `BufStream<MaybeTlsStream<S, T>>` moved WHOLE into `Connection::new`; the buffer and its unread tail survive the handover. |
| 4 | `binary_copy.rs:63 write_raw` splits the row buffer before an await; cancelling discards up to 4KB of already-`Ok` rows | **FIXED, AND THE GUARD IS BOUND** | `send_buffered_rows` splits off only the current row (`buf.split_off(row_start)`) before awaiting `poll_ready`, so cancellation rolls back that row alone. Mutation to `BytesMut::new()` fails exactly one test: `cancelling_a_backpressured_flush_keeps_completed_rows`. |
| 5 | `copy_in.rs` `poll_ready`/`poll_flush` treat the sink's OWN close as a lost connection | **FIXED, AND THE GUARD IS BOUND** | `poll_flush` reads `Poll::Ready(Ok(())) if disconnected && !closed_by_sink`, where `closed_by_sink` is `matches!(self.state, SinkState::ReadingAfterClose)`. Dropping `&& !closed_by_sink` fails exactly one test: `flush_after_cancelled_close_preserves_copy_completion`. |
| 6 | the TLS split discards unconsumed ciphertext (`tls_sansio.rs:750`, later `maybe_tls_stream.rs:167`) | **FALSE** | `TlsStreamCore::try_into_split` carries `cipher`, `cipher_len`, `cipher_read` AND `plain` into the read half, and the refused-split arm rebuilds all four. `MaybeTlsStream`'s split is a pure forward that holds no buffer of its own. |

Both mutation probes above satisfy one-copy-one-failure: one mutation, exactly
one failing test, `git status` showing `M` before the run.

## The one survivor

`connect_raw.rs`, in `Handshake`'s read loop:

```rust
BackendMessage::Normal { messages, .. } => { self.pending = messages; }
```

The `..` discards `deferred_error`. What bounds it, and what was checked here:

- In `codec.rs`'s `read_backend_with_async_storage`, **all five** `deferred_error`
  assignments are gated on `saw_error_response`. A `Normal` batch can therefore
  only carry a deferred error if that same batch already contains an
  `ErrorResponse` frame.
- The malformed tail is not consumed. The walk `break`s and leaves it in the
  stream buffer, so a later decode re-discovers it - as a hard `Err` that time,
  because `saw_error_response` is false on the new batch.
- The buffer survives the handover to `Connection::new` (see item 3), so
  "re-discovered later" is a real path and not a hypothetical.

So the discard is safe **iff** the handshake can never return successfully after
draining a batch that contained an `ErrorResponse`. That is the open question,
and it is a question about the startup and authentication consumers, not about
the codec. It is dispatched as its own job.

## What to do with the list

Do not carry it forward again. Items 2 through 6 are closed; item 1 is tracked
above with its evidence and its actual open question. A finding that survives a
cycle should be re-derived on the cycle that quotes it, or dropped.
