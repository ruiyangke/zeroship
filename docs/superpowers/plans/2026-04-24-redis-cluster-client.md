# Redis Cluster Client — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Redis/Dragonfly cluster-protocol support to `compio-redis` so `@zeroship/kv` can deploy to multi-node Dragonfly on day 1. Single-node `Client` stays unchanged for dev; a new `ClusterClient` sits alongside and handles `CLUSTER SLOTS` topology + `MOVED`/`ASK` redirects. Hash-tag scoping we already ship (`{app_id}:<key>`) means one app's keyspace lives on one shard, so day-1 workloads never cross slots.

**Architecture:**
- `Client` (existing) — single TCP connection, unchanged.
- `Pool` (existing) — unchanged; stays per-node.
- `ClusterState` (new) — `[Option<NodeAddr>; 16384]` slot map + `HashMap<NodeAddr, Pool>` pool cache. Interior `RefCell` because compio is `!Send` and the `ClusterClient` is `!Sync` (per-worker pattern).
- `ClusterClient` (new) — holds `ClusterState`, exposes the same command surface as `Client` but every command is routed via `send_to_slot` which: hashes key → slot → node → pool → acquire → send. On `-MOVED` reply: update map + retry once. On `-ASK` reply: one-shot `ASKING` prefix + retry on redirect target.
- `plugin-kv::backend::redis` (existing) — grow a `connect()` path that detects `?cluster=true` in URL and builds `ClusterClient` instead of `Pool`.

**Tech stack:** Rust, compio async, `redis_protocol::redis_keyslot` (CRC16 + hash-tag extraction, already available in our v6 dep), Dragonfly `--cluster_mode=yes` for test fixtures.

**Spec-level decisions (already made):**
- Include both `MOVED` (map update + retry) and `ASK` (one-shot ASKING prefix) since the delta is ~30 LOC.
- Retry budget: max 2 redirects per command, then error (prevents infinite loops during flapping).
- Multi-key commands (`MGET`, `MSET`): validate all keys hash to the same slot; error `Error::CrossSlot` otherwise. Day-1 apps never hit this because all their keys share an `{app_id}` hash tag.
- Fail closed: if user points a `ClusterClient` at a non-cluster Redis, `CLUSTER SLOTS` returns an error and connect fails. They'd use `Client` instead.
- Seed list: `ClusterClient::connect(&[url1, url2, url3])` — client probes seeds until one responds; then `CLUSTER SLOTS` from that one gives the full topology.
- `ClusterClient` exposes the same public method signatures as `Client` for the routable commands. `ping`, `auth`, `select` (non-routable) are per-node operations and aren't on `ClusterClient`.

---

## File Structure

**New files:**
- `crates/compio-redis/src/cluster.rs` — `ClusterClient`, `ClusterState`, `NodeAddr`, redirect parser (~350 LOC)
- `crates/compio-redis/tests/cluster.rs` — integration tests against a real 3-node Dragonfly (~150 LOC)
- `docker-compose.cluster.yml` — 3-node Dragonfly fixture for tests (~40 LOC)

**Modified files:**
- `crates/compio-redis/src/lib.rs` — re-export `ClusterClient`, `ClusterError`
- `crates/compio-redis/src/error.rs` — add `Error::Moved { slot, addr }`, `Error::Ask { addr }`, `Error::CrossSlot`, `Error::NoRoute`, `Error::ClusterBootstrap(String)`
- `crates/compio-redis/src/client.rs` — expose `send_recv_frame` as `pub(crate)`, add `asking()` per-connection primitive
- `crates/compio-redis/src/pool.rs` — no changes expected; `Pool` stays per-node
- `crates/compio-redis/Cargo.toml` — no new deps (`redis_keyslot` already in `redis-protocol` v6)
- `crates/plugin-kv/src/backend/redis.rs` — detect `?cluster=true` in URL; use `ClusterClient`-backed impl when set
- `crates/plugin-kv/Cargo.toml` — no new deps
- `README.md` — one-line note on cluster config string

**Out of scope:**
- Pub/sub (not used by plugin-kv)
- Transactions (`MULTI`/`EXEC`) across slots
- Cluster discovery via gossip (only use `CLUSTER SLOTS` from seed nodes; topology refresh on `MOVED`)
- Read-from-replicas (always hit primary)
- TLS (not configured anywhere yet)

---

## Task CS1: Add cluster error variants + `redis_keyslot` import

**Files:**
- Modify: `crates/compio-redis/src/error.rs`

- [ ] **Step 1: Extend the `Error` enum**

Open `crates/compio-redis/src/error.rs`. Add these variants alongside the existing ones:

```rust
/// Cluster topology returned a MOVED redirect — the slot we hashed to
/// is owned by a different node. Includes the slot + the authoritative
/// `host:port`. Caller updates its slot map and retries.
Moved { slot: u16, addr: String },

/// Cluster topology returned an ASK redirect — transient during online
/// resharding. The slot is being migrated; caller must connect to
/// `addr` and issue `ASKING` before retrying exactly this one command.
Ask { slot: u16, addr: String },

/// Multi-key command spanned multiple slots. Redis/Dragonfly require
/// all keys in a single command to hash to the same slot; caller must
/// split the batch or use hash-tags.
CrossSlot,

/// The slot computed from the key isn't mapped in the current
/// topology. Either the cluster hasn't finished coming up or a
/// topology refresh is needed.
NoRoute { slot: u16 },

/// `CLUSTER SLOTS` either errored or returned a shape we don't understand.
ClusterBootstrap(String),
```

- [ ] **Step 2: Extend `Display` + `From` conversions as needed**

If the `Error` enum uses `thiserror` or manual `Display`, add matching arms. Follow the style of the existing variants verbatim.

- [ ] **Step 3: Verify**

Run: `cargo check -p compio-redis`

Expected: clean build. New variants unused yet — that's fine.

- [ ] **Step 4: Commit**

```bash
git add crates/compio-redis/src/error.rs
git commit -m "compio-redis: error variants for cluster MOVED/ASK/cross-slot"
```

---

## Task CS2: Parse MOVED/ASK replies + add `Client::asking()`

**Files:**
- Modify: `crates/compio-redis/src/client.rs`

- [ ] **Step 1: Add the `asking()` command on `Client`**

The `ASKING` command is sent by a caller immediately after receiving `-ASK <slot> <addr>` to tell the target node it's OK to process the next command even though it doesn't own the slot during the migration window. Open `crates/compio-redis/src/client.rs`; in the command-wrappers section (near `ping`):

```rust
/// Send the ASKING marker — used once, after an -ASK redirect, before
/// replaying the redirected command. The target node answers with +OK.
pub async fn asking(&mut self) -> Result<()> {
    let frame = self.send_recv(build_cmd(&[b"ASKING"])).await?;
    expect_ok(frame)
}
```

- [ ] **Step 2: Expose a crate-internal raw send_recv**

The cluster layer needs to send pre-encoded frames through an acquired connection. If `send_recv` is already `pub(crate)` on `Client`, skip this. Otherwise relax its visibility:

```rust
// existing
async fn send_recv(&mut self, cmd: OwnedFrame) -> Result<OwnedFrame> { ... }

// change to
pub(crate) async fn send_recv(&mut self, cmd: OwnedFrame) -> Result<OwnedFrame> { ... }
```

- [ ] **Step 3: Add a redirect parser helper in `cluster.rs` (creates the file)**

Create `crates/compio-redis/src/cluster.rs` with just the parser for now:

```rust
//! Redis/Dragonfly cluster client. Handles `CLUSTER SLOTS` topology,
//! slot-to-node routing, and MOVED/ASK redirect retries.
//!
//! Single `Client` + `Pool` stay unchanged for single-node deployments
//! (dev, single-Dragonfly prod). This file adds the `ClusterClient`
//! type that layers on top of Pools keyed by node address.

use crate::error::{Error, Result};

/// Parse a `-MOVED <slot> <host:port>` or `-ASK <slot> <host:port>`
/// reply into our error variants. Returns `None` if the server error
/// isn't a redirect.
pub(crate) fn parse_redirect(server_msg: &str) -> Option<Error> {
    let mut it = server_msg.splitn(3, ' ');
    let kind = it.next()?;
    let slot = it.next()?.parse::<u16>().ok()?;
    let addr = it.next()?.to_string();
    match kind {
        "MOVED" => Some(Error::Moved { slot, addr }),
        "ASK"   => Some(Error::Ask { slot, addr }),
        _ => None,
    }
}

#[cfg(test)]
mod redirect_tests {
    use super::*;
    #[test]
    fn parse_moved() {
        let e = parse_redirect("MOVED 3999 127.0.0.1:7002").unwrap();
        match e {
            Error::Moved { slot, addr } => {
                assert_eq!(slot, 3999);
                assert_eq!(addr, "127.0.0.1:7002");
            }
            _ => panic!("expected Moved"),
        }
    }

    #[test]
    fn parse_ask() {
        let e = parse_redirect("ASK 5000 10.0.0.4:6379").unwrap();
        match e {
            Error::Ask { slot, addr } => {
                assert_eq!(slot, 5000);
                assert_eq!(addr, "10.0.0.4:6379");
            }
            _ => panic!("expected Ask"),
        }
    }

    #[test]
    fn parse_unrelated_error_returns_none() {
        assert!(parse_redirect("WRONGTYPE Operation against a key holding the wrong kind of value").is_none());
    }
}
```

Don't register the module in `lib.rs` yet — CS5 does that once the public `ClusterClient` exists.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p compio-redis cluster::redirect_tests 2>&1 | tail -5`

Expected: 3 tests pass. (Tests are inside the module so they compile even though the module isn't re-exported yet.)

Hmm — an unregistered module won't compile. Alternative: Register the mod as `pub(crate) mod cluster;` in `lib.rs` now, and leave it scoped until CS5 exposes types publicly. Make this adjustment.

- [ ] **Step 5: Commit**

```bash
git add crates/compio-redis/src/client.rs crates/compio-redis/src/cluster.rs crates/compio-redis/src/lib.rs
git commit -m "compio-redis: ASKING primitive + MOVED/ASK reply parser"
```

---

## Task CS3: Implement `CLUSTER SLOTS` parsing

**Files:**
- Modify: `crates/compio-redis/src/cluster.rs`

Wire format of `CLUSTER SLOTS` reply (RESP2 array; Dragonfly matches Redis):

```
1) 1) (integer) 0               ; first slot in this range
   2) (integer) 5460            ; last slot
   3) 1) "127.0.0.1"            ; primary host
      2) (integer) 7000         ; primary port
      3) "node-id-string"       ; primary node id (ignore)
   4) 1) "127.0.0.1"            ; replica 1 (ignore)
      2) (integer) 7004
      3) "replica-node-id"
2) 1) (integer) 5461
   2) (integer) 10922
   ...
```

We only care about the slot range + the primary `host:port`. Replicas are ignored (we always route to the primary).

- [ ] **Step 1: Define `NodeAddr` + `ClusterTopology`**

Append to `crates/compio-redis/src/cluster.rs`:

```rust
use redis_protocol::resp2::types::OwnedFrame;
use redis_protocol::redis_keyslot;

/// A cluster node address, canonicalized to `host:port` text form so it
/// can be used as a `HashMap` key for the pool cache.
pub type NodeAddr = String;

/// Slot-to-node mapping. Exactly 16384 slots per the Redis cluster spec.
pub(crate) const NUM_SLOTS: usize = 16384;

#[derive(Default)]
pub(crate) struct ClusterTopology {
    pub slots: Vec<Option<NodeAddr>>, // len = NUM_SLOTS
}

impl ClusterTopology {
    pub fn empty() -> Self {
        Self { slots: vec![None; NUM_SLOTS] }
    }

    pub fn node_for_slot(&self, slot: u16) -> Option<&NodeAddr> {
        self.slots.get(slot as usize).and_then(|o| o.as_ref())
    }
}
```

- [ ] **Step 2: Parse a `CLUSTER SLOTS` reply frame into a topology**

Append:

```rust
/// Parse the RESP2 array returned by `CLUSTER SLOTS` into a topology.
/// Returns `Error::ClusterBootstrap` on any shape mismatch.
pub(crate) fn parse_cluster_slots(frame: OwnedFrame) -> Result<ClusterTopology> {
    let ranges = match frame {
        OwnedFrame::Array(a) => a,
        OwnedFrame::Error(m) => return Err(Error::ClusterBootstrap(m)),
        other => return Err(Error::ClusterBootstrap(format!(
            "CLUSTER SLOTS: expected array, got {other:?}"
        ))),
    };

    let mut topo = ClusterTopology::empty();
    for range in ranges {
        let items = match range {
            OwnedFrame::Array(a) => a,
            other => return Err(Error::ClusterBootstrap(format!(
                "CLUSTER SLOTS range: expected array, got {other:?}"
            ))),
        };
        if items.len() < 3 {
            return Err(Error::ClusterBootstrap("range has < 3 elements".into()));
        }
        let mut iter = items.into_iter();
        let start = expect_u16(iter.next().unwrap(), "slot start")?;
        let end   = expect_u16(iter.next().unwrap(), "slot end")?;
        let primary = iter.next().unwrap();
        let primary_items = match primary {
            OwnedFrame::Array(a) => a,
            other => return Err(Error::ClusterBootstrap(format!(
                "primary node: expected array, got {other:?}"
            ))),
        };
        if primary_items.len() < 2 {
            return Err(Error::ClusterBootstrap("primary has < 2 elements".into()));
        }
        let mut pi = primary_items.into_iter();
        let host = expect_bulk_string(pi.next().unwrap(), "host")?;
        let port = expect_u16(pi.next().unwrap(), "port")?;
        let addr = format!("{host}:{port}");

        for s in start..=end {
            if (s as usize) >= NUM_SLOTS {
                return Err(Error::ClusterBootstrap(format!("slot {s} out of range")));
            }
            topo.slots[s as usize] = Some(addr.clone());
        }
    }
    Ok(topo)
}

fn expect_u16(f: OwnedFrame, label: &str) -> Result<u16> {
    match f {
        OwnedFrame::Integer(n) if n >= 0 && n <= u16::MAX as i64 => Ok(n as u16),
        other => Err(Error::ClusterBootstrap(format!("{label}: not a u16 integer: {other:?}"))),
    }
}

fn expect_bulk_string(f: OwnedFrame, label: &str) -> Result<String> {
    match f {
        OwnedFrame::BulkString(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        OwnedFrame::SimpleString(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        other => Err(Error::ClusterBootstrap(format!("{label}: not a string: {other:?}"))),
    }
}
```

- [ ] **Step 3: Unit test the parser with a synthetic frame**

Append a test that builds a minimal `CLUSTER SLOTS` reply by hand and verifies the topology:

```rust
#[cfg(test)]
mod slot_parse_tests {
    use super::*;
    use redis_protocol::resp2::types::OwnedFrame;

    fn bulk(s: &str) -> OwnedFrame { OwnedFrame::BulkString(s.as_bytes().to_vec()) }
    fn int(n: i64) -> OwnedFrame { OwnedFrame::Integer(n) }
    fn array(v: Vec<OwnedFrame>) -> OwnedFrame { OwnedFrame::Array(v) }

    #[test]
    fn parses_two_range_topology() {
        // Slots 0..5460 → node-a:7000;  5461..16383 → node-b:7001
        let reply = array(vec![
            array(vec![
                int(0), int(5460),
                array(vec![bulk("127.0.0.1"), int(7000), bulk("node-a-id")]),
            ]),
            array(vec![
                int(5461), int(16383),
                array(vec![bulk("127.0.0.1"), int(7001), bulk("node-b-id")]),
            ]),
        ]);
        let topo = parse_cluster_slots(reply).expect("parse");
        assert_eq!(topo.node_for_slot(0),      Some(&"127.0.0.1:7000".to_string()));
        assert_eq!(topo.node_for_slot(5460),   Some(&"127.0.0.1:7000".to_string()));
        assert_eq!(topo.node_for_slot(5461),   Some(&"127.0.0.1:7001".to_string()));
        assert_eq!(topo.node_for_slot(16383),  Some(&"127.0.0.1:7001".to_string()));
    }

    #[test]
    fn errors_on_non_array() {
        let reply = OwnedFrame::Error("no cluster mode".into());
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn slot_hash_matches_redis_keyslot() {
        // Well-known from redis docs: "foo" → slot 12182
        assert_eq!(redis_keyslot(b"foo"), 12182);
        // Hash-tag isolates keys under a shared slot.
        assert_eq!(redis_keyslot(b"{app1}:a"), redis_keyslot(b"{app1}:b"));
    }
}
```

- [ ] **Step 4: Verify**

Run: `cargo test -p compio-redis cluster::slot_parse_tests 2>&1 | tail -10`

Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/compio-redis/src/cluster.rs
git commit -m "compio-redis: CLUSTER SLOTS parser + NodeAddr/ClusterTopology types"
```

---

## Task CS4: `ClusterClient::connect` with seed bootstrap

**Files:**
- Modify: `crates/compio-redis/src/cluster.rs`
- Modify: `crates/compio-redis/src/lib.rs`

- [ ] **Step 1: Add the `ClusterClient` struct + connect**

Append to `cluster.rs`:

```rust
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::client::Client;
use crate::pool::Pool;
use crate::protocol::build_cmd;

/// Interior shared state — slot map + per-node pool cache. Wrapped in
/// `Rc<RefCell<…>>` because compio futures are `!Send`, so everything
/// lives per-thread.
struct Inner {
    topology: ClusterTopology,
    pools: HashMap<NodeAddr, Pool>,
    pool_size: usize,
    /// Saved so we can rebuild pools for newly-discovered nodes.
    password: Option<String>,
    db: Option<i64>,
}

/// Cluster-aware client. One per worker thread; cheap to clone (the
/// clone shares topology + pool cache via `Rc`).
#[derive(Clone)]
pub struct ClusterClient {
    inner: Rc<RefCell<Inner>>,
}

impl ClusterClient {
    /// Connect to a cluster using a list of seed URLs. Probes seeds in
    /// order; the first one that answers `CLUSTER SLOTS` seeds the
    /// topology. `pool_size` caps connections per node.
    pub async fn connect(seeds: &[&str], pool_size: usize) -> Result<Self> {
        if seeds.is_empty() {
            return Err(Error::ClusterBootstrap("no seed URLs provided".into()));
        }
        let mut last_err: Option<Error> = None;
        for url in seeds {
            match Self::bootstrap_from(url, pool_size).await {
                Ok(c) => return Ok(c),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            Error::ClusterBootstrap("all seeds failed".into())
        }))
    }

    async fn bootstrap_from(url: &str, pool_size: usize) -> Result<Self> {
        // Open one probe connection and issue CLUSTER SLOTS.
        let mut probe = Client::connect(url).await?;
        let frame = probe.send_recv(build_cmd(&[b"CLUSTER", b"SLOTS"])).await?;
        let topology = parse_cluster_slots(frame)?;

        // Capture auth credentials by re-parsing the URL so pools built
        // for newly-discovered nodes can authenticate.
        let (password, db) = credentials_from_url(url)?;

        let pools = HashMap::new();
        let inner = Inner { topology, pools, pool_size, password, db };
        Ok(Self { inner: Rc::new(RefCell::new(inner)) })
    }

    /// Return a pool for `addr`, opening one lazily on first use. The
    /// newly-opened Pool inherits auth/db from the seed URL.
    async fn pool_for(&self, addr: &str) -> Result<Pool> {
        if let Some(p) = self.inner.borrow().pools.get(addr).cloned() {
            return Ok(p);
        }
        let (pw, db) = {
            let b = self.inner.borrow();
            (b.password.clone(), b.db)
        };
        let url = build_node_url(addr, pw.as_deref(), db);
        let size = self.inner.borrow().pool_size;
        let pool = Pool::connect(&url, size).await.map_err(|e| Error::Pool(format!("{e}")))?;
        self.inner.borrow_mut().pools.insert(addr.to_string(), pool.clone());
        Ok(pool)
    }

    /// Replace the owner of a single slot. Called when a MOVED reply
    /// arrives with a more recent mapping than our cache.
    fn set_slot(&self, slot: u16, addr: &str) {
        let mut b = self.inner.borrow_mut();
        b.topology.slots[slot as usize] = Some(addr.to_string());
    }
}

fn credentials_from_url(url: &str) -> Result<(Option<String>, Option<i64>)> {
    let parsed = url::Url::parse(url).map_err(|e|
        Error::Config(format!("bad seed URL '{url}': {e}")))?;
    let pw = parsed.password().map(str::to_string);
    let db = parsed.path().trim_start_matches('/').parse::<i64>().ok();
    Ok((pw, db))
}

fn build_node_url(addr: &str, password: Option<&str>, db: Option<i64>) -> String {
    let mut s = String::from("redis://");
    if let Some(p) = password {
        s.push(':');
        s.push_str(p);
        s.push('@');
    }
    s.push_str(addr);
    if let Some(d) = db {
        s.push('/');
        s.push_str(&d.to_string());
    }
    s
}
```

- [ ] **Step 2: Re-export `ClusterClient` in `lib.rs`**

Open `crates/compio-redis/src/lib.rs`. Change `pub(crate) mod cluster;` (added in CS2) to `pub mod cluster;` and add:

```rust
pub use cluster::ClusterClient;
```

- [ ] **Step 3: Verify compile**

Run: `cargo check -p compio-redis`

Expected: clean. Warnings about unused `Inner` fields are fine — next task wires them.

- [ ] **Step 4: Commit**

```bash
git add crates/compio-redis/src/cluster.rs crates/compio-redis/src/lib.rs
git commit -m "compio-redis: ClusterClient::connect — seed bootstrap + topology snapshot"
```

---

## Task CS5: `send_to_slot` — routed execution with MOVED/ASK retry

**Files:**
- Modify: `crates/compio-redis/src/cluster.rs`

- [ ] **Step 1: Implement the routed send**

Append to the `impl ClusterClient` block:

```rust
/// Send an encoded command, routed to the owner of the slot of
/// `routing_key`. Transparently handles `-MOVED` (update slot map +
/// retry once on the new owner) and `-ASK` (one-shot redirect with
/// ASKING prefix). Retries capped at 2 redirects.
pub(crate) async fn send_to_slot(
    &self,
    routing_key: &[u8],
    cmd: OwnedFrame,
) -> Result<OwnedFrame> {
    const MAX_REDIRECTS: u8 = 2;
    let slot = redis_keyslot(routing_key);
    let mut redirects = 0u8;

    // First attempt uses our cached topology.
    let mut addr_override: Option<String> = None;
    let mut ask_once = false;

    loop {
        let addr = match addr_override.take() {
            Some(a) => a,
            None => self
                .inner
                .borrow()
                .topology
                .node_for_slot(slot)
                .cloned()
                .ok_or(Error::NoRoute { slot })?,
        };

        let pool = self.pool_for(&addr).await?;
        let mut conn = pool.acquire().await.map_err(|e| Error::Pool(format!("{e}")))?;

        // After an -ASK redirect, the target needs an ASKING marker
        // before the replay. This is stateful per connection, one-shot.
        if ask_once {
            conn.asking().await?;
            ask_once = false;
        }

        match conn.send_recv(cmd.clone()).await {
            Ok(frame) => return Ok(frame),
            Err(Error::Server(msg)) => {
                if let Some(redirect) = parse_redirect(&msg) {
                    if redirects >= MAX_REDIRECTS {
                        return Err(Error::ClusterBootstrap(format!(
                            "exceeded {MAX_REDIRECTS} redirects, last: {msg}"
                        )));
                    }
                    redirects += 1;
                    match redirect {
                        Error::Moved { slot: s, addr: new_addr } => {
                            self.set_slot(s, &new_addr);
                            addr_override = Some(new_addr);
                            continue;
                        }
                        Error::Ask { addr: new_addr, .. } => {
                            addr_override = Some(new_addr);
                            ask_once = true;
                            continue;
                        }
                        _ => unreachable!(),
                    }
                }
                return Err(Error::Server(msg));
            }
            Err(e) => return Err(e),
        }
    }
}
```

Note: `OwnedFrame` must implement `Clone` for `cmd.clone()` to compile. If the redis_protocol v6 `OwnedFrame` doesn't derive Clone on all variants, add a local helper that re-encodes the frame from a `Vec<Vec<u8>>` command representation. Check via `grep "derive.*Clone" ~/.cargo/registry/src/*/redis-protocol-6.0.0/src/resp2/types.rs`.

If not `Clone`: change the signature to take `&[&[u8]]` (command parts), encode with `build_cmd(parts)` inside the retry loop so each iteration builds its own frame.

- [ ] **Step 2: Reality-check the clone assumption**

Run:

```bash
grep -n "^pub enum OwnedFrame\|#\[derive" ~/.cargo/registry/src/index.crates.io-*/redis-protocol-6.0.0/src/resp2/types.rs | head
```

If `Clone` isn't derived on `OwnedFrame`, refactor Step 1's signature to:

```rust
pub(crate) async fn send_to_slot(
    &self,
    routing_key: &[u8],
    parts: &[&[u8]],
) -> Result<OwnedFrame>
```

and `build_cmd(parts)` inside the loop each iteration.

- [ ] **Step 3: Verify compile**

Run: `cargo check -p compio-redis`

- [ ] **Step 4: Commit**

```bash
git add crates/compio-redis/src/cluster.rs
git commit -m "compio-redis: routed send_to_slot with MOVED/ASK retry"
```

---

## Task CS6: Command wrappers on `ClusterClient`

**Files:**
- Modify: `crates/compio-redis/src/cluster.rs`

- [ ] **Step 1: Port every single-key command**

Append to `impl ClusterClient`:

```rust
pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"GET", key.as_bytes()])).await?;
    crate::protocol::expect_bulk_or_null(frame)
}

pub async fn set(&self, key: &str, value: &[u8], ttl_ms: Option<u64>) -> Result<()> {
    let frame = if let Some(ms) = ttl_ms {
        let ms_s = ms.to_string();
        self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"SET", key.as_bytes(), value, b"PX", ms_s.as_bytes()])).await?
    } else {
        self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"SET", key.as_bytes(), value])).await?
    };
    crate::protocol::expect_ok(frame)
}

pub async fn set_nx(&self, key: &str, value: &[u8], ttl_ms: Option<u64>) -> Result<bool> {
    let frame = if let Some(ms) = ttl_ms {
        let ms_s = ms.to_string();
        self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"SET", key.as_bytes(), value, b"NX", b"PX", ms_s.as_bytes()])).await?
    } else {
        self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"SET", key.as_bytes(), value, b"NX"])).await?
    };
    match frame {
        OwnedFrame::SimpleString(s) if s == b"OK" => Ok(true),
        OwnedFrame::Null => Ok(false),
        OwnedFrame::Error(msg) => Err(Error::Server(msg)),
        other => Err(Error::Unexpected(format!("SET NX: {other:?}"))),
    }
}

pub async fn del(&self, key: &str) -> Result<bool> {
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"DEL", key.as_bytes()])).await?;
    Ok(crate::protocol::expect_integer(frame)? > 0)
}

pub async fn exists(&self, key: &str) -> Result<bool> {
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"EXISTS", key.as_bytes()])).await?;
    Ok(crate::protocol::expect_integer(frame)? > 0)
}

pub async fn pexpire(&self, key: &str, ttl_ms: u64) -> Result<bool> {
    let ms = ttl_ms.to_string();
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"PEXPIRE", key.as_bytes(), ms.as_bytes()])).await?;
    Ok(crate::protocol::expect_integer(frame)? > 0)
}

pub async fn pttl(&self, key: &str) -> Result<i64> {
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"PTTL", key.as_bytes()])).await?;
    crate::protocol::expect_integer(frame)
}

pub async fn incr_by(&self, key: &str, delta: i64) -> Result<i64> {
    let d = delta.to_string();
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"INCRBY", key.as_bytes(), d.as_bytes()])).await?;
    crate::protocol::expect_integer(frame)
}

pub async fn decr_by(&self, key: &str, delta: i64) -> Result<i64> {
    let d = delta.to_string();
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"DECRBY", key.as_bytes(), d.as_bytes()])).await?;
    crate::protocol::expect_integer(frame)
}

pub async fn strlen(&self, key: &str) -> Result<u64> {
    let frame = self.send_to_slot(key.as_bytes(),
        build_cmd(&[b"STRLEN", key.as_bytes()])).await?;
    Ok(crate::protocol::expect_integer(frame)?.max(0) as u64)
}
```

- [ ] **Step 2: Verify compile**

Run: `cargo check -p compio-redis`

Expected: clean. Each wrapper is 2-4 lines and routes via `send_to_slot`.

- [ ] **Step 3: Commit**

```bash
git add crates/compio-redis/src/cluster.rs
git commit -m "compio-redis: ClusterClient single-key command wrappers"
```

---

## Task CS7: Multi-key + SCAN commands with cross-slot validation

**Files:**
- Modify: `crates/compio-redis/src/cluster.rs`

- [ ] **Step 1: Same-slot validator helper**

Append a private helper:

```rust
/// Ensure every key in `keys` hashes to the same slot. Returns the
/// slot, or `Error::CrossSlot` if they disagree.
fn same_slot_or_err<T: AsRef<[u8]>>(keys: &[T]) -> Result<u16> {
    let first = keys.first().ok_or(Error::CrossSlot)?;
    let slot = redis_keyslot(first.as_ref());
    for k in &keys[1..] {
        if redis_keyslot(k.as_ref()) != slot {
            return Err(Error::CrossSlot);
        }
    }
    Ok(slot)
}
```

- [ ] **Step 2: `mget` / `mset`**

```rust
pub async fn mget(&self, keys: &[&str]) -> Result<Vec<Option<Vec<u8>>>> {
    if keys.is_empty() { return Ok(Vec::new()); }
    let _slot = same_slot_or_err(keys)?;
    let mut parts: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
    parts.push(b"MGET");
    for k in keys { parts.push(k.as_bytes()); }
    let frame = self.send_to_slot(keys[0].as_bytes(), build_cmd(&parts)).await?;
    let items = crate::protocol::expect_array(frame)?;
    items.into_iter().map(crate::protocol::expect_bulk_or_null).collect()
}

pub async fn mset(&self, kvs: &[(&str, &[u8])]) -> Result<()> {
    if kvs.is_empty() { return Ok(()); }
    let key_bytes: Vec<&[u8]> = kvs.iter().map(|(k, _)| k.as_bytes()).collect();
    let _slot = same_slot_or_err(&key_bytes)?;
    let mut parts: Vec<&[u8]> = Vec::with_capacity(kvs.len() * 2 + 1);
    parts.push(b"MSET");
    for (k, v) in kvs {
        parts.push(k.as_bytes());
        parts.push(v);
    }
    let frame = self.send_to_slot(kvs[0].0.as_bytes(), build_cmd(&parts)).await?;
    crate::protocol::expect_ok(frame)
}
```

- [ ] **Step 3: `scan` — per-node iteration**

SCAN in cluster mode is per-node. Users of `ClusterClient::scan` get keys from ONE node per call; plugin-kv's list() will iterate across nodes when needed. Since `{app_id}` hash-tag scoping keeps one app on one node, plugin-kv only needs to scan the app's specific node.

```rust
/// SCAN against the node that owns `routing_key`'s slot. Caller loops
/// with the returned cursor until "0". This scans ONE node; to scan the
/// full cluster, iterate nodes manually (rarely needed — hash-tag
/// scoping keeps per-app keys on a single node).
pub async fn scan(
    &self,
    routing_key: &str,
    cursor: &str,
    pattern: &str,
    count: u32,
) -> Result<(String, Vec<String>)> {
    let c = count.to_string();
    let frame = self.send_to_slot(routing_key.as_bytes(),
        build_cmd(&[
            b"SCAN", cursor.as_bytes(), b"MATCH", pattern.as_bytes(),
            b"COUNT", c.as_bytes(),
        ])).await?;
    let items = crate::protocol::expect_array(frame)?;
    if items.len() != 2 {
        return Err(Error::Unexpected(format!(
            "SCAN: expected 2-elem array, got {}", items.len())));
    }
    let mut iter = items.into_iter();
    let cursor = crate::protocol::expect_bulk_or_null(iter.next().unwrap())?
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();
    let keys_frame = iter.next().unwrap();
    let keys_arr = crate::protocol::expect_array(keys_frame)?;
    let keys: Vec<String> = keys_arr
        .into_iter()
        .filter_map(|f| match crate::protocol::expect_bulk_or_null(f).ok().flatten() {
            Some(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            None => None,
        })
        .collect();
    Ok((cursor, keys))
}
```

- [ ] **Step 4: Cross-slot unit test**

Append:

```rust
#[cfg(test)]
mod cross_slot_tests {
    use super::*;

    #[test]
    fn same_hash_tag_ok() {
        // Same {app1} hash tag → same slot.
        let keys = vec!["{app1}:a", "{app1}:b", "{app1}:c"];
        same_slot_or_err(&keys).expect("same slot");
    }

    #[test]
    fn different_hash_tags_err() {
        let keys = vec!["{app1}:a", "{app2}:b"];
        assert!(matches!(same_slot_or_err(&keys), Err(Error::CrossSlot)));
    }

    #[test]
    fn plain_keys_usually_different() {
        // "foo" slot 12182; "bar" slot 5061 — different.
        let keys = vec!["foo", "bar"];
        assert!(matches!(same_slot_or_err(&keys), Err(Error::CrossSlot)));
    }
}
```

- [ ] **Step 5: Verify**

Run: `cargo test -p compio-redis cluster 2>&1 | tail -10`

Expected: all cluster module unit tests pass (parser + cross-slot + redirect).

- [ ] **Step 6: Commit**

```bash
git add crates/compio-redis/src/cluster.rs
git commit -m "compio-redis: ClusterClient MGET/MSET/SCAN with cross-slot guard"
```

---

## Task CS8: Docker-compose 3-node Dragonfly fixture

**Files:**
- Create: `docker-compose.cluster.yml`

- [ ] **Step 1: Write the compose file**

Create `docker-compose.cluster.yml`:

```yaml
# 3-node Dragonfly cluster for integration tests.
#
# Usage:
#   docker compose -f docker-compose.cluster.yml up -d
#   # Wait ~2s for election, then bootstrap the cluster:
#   ./scripts/bootstrap-dragonfly-cluster.sh
#   DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
#     cargo test -p compio-redis --test cluster -- --nocapture
#   docker compose -f docker-compose.cluster.yml down -v

services:
  dragonfly-0:
    image: docker.dragonflydb.io/dragonflydb/dragonfly:latest
    command: >
      --cluster_mode=yes
      --port=7000
      --logtostderr
      --cluster_announce_ip=127.0.0.1
    ports:
      - "7000:7000"

  dragonfly-1:
    image: docker.dragonflydb.io/dragonflydb/dragonfly:latest
    command: >
      --cluster_mode=yes
      --port=7001
      --logtostderr
      --cluster_announce_ip=127.0.0.1
    ports:
      - "7001:7001"

  dragonfly-2:
    image: docker.dragonflydb.io/dragonflydb/dragonfly:latest
    command: >
      --cluster_mode=yes
      --port=7002
      --logtostderr
      --cluster_announce_ip=127.0.0.1
    ports:
      - "7002:7002"
```

- [ ] **Step 2: Bootstrap script**

Dragonfly cluster mode requires a one-time `DFLYCLUSTER CONFIG` push that assigns slot ranges to nodes. Create `scripts/bootstrap-dragonfly-cluster.sh`:

```bash
#!/usr/bin/env bash
# One-shot: tell all 3 Dragonfly nodes about the cluster topology.
# Slots 0–5460 → node-0, 5461–10922 → node-1, 10923–16383 → node-2.
#
# Requires redis-cli on PATH.
set -euo pipefail

# Dragonfly (unlike Redis) uses a flat JSON-ish config pushed via the
# DFLYCLUSTER CONFIG admin command. See Dragonfly cluster mode docs.
config_json=$(cat <<'JSON'
[
  { "slot_ranges": [{"start": 0,     "end": 5460 }], "master": { "id": "node-0", "ip": "127.0.0.1", "port": 7000 }, "replicas": [] },
  { "slot_ranges": [{"start": 5461,  "end": 10922}], "master": { "id": "node-1", "ip": "127.0.0.1", "port": 7001 }, "replicas": [] },
  { "slot_ranges": [{"start": 10923, "end": 16383}], "master": { "id": "node-2", "ip": "127.0.0.1", "port": 7002 }, "replicas": [] }
]
JSON
)

for port in 7000 7001 7002; do
  echo "-- pushing config to :$port --"
  redis-cli -p "$port" DFLYCLUSTER CONFIG "$config_json"
done

echo "-- cluster SLOTS on :7000 --"
redis-cli -p 7000 CLUSTER SLOTS
```

- [ ] **Step 3: Make script executable**

```bash
chmod +x scripts/bootstrap-dragonfly-cluster.sh
```

- [ ] **Step 4: Manual dry-run (no commit needed, just sanity check)**

```bash
docker compose -f docker-compose.cluster.yml up -d
sleep 3
./scripts/bootstrap-dragonfly-cluster.sh
redis-cli -p 7000 CLUSTER SLOTS
# ^ should print 3 slot ranges
docker compose -f docker-compose.cluster.yml down -v
```

- [ ] **Step 5: Commit**

```bash
git add docker-compose.cluster.yml scripts/bootstrap-dragonfly-cluster.sh
git commit -m "docker: 3-node Dragonfly cluster fixture for integration tests"
```

---

## Task CS9: Cluster integration tests

**Files:**
- Create: `crates/compio-redis/tests/cluster.rs`

- [ ] **Step 1: Write the integration test file**

Create `crates/compio-redis/tests/cluster.rs`:

```rust
//! Cluster integration tests against a live 3-node Dragonfly.
//!
//! Bring up the cluster first:
//!   docker compose -f docker-compose.cluster.yml up -d
//!   ./scripts/bootstrap-dragonfly-cluster.sh
//!
//! Then:
//!   DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
//!     cargo test -p compio-redis --test cluster -- --nocapture

use compio_redis::ClusterClient;

fn seeds() -> Option<Vec<String>> {
    std::env::var("DRAGONFLY_CLUSTER_SEEDS").ok().map(|s|
        s.split(',').map(|x| x.trim().to_string()).collect())
}

fn seeds_refs(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

#[compio::test]
async fn connect_and_roundtrip() {
    let Some(s) = seeds() else {
        eprintln!("skip: DRAGONFLY_CLUSTER_SEEDS not set");
        return;
    };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    cc.set("zs:cluster:k1", b"hello", None).await.expect("set");
    let v = cc.get("zs:cluster:k1").await.expect("get");
    assert_eq!(v.as_deref(), Some(b"hello".as_ref()));

    cc.del("zs:cluster:k1").await.expect("del");
    assert!(cc.get("zs:cluster:k1").await.unwrap().is_none());
}

#[compio::test]
async fn hash_tag_isolation_hits_one_node() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    // All keys share an {app42} hash tag — same slot, same node.
    for i in 0..10 {
        cc.set(&format!("{{app42}}:k{i}"), b"v", None).await.unwrap();
    }
    let values = cc.mget(&[
        "{app42}:k0", "{app42}:k5", "{app42}:k9",
    ]).await.expect("mget same slot");
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|v| v.is_some()));

    for i in 0..10 {
        cc.del(&format!("{{app42}}:k{i}")).await.unwrap();
    }
}

#[compio::test]
async fn cross_slot_mget_errors() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    // No hash tags — keys almost certainly land on different slots.
    let err = cc.mget(&["foo", "bar"]).await.unwrap_err();
    matches!(err, compio_redis::error::Error::CrossSlot);
}

#[compio::test]
async fn atomic_incr_survives_routing() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    cc.del("{appC}:counter").await.ok();
    for expected in 1..=20 {
        let v = cc.incr_by("{appC}:counter", 1).await.unwrap();
        assert_eq!(v, expected);
    }
    cc.del("{appC}:counter").await.ok();
}

#[compio::test]
async fn scan_against_routing_key_returns_matching() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let prefix = "{appS}:item";
    for i in 0..5 {
        cc.set(&format!("{prefix}:{i}"), b"x", None).await.unwrap();
    }

    let mut cursor = String::from("0");
    let mut found = Vec::new();
    loop {
        let (next, keys) = cc.scan("{appS}", &cursor, &format!("{prefix}:*"), 100)
            .await.expect("scan");
        found.extend(keys);
        if next == "0" { break; }
        cursor = next;
    }
    found.sort();
    assert_eq!(found.len(), 5);

    for i in 0..5 {
        cc.del(&format!("{prefix}:{i}")).await.unwrap();
    }
}
```

- [ ] **Step 2: Bring up cluster + run tests**

```bash
docker compose -f docker-compose.cluster.yml up -d
sleep 3
./scripts/bootstrap-dragonfly-cluster.sh
DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
  cargo test -p compio-redis --test cluster 2>&1 | tail -15
```

Expected: 5 tests pass. If any fail, diagnose — likely topology not yet converged; `sleep 3` may not be enough.

- [ ] **Step 3: Tear down**

```bash
docker compose -f docker-compose.cluster.yml down -v
```

- [ ] **Step 4: Commit**

```bash
git add crates/compio-redis/tests/cluster.rs
git commit -m "compio-redis: integration tests against 3-node Dragonfly cluster"
```

---

## Task CS10: Plugin-kv cluster-aware backend

**Files:**
- Modify: `crates/plugin-kv/src/backend/redis.rs`

The existing `Redis` backend holds a URL + `Pool` (per-thread). We extend it to optionally hold a `ClusterClient` based on URL query parameter `?cluster=true`. Everything downstream (the trait impl) delegates to whichever handle is active.

- [ ] **Step 1: Read the current backend**

```bash
cat crates/plugin-kv/src/backend/redis.rs
```

Note the `POOLS` thread_local cache and the `pool()` helper.

- [ ] **Step 2: Add a second cache for cluster clients**

Edit `crates/plugin-kv/src/backend/redis.rs`. Add a sibling `thread_local!`:

```rust
thread_local! {
    /// Per-thread cluster client cache — keyed by the *sorted seed URL
    /// list* so two `Redis` backends with the same seeds share a handle.
    static CLUSTER_CLIENTS: RefCell<std::collections::HashMap<String, compio_redis::ClusterClient>> =
        RefCell::new(std::collections::HashMap::new());
}
```

- [ ] **Step 3: URL-mode detection**

Add a helper:

```rust
fn is_cluster_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.query_pairs().find(|(k, _)| k == "cluster").map(|(_, v)| v.into_owned()))
        .is_some_and(|v| v == "true" || v == "1" || v == "yes")
}

/// Extract seed URLs from a cluster config. Accepts a comma-delimited
/// list under the `?seeds=` query param, or falls back to the base URL
/// as the sole seed.
fn seeds_from_url(url: &str) -> Vec<String> {
    if let Ok(u) = url::Url::parse(url) {
        if let Some((_, seeds)) = u.query_pairs().find(|(k, _)| k == "seeds") {
            return seeds.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
    }
    vec![url.to_string()]
}
```

- [ ] **Step 4: Split the backend impl**

Rewrite the `Backend for Redis` impl to dispatch between pool and cluster modes:

```rust
impl Redis {
    async fn with_conn<F, Fut, R>(&self, _routing_hint: &str, f: F) -> Result<R, String>
    where
        F: FnOnce(ConnHandle<'_>) -> Fut,
        Fut: std::future::Future<Output = Result<R, String>>,
    {
        if is_cluster_url(&self.url) {
            let client = self.cluster().await?;
            f(ConnHandle::Cluster(&client)).await
        } else {
            let pool = self.pool().await?;
            let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
            f(ConnHandle::Pool(&mut conn)).await
        }
    }

    async fn cluster(&self) -> Result<compio_redis::ClusterClient, String> {
        let seeds = seeds_from_url(&self.url);
        let cache_key = {
            let mut s = seeds.clone();
            s.sort();
            s.join(",")
        };
        if let Some(c) = CLUSTER_CLIENTS.with(|c| c.borrow().get(&cache_key).cloned()) {
            return Ok(c);
        }
        let refs: Vec<&str> = seeds.iter().map(|s| s.as_str()).collect();
        let c = compio_redis::ClusterClient::connect(&refs, self.max_size)
            .await
            .map_err(|e| format!("kv: cluster connect: {e}"))?;
        CLUSTER_CLIENTS.with(|ch| ch.borrow_mut().insert(cache_key, c.clone()));
        Ok(c)
    }
}

enum ConnHandle<'a> {
    Pool(&'a mut compio_redis::PooledConn),
    Cluster(&'a compio_redis::ClusterClient),
}
```

Then for each trait method (get, set, delete, incr, list), route to the right handle:

```rust
async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, String> {
    let scoped = scope(app_id, key);
    self.with_conn(&scoped, |h| async move {
        let bytes = match h {
            ConnHandle::Pool(c) => c.get(&scoped).await,
            ConnHandle::Cluster(c) => c.get(&scoped).await,
        }.map_err(|e| format!("kv: get: {e}"))?;
        Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }).await
}

// Similar for set/delete/incr/list.
// For list(): in cluster mode, pass the app_id as the SCAN routing key,
// and use the hash-tag pattern so every scan iteration hits the node
// that owns this app's keyspace.
```

*Implementation note:* if the `async move` closure shape causes lifetime headaches with `ConnHandle::Pool(&mut …)`, inline the branching without a closure — two match arms per method is acceptable.

- [ ] **Step 5: Build + verify**

Run: `cargo check -p zeroship-plugin-kv`

Expected: clean. The single-node `pool()` path is unchanged.

- [ ] **Step 6: Smoke-test against single-node Redis**

Since the existing plugin-kv integration tests use single-node, they should still pass:

```bash
ZEROSHIP_KV_URL=redis://127.0.0.1:6379 cargo test -p zeroship-plugin-kv 2>&1 | tail -10
```

Expected: green. No cluster involvement — the non-cluster URL takes the pool path.

- [ ] **Step 7: Smoke-test against cluster**

```bash
docker compose -f docker-compose.cluster.yml up -d
sleep 3 && ./scripts/bootstrap-dragonfly-cluster.sh
ZEROSHIP_KV_URL='redis://127.0.0.1:7000/?cluster=true&seeds=redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
  cargo test -p zeroship-plugin-kv 2>&1 | tail -10
docker compose -f docker-compose.cluster.yml down -v
```

Expected: green. Plugin-kv tests exercise atomic INCR, TTL, list, etc. across the cluster.

- [ ] **Step 8: Commit**

```bash
git add crates/plugin-kv/src/backend/redis.rs
git commit -m "plugin-kv: cluster-aware Redis backend — ?cluster=true + seed list"
```

---

## Task CS11: Full workspace verification

**Files:** (no edits)

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace --release 2>&1 | tail -5`

Expected: clean.

- [ ] **Step 2: Full test suite (ex. live-DB deps)**

Run: `cargo test --workspace --exclude compio-postgres --exclude zeroship-plugin-db 2>&1 | grep -E "^test result|FAILED"`

Expected: every line `ok`.

- [ ] **Step 3: Local Redis regression**

Run: `REDIS_TEST_URL=redis://127.0.0.1:6379 cargo test -p compio-redis --test integration 2>&1 | tail -5`

Expected: 11 tests pass (same as before CS1).

- [ ] **Step 4: Cluster integration rerun**

Run (cluster must still be up):

```bash
DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
  cargo test -p compio-redis --test cluster 2>&1 | tail -10
```

Expected: 5 tests pass.

- [ ] **Step 5: Quick diff review**

Run: `git log --oneline | head -15 && git diff HEAD~11 --stat | tail -20`

Expected:
- 10–11 commits over the CS1–CS10 range.
- `crates/compio-redis/src/cluster.rs` ~350 LOC.
- `crates/compio-redis/tests/cluster.rs` ~150 LOC.
- `crates/plugin-kv/src/backend/redis.rs` grew modestly (~80 LOC).
- No unrelated changes.

- [ ] **Step 6: Record outcome**

Done. Post the final test counts + any failed checks in a status message.

---

## Out-of-scope reminders

- **No PubSub** — plugin-kv doesn't use it.
- **No MULTI/EXEC across slots** — day-1 apps don't need cross-slot transactions.
- **No replica reads** — simplifies routing; add later if read-heavy workload shows up.
- **No TLS** — not configured anywhere else yet; matches existing single-node path.
- **No cluster discovery via gossip** — we only call `CLUSTER SLOTS` on connect + on-demand after `MOVED`; flat polling is sufficient at zeroship's day-1 scale.
