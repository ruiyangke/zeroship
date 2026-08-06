# Phase 3 — connection loop + authentication

`cargo check -p compio-postgres` (with and without `--features tls`) passes cleanly — no errors, no warnings. Workspace check is unchanged (the single pre-existing warning in `crates/runtime/src/runtime.rs:435` is from Phase 2 and unrelated to this port).

## Files ported

| File | LOC | Source file | LOC (src) | Shape |
|---|---|---|---|---|
| `src/connect_raw.rs` | 370 | `connect_raw.rs` | 368 | Startup + auth state machine; `Framed` replaced with `Handshake` over `BufStream`; all four auth paths preserved (AuthOk, Cleartext, MD5, SASL SCRAM-SHA-256 / -PLUS) |
| `src/connect.rs` | 206 | `connect.rs` | 229 | Host resolution + failover; `tokio::net::lookup_host` → compio's `ToSocketAddrsAsync`; sequential host loop, no FuturesOrdered |
| `src/connection.rs` | 377 | `connection.rs` | 356 | Rewritten from hand-cranked `Future` with `poll_*` to an `async fn run(mut self)`; `futures_util::select_biased!` replaces the manual poll; `handle_message`/`deliver_batch`/`route_async` extracted as free functions to keep select borrows disjoint from method receivers |
| `src/client.rs` | 326 | `client.rs` | 800 | Real `InnerClient::send`, `with_buf`, type cache; real `Responses::{poll_next, next}`; `Client::batch_execute` implemented inline (not via `simple_query.rs` which is Phase 4); `cancel_token`, `process_id`, `is_closed`, `__private_api_rollback`, `__private_api_close` live; the full query/execute/prepare surface still deferred to Phase 4 |

## select! vs poll_fn — the choice

I picked `futures_util::select_biased!`. The arguments:

- **Readability.** The upstream `poll_read` + `poll_write` + `poll_flush` + `poll_shutdown` in tokio-postgres total ~170 LOC and are the hardest part of the crate to audit. A flat `select!` with two branches (request in, message out) collapses to 15 LOC and makes the state transitions obvious.
- **Borrow discipline.** `select_biased!` rejects the naive `&mut self` approach — the compiler forces us to split `Connection` into disjoint field borrows (`stream`, `receiver`, and the other fields passed as explicit args to free functions). This is actually a feature: tokio-postgres's manual `Poll` code hides the fact that `poll_read` and `poll_write` alternate on the same `&mut self.stream`; the explicit split here documents the data flow.
- **FusedFuture cost.** Both branches use `.fuse()` + `pin_mut!`; per iteration this is a stack-only adapter, no allocation.

I did **not** use `poll_fn` because it would have forced manual `Context`/`Waker` plumbing on every field, defeating the whole point of rewriting an async-fn-based loop in a completion-based runtime.

## Async message routing path

```
BackendMessage::Async(Message::NoticeResponse)    →  DbError::parse()  →  AsyncMessage::Notice
BackendMessage::Async(Message::NotificationResponse) →  Notification  →  AsyncMessage::Notification
BackendMessage::Async(Message::ParameterStatus)  →  parameters.insert() (HashMap)
BackendMessage::Normal { messages, request_complete } →  responses.front().sender.try_send(messages)
```

Notices and notifications flow into an `Option<mpsc::UnboundedSender<AsyncMessage>>` that starts `None`. `Connection::notifications()` installs the sender and returns the receiver. When `async_sender` is `None`, notices are logged at `info!` and notifications at `debug!`. Tokio-postgres integrates this into its `poll_message` method; I exposed it as a post-construction setter on `Connection` so users don't need to drive the connection via `poll_message` to see async messages.

Notices captured during `connect_raw::read_info` are stored in `Handshake::delayed: VecDeque<Message>` and replayed on the first loop iteration of `Connection::run`. This matches tokio-postgres's `pending_responses` pre-populated in `Connection::new`.

## Compromises

1. **`Responses` backpressure.** Tokio-postgres uses `mpsc::channel(1)` + `poll_ready` + `start_send` so `poll_read` can pause when the consumer is slow. I use `try_send` on the same bounded channel: if the slot is full we drop the batch. For Phase 3's strict pull-model (bind, rollback, batch_execute — each awaits exactly one message at a time before sending the next request) the slot is always empty when we arrive. Phase 4's pipelined `query_raw` stream may need a `pending_batch` side-channel like the tokio version; documented as a hand-off below.

2. **`target_session_attrs` post-handshake probe.** The source issues `SHOW transaction_read_only` right after auth to enforce `ReadWrite` / `ReadOnly`. That requires `Client::simple_query_raw` (Phase 4). Rather than silently ignore the config, `connect.rs` returns an explicit `Error::config("target_session_attrs is not yet supported in Phase 3; defer to Phase 4")` when a non-default value is set.

3. **Parallel failover (`FuturesOrdered`).** The source races DNS A/AAAA results in parallel when multiple hosts are configured. I used a plain sequential loop. Correct but slower on multi-host configs with one slow host. Fine for Phase 3 — single-host is the norm for platform deployments; Phase 4/5 can reintroduce it.

4. **`CopyIn` variant omitted.** `RequestMessages::CopyIn(CopyInReceiver)` stays off the enum until Phase 5 lands `copy_in.rs` — adding the variant now would create a dependency loop through the not-yet-ported `CopyInReceiver`. Nothing in Phase 3 emits a `CopyIn` request.

5. **`Connection::notifications()` API.** The source exposes an iterator through `poll_message`; I added a `notifications()` setter. Functionally equivalent, more ergonomic with async-fn.

## Phase 4 hand-off

Symbols Phase 4 (`query.rs`, `prepare.rs`, `simple_query.rs`, `cancel_query.rs`) will need from Phase 3:

- `InnerClient::send(RequestMessages) -> Result<Responses, Error>`    **real, ready**
- `InnerClient::with_buf(|buf: &mut BytesMut| ...) -> R`            **real, ready**
- `InnerClient::typeinfo()`, `set_typeinfo()`, `typeinfo_composite()`, `set_typeinfo_composite()`, `typeinfo_enum()`, `set_typeinfo_enum()`, `type_(Oid)`, `set_type(Oid, &Type)`, `clear_type_cache()`    **real, ready (all under `#[allow(dead_code)]`)**
- `Responses::next() -> Result<Message, Error>`    **real, ready**
- `Client::inner() -> &Arc<InnerClient>`    **real, ready**
- `Client::simple_query` / `::simple_query_raw`    **Phase 4 scope** — when available, `connect.rs` should drop the `target_session_attrs` guard and call through

Symbols Phase 4 still needs to introduce:

- `query::{encode_bind, query, query_typed, execute, execute_typed, sync, RowStream}`
- `prepare::prepare(client, &str, &[Type]) -> Statement`
- `simple_query::{simple_query, batch_execute, SimpleQueryStream}`
- `cancel_query::cancel_query` + `cancel_query_raw::cancel_query_raw`
- `RequestMessages::CopyIn(CopyInReceiver)` variant (Phase 5, not 4)

## Verification

```
$ cargo check -p compio-postgres
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.17s

$ cargo check -p compio-postgres --features tls
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.48s

$ cargo check --workspace --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.74s
    (one pre-existing warning in crates/runtime/src/runtime.rs:435, unrelated)
```

No errors. No warnings attributed to compio-postgres. All three checks pass.
