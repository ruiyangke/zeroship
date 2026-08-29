# compio-postgres architecture: measured critique and a staged plan

Date: 2026-08-29. Every number here is measured, not estimated. The method is at
the bottom so it can be re-run.

## The headline defect: the connection core is one cycle

**16 of the crate's modules form a single strongly-connected component.** Every
one of the 240 ordered pairs among them is mutually reachable:

    cancel_token  client     config      connect     connection  connect_raw
    connect_socket connect_tls copy_in   copy_out    maybe_tls_stream
    prepare       query      simple_query statement  tls

There is no layering inside that set. You cannot reason about, test, or replace
any of those modules without the other fifteen.

The periphery IS correctly layered and should be left alone: `escape`,
`replication`, `transaction`, `codec`, `buf_stream`, `types`, `error`,
`keepalive`, `portal`, `row`, `bind`, `binary_copy` are all outside the cycle.

**A caution about this measurement.** A first pass reported 19 modules and 172
edges. That was WRONG: `grep 'crate::[a-z_]+'` also matches rustdoc intra-doc
links like `[`crate::transaction`]`, so documentation was counted as
dependency. `escape.rs` has no `use crate::` at all and was falsely placed in a
cycle. Stripping comments gives **164 edges and 16 modules**. Strip comments
before drawing a module graph.

## What actually creates the cycle

Not deep entanglement - **convenience methods placed on the wrong type.**

### `config.rs` (3445 production lines, highest fan-in in the crate at 17)

A configuration module should be a leaf: data plus parsing. This one imports
`connect`, `connect_raw`, `connect_tls`, `tls`, `Client` and `Connection`.

Every production use of those imports is inside TWO methods:

    2421  pub async fn connect<T>(&self, tls: T) -> Result<(Client, Connection<...>), Error>
    2443  pub async fn connect_raw<S, T>(...)

That is roughly 55 lines out of 3445. Those 55 lines put the module that 17
other modules depend on inside the cycle.

### `client.rs`

The same shape. `Client` carries convenience methods (`query`, `copy_in`,
`copy_out`, `simple_query`, `prepare`) that make it depend on the operation
modules, while those modules need `Client`/`InnerClient` to execute. Five edges.

## The fix, and what it is measured to buy

Rust permits an inherent `impl` block in any module of the defining crate. So
the methods can MOVE without the public API changing at all: `config.connect(tls).await`
still compiles and still means the same thing.

Simulated by removing the corresponding edges and recomputing the SCC:

| Stage | Change | SCC size | Modules freed |
| --- | --- | ---: | --- |
| baseline | - | 16 | - |
| 1 | move `impl Config { connect, connect_raw }` into `connect.rs` | **10** | cancel_token, config, connect, connect_raw, connect_socket, tls |
| 2 | move `impl Client { query, copy_in, copy_out, simple_query, prepare }` into their own modules | **8** | client, copy_out |

Stage 1 moves ~55 lines and frees six modules, including the crate's most
depended-upon one. That is the highest-leverage change available.

After stage 2 the residue is `connection, connect_tls, copy_in,
maybe_tls_stream, prepare, query, simple_query, statement` - protocol
operations that genuinely interact with the connection loop. Those cycles are
intrinsic, not accidental, and chasing them further would be churn.

## Second defect: the split read path is under-tested, and it is the hot one

`BufStream` has two read paths - whole-stream, and split-into-halves. The
mutation sweep found the same mutation repeatedly KILLED on the whole-stream
path and SURVIVING on its split twin; one whole-stream mutation failed 17
library tests while the identical split-path change cleared the entire library
target. Test population, counted independently:

    buf_stream.rs unit tests        23
      of which exercise split        6
    suite files touching split       1

**The split path is what the multiplexed connection loop uses** - the production
path for ordinary concurrent queries. The whole-stream path is the serialized
fallback. The better-tested half is the less-used one.

This is a testing gap rather than a structural one, but it is listed here
because it predicts where the next defect will be, and because the same
asymmetry plausibly exists in `tls_sansio.rs` (`TlsReadHalf`/`TlsWriteHalf`) and
`maybe_tls_stream.rs`.

## Third observation: module size is not itself the problem

    config.rs      3445 prod  (2072 test)
    connection.rs  3205 prod  (3764 test)
    replication.rs 2912 prod  (2977 test)
    pool.rs        2752 prod  (2342 test)

`connection.rs` at 3205 production lines is large, but it is one coherent thing:
the connection event loop. Splitting it for size alone would create more edges,
not fewer. Size is worth revisiting only after the cycle is broken, when the
seams are visible.

## Method, so this can be re-run

```bash
cd libs/compio-postgres/src
for f in *.rs; do m=${f%.rs}; [ "$m" = lib ] && continue
  awk '/^#\[cfg\(test\)\]/{exit} {sub(/\/\/.*$/,""); print}' "$f" \
    | grep -oE "crate::[a-z_]+" | sed 's/crate:://' | sort -u | grep -vx "$m" \
    | while read d; do [ -f "$d.rs" ] && echo "$m $d"; done
done | sort -u > edges.txt
```

Then iterate `join` to a transitive closure and take `$1==$2` for cycle
members. Two properties matter: strip comments (rustdoc links are not
dependencies), and stop at the first `#[cfg(test)]` (test-only imports are not
architecture).

## An independent critique, triaged against the code (2026-08-29)

A read-only reviewer was given the sections above and asked for design defects
NOT in the module graph - invariants held by convention, state machines encoded
in booleans, the error model, parallel implementations, and cancellation/Drop.
It returned seven findings. Each was checked against the source before being
believed; two are less severe than they read, and the reasons are worth keeping.

**1. A failed bare-client cancel can still cancel the next query. CONFIRMED,
being fixed.** `cancel_query_raw.rs` writes the packet, THEN flushes, shuts
down, and waits for the postmaster's EOF. If any step after `write_all` fails,
the caller gets `Err` while the bytes are already gone. The crate documents
exactly this hazard on `wait_for_server_close` - "a delayed cancel could arrive
after the main session's `Sync` and cancel that backend's next query" - and
`CancelAbandonmentGuard` states the asymmetry deliberately: the pool records the
uncertainty "without changing the ordinary bare-client error path". The fix is
to separate provably-unsent (DNS, connect, TLS - nothing left the process) from
possibly-sent (anything at or after the write), and retire only the latter.
Retiring on EVERY failed cancel would be a worse bug, so that direction needs a
test too.

**3. `notifications()` is an unbounded queue. REAL BUT ARGUABLY CORRECT.** The
counter-argument is strong and the reviewer made it: the connection loop cannot
await a bounded consumer without stalling unrelated protocol progress, and
silently dropping LISTEN/NOTIFY events breaks the semantics callers rely on.
The receiver is handed to the CALLER, so the buffer is theirs. What is missing
is not a bound but a SENTENCE: the method's doc is long and detailed about a
different known defect (TLS plus the serialized loop) and says nothing about the
caller's obligation to drain.

**5. `Statement` ownership is stored but never checked. REAL, LOWER SEVERITY
THAN IT READS.** `StatementInner` holds `client: Weak<InnerClient>` and the type
says "prepared statements can only be used with the connection that created
them", but `into_statement` never compares. The obvious fear is a name
collision executing the WRONG SQL - and that cannot happen here:
`prepare.rs:81` generates names from a PROCESS-GLOBAL `static NEXT_ID:
AtomicUsize`, so `s42` is unique across every connection in the process. A
foreign statement therefore produces a server-side 26000, which this crate
already handles carefully and tests
(`statement_cache_requires_server_provenance_before_retrying_26000`). The defect
is a remote error where a local one would be clearer, not a correctness hole.

Findings 2, 4, 6 and 7 - error identity varying with response-channel
occupancy, `connect_raw` accepting unrunnable streams, a host-local encoding
refusal reclassified as fatal, and COPY recovery permitting illegal states -
are recorded here as open and not yet independently verified.

**The triage matters more than the list.** Two of seven changed severity once
checked against the source, both because a fact elsewhere in the crate bounded
the damage: a global counter in one case, existing tested 26000 handling in the
other. Rank findings after reading what constrains them, not on first reading.

## Correction: the stage-2 plan above was wrong, and the reason generalises

The table near the top predicted that moving `impl Client`'s operation methods
would take the SCC from 10 to **8**, "freeing client and copy_out". Measured, it
takes it to **9** and frees **copy_out alone**. `client` stays in the cycle,
because cutting its OUTGOING edges does nothing about `connection -> client`,
`query -> client`, `copy_in -> client`, `copy_out -> client`, `prepare ->
client`, `simple_query -> client` and `statement -> client`, all of which remain,
alongside `client -> connection`.

The error came from simulating a BUNDLE of five edge removals and then
attributing the result to the bundle's theme. A bundle's effect is not evidence
about any member. The right instrument is one edge at a time:

```bash
# for each intra-SCC edge, drop only that edge and recompute the SCC
sccprobe.sh edges.txt <src> <dst>
```

Run over all 31 intra-SCC edges, that says only **four** are load-bearing at all:

| edge cut (alone) | SCC | frees |
| --- | ---: | --- |
| `maybe_tls_stream -> connect_tls` | **8** | connect_tls, maybe_tls_stream |
| `connect_tls -> maybe_tls_stream` | **8** | connect_tls, maybe_tls_stream |
| `prepare -> statement` | 9 | statement |
| `client -> copy_out` | 9 | copy_out |

The other **27 edges are redundant**: cutting any one of them alone changes
nothing, because the cycle routes around it. Effort spent on those 27 buys
exactly zero, and four of the five edges in the stage-2 plan are among them.

## The cut that was actually taken: `Encryption` to a leaf

The two highest-value cuts are the two directions of one two-module cycle, so
only one type had to move. `Encryption` - a two-variant enum plus
`first_for(SslMode)` - lived in `connect_tls.rs`, the module that PERFORMS
negotiation, while `maybe_tls_stream.rs` imported it only to NAME the answer in
`negotiated_encryption()`. `connect_tls` imports `maybe_tls_stream` to build the
stream. Two edges, two modules, one cycle.

A grep for the type found five users. The compiler found **eleven**:
`cancel_query`, `cancel_query_raw`, `cancel_token`, `client`, `connect`,
`connect_raw`, `connect_tls`, `connection`, `maybe_tls_stream`, `replication`,
`test_utils`. Two of them (`connect_raw`, `replication`) were invisible to the
first grep because they import it as `use crate::connect_tls::{Encryption,
negotiate_tls}` - a braced import the pattern did not match. **Let the type
checker enumerate call sites; a grep answers spelling.**

`src/encryption.rs` is now a leaf depending only on `config`. Measured result:

    SCC 10 -> 8      connect_tls and maybe_tls_stream both freed
    edges 162 -> 167

**Edges went UP while the cycle went DOWN, and that is the point.** Eleven
modules now name a leaf instead of naming the negotiation module, which adds
edges and removes coupling. Anyone re-running the method should expect this and
not read the edge count as a regression.

## What is left, and why the next cut is not obvious

The residual SCC is `client, connection, copy_in, copy_out, prepare, query,
simple_query, statement`. The two remaining load-bearing edges are
`prepare -> statement` and `client -> copy_out`, each worth exactly one module.

Both are worth LESS than they look. `client -> copy_out` is a single `use
crate::copy_out::CopyOutStream` - the RETURN TYPE of `Client::copy_out`. Moving
that method into `copy_out.rs` would free one module and scatter the crate's
primary public API across five files, so `client.rs` would no longer show what a
`Client` can do. That is a maintainability LOSS bought with a metric GAIN, and
the metric is not the goal. Recommend NOT taking it.

Beyond those two, nothing single-edge remains: the eight-module core is a
genuine mutual dependency between the connection loop and the protocol
operations that drive it. Stop here.

## Finding 2 checked and DISPROVED (2026-08-29)

The claim was that an error's identity varies with response-channel occupancy:
`Responses::poll_next` returns `Error::db(body)` when an `ErrorResponse` arrives
in-band, but reaches a different constructor when the channel closes first, so
the same server failure would surface as two different errors depending on a
race.

It does not. `client.rs` has two routes:

    if let Message::ErrorResponse(body) = message {
        let error = Error::db(body);            // in-band
    ...
    if let Some(error) = self.request_server_error.lock().take() {
        return Poll::Ready(Err(Error::from_db_error(error)));   // channel closed
    }

and `error/mod.rs:644` shows the first delegates to the second:

    pub(crate) fn db(error: ErrorResponseBody) -> Error {
        match DbError::parse(&mut error.fields()) {
            Ok(e) => Error::from_db_error(e),
            ...

Both routes produce `Kind::Db` wrapping the same `DbError`. The identity is
stable across the race. The only case that differs is the one where NO server
error was recorded on either channel, which yields `Kind::Closed` - and
reporting a close differently from a server error is correct, not a defect.

**Three of the seven findings have now changed status on inspection** (2 wrong,
3 and 5 milder than stated). The pattern is consistent: each was written from
the shape of the code at one site, and each was bounded by something one
indirection away - a delegating constructor, a process-global counter, existing
26000 handling. The reviewer could not have seen any of them without following
the call.

## Finding 6 checked and DISPROVED (2026-08-29)

The claim was that a host-local encoding refusal is reclassified as fatal, so a
purely client-side failure would abort a multi-host connect that should have
tried the next host. The site is real, `connect_raw.rs:756`:

    frontend::query(probe.query(), &mut buf)
        .map_err(Error::encode)
        .map_err(Error::target_session_attrs_fatal)?;

and `Kind::TargetSessionAttrsFatal` does mean what the finding says: "no
transport, address, configured host, or `prefer-standby` pass may be retried".

It fails on two independent grounds.

**The arm is unreachable.** `probe.query()` is a `const fn` returning one of two
string literals - `"SHOW transaction_read_only"` and `"SELECT
pg_catalog.pg_is_in_recovery()"`. `frontend::query` fails only on an interior
NUL. Neither literal has one, and no caller supplies the string.

**If it were reachable, fatal would be CORRECT.** An encode refusal of a
compile-time constant is deterministic and host-independent: every remaining
host would fail identically. Retrying them is guaranteed waste, which is exactly
the condition `TargetSessionAttrsFatal` exists to express. Reclassifying it as
retryable would be the defect.

**Four of seven findings have now changed status** (2 and 6 wrong, 3 and 5
milder). One - the post-write cancel - is confirmed and being fixed. Findings 4
and 7 remain unverified; 7 is under investigation as the `copy_in` state
question.

The error model itself came out of this well, and that is worth recording since
the review asked whether the classification is load-bearing or decorative. It is
load-bearing and documented: `Tls`, `TlsHandshake` and `TlsUnattested` are three
kinds for what a naive model would call one, and they exist because `sslmode=prefer`
must retry exactly two of them in plaintext and nothing else. `Closed` versus
`Cancelled` splits "the socket is gone" from "the socket is fine but the two
sides disagree about where they are in the byte stream". Each carries its libpq
analogue or an explicit note that libpq has none.

## The split-path test gap has a structural cause, measured 2026-08-29

The section above predicted the split read path was under-tested and said the
same asymmetry "plausibly exists" in `tls_sansio.rs` and `maybe_tls_stream.rs`.
Two mutation sweeps have now measured it:

    buf_stream.rs split half   7 mutations,  4 SURVIVED
    tls split halves           19 mutations, 10 SURVIVED (sweep in progress)

Fourteen behaviours where changing the code broke no test, all on the halves the
multiplexed connection loop actually runs.

**The cause is not that someone forgot to write tests.** `ReadFramer` already
exists as a trait and BOTH `BufStream` and `BufReadHalf` implement it - `fill`,
`buf`, `peek_u32_be`, `validate_length`. The trait unifies the INTERFACE and
leaves each type its OWN BODY. Measured on `buf_stream.rs`: the whole-stream
read region is 71 lines, the split one 79, and **26 non-trivial lines are
character-identical**, including every guard the sweep mutated:

    if min_bytes > self.max_message_size {
    if n == 0 {
    if self.read_scratch.is_empty() {
    let buf = std::mem::take(&mut self.read_scratch);
    let (n, buf) = self.read_raw(buf).await?;

So each guard exists twice, and a test written against one copy says nothing
about the other. That is exactly how the sweep's first attempt went wrong on
2026-08-29: line numbers derived from the whole-stream copy mutated code the
split tests never execute, and every mutation "passed".

**The two impls differ in one thing only: how they obtain bytes.** Both call
`self.read_raw(buf)`; `BufStream` reads the whole stream, `BufReadHalf` reads
its half. Everything after that - the size guard, the zero-byte EOF check, the
scratch reuse, the accumulation loop, the length validation - is the same
framing logic written twice.

**The fix is to hoist the framing body, not to keep adding paired tests.** One
implementation parameterised over the byte source, with `read_raw` as the only
thing the two supply. Then there is one guard per behaviour and one place a test
can bind to, and the whole class of "killed here, survived there" disappears
rather than being chased.

That is a larger change than this hardening pass should make unannounced, and
the 14 tests being added now are worth having either way - they pin the current
behaviour, which is exactly what makes such a refactor safe to attempt later.
Recording it as the next structural move, with the measurement that justifies it.

## Checked and adequate: the serialized loop is not the next gap

The two mutation sweeps invite the obvious follow-up - `serialized_loop` has 25
tests against `suite`'s 709, so is the fallback path under-tested too? Checked
2026-08-29; it is not, and the reason is what it is FOR.

`connection.rs:51` states the entry condition: `run_serialized` is "the fallback
for a stream that refuses to split", reached only by "a custom `TlsConnect`
whose stream answers `Err` to `try_into_split`". TCP, Unix sockets and the
built-in rustls transport all split, so production never runs it. Its 25 tests
are not covering a hot path with a thin suite; they are covering a narrow
compatibility path in proportion.

What the file covers, named: queries and parameters, an error leaving the
session usable, transactions, COPY OUT, COPY IN, a post-COPY-IN response error
not leaving a second `ReadyForQuery`, portal paging, a notice raised by an
awaited statement, an abandoned query, a read timeout retiring the session, and
a cancelled query. That is the fallback's surface, not a sample of it.

It also carries `the_harness_is_really_on_the_serialized_loop` - a test whose
only job is to confirm the harness reaches the path it claims to test. That is
the same guard this effort has had to add by hand elsewhere (the pooler's login
count, the soak's `max_connections` gate), and here it was already written.

**The asymmetry the sweeps found is specific and does not generalise to "small
test file means gap".** It was caused by ONE structural fact - two bodies behind
one trait, 26 identical lines - not by test-count imbalance. Ratios of test
counts are not evidence; the duplication was.

## The read framing is now one body, and what that cost

Landed `bf709efa6`. `buf_stream.rs`: 87 insertions, 98 deletions, one file.

`BufStream` and `BufReadHalf` now both call `fill_read_buffer`, and the shared
free functions are `read_with_deadline`, `read_raw_from`, `fill_read_buffer`,
`peek_u32_be_from`, `validate_length_against` and `flush_retry_interrupted`.
Production-only census, each guard appearing exactly ONCE where it appeared
twice before:

    min_bytes > max_message_size      1
    if n == 0                         1
    read_scratch.is_empty()           1
    while read_buf.len() < min_bytes  1
    total > max_message_size          1

`fill_read_buffer(` has exactly two callers - line 492 (`BufStream`) and 646
(`BufReadHalf`).

**The hot-path constraint held.** This is the driver's per-message read path, so
the brief refused any design costing an allocation or a virtual call.
`Box<dyn Future>` occurrences: 2 before, 2 after - both pre-existing timer
machinery - and the diff adds and removes ZERO lines containing `Box<`, `dyn `
or `async_trait`. `read_raw_from<R>` and `fill_read_buffer<R>` are generic, so
monomorphised. The cost paid is a six-argument call and slightly less locality,
which is the tradeoff the agent argued for and I agree with.

**Verified at 1354 passed / 0 failed on BOTH servers**, `CARGO_EXIT=0`, 7
targets each - the same count as before the refactor, so nothing was lost or
skipped.

**One claim NOT made.** Mutating the single accumulation loop kills a split test
and NO `serialized_loop` test, so "one mutation now binds both paths" is true
per-guard, not in general - `serialized_loop` does not exercise partial-read
accumulation. What IS true is that there is now one place to fix and one place
to bind, so the two copies can no longer drift apart unnoticed. That was the
defect; it is gone.

**A verdict this crate's mutation work needs and did not have: WEDGED.**
Replacing the single `if n == 0` EOF guard with `if false` does not fail the
suite, it HANGS it - `fill` spins on a stream that will never deliver more
bytes. An untrapped run then dies on the harness timeout, which kills the
restore before it runs and leaves the tree mutated. Wrap mutation runs in
`timeout`, and confirm restoration with `git diff --numstat` rather than with
the absence of an error message.

## Cancellation and Drop: checked, and the dimension is sound

The independent critique asked where `Drop` does real work and where a
cancellation at an await point leaves inconsistent state, noting that
`release.rs` documents one such case deliberately and asking for undocumented
ones. Surveyed 2026-08-29.

**19 `Drop` impls in production. Exactly TWO do work beyond setting a flag.**
The other seventeen - permit guards, waiters, portal and savepoint cleanup,
the copy-append commit guard, the reader registration, the cancel abandonment
guard - only flip state or release a slot, which is what a destructor in an
async runtime should be limited to.

The two that act:

`StatementInner::drop` -> `close_statement`. It takes no new allocation
(`with_buf`), and on an unencodable name it LOGS and returns rather than
panicking, with the reason recorded at the site: "silence here is
indistinguishable from a successful DEALLOCATE". The send is
`let _ = client.send_with(..., RequestDisposition::Housekeeping,
TransactionEffect::Neutral)` - fire-and-forget, which is correct in a
destructor: a connection that is already gone has no statement left to close,
and there is no runtime to await on.

`CommandRecoveryGuard::drop` -> `self.client.force_close()` behind an `armed`
flag. Synchronous, no await, and the arming is what makes it a no-op on the
success path.

**Nothing here needs changing.** Naming what was checked is the useful output:
a destructor that blocks, allocates, awaits or panics is the failure mode, and
this crate has none. The critique's other five dimensions produced two
disproved findings, two downgrades and one confirmed defect, all recorded in
`docs/reviews/2026-08-28-open-findings-re-derived.md`; this is the sixth and it
is clean.

## Re-derived at the end of the session, because a repeated number rots

The figures above were measured when each cut landed. Re-measured at
`07535df4c`, after the framing unification and every test addition:

    edges 164      SCC 8
    client connection copy_in copy_out prepare query simple_query statement

The SCC is unchanged at 8 and holds the same eight modules. The edge count is
**164, not the 167 recorded earlier** - extracting `read_with_deadline`,
`read_raw_from`, `fill_read_buffer`, `peek_u32_be_from`,
`validate_length_against` and `flush_retry_interrupted` into free functions
removed three cross-module references as a side effect.

Session arc, all measured rather than carried:

    16 modules, 162 edges   start
    10 modules, 162 edges   Config::connect moved out of config.rs
     8 modules, 167 edges   Encryption moved to its own leaf
     8 modules, 164 edges   read framing unified

Edges rose then fell while the cycle only shrank, which is the point made
earlier: edge count is not the health metric, the cycle is.

### The framing refactor verified across every feature shape

`verify.sh` re-run after `bf709efa6`, exit 0:

    default          binaries=5/5 tests=1235 ok
    statement-cache  binaries=5/5 tests=1236 ok
    suite-over-tls   binaries=5/5 tests=1259 ok
    tls_live         binaries=1/1 tests=48 ok
    rss-growth-rule  ruled_on=9 ok
    VERIFY: all modes green

This mattered and was nearly skipped. The refactor had already passed 1354/0 on
both servers, which is ONE feature resolution. `suite-over-tls` routes every
byte of the suite through the TLS stream instead of a plain socket, and the
default `cargo test` does not even BUILD it. A buffered-read change is exactly
the kind that can behave differently there. Passing the default shape twice is
not evidence about shapes that were never compiled.

## RETRACTION: the TLS layer is already unified, and I claimed otherwise

I wrote above that the split/whole duplication removed from `buf_stream.rs`
"still exists in the TLS layer", called it "the one piece of real structural
debt", and dispatched a job to fix it. **That was wrong.** The job measured
first, as its brief required, and stopped without changing anything.

The bodies are one line each:

    impl<S> AsyncRead for TlsStreamCore<S> {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            self.reader.read_into(buf).await
        }
    }
    impl<R> AsyncRead for TlsReadHalf<R> {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            self.reader.read_into(buf).await
        }
    }

`TlsReader::read_into` IS the shared body; the two impls are adapters onto it.
`MaybeTlsStream::read` is a two-arm match delegating to the inner stream. There
is no second copy of any framing logic to unify.

The write side is the same. Of the 11 `fn flush` in `tls_sansio.rs`:

    2  shared free functions - flush_outgoing, flush_through (the implementation)
    2  real impls, 3 and 4 lines, both delegating to those
    7  test doubles nested in `mod tests`

**How I got it wrong: I counted grep hits instead of reading bodies.** The
evidence I used was `grep -c 'fn flush'` = 11 and `grep -c 'ReadHalf\|WriteHalf'`
= 20 and 32. Those count occurrences, and an occurrence includes a one-line
delegation and a test mock. `buf_stream.rs` really did have 26 character-
identical lines - I measured that one properly, and then generalised the
CONCLUSION to a neighbour without repeating the MEASUREMENT.

This is the failure this document catalogues in carried findings - grep answers
spelling, not behaviour - committed by me, on my own finding, after correcting
two others for the same thing today.

**What survives.** The `buf_stream` unifications were real and are landed. The
mutation survivors in the TLS split halves were real and are now tested. What
does not survive is the claim that TLS carries the same duplication: it does
not, and the adapters there are the right shape already.
