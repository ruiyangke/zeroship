# Eleven `Debug` impls never run, and five of them are the ones that redact

Measured 2026-08-31 at `e98446875` from the same `cargo llvm-cov` JSON as
`2026-08-31-coverage-seam-retirement-drain.md`. Region counts are per impl
block, taken from the coverage segments, not estimated.

    file              Debug impl for            regions  executed
    cancel_token.rs   CancelToken                    61         0   REDACTS
    tls_rustls.rs     RustlsConnect                  14         0   REDACTS
    tls_rustls.rs     RustlsStream                   14         0   REDACTS
    binary_copy.rs    BinaryCopyOutRow               11         0   REDACTS
    to_statement.rs   ToStatementType                15         0   REDACTS
    connection.rs     Connection                     53         0
    replication.rs    ReplicationConnection          17         0
    replication.rs    ReplicationStream              17         0
    tls_sansio.rs     CloseNotifySent                 5         0
    tls_sansio.rs     TlsReadHalf                     5         0
    tls_sansio.rs     TlsWriteHalf                    5         0
    pool.rs           PoolConfig                     31        31
    pool.rs           Pool                           39        39
    pool.rs           OwnedPooledClient               5         5

`pool.rs` is the control: its three impls run, so the measurement is not simply
blind to `Debug`.

## Why this is not cosmetic

**A redaction that never executes is an unverified claim about secret handling.**
`CancelToken::fmt` exists specifically to keep two things out of logs:

    let socket_config = self.socket_config.as_ref().map(|_| "<redacted>");
    let secret_key = self.secret_key.as_ref().map(|_| "<redacted>");

The cancel secret key is a bearer credential - anything holding it can cancel
that session's queries. The impl is 61 regions of formatting logic asserting
that it does not leak, and no test has ever run a line of it. A field added
later that prints the raw key would be caught by nothing.

`ChannelBinding` in `tls.rs` shows the shape the others lack: its test at
`tls.rs:437` asserts `debug.contains("<redacted>")`. That is the only place in
the crate where a redaction claim is checked.

**Two of the unexecuted impls can do worse than leak.** `Connection::fmt` takes
locks:

    let parameter_count = self.parameters.lock().len();
    let has_terminal_server_error = self.terminal_server_error.lock().is_some();

A `dbg!(&connection)` from inside code already holding either lock deadlocks.
Nothing exercises that path, so the hazard is unmeasured rather than absent.

## What is worth doing

A smoke test per impl, asserting two things:

1. formatting completes - it does not panic, and does not deadlock under a
   watchdog;
2. for the five that redact, the rendered string contains `<redacted>` and does
   NOT contain the secret it stands for.

That is cheap, it closes eleven unexecuted impls, and for the redacting five it
converts a claim into a check. It also guards the direction that actually bites:
someone adding a field to `CancelToken` and rendering it by default.

Note the ordering constraint for `Connection`: `connection.rs` is under active
work in another worktree at the time of writing, so that one is deliberately
left out of the first batch.

## Correction: two of these were covered before I briefed them again

The table above is a snapshot from `e98446875`, and it was cited as current on
2026-08-31 in a brief that told an agent `ReplicationConnection` and
`ReplicationStream` "both have `Debug` impls that no test has ever executed".

That was false when written. `0fe11fa9f` had already added
`replication_connection_debug_names_its_type` and
`replication_stream_debug_names_its_type`, which execute both impls.

The agent did the sensible thing with a target that was already covered: it
REPLACED those two tests with stronger ones, adding a three-second formatting
watchdog and asserting that stream state, parameter values, socket config and
the cancel bearer key stay out of the rendered string. Net test count moved by
zero, which is how the mistake surfaced - the predicted suite total came out
right only because two tests were deleted as two were added.

**This is the same error as the standing-findings list**, made by the same
route: a recorded finding was quoted into new work without being re-derived
against the tree. A coverage table is a measurement with a commit attached, not
a standing fact, and the fix is the one that retired the findings list -
re-derive, or do not cite.

Rows known closed since this table was taken:

    cancel_token.rs   CancelToken            closed (redaction proved)
    tls_rustls.rs     RustlsConnect          closed
    connection.rs     Connection             closed (deadlock watchdog)
    replication.rs    ReplicationConnection  closed by 0fe11fa9f, later strengthened
    replication.rs    ReplicationStream      closed by 0fe11fa9f, later strengthened

Any remaining row must be re-measured before it is briefed again.
