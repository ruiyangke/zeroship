# node:net + node:tls for the zeroship V8 runtime — design proposal

**Status:** proposal (untracked draft, not committed)
**Date:** 2026-06-27
**Scope:** add `node:net` + `node:tls` to `crates/runtime` (zeroship-runtime) so raw-TCP npm packages — crucially the pure-JS DB drivers `pg`, `mysql2`, `ioredis` — run inside the V8 runtime, **without** introducing tokio/libuv. This unlocks a JS-driver transport for the `zeroship-migrate` engine (live MySQL via `mysql2`, no bespoke `compio-mysql`).

**Revision note (post-critique):** this draft was hardened against an adversarial review. The load-bearing changes from that pass: (S1) the outbound writer is **bounded with a hard destroy-the-socket ceiling**, not an advisory counter; (S2) `net_egress_bytes` is wired into the **gateway spend-limit Degrade/Block enforcement path**, plus a per-socket/per-app hard egress ceiling independent of billing; (S3) the allowlist is stated honestly as a **compromised-dependency** control (not a malicious-creator one), entries are **operator-reviewed** not creator-self-serve, and shared-infra-fronting wildcards are forbidden; (S4) a **capability-kind check** is a designed enforcement layer in §3, not just a Phase-D assertion; (S5) **`Trusted` requires `ca`-pinning, never blanket no-verify** — verify-disable is `ZEROSHIP_DEV`-only; (S6) **off-thread / timeout-bounded DNS** for the net path so a slow authoritative NS can't stall the compio loop; (S7) a **process-wide socket ceiling** above the per-app cap. The headline acceptance bar is demoted to **Phase C (`pg` returns real rows)** since the `zeroship-migrate` crates do not yet exist in main (C2); Phase E is marked blocked-on-crate-existence.

---

## 0. Thesis + governing principles

`node:net.Socket` is the native WebSocket client **with the frame codec removed**: a long-lived compio task reads bytes off a `compio::net::TcpStream` / `compio_tls::TlsStream` and pumps them into V8 as events, with a **bounded** channel-fed writer, real backpressure, and a one-shot wakeup into the pump loop. All of that already exists and drives the WebSocket client (`crates/runtime/src/web/websocket/network.rs`, `dispatch.rs`, `handshake.rs`). We **reuse it wholesale**; raw bytes become `'data'` events instead of decoded frames.

Three non-negotiable principles:

1. **Zero tokio.** Every byte stays on compio/io_uring. No new event-loop concept, no libuv. The TLS connector, SSRF resolver, timer facility, writer channel, and backpressure poll_fn are reused verbatim from the WS/fetch path. A CI grep gate over `crates/runtime/src/node/{net,tls}` rejects any `tokio`/`libuv` reference.
2. **Reuse existing TLS + SSRF, do not reinvent.** TLS is `compio_tls::{TlsConnector, TlsStream}` + rustls(aws_lc_rs), built by `build_tls_connector` (`handshake.rs:346`). SSRF is `transport/ssrf.rs` (`resolve_and_check_ssrf`/`is_blocked_ip`). node:net routes **every** outbound connect through the SSRF gate exactly as fetch/WebSocket do.
3. **Minimal viable subset.** Implement the closure of what `new Client(...).connect()` touches for `pg`/`mysql2`/`ioredis` — not all of Node net/tls. Inbound servers, Unix sockets, full `stream.Duplex`/`pipe()`, ALPN, session resumption are out or deferred.

The non-negotiable security invariant: **SSRF is unconditional on every tier; the capability only narrows public reach further. Default is no net. The trusted tier is a property of who *built* the runtime, never something app JS can request. TLS fails secure (verify-on, `ca`-pinning not verify-disable). Outbound volume is bounded and stoppable (hard write ceiling + spend-enforced egress), not merely metered.**

**Security-boundary honesty (set the expectations up front):**
- The **SSRF gate, the process-wide + per-app socket caps, the hard write/egress ceilings, and the module-resolution gate** are *enforcement* — they hold against a fully malicious creator running arbitrary JS in the isolate, because they live in trusted Rust below the V8 boundary and cannot be reached by app code.
- The **host:port allowlist** is a *blast-radius* control. It defends against a **compromised/leaked dependency** inside an otherwise-honest app (the dep can only reach the creator's declared DB host). It does **not** defend against a **malicious creator**, who can simply declare their own exfil host as an allowlist entry. The control against a malicious creator is *egress attribution + spend enforcement + abuse response*, not the allowlist. This distinction is reflected in the threat table below — do not over-read the allowlist.
- The **capability-kind check** (`query`/`mutation` may not open sockets) is *defense-in-depth* layered on the already-typed TS surface, not the sandbox boundary. `NetPolicy` is the boundary. See `rpc/capability.rs:44-50` ("not a sandbox boundary").

---

## 1. API scope (DB-driver-sufficient subset)

Traced against the connection layer of node-postgres, mysql2, and ioredis. The union is small: DB drivers use `net.Socket` as a **raw byte Duplex** (they attach `'data'`/`'error'`/`'close'`/`'end'` and parse bytes themselves; none call `setEncoding`), and **all three upgrade an existing TCP socket to TLS** (STARTTLS: connect plaintext, exchange one byte, then wrap). ioredis additionally does direct `tls.connect({host,port})` for `rediss://`.

### node:net — IN SCOPE (v1)

- **Construction/factories:** `new net.Socket([opts])`, `net.createConnection(opts[,cb])`, `net.connect(opts[,cb])` (alias), `socket.connect(opts[,cb])` **and** `socket.connect(port[,host][,cb])` (pg's positional form). All normalize to `{port,host}` in the `.js` shim; optional `cb` registers a one-shot `'connect'` listener.
- **Write/lifecycle:** `socket.write(data[,enc][,cb])` → returns **boolean** backpressure signal (mysql2 checks it); `socket.end([data][,cb])` (graceful half-close); `socket.destroy([err])` (immediate teardown); `socket.pause()`/`socket.resume()` (flow control mapped onto the existing recv-backpressure waker).
- **Events:** `'connect'`, `'ready'` (emit right after connect), `'data'` (raw Buffer), `'error'`, `'end'`, `'close'(hadError)`, `'drain'`.
- **Tuning (return `this`):** `setNoDelay([on])` (→ `TcpStream::set_nodelay`, already used at `handshake.rs:252`), `setKeepAlive([on][,ms])`, `setTimeout(ms[,cb])` + `'timeout'` (JS-side idle timer, does **not** auto-destroy — Node semantics).
- **Properties:** `connecting`, `destroyed`, `pending`, `readyState`.
- **Static:** `net.isIP/isIPv4/isIPv6` (pure JS, no I/O).

### node:net — DEFERRED

Diagnostic props (`remoteAddress`/`remotePort` cheap — provide early from the resolved `SocketAddr`; `bytesRead`/`bytesWritten`/`localAddress` later); `ref()`/`unref()` (no-ops returning `this`); **Unix-domain sockets** (`{path}` throws `ERR_NOT_IMPLEMENTED` — bypasses IP SSRF gate, remote managed DBs don't need it); `'lookup'`, `setEncoding`, `address()`, `allowHalfOpen` toggling.

**"Deferred" must mean present-but-inert, never absent (C1 — graceful degrade, not crash).** A method that a driver calls but we haven't truly implemented must be a **no-op that preserves the contract**, not a missing property — a missing method is a `TypeError` that crashes the driver mid-connection, which is *worse* than a degraded behavior. Concretely, the `.js` shim **must** define, as inert-but-present:
- `cork()` / `uncork()` → no-ops (write coalescing is an optimization; mysql2/pg correctness does not depend on it). Present so `Writable.prototype` calls don't throw.
- `ref()` / `unref()` → return `this` (single compio loop; nothing to ref).
- `setEncoding(enc)` → records the encoding and (for the DB-driver subset) still delivers `Buffer`; drivers don't call it, but `Readable.prototype` paths may.
- `process.nextTick` / `setImmediate` / `queueMicrotask` — these are **not** net surface but are **load-bearing for `pg`/`mysql2` callback ordering** and MUST exist with correct-enough semantics (see §4 "node-compat completeness"). Verifying they are present and ordered is an explicit Phase-A/C task, not an assumption.

### node:net — OUT

`net.Server`/`createServer`/`listen`/inbound (gateway owns inbound); `net.BlockList`/`SocketAddress`; IPC/fd-passing; named pipes/Windows.

### node:tls — IN SCOPE

- **`tls.connect(options[,cb])` in two modes, both required:**
  1. **Socket upgrade** (`options.socket` = a connected `net.Socket`): STARTTLS. Take the underlying `TcpStream` out of the existing socket's native state, feed to `connector.connect(&servername, tcp)` (`handshake.rs:267`). This is the load-bearing requirement; maps onto `EstablishedStream::{Plain,Tls}` (`handshake.rs:64`) going `Tls` mid-life.
  2. **Direct TLS** (`options.{host,port}`, no socket): ioredis `rediss://`. Identical to the `wss://` branch.
  - `cb` registers a one-shot `'secureConnect'` listener.
- **Options honored:** `servername` (SNI — required by managed DBs; already threaded at `handshake.rs:269`); `rejectUnauthorized` (default **true**); `ca` (custom roots, seed a per-connection `RootCertStore`); `cert`/`key`/`passphrase` (mTLS via `with_client_auth_cert`, second-priority).
- **`TLSSocket`** is-a `net.Socket` — exposes the entire net.Socket surface over the TLS stream (reuse the single-task `run_tls_driver`, `network.rs:550`).
- **Events:** `'secureConnect'` + inherited net events.
- **Properties:** `encrypted === true`; `authorized`/`authorizationError` (from the rustls verify result).

### node:tls — DEFERRED / OUT

Deferred: `getPeerCertificate([detailed])` (only needed for cert pinning — most likely first follow-up), `getProtocol`/`getCipher`/`getPeerX509Certificate`/`exportKeyingMaterial`, ALPN, session resumption, standalone `SecureContext`, custom `checkServerIdentity` override. Out: `tls.createServer`/`TLSServer`, `createSecurePair`, PSK/SNICallback/ticket keys.

**Net effect:** ~1–2 host classes (`Socket`; `TLSSocket` can be the same class with a TLS-mode flag since `EstablishedStream` already unifies plain/TLS), two `.js` shims layering EventEmitter/Duplex over native events, and two new arms in `resolve_native`/`is_native`.

---

## 2. Native architecture (#[v8_class] host objects + EventEmitter shim)

### Design stance — generalize the WebSocket driver

| WebSocket machinery | node:net reuse |
| --- | --- |
| `NativeWsState` table keyed by `u32`, `native_websockets: HashMap` (`network.rs:211`) | `NativeSocketState` table keyed by `u32` (`native_sockets`) |
| `push_event` → `spawned_ops.push(one-shot OpResult)` → `pump_notify_tx.try_send(())` (`network.rs:276-287`) | identical, with `OpResult::SocketEvent { socket_id }` |
| pump arm `OpResult::WebSocketEvent` → `dispatch_pending_ws_events` (`runtime.rs:2298`, `dispatch.rs:22`) | `OpResult::SocketEvent` → `dispatch_pending_socket_events` |
| `await_recv_drain` / `RECV_BACKPRESSURE_CAP` (`network.rs:196,289`) | identical inbound backpressure (`pause`/`resume`) |
| `buffered_amount`+`full`+`decrement_buffered_amount` (`network.rs:1008`) | outbound `bufferedAmount` → `write()` bool + `'drain'` |
| `build_tls_connector` + `connector.connect(&host,tcp)` (`handshake.rs:346,267`) | generalized for SNI/`rejectUnauthorized`/`ca` |
| `validate_url` + `resolve_and_check_ssrf` (`handshake.rs:229`, `ssrf.rs:78,141`) | reused at connect, behind a `NetPolicy` |
| synthetic-module mint (`path/mod.rs:35`, `crypto/module.rs:18`, `native_modules.rs:22`) | add `node:net`/`node:tls`/`node:events` arms |

**Driver shape, picked as the WS code does:**
- **Plain TCP** → the **two-task read/write split** (`run_plain_driver`, `network.rs:491`) over `Rc<TcpStream>`. io_uring multiplexes concurrent submissions on one fd → true full-duplex, which pg/mysql2 pipelining and ioredis auto-pipelining rely on.
- **TLS** → the **single-task cooperative `select`** (`run_tls_driver`, `network.rs:550`) because `TlsStream` is not aliasable. This is also the shape used for the STARTTLS upgrade, since it needs sole ownership of the `TcpStream` at the upgrade instant.

### File layout

```
crates/runtime/src/node/
├── events/  mod.rs (synthetic node:events) + events.js (minimal EventEmitter)
├── net/     mod.rs (synthetic node:net) · socket.rs (#[v8_class] NativeSocket + factories)
│            driver.rs (compio connect + select loop) · state.rs (NativeSocketState + SocketEvent)
│            dispatch.rs (V8 emit arm) · net.js (EventEmitter/Duplex Socket facade)
└── tls/     mod.rs (synthetic node:tls) · upgrade.rs (generalized build_tls_connector + upgrade op)
             tls.js (TLSSocket extends net.Socket; tls.connect)
```

Wiring (one-line edits): `node/mod.rs` add `pub mod {events,net,tls}`; `core/native_modules.rs:26-51` add three arms in `resolve_native` + `is_native`; `core/state.rs` add `native_sockets`/`next_native_socket_id`/`native_socket_wrappers`/`net_policy` + `OpResult::SocketEvent`; `core/runtime.rs:2298` add the pump arm. Feature-gate the whole thing behind `runtime_node_net` (mirroring the `runtime_native_websocket` cfg).

### The native I/O object

`#[v8_class] NativeSocket` (the `Cipher` host-object pattern, `crypto/cipher.rs:105`) is a **thin handle** holding `socket_id: u32` in internal field 0; the stream and channels live in `NativeSocketState` looked up by id (exactly like WS). `write` is a plain `#[v8_method]` (sync — Node's `write()` returns a boolean), `&self` + a `mpsc` channel to the driver, sidestepping the `&mut self`-across-`.await` rejection in the macro (`runtime-macros/src/lib.rs:111-115`). The only async work is the inbound pump.

```rust
pub struct NativeSocketState {
    events: VecDeque<SocketEvent>,            // pumped to V8
    recv_backpressure_waker: Option<Waker>,
    paused: bool,                             // socket.pause()/resume()
    // Writer channel is BOUNDED. The bound is the soft high-water mark
    // (advisory backpressure); the HARD ceiling below is independent and
    // is enforced by `write()` itself, not by channel capacity.
    write_tx: Option<mpsc::Sender<WriteCmd>>,
    control_tx: Option<mpsc::UnboundedSender<Control>>, // control is tiny + rare, ok unbounded
    buffered_amount: Rc<Cell<u64>>,           // outbound bufferedAmount (bytes queued, not yet flushed)
    high_water: Rc<Cell<bool>>,               // write() false → emit 'drain' on clear
    egress_total: Rc<Cell<u64>>,              // lifetime bytes accepted for send (per-socket egress ceiling)
    remote: Option<SocketAddr>,
}
enum SocketEvent { Connect, Data(Vec<u8>), Drain, End, Error{message,code}, Close{had_error}, WriteComplete{seq} }
enum WriteCmd { Data(Vec<u8>), End }
enum Control { Destroy, SetNoDelay(bool), SetKeepAlive(bool,u64), UpgradeTls(TlsUpgradeParams) }
```

**S1 — the writer is bounded and `write()` enforces a hard ceiling (must-fix; highest severity).** Node's `write()` returning `false` is *advice* the app may ignore; with an unbounded channel a malicious or buggy app that ignores the boolean keeps calling `write()` and `buffered_amount` becomes a number that climbs while the channel eats unbounded heap — on a worker hosting many isolates per thread, one app OOMs the whole process. Therefore `write()` is **enforcement, not accounting**:
1. `write()` adds `data.len()` to `buffered_amount` and to `egress_total`, then checks two ceilings.
2. If `buffered_amount` would exceed `OUTBOUND_HARD_CAP` (a hard per-socket byte ceiling, distinct from and well above the WS-style high-water mark), `write()` does **not** queue the chunk — it **destroys the socket** with `ERR_STREAM_WRITE_AFTER_END`-class error (emits `'error'`+`'close'`) and returns. The app cannot grow heap past the cap by ignoring backpressure.
3. If `egress_total` would exceed the per-socket / per-app egress ceiling (§3 resource limits), same destroy-the-socket outcome.
4. Below the hard cap, `write()` returns the **boolean** high-water signal (`buffered_amount >= HIGH_WATER`) as advisory backpressure; the driver decrements `buffered_amount` as bytes actually flush to the kernel and emits `'drain'` when it crosses back below the mark.

The bounded `write_tx` is a second line of defense: even if accounting drifted, the channel's own capacity bounds in-flight heap; the driver drains it FIFO. The contradiction the critic flagged (`UnboundedSender` in one place, "bounded like WS" in another) is resolved decisively in favor of **bounded + hard-cap-destroy**.

### Async → JS bridge (the core)

The driver task calls `push_socket_event(state, id, ev)` — exact transcription of `push_event_pub` (`network.rs:249-287`): append to `events`, push `Box::pin(async { OpResult::SocketEvent { socket_id } })` onto `spawned_ops`, nudge `pump_notify_tx`. The V8-thread pump matches the new arm next to `OpResult::WebSocketEvent` and calls `dispatch_pending_socket_events`, which **drains the whole queue in one V8 turn (FIFO)**, then runs one `perform_microtask_checkpoint()` — the Node-correct "deliver all ready `'data'` chunks this tick, then microtasks run once" shape that lets a driver resolve a query promise after the synchronous burst.

Events are emitted through **EventEmitter, not EventTarget**: `dispatch_one` reads the JS `emit`/`__zsEmit` off the wrapper and `Function::call`s it with a `Buffer` (via the existing `node/buffer` emitter, `crypto/cipher.rs:191`). The `.js` shim provides the EventEmitter/Duplex prototype; Rust only needs `emit`/`write`/`end`/`pause`/`resume`/`setTimeout`/`destroy`.

### JS shim (EventEmitter facade)

There is no native `EventEmitter` today, so we add a minimal `node:events`. Because synthetic modules have no imports, the handoff (matching `path.js` IIFE) is: `events.js` returns `{EventEmitter,default}` and stashes `globalThis.__zsEventEmitter`; `net.js` builds `class Socket extends globalThis.__zsEventEmitter`, returns the namespace, and stashes `globalThis.__zsNetSocket` so `tls.js` can `class TLSSocket extends globalThis.__zsNetSocket`. The `Socket` is Duplex-shaped *enough* for the three drivers (raw `'data'`/`.write`/`.end`); full `stream.Readable`/`pipe()` is out of scope — none of the drivers route the socket through `.pipe()`.

### TLS upgrade path (the one genuinely new mechanism)

`tls.connect({socket})` calls native `__zsTlsUpgrade(netSocket, params)` which sends `Control::UpgradeTls` to the existing net driver task and awaits a oneshot. The driver loop (sole owner of the `TcpStream`) handles it at the top of its `select`, **asserts its inbound `read_buffer` is empty** (STARTTLS guarantees the server sends nothing between `'S'` and ServerHello; if non-empty → fail `ERR_TLS_HANDSHAKE` rather than corrupt the record stream, since `compio_tls::TlsConnector::connect` takes the `TcpStream` by value with no leftover-prepend hook), moves the owned `TcpStream` into `build_tls_connector_with(params).connect(&servername, tcp).await`, and **continues the same loop** over `TlsStream<TcpStream>` keeping the same `socket_id`/channels/queue — seamless from JS. `build_tls_connector_with` generalizes the WS connector (`handshake.rs:346`) to thread node-tls options (extra `ca`, `rejectUnauthorized` → custom no-verify verifier, optional client-auth cert) into the same rustls(aws_lc_rs) builder.

---

## 3. Security gating (the key fork — prominent)

A `node:net` socket is **strictly more powerful than `fetch`**: arbitrary protocol, arbitrary port, no scheme allowlist, no CORS, no redirect re-validation. The model is off-by-default, non-bypassably SSRF-gated, allowlist-scoped per app, resource-bounded (per-app **and** process-wide), egress-enforced (not merely metered), with a distinct trusted tier for migrate.

### Threat model

Each row marks whether the control is **enforcement** (stops the attack in trusted Rust) or **blast-radius / attribution** (limits or attributes, but a malicious creator can work within it). Read the qualifiers — the previous draft overclaimed the allowlist.

| Threat | Vector | Control | Kind |
| --- | --- | --- | --- |
| Internal port scan / lateral movement | `net.connect({host:'10.0.0.5',port:6379})`; metadata `169.254.169.254`, CGNAT `100.64/10` | SSRF gate (`is_blocked_ip`, `ssrf.rs:40`) — **always on, every tier**, no capability disables it; hand validated `SocketAddr` straight to `connect` | **Enforcement** |
| DNS rebinding | public host → RFC1918 between check and connect | `resolve_and_check_ssrf` returns a pre-validated `SocketAddr`; connect to that exact address, never re-resolve (`handshake.rs:236-249`) | **Enforcement** |
| Slow-DNS loop stall (S6) | allowlisted host points at a slow/timing-out authoritative NS; driver reconnects aggressively | DNS runs **off the V8 thread on a spawned compio task** (it already does for WS/fetch) **with a hard resolve timeout**; a stalled resolve fails that one connect, never the loop | **Enforcement** |
| Exfiltration over non-HTTP channel | raw TCP to attacker host:port | vs **leaked dependency**: host:port allowlist confines reach to the declared DB host. vs **malicious creator**: NOT prevented (creator declares own host) → **egress is spend-enforced** (Degrade/Block) + per-socket/app egress ceiling + attribution | allowlist = blast-radius; **egress ceiling + spend = enforcement** |
| Platform-as-proxy / amplification (spam, SMTP, port-25, domain-fronted tunnel over :443) | use fleet egress IPs to attack third parties | concurrent-socket cap (per-app **and process-wide**) + egress ceiling + **operator-reviewed** allowlist forbidding shared-infra-fronting wildcards + abuse response on attribution | concurrency/egress = **enforcement**; allowlist authoring = **policy boundary** |
| Shared-worker OOM via ignored backpressure (S1) | ignore `write()===false`, keep writing | bounded writer channel + **hard `OUTBOUND_HARD_CAP` that destroys the socket** | **Enforcement** |
| FD / connection-storm exhaustion of the shared process (S7) | N isolates × per-app `max_sockets` → fd exhaustion | per-app `max_sockets` **and** a process-wide `GLOBAL_MAX_SOCKETS` ceiling; connect refused past either | **Enforcement** |
| Resource exhaustion (slowloris) | open many sockets, never read | per-app socket cap, connect timeout, idle timeout, bounded read buffer / `RECV_BACKPRESSURE_CAP` | **Enforcement** |
| TLS MITM / downgrade (S5) | `rejectUnauthorized:false` to accept a forged cert on the admin-cred channel | verify-on by default; `ca`-pinning is the custom-CA path; **verify-disable denied for `Allowlist` AND `Trusted`** — permitted only under `ZEROSHIP_DEV` | **Enforcement** |
| `query`/`mutation` handler opens a socket (S4) | read-only/transactional procedure tries raw TCP | connect native op consults `current_kind()`; refuses with `capability_violation` (defense-in-depth over TS types) | Defense-in-depth |

### Recommendation: **Option A** — off by default + per-app capability grant + host:port allowlist; separate trusted tier for migrate

Rejected alternatives: **Option B (on for all, SSRF + port-restricted)** — a static port allowlist still lets every app open raw TCP to any public host (443 tunnels anything); turnkey spam/proxy/exfil channel. **Option C (trusted-only)** as the *sole* policy — forecloses the legitimate creator use case (a creator app talking to its *own* managed DB at a public host). C is correct only for the migrate authoring sandbox.

```rust
// crates/runtime/src/transport/net_policy.rs (new)
pub enum NetPolicy {
    Denied,                                       // default for every creator app: modules don't resolve
    Allowlist {                                   // creator app; SSRF still on top
        entries: Vec<HostPort>,
        max_sockets: u32,                         // per-app concurrent-socket cap
        egress_ceiling_bytes: u64,                // per-app hard egress ceiling (independent of billing)
    },
    Trusted { max_sockets: u32 },                 // platform/migrate executor; SSRF still on; no host allowlist
}
pub struct HostPort {
    host: HostPattern, // exact, or a single leading-wildcard "*.neon.tech".
                       // FORBIDDEN: bare "*"; wildcards that front shared infra
                       // (e.g. "*.herokuapp.com", "*.cloudfront.net",
                       // "*.r2.dev", "*.amazonaws.com") — these are a
                       // domain-fronted tunnel to anywhere. Operator-curated
                       // deny-list of frontable suffixes is checked at
                       // allowlist-authoring time, not connect time.
    port: u16,         // exact, no ranges
}
```

- **Default `Denied`** — fail-closed, matching the SEC-5 posture (commit `e919af51`). Free tier and most apps never touch raw TCP.
- **Grant is data**, set by the control plane on the app/deploy record, threaded via a new `RuntimeBuilder::net_policy(...)` (sibling of `app_id`/`plugins` at `core/runtime.rs:483-492`) into `RuntimeState`.
- **Who authors allowlist entries — the real security boundary (S3).** Allowlist entries are **operator-reviewed, not creator-self-serve at will.** The control plane records the entries, but their *admission* is gated: (a) a creator-facing flow may *propose* a host:port for their own managed DB, but (b) entries are validated against the operator-curated frontable-suffix deny-list and the bare-`*`/broad-wildcard ban **server-side, in the control plane, before they ever reach a deploy record**, and (c) anything that fails the automatic checks (or any entry on a sensitive port like 25/465/587 SMTP) requires operator sign-off. The runtime treats the policy as already-vetted data; the *authoring* gate is where malicious-creator wildcards are stopped. This is stated plainly because the allowlist alone does **not** stop a malicious creator (see below).
- **Allowlist over bare capability — but be honest about what it buys:** a capability with no scope *is* Option B. The allowlist turns "this app can use its database" into "this app can reach *only* its declared DB host". That is a real, valuable control **against a compromised/leaked dependency** — a malicious transitive dep inside an honest app reaches only the creator's DB. It is **not** a control against a **malicious creator**, who lists their own exfil host. Against the malicious creator the operative controls are: (i) egress is **spend-enforced** and hard-capped (§ resource limits), (ii) all net bytes are **attributed** to `app_id` for abuse response, and (iii) authoring-time review of the entries themselves. The threat table marks each row accordingly; do not present the allowlist as exfil *prevention*.

### Enforcement — four independent layers in trusted Rust, any one fails closed

Every layer is below the V8 boundary, so none is reachable by app JS. The order is "cheapest + earliest first".

0. **Capability-kind check at `connect()` (S4 — designed here, not just tested).** The native connect op calls `rpc::capability::current_kind()` (`rpc/capability.rs:124`) **first**. If it returns `Some(Query)` or `Some(Mutation)`, the op refuses with the `capability_violation` envelope (`build_capability_violation`, `rpc/capability.rs:293`) — the same rail by which fetch refuses under `Mutation` and plugin-db writes refuse under `Query`. Only `Action`/`Stream`/`Subscription` (and the trusted transport, which runs with no kind frame) may open sockets. **Caveat made explicit:** `CURRENT_KIND` is documented as *defense-in-depth, not a sandbox boundary* (`rpc/capability.rs:44-50`) — it relies on `__zsDispatch` calling `__zsEnterKind`. The actual boundary that stops a hostile isolate is `NetPolicy` + SSRF + the caps; the kind-check is there so a read-only procedure compiled without strict TS still can't casually open a socket. The design does **not** lean on it as the security boundary.
1. **Module resolution** — `resolve_native`/`is_native` gain the net/tls arms **only when `NetPolicy != Denied`**. Under `Denied`, `import 'node:net'` throws `ERR_MODULE_NOT_FOUND` — the sandbox cannot even name the surface.
2. **Allowlist check at `connect()`** — synchronous, on the V8 thread, before any task spawns or DNS runs; matches `(host,port)` against the allowlist (`Trusted` skips host-match but still counts toward caps). Also re-checks the per-app `max_sockets` and process-wide `GLOBAL_MAX_SOCKETS` here. Reject → throw synchronously (`EMFILE`-class for caps, allowlist-miss error for host).
3. **SSRF gate at the connect task** — **unconditional, every tier including `Trusted`**: `resolve_and_check_ssrf(host,port)` (`ssrf.rs:141`) → hand the validated `SocketAddr` straight to `TcpStream::connect`, exactly as the WS handshake does (`handshake.rs:229-249`). **S6 hardening:** `resolve_and_check_ssrf` uses synchronous `std::net::ToSocketAddrs` (`ssrf.rs:152`). WS/fetch tolerate this once-per-request, but DB drivers reconnect/pool aggressively against attacker-influenceable hostnames, so the net path **must** run resolution (a) off the V8 thread on the spawned connect task (it already is — the task, not the pump, calls it) **and** (b) under a **hard resolve timeout** (a bounded `compio::time::timeout` wrapping the resolve, or a small off-loop resolver pool) so a slow authoritative NS fails that one connect instead of stalling the single-threaded compio loop and every co-resident app. This timeout is a net-path addition; it does not change fetch/WS behavior.

**Dev caveat (must be explicit):** `is_blocked_ip` is bypassed under `ZEROSHIP_DEV` (`ssrf.rs:144`) so dev can reach `localhost` Postgres — same relaxation net inherits as fetch/WS. `ZEROSHIP_DEV` is never set in the worker/prod vector.

### The migrate split

- **Migrate authoring sandbox** (the former in-tree JS authoring adapter, "no ambient Node, no fs, no net") → `NetPolicy::Denied`; net/tls modules simply not registered. The AI-authored migration program manipulates IR + emits a plan; it must never open a socket.
- **Migrate executor** (trusted context that applies the plan against the live DB, where `pg`/`mysql2` run) → `NetPolicy::Trusted`, or preferably `Allowlist([db_host:db_port])` since the executor knows its one destination. Net is a property of *context construction* in trusted Rust; a creator app can never become `Trusted`.

### TLS defaults

Reuse `build_tls_connector` (rustls + aws_lc_rs + native roots) — full chain + hostname verification = `rejectUnauthorized:true`, the default. Refinements:
- **`ca`** — per-connection `RootCertStore` from supplied PEM. This is the *correct* way to reach a custom-CA managed DB: you **pin the CA**, you do **not** disable verification. Available to `Allowlist` and `Trusted`.
- **`rejectUnauthorized:false` (verify-disable) — `ZEROSHIP_DEV`-only (S5).** The previous draft permitted it under `Trusted`. That is the worst place to allow it: the migrate executor (`Trusted`) carries **admin DB creds and applies schema mutations** — the single connection where a MITM is most catastrophic. So:
  - `Allowlist` apps: **denied** (throws `ERR_TLS_REJECT_UNAUTHORIZED_DISABLED`).
  - `Trusted`: **denied** — `Trusted` must reach a custom-CA DB via **`ca`-pinning**, never blanket no-verify. A `Trusted` context with a self-signed DB cert supplies the cert as `ca`.
  - Permitted **only** under `ZEROSHIP_DEV` (never set in worker/prod), for local self-signed dev DBs.
- **`servername`/SNI** settable independent of host but never relaxes verification; required by managed DBs (`handshake.rs:269`).

### Resource limits (per app AND process-wide)

- **Max concurrent sockets, per app** — counter on `RuntimeState` modeled on `in_flight_fetches` (`state.rs:451`); `connect()` checks against `max_sockets`, throws `EMFILE`-style; decrement on close.
- **Process-wide socket ceiling (S7).** A worker holds **many isolates per thread** under LRU; per-app caps alone allow N apps × `max_sockets` → fd exhaustion / connection storms on the shared process. Add a **`GLOBAL_MAX_SOCKETS`** ceiling — a process-global atomic counter (shared across all isolates/threads, alongside the worker's isolate cache) checked at `connect()` in addition to the per-app cap. Past the global ceiling, `connect()` refuses regardless of per-app headroom. This is the missing global ceiling the critic flagged.
- **Outbound hard cap (S1)** — `OUTBOUND_HARD_CAP` per socket; `write()` past it destroys the socket (see §2). This bounds heap, independent of the advisory high-water mark.
- **Connect timeout** — reuse `HandshakeOptions.connect_timeout` (`handshake.rs:181`, 30s). Plus the S6 **DNS resolve timeout** on the net path.
- **Idle timeout** — wall-clock ticker (analogous to idle-GC, `runtime.rs:174`) force-closing stale sockets.
- **Bounded read buffer / backpressure** — `RECV_BACKPRESSURE_CAP`/`await_recv_drain` (`network.rs:196,289`); outbound bounded writer channel (§2).

### Egress: enforced, not just metered (S2 — must-fix)

The previous draft emitted `net_egress_bytes` into the `Meter` and called the allowlist + metering an exfil mitigation. **Metering is a billing signal, not a control** — the gateway's Stream-1 Degrade/Block spend enforcement never sees raw net bytes (they are invisible to the HTTP-layer counters), so a malicious app could proxy/exfil unbounded volume to an allowlisted host, get billed, and never be stopped. Net egress must be **enforced on the same path that already throttles/blocks spend**:

1. **Per-socket + per-app hard egress ceiling, independent of billing.** `egress_total` (per socket) and a per-app aggregate are checked in `write()`; crossing `egress_ceiling_bytes` (on the `Allowlist`/`Trusted` policy) **destroys the socket** and refuses further connects for the app's current dispatch. This is a hard stop that does not depend on the billing pipeline being live.
2. **Wire `net_egress_bytes`/`net_ingress_bytes` into the spend engine.** Emit them into the `Meter` stamped with the server-injected `app_id` (the connect/driver task is the natural emission point), **and** ensure they feed the same spend-limit aggregation the gateway consults for Warn→Degrade→Block (Stream-1, `docs/reference/billing-metering.md`). When an app crosses its spend limit, the gateway's existing Degrade (throttle) → Block (402) path applies to net-bearing dispatches too — so net traffic is subject to the same enforcement as HTTP egress, not just invoiced.
3. **Attribution.** Every byte is attributed to `app_id` for abuse response — the malicious-creator control of last resort, since the allowlist does not prevent a self-declared exfil host.

`net_connect` is also emitted (connection count) for both `Allowlist` and `Trusted`.

---

## 4. Async / event-loop integration (zero-tokio)

Maps onto four existing primitives, all confirmed in-tree: async result → V8 callback (`spawned_ops.push` + `pump_notify_tx`, `network.rs:276`); pump drains + enters V8 once per batch (`pump_loop`, `runtime.rs:1038-1215`); writer decoupled from reader (`futures::channel::mpsc::unbounded` + dedicated task, `network.rs:381,921`); backpressure halts the read (`await_recv_drain` poll_fn woken by V8 drain, `network.rs:289-323`); timers (`spawned_timers` → `compio::time::sleep` → `TimerResult`, `runtime.rs:1989`).

**Read-pump → `'data'`:** `run_reader_loop` (`network.rs:816`) with the framer deleted — `await_recv_drain` (backpressure), `read(chunk).await` (one io_uring read), `n==0` → `End` then `Close`, else `Data(chunk[..n])` via `push_event`. The pump drains the whole queue in one V8 turn then one microtask checkpoint.

**Write path:** **bounded**-channel writer (§2 S1); `write()` first enforces the `OUTBOUND_HARD_CAP` and egress ceilings (destroy-the-socket past either), then returns the boolean from buffered-amount accounting (`decrement_buffered_amount`, `network.rs:1008`); writer posts `WriteComplete{seq}` back through `push_event` so the `cb` fires and `'drain'` emits on the pump turn. **Invariant:** all JS callback/`emit` invocation stays on the pump V8 turn; a compio task never holds the isolate — it only posts work + notifies, and must **never hold a `borrow_mut` on `native_sockets[id]` across an `emit`** (follow the WS `drain_events` collect-then-drop discipline, `network.rs:303-323`).

**EventEmitter ordering vs the promise model — resolved conflicts:**
1. `'connect'` before first `'data'` — free (connect pushed before reader spawns; FIFO queue + FIFO drain).
2. **`pause()` inside a `'data'` handler must suppress remaining queued chunks this tick** — the dispatch loop re-checks `paused` between emits and **re-queues the undelivered remainder at the front** (replays on `resume()`). This is a deliberate deviation from the WS drain-all-at-once loop.
3. `'end'` (EOF) after in-flight `'data'` — FIFO-guaranteed.
4. `'error'` then `'close'`, never reversed; `close` is the last event ever — driver pushes nothing after `Close` (the WS `fail_connection` order, `network.rs:983`).
5. Synchronous re-entrant `emit` — safe (deeper JS on the same entered-isolate stack); the only hazard is RefCell re-borrow, mitigated by short released borrows.

**Backpressure:** `pause()` sets a `Cell<bool>` checked in `await_recv_drain` *before* the `read` call, so it genuinely stops the next kernel read (stronger than libuv `uv_read_stop`). pg `COPY ... TO STDOUT` and mysql2 streaming queries work correctly.

**Timers:** `socket.setTimeout(ms,cb)` rides the existing `spawned_timers`; rolling idle timeout via a `last_activity: Cell<Instant>` bumped on `Data`/`WriteComplete`, re-arming instead of churning per-byte; `'timeout'` does **not** auto-destroy (DB pool keepalive depends on this).

**node-compat completeness — the single biggest risk to "runs a real driver" (C1).** "Duplex-shaped *enough*" is an empirical claim, and the headline proof depends on it. The drivers lean on host surface beyond raw `'data'`/`.write`:
- **`process.nextTick` ordering.** `pg` and `mysql2` sequence callbacks with `process.nextTick`. Our async→JS bridge is "drain all ready chunks this V8 turn, then one `perform_microtask_checkpoint()`". `nextTick` must be implemented and must run **before** the Promise-microtask checkpoint within a turn (Node semantics: nextTick queue drains ahead of microtasks). If `process.nextTick` is mapped naively to `queueMicrotask`, ordering can subtly break driver state machines. This must be verified, not assumed — it is a Phase-A/C exit criterion.
- **`setImmediate`.** Some driver paths use it; map onto the existing timer/`spawned_ops` facility (macrotask after the current turn). Must exist.
- **`cork`/`uncork`** — **present as no-ops** (§1), never absent.
- **`Readable`-based stream path.** If `mysql2`'s streaming-query path drives the socket through a real `Readable`, the "Duplex-shaped enough" subset is insufficient and the shim must provide a minimal `Readable` (push/`'readable'`/`read()`), not just `'data'`. Phase C/E empirically determine whether this path is hit by the pinned driver versions; if hit, it is in-scope, not deferred. The phase plan vendors **pinned** driver trees precisely so this is a fixed, testable target rather than a moving one.

The mitigation is process discipline: vendor pinned `pg`/`mysql2` (Phase C/E), and treat any `TypeError: x is not a function` from the driver as a **missing inert shim** to add, never a deferral.

**Idle long-lived DB connections + LRU eviction — the critical flag (F1).** The pump survives idle correctly (reader parked in a pending io_uring read, pump parked on `notify_rx`; a server push wakes both). But `evict_lru` (`worker/src/cache.rs:391-416`) selects purely by `min last_used` and on eviction only fires in-flight `AbortController`s before dropping the `Runtime` — **silently killing open DB sockets and per-socket compio tasks.** Two failure modes: idle pool eviction (functionally survivable — pools reconnect — but defeats reuse and can thrash DB `max_connections`); **mid-transaction eviction** (BEGIN issued, COMMIT pending → socket dropped → DB rolls back → silent data-correctness surprise for migrate). Required additions:
1. **Activity bumps `last_used`** — wire each `SocketEvent` dispatch + `write` to the same `last_request_ts` signal `handle_async_event` already sets (`runtime.rs:2030`).
2. **Open-socket count in the LRU key** (prefer evicting socket-free isolates) + a **max-socket-age cap** that force-closes truly stale connections — *not* a hard pin (a leaked socket would pin forever).
3. **Graceful FIN-on-evict** — send each open socket a clean `destroy`/FIN alongside the `AbortController` fan-out (orderly close, not RST).
4. **Migrate executor takes an explicit isolate lease** for the migration's duration rather than relying on `last_used`, sidestepping the eviction race entirely (migrations are a bounded job, not under LRU churn).

---

## 5. Phased implementation plan + acceptance bar

Per the repo faithful-e2e rule, the binding proof is an **unmodified npm DB driver connecting to a live database through the real Runtime + compio event loop and returning real rows** — not a socket unit test. Each phase's suite is the exit gate for the next.

**Acceptance-bar honesty (C2).** The `zeroship-migrate*` crates **do not exist in the clean main checkout** (Open Q7). Therefore the *currently gateable* headline deliverable is **Phase C — `pg` returns real rows over node:net through the real Runtime**. Phase E (the migrate-apply capstone, the stated motivation) is **blocked-on-crate-existence** and cannot be the acceptance bar today; it is specified here so it is ready the moment the migrate crates land, but the effort's shippable proof is Phase C + the security suite (Phase D). This corrects the prior draft, which presented the migrate-apply journal row as "the single signal the whole effort delivered its purpose" while admitting the crate doesn't exist.

**Reused test infra:** drive JS through a real Runtime + await async settle (`capability.rs:37-78`); live-DB skip gate (`require_pg_or_skip`, `native_transaction.rs:55`); real server in a thread (`websocket_e2e.rs:17-60`); in-process TCP server (`wpt_fetch_basic_network.rs:99-183`); subset pass-rate asserter (`finish()`, `wpt_fetch_basic_network.rs:613`); compio test driver (`Runtime::new().block_on`). Standardize `PG_TEST_URL` (default `postgres://postgres:zeroship@localhost:5440/zeroship`) + `MYSQL_TEST_URL` (default `:3306`); **skip-is-failure-in-CI** — the gate helper panics instead of returning `None` when `ZEROSHIP_CI=1`, so a misconfigured DB can't green the headline proof by skipping (the direct lesson from the auto-tx bug).

- **Phase A — node:net conformance (`tests/node_net.rs`).** No DB. In-process TCP + blackhole peer. Lifecycle happy path (connect→write→echo→`'data'` Buffer→end→close, ordering asserted); half-close (`'end'` vs `'close'`); error (`ECONNREFUSED` + `hadError`; blackhole → `setTimeout`→`'timeout'`); **backpressure (`write()===false`→`'drain'` contract, load-bearing)**; `setNoDelay`/`setKeepAlive` accepted. Bar: **100% of the curated subset**.
- **Phase B — node:tls conformance (`tests/node_tls.rs`).** In-thread rustls self-signed server. Handshake + app-data roundtrip via **`ca`-pinning** (supply the self-signed cert as `ca` — the recommended custom-CA path, verification stays on); `rejectUnauthorized:true` (default) rejects an unpinned self-signed cert (no `'secureConnect'`); `rejectUnauthorized:false` completes **only when the test sets `ZEROSHIP_DEV`** (and throws otherwise under `Allowlist`/`Trusted` — that negative is asserted in Phase D, S5); SNI/`servername` captured. Bar: **100%**.
- **Phase C — REAL pg E2E (`tests/node_pg_e2e.rs`), the headline proof (gateable today).** Vendored pinned `pg` tree under `tests/fixtures/node_modules/pg/` (WPT pristine-input discipline), loaded via the multi-`ModuleEntry` builder. `new Client(PG_TEST_URL).connect()` → `query("select $1::int as n,'ok' as s",[42])` → assert `rows==[{n:42,s:"ok"}]` (proves startup→auth→extended-protocol→RowDescription/DataRow decode ran over the socket). Plus a `pg.Pool` INSERT+SELECT with a >64KB result set (multi-statement framing + backpressure) and an `ssl:{ca:<pem>}` **`ca`-pinned** variant against TLS-enabled Postgres (proves node:tls carries the same driver via the recommended verify-on path, not verify-disable). Plus a `process.nextTick`/callback-ordering assertion (C1) since `pg` depends on it. Zero-tokio source grep gate.
- **Phase D — security/gating (`tests/node_net_security.rs`), the *enforcement* proof — gateable today.** Every must-fix gets a regression test that fails pre-fix:
  - **SSRF/metadata block** — `169.254.169.254`, `100.64/10` CGNAT, and public-host→RFC1918 (rebind) denied before any bytes; run **without** dev mode.
  - **DNS timeout (S6)** — a hostname pointed at a deliberately slow/blackholed resolver fails *that* connect within the resolve timeout and does **not** stall a second concurrent socket's progress on the same loop.
  - **Module-resolution gate** — sandbox-mode (`Denied`) Runtime `import 'node:net'`→`ERR_MODULE_NOT_FOUND`; `Allowlist`/`Trusted` resolve.
  - **Capability-kind check (S4)** — a `query()` and a `mutation()` handler that call `net.connect(...)` each hit `capability_violation`; an `action()`/trusted-transport handler succeeds.
  - **Allowlist authoring** — control-plane validation rejects bare `*` and frontable-suffix wildcards (`*.herokuapp.com`, `*.cloudfront.net`) and SMTP ports without operator sign-off.
  - **Outbound OOM cap (S1)** — a handler that ignores `write()===false` and floods past `OUTBOUND_HARD_CAP` gets its socket destroyed (`'error'`+`'close'`), heap bounded; the process survives.
  - **Egress ceiling + spend enforcement (S2)** — crossing the per-app `egress_ceiling_bytes` destroys the socket; and `net_egress_bytes` is asserted to land in the spend-aggregation the gateway consults (Degrade/Block), not only the invoice meter.
  - **Process-wide socket ceiling (S7)** — N apps × `max_sockets` past `GLOBAL_MAX_SOCKETS` → connect refused regardless of per-app headroom.
  - **TLS verify (S5)** — bad/self-signed cert rejected at default for `Allowlist` **and** `Trusted`; `rejectUnauthorized:false` throws under `Allowlist` and `Trusted`, succeeds only under `ZEROSHIP_DEV`; `ca`-pinning reaches a self-signed DB without disabling verification.
- **Phase E — integration milestone (proves the motivation) — BLOCKED on `zeroship-migrate*` crate existence (C2).** mysql2 against live MySQL (`tests/node_mysql2_e2e.rs`, `createConnection`→`SELECT 1+1`→CREATE/INSERT/SELECT — the "live MySQL without compio-mysql" goal) is gateable as soon as node:net lands. **Capstone (`tests/migrate_js_transport_apply.rs`), deferred until the migrate crates exist:** the `zeroship-migrate` engine on the JS-driver transport uses `pg` (trusted context) to **APPLY a migration end to end** over node:net — advisory lock, changeset in a txn, immutable journal row written, schema mutation observable via a second connection. Must use the REAL apply path (a shimmed transport would pass while the real socket path is broken — the auto-tx lesson).

**Definition of done (today):** Phase A 100% · B 100% · **C `pg` returns `[{n:42,s:"ok"}]` + Pool/INSERT + SSL green — the headline proof** · D all enforcement regressions green (SSRF/CGNAT/rebind-deny, DNS-timeout, sandbox-no-register, capability_violation, allowlist-authoring-reject, OOM-cap-destroy, egress-ceiling + spend-feed, global-socket-ceiling, TLS-verify incl. Trusted-no-disable). **Phase C passing + Phase D fully green is the shippable signal.** **Deferred until the migrate crates land:** Phase E mysql2 query green AND migrate-apply writes a journal row and mutates schema — at which point the migrate-apply capstone becomes the end-to-end signal that the effort delivered its stated motivation.

---

## 6. Open questions for review

1. **Metering** — RESOLVED to *enforcement, not just metering* (S2): raw-`net` egress is hard-capped per socket/app **and** wired into the gateway spend Degrade/Block path, not merely invoiced. Remaining question: the exact metric name(s) and whether ingress is also spend-enforced or attribution-only. The connect/driver task is the emission point.
2. **`getPeerCertificate`** — confirm no first-party managed-DB target requires cert pinning at launch; if one does it promotes DEFERRED→IN.
3. **Unix sockets** — confirm migrate only ever targets TCP endpoints (it does, per "live MySQL via mysql2") so the deferral is safe.
4. **STARTTLS over-read** — the §2 "read_buffer empty at upgrade" invariant holds for PG/MySQL by protocol design but is an assertion; a leftover-prepend `compio_tls` adapter is deferred until a driver needs it.
5. **`unref()`** — no-op under the single compio loop; revisit if a driver relies on it to let the loop exit.
6. **`PG_TEST_URL` default** differs across existing suites (`:5440` in compose/memory vs `:5434` in `native_transaction.rs:50`). Pin ONE default for the runtime suite and note the discrepancy.
7. **`zeroship-migrate*` crates do not exist in the clean main checkout** — Phases D/E migrate tests are specified against the *planned* crate; the node:net side only needs the registration seam (`resolve_native`) to be context-conditional so a host can build a Runtime with net withheld.

---

## Key files (all under `crates/runtime/src/`, absolute paths)

- `web/websocket/network.rs` — the driver / event-bridge / backpressure / writer template
- `web/websocket/handshake.rs` (`:64,:181,:229-272,:346-368`) — TCP+TLS connect, `build_tls_connector`, SSRF call site
- `web/websocket/dispatch.rs` — V8-thread emit arm
- `transport/ssrf.rs` (`:40,:78,:141,:251`) — the unconditional SSRF gate behind `NetPolicy`
- `node/crypto/cipher.rs` (`:105-544`) — `#[v8_class]` host object + finalizer pattern
- `node/path/mod.rs` (`:35-74`) — synthetic-module IIFE shim
- `core/native_modules.rs` (`:22-51`) — resolver wiring (context-conditional gate)
- `core/state.rs` (`:451,:604-615,:979`) — per-id state table, `in_flight_fetches`, `OpResult`
- `core/runtime.rs` (`:174,:483-492,:1038-1215,:2030,:2297-2349`) — builder, pump, timers
- `worker/src/cache.rs` (`:391-416`) — LRU eviction (F1 lifecycle changes)
- new: `transport/net_policy.rs`, `node/{events,net,tls}/`
