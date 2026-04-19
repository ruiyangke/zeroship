# Phase 2 — I/O foundation

`cargo check -p compio-postgres` passes cleanly (no errors, no warnings) with and without the `tls` feature.

## Files ported

| File | LOC | Source file | LOC (src) | Shape |
|---|---|---|---|---|
| `src/socket.rs` | 73 | `socket.rs` | 75 | Rewritten for compio async-fn I/O; `Inner` enum preserved |
| `src/tls.rs` | 172 | `tls.rs` | 164 | Copy-near-verbatim; bound swap `tokio::io` → `compio::io` |
| `src/maybe_tls_stream.rs` | 70 | `maybe_tls_stream.rs` | 71 | Plain enum (compio needs no pin-projection) |
| `src/codec.rs` | 186 | `codec.rs` | 98 | Rewritten: `tokio_util::codec::{Encoder, Decoder}` replaced with async free functions over `BufStream` |
| `src/buf_stream.rs` | 167 | (internal) | — | Adapted from `crates/pg/src/stream.rs`; now generic over `S: AsyncRead + AsyncWrite` instead of concrete TcpStream/TlsStream |
| `src/connect_tls.rs` | 82 | `connect_tls.rs` | 60 | Logic preserved; `write_all`/`read_exact` swapped to compio owned-buffer variants |
| `src/connect_socket.rs` | 90 | `connect_socket.rs` | 73 | Logic preserved; `tokio::net` → `compio::net`, `tokio::time::timeout` → `compio::time::timeout`; `socket2::SockRef` keepalive unchanged |

`lib.rs` updated: three inline stub modules (`codec`, `socket`, `tls`) removed, replaced by real `mod` declarations. New declarations added for `buf_stream`, `codec`, `connect_socket`, `connect_tls`, `maybe_tls_stream`, `socket`, `tls`. An `Addr` enum was added to the Phase-1 `client` stub so `connect_socket` can reference the shape Phase 4's `client.rs` will fill in.

## Surprises and compromises

- **`tokio_util::codec` has no direct replacement.** I replaced it with free functions (`write_frontend`, `read_backend`) over the in-house `BufStream`. This inverts control flow a little: rather than calling `framed.next()` and letting the codec pull from an internal buffer, the caller explicitly drives `read_backend` which fills and parses in one call. Semantically identical (same `BackendMessage::{Normal, Async}` enum, same `BackendMessages` iterator shape) — Phase 3's `connection.rs` should translate from upstream with only cosmetic differences.

- **BufStream had to become generic.** The `crates/pg/src/stream.rs` version used a concrete `enum StreamInner { Tcp, Tls }`. For the new crate, TLS sits inside `MaybeTlsStream<Socket, T>` and the whole thing needs to flow through `BufStream`, so I made `BufStream<S>` generic over any `AsyncRead + AsyncWrite`. The 64 MB length cap and peek-then-validate flow are preserved byte-for-byte.

- **`Error` enum shape differs from the old `pg` crate.** Old crate had `Error::{Protocol(String), Io(io::Error)}` variants. New crate's `Error` is opaque with factory methods (`Error::io`, `Error::parse`, `Error::tls`, etc.). I wrapped oversize-message violations in `Error::io(io::Error::new(InvalidData, ...))` rather than introduce a `Protocol(String)` variant — matches how tokio-postgres reports malformed framing.

- **No `Result` alias.** I used `std::result::Result<T, Error>` explicitly throughout, as the spec suggested. Phase 3+ can introduce one if the verbosity hurts, but it wasn't needed here.

- **`connect_tls.rs` uses owned-buffer I/O.** Tokio's `AsyncReadExt::read_exact(&mut [u8])` doesn't exist in compio; compio's `read_exact` takes an owned `IoBufMut`. Functionally equivalent, just `Vec<u8>` rather than `[u8; 1]` for the single-byte negotiation response.

- **`compio::time::timeout` returns `Result<T, Elapsed>` (same shape as tokio).** `connect_socket.rs` handles the three cases identically.

## Phase 3 hand-off

For `connection.rs`, the codec layer exposes three things:

1. **`FrontendMessage::{Raw(Bytes), CopyData(...)}`** — same discriminators as tokio-postgres. Phase 3's request dispatch builds these and sends them to the connection loop via `futures_channel::mpsc`.

2. **`pub async fn write_frontend<S>(stream: &mut BufStream<S>, msg: FrontendMessage)`** — call once per message, then `stream.flush().await?` to drain the accumulated writes to the socket in a single syscall. Matches the "batch then flush" cadence tokio-postgres achieves via `SinkExt::send_all` + `Sink::poll_flush`.

3. **`pub async fn read_backend<S>(stream: &mut BufStream<S>) -> Result<BackendMessage, Error>`** — the read half of the select loop. Returns either a `Normal { messages, request_complete }` batch (to be routed to the head of `VecDeque<Response>`) or a single `Async(Message)` (to be routed to the dedicated `AsyncMessage` channel on `InnerClient`). Iteration over `BackendMessages` is via `FallibleIterator::next()`.

Connection's main loop is something like:

```rust
loop {
    futures_util::select! {
        req = receiver.next() => {
            // write + buffer + flush in batch
        }
        msg = read_backend(&mut stream).fuse() => {
            match msg? {
                BackendMessage::Normal { messages, request_complete } => { ... }
                BackendMessage::Async(m) => { route to async channel }
            }
        }
    }
}
```

The length cap (`BufStream::validate_length`) fires inside `read_backend` before any payload buffering, so a malicious or misbehaving server can't OOM the connection task.

For `connect.rs`, `connect_socket.rs` gives you `Socket`, `connect_tls.rs` gives you `MaybeTlsStream<Socket, T::Stream>`. Wrap the result in `BufStream::new(stream)` and pass it to `connect_raw.rs` for the SASL/MD5/password handshake.

## Verification

```
$ cargo check -p compio-postgres
    Finished `dev` profile [unoptimized + debuginfo] target(s)

$ cargo check -p compio-postgres --features tls
    Finished `dev` profile [unoptimized + debuginfo] target(s)

$ cargo check --workspace --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

No errors. No warnings from compio-postgres (the workspace has one pre-existing warning in `crates/runtime/src/runtime.rs:435` unrelated to this port).
