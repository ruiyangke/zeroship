//! Redis/Dragonfly cluster client. Handles `CLUSTER SLOTS` topology,
//! slot-to-node routing, and MOVED/ASK redirect retries.
//!
//! Single `Client` + `Pool` stay unchanged for single-node deployments
//! (dev, single-Dragonfly prod). This file adds the `ClusterClient`
//! type that layers on top of Pools keyed by node address.

use crate::error::{Error, Result};
use redis_protocol::redis_keyslot;
use redis_protocol::resp2::types::OwnedFrame;

/// Parse a `-MOVED <slot> <host:port>` or `-ASK <slot> <host:port>`
/// reply into our error variants. Returns `None` if the server error
/// isn't a redirect.
pub(crate) fn parse_redirect(server_msg: &str) -> Option<Error> {
    let mut it = server_msg.splitn(3, ' ');
    let kind = it.next()?;
    let slot = it.next()?.parse::<u16>().ok()?;
    // CR-CLUSTER-1: the slot must be a real cluster slot. A bare u16 admits
    // 0..=65535, but the slot map is exactly NUM_SLOTS (16384) long, so an
    // out-of-range value like 40000 would later index-panic in `set_slot`.
    // Reject it here so an out-of-range MOVED/ASK is treated as a
    // non-redirect server error, never an Error::Moved/Ask.
    if slot as usize >= NUM_SLOTS {
        return None;
    }
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

    #[test]
    fn parse_empty_string_returns_none() {
        // splitn(3) yields at least one item even for "", but slot parse fails.
        assert!(parse_redirect("").is_none());
    }

    #[test]
    fn parse_malformed_slot_returns_none() {
        // "abc" isn't a u16.
        assert!(parse_redirect("MOVED abc 127.0.0.1:7000").is_none());
    }

    #[test]
    fn parse_missing_addr_returns_none() {
        assert!(parse_redirect("MOVED 1234").is_none());
    }

    #[test]
    fn parse_slot_out_of_u16_returns_none() {
        // 99999 overflows u16.
        assert!(parse_redirect("MOVED 99999 127.0.0.1:7000").is_none());
    }

    #[test]
    fn parse_slot_in_u16_but_out_of_cluster_range_returns_none() {
        // CR-CLUSTER-1: 40000 is a valid u16 but >= NUM_SLOTS (16384). A
        // bare u16 parse would accept it and the MOVED arm would then do a
        // RAW index into the 16384-long slot Vec → out-of-bounds panic. The
        // parser MUST reject any slot >= NUM_SLOTS so an out-of-range
        // redirect never becomes an Error::Moved/Ask.
        assert!(parse_redirect("MOVED 40000 6.6.6.6:6379").is_none());
        assert!(parse_redirect("ASK 40000 6.6.6.6:6379").is_none());
        // Boundary: NUM_SLOTS itself (16384) is out of range; the last
        // valid slot (16383) is still a redirect.
        assert!(parse_redirect("MOVED 16384 1.2.3.4:6379").is_none());
        assert!(parse_redirect("MOVED 16383 1.2.3.4:6379").is_some());
    }

    #[test]
    fn parse_lowercase_is_not_a_redirect() {
        // Redis/Dragonfly emit MOVED/ASK uppercase only; we're strict so a
        // lowercase payload is treated as an unrelated server message.
        assert!(parse_redirect("moved 1 127.0.0.1:7000").is_none());
        assert!(parse_redirect("ask 1 127.0.0.1:7000").is_none());
    }

    #[test]
    fn parse_tryredirect_kinds_we_dont_know() {
        // Any non-MOVED/ASK first word is not a redirect, regardless of shape.
        assert!(parse_redirect("WAIT 0 1000").is_none());
        assert!(parse_redirect("BUSYKEY Target key name already exists").is_none());
    }

    #[test]
    fn addr_with_ipv6_braced() {
        // Real-world MOVED replies can carry a bracketed IPv6 "[::1]:6379".
        // Our parser takes everything after the second space verbatim, which
        // is correct — the connector later parses it. The point: we don't
        // mangle the address.
        let e = parse_redirect("MOVED 100 [::1]:6379").unwrap();
        match e {
            Error::Moved { slot, addr } => {
                assert_eq!(slot, 100);
                assert_eq!(addr, "[::1]:6379");
            }
            _ => panic!("expected Moved"),
        }
    }
}

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

    /// Distinct node addresses present in this topology. Used to build the
    /// SSRF allowlist: every node the authoritative `CLUSTER SLOTS` reply
    /// names is, by definition, a trusted cluster member.
    pub fn node_addrs(&self) -> impl Iterator<Item = &NodeAddr> {
        // De-dup is left to the caller's set insert; we just yield owners.
        self.slots.iter().filter_map(|o| o.as_ref())
    }
}

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

    #[test]
    fn empty_topology_parses_to_all_none() {
        let topo = parse_cluster_slots(OwnedFrame::Array(vec![])).unwrap();
        assert!(topo.slots.iter().all(Option::is_none));
        assert!(topo.node_for_slot(0).is_none());
        assert!(topo.node_for_slot(16383).is_none());
    }

    #[test]
    fn single_range_covers_whole_slot_space() {
        let reply = array(vec![
            array(vec![
                int(0), int(16383),
                array(vec![bulk("10.0.0.1"), int(6379), bulk("node-x-id")]),
            ]),
        ]);
        let topo = parse_cluster_slots(reply).unwrap();
        assert_eq!(topo.node_for_slot(0), Some(&"10.0.0.1:6379".to_string()));
        assert_eq!(topo.node_for_slot(8192), Some(&"10.0.0.1:6379".to_string()));
        assert_eq!(topo.node_for_slot(16383), Some(&"10.0.0.1:6379".to_string()));
    }

    #[test]
    fn replicas_ignored_in_range() {
        // Reply carries primary + 2 replicas. Only the primary should land
        // in our slot map — replica reads aren't supported day-one.
        let reply = array(vec![
            array(vec![
                int(0), int(100),
                array(vec![bulk("primary-host"), int(1000), bulk("p-id")]),
                array(vec![bulk("replica-1"),    int(2000), bulk("r1-id")]),
                array(vec![bulk("replica-2"),    int(3000), bulk("r2-id")]),
            ]),
        ]);
        let topo = parse_cluster_slots(reply).unwrap();
        assert_eq!(topo.node_for_slot(50), Some(&"primary-host:1000".to_string()));
    }

    #[test]
    fn range_with_too_few_elements_errors() {
        // Missing the primary-node triplet.
        let reply = array(vec![array(vec![int(0), int(5)])]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn primary_with_too_few_elements_errors() {
        // Primary missing port.
        let reply = array(vec![
            array(vec![
                int(0), int(5),
                array(vec![bulk("127.0.0.1")]),
            ]),
        ]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn slot_out_of_bounds_errors() {
        let reply = array(vec![
            array(vec![
                int(0), int(100_000), // well past 16383
                array(vec![bulk("127.0.0.1"), int(7000), bulk("x")]),
            ]),
        ]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn negative_slot_errors() {
        let reply = array(vec![
            array(vec![
                int(-1), int(100),
                array(vec![bulk("127.0.0.1"), int(7000), bulk("x")]),
            ]),
        ]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn non_integer_port_errors() {
        let reply = array(vec![
            array(vec![
                int(0), int(100),
                array(vec![bulk("127.0.0.1"), bulk("seven-thousand"), bulk("x")]),
            ]),
        ]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn integer_host_errors() {
        let reply = array(vec![
            array(vec![
                int(0), int(100),
                array(vec![int(42), int(7000), bulk("x")]),
            ]),
        ]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn primary_not_an_array_errors() {
        let reply = array(vec![
            array(vec![
                int(0), int(100),
                bulk("this should be an array"),
            ]),
        ]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn range_not_an_array_errors() {
        // Top-level Array containing a non-array element.
        let reply = array(vec![bulk("not an array")]);
        assert!(matches!(parse_cluster_slots(reply), Err(Error::ClusterBootstrap(_))));
    }

    #[test]
    fn multi_range_later_overlaps_win() {
        // If a malformed reply overlaps ranges, the later range's primary
        // wins — documented behavior. Matches Redis behavior on duplicate
        // slot assignments during resharding.
        let reply = array(vec![
            array(vec![
                int(0), int(100),
                array(vec![bulk("first"), int(1111), bulk("f")]),
            ]),
            array(vec![
                int(50), int(150),
                array(vec![bulk("second"), int(2222), bulk("s")]),
            ]),
        ]);
        let topo = parse_cluster_slots(reply).unwrap();
        assert_eq!(topo.node_for_slot(0),   Some(&"first:1111".to_string()));
        assert_eq!(topo.node_for_slot(49),  Some(&"first:1111".to_string()));
        // 50..=100 now point at `second`.
        assert_eq!(topo.node_for_slot(50),  Some(&"second:2222".to_string()));
        assert_eq!(topo.node_for_slot(100), Some(&"second:2222".to_string()));
        assert_eq!(topo.node_for_slot(150), Some(&"second:2222".to_string()));
        // 151+ is untouched.
        assert!(topo.node_for_slot(151).is_none());
    }

    #[test]
    fn simple_string_host_accepted() {
        // Some Redis flavors emit host as a simple string rather than bulk;
        // our parser accepts both.
        let reply = array(vec![
            array(vec![
                int(0), int(100),
                array(vec![
                    OwnedFrame::SimpleString(b"simple-host".to_vec()),
                    int(6379),
                    bulk("node-id"),
                ]),
            ]),
        ]);
        let topo = parse_cluster_slots(reply).unwrap();
        assert_eq!(topo.node_for_slot(10), Some(&"simple-host:6379".to_string()));
    }
}

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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
    /// CR-CLUSTER-2 / REDIS-SSRF-1: the set of operator-trusted node
    /// addresses (normalized `host:port`) we are willing to connect to
    /// with the cluster credentials. Seeded from the configured seed URLs
    /// and extended with the addresses in any authoritative `CLUSTER
    /// SLOTS` topology learned from a trusted node. A server-supplied
    /// redirect/topology address that isn't in here is refused before we
    /// connect (and before we replay the password).
    known_nodes: HashSet<NodeAddr>,
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

        // CR-CLUSTER-2: build the SSRF allowlist. The seed (operator-
        // configured, trusted) plus every node named by the authoritative
        // CLUSTER SLOTS topology we just parsed from it. These are the only
        // addresses we'll later connect to with the cluster password.
        let mut known_nodes: HashSet<NodeAddr> = HashSet::new();
        if let Some(seed_addr) = host_port_from_url(url) {
            known_nodes.insert(seed_addr);
        }
        for addr in topology.node_addrs() {
            known_nodes.insert(normalize_node_addr(addr));
        }

        let pools = HashMap::new();
        let inner = Inner { topology, pools, pool_size, password, db, known_nodes };
        Ok(Self { inner: Rc::new(RefCell::new(inner)) })
    }

    /// Return a pool for `addr`, opening one lazily on first use. The
    /// newly-opened Pool inherits auth/db from the seed URL.
    async fn pool_for(&self, addr: &str) -> Result<Pool> {
        if let Some(p) = self.inner.borrow().pools.get(addr).cloned() {
            return Ok(p);
        }
        // CR-CLUSTER-2 / REDIS-SSRF-1: never open a credentialed connection
        // to an address the cluster merely *claimed* (via MOVED/ASK or a
        // topology entry) unless it's an operator-trusted node. This stops
        // SSRF to internal services / the cloud metadata endpoint and
        // exfiltration of the cluster password to an attacker-chosen host.
        // Checked BEFORE any connect so a rejection costs no network I/O.
        {
            let b = self.inner.borrow();
            if !is_known_node(addr, &b.known_nodes) {
                return Err(Error::Config(format!(
                    "refusing to connect to untrusted cluster address '{addr}': \
                     not an operator-configured seed or a node in the verified \
                     CLUSTER SLOTS topology (possible MOVED/ASK SSRF)"
                )));
            }
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
        // CR-CLUSTER-1: checked write (mirrors the safe `.get()` in
        // `node_for_slot`). An out-of-range slot is silently ignored rather
        // than panicking the worker via a raw `Vec` index. `parse_redirect`
        // already bounds the slot, so this is defense in depth.
        if let Some(s) = b.topology.slots.get_mut(slot as usize) {
            *s = Some(addr.to_string());
        }
    }

    /// Test-only constructor: build a client around a given topology with
    /// an empty pool cache and no credentials. Lets unit tests drive
    /// `set_slot` / routing without a live cluster.
    #[cfg(test)]
    pub(crate) fn for_test(topology: ClusterTopology) -> Self {
        Self::for_test_with_known(topology, HashSet::new())
    }

    /// Test-only constructor that also seeds the SSRF allowlist.
    #[cfg(test)]
    pub(crate) fn for_test_with_known(
        topology: ClusterTopology,
        known_nodes: HashSet<NodeAddr>,
    ) -> Self {
        let inner = Inner {
            topology,
            pools: HashMap::new(),
            pool_size: 1,
            password: None,
            db: None,
            known_nodes,
        };
        Self { inner: Rc::new(RefCell::new(inner)) }
    }

    /// Test-only: read the owner currently recorded for a slot.
    #[cfg(test)]
    pub(crate) fn slot_owner(&self, slot: u16) -> Option<String> {
        self.inner.borrow().topology.node_for_slot(slot).cloned()
    }

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

            // `send_recv` returns the raw frame; -MOVED / -ASK come back
            // as `OwnedFrame::Error("MOVED …")`. Intercept those before
            // surfacing the frame to the caller.
            let frame = conn.send_recv(cmd.clone()).await?;
            if let OwnedFrame::Error(msg) = &frame
                && let Some(redirect) = parse_redirect(msg)
            {
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
            return Ok(frame);
        }
    }

    // -----------------------------------------------------------------
    // Single-key command wrappers — all route via `send_to_slot`.
    // -----------------------------------------------------------------

    /// GET a key. Returns None when the key doesn't exist.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"GET", key.as_bytes()])).await?;
        crate::protocol::expect_bulk_or_null(frame)
    }

    /// SET key value [PX millis]. Always overwrites.
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

    /// `SET key value NX [PX ms]` — lock primitive. Returns true on
    /// create, false if the key already existed.
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

    /// DEL key. Returns true if the key existed.
    pub async fn del(&self, key: &str) -> Result<bool> {
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"DEL", key.as_bytes()])).await?;
        Ok(crate::protocol::expect_integer(frame)? > 0)
    }

    /// EXISTS key. Returns true if the key exists.
    pub async fn exists(&self, key: &str) -> Result<bool> {
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"EXISTS", key.as_bytes()])).await?;
        Ok(crate::protocol::expect_integer(frame)? > 0)
    }

    /// PEXPIRE key ms — set/refresh TTL in ms. False when key missing.
    pub async fn pexpire(&self, key: &str, ttl_ms: u64) -> Result<bool> {
        let ms = ttl_ms.to_string();
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"PEXPIRE", key.as_bytes(), ms.as_bytes()])).await?;
        Ok(crate::protocol::expect_integer(frame)? > 0)
    }

    /// PTTL key — remaining TTL in ms. Wire: >=0 / -1 (no TTL) / -2 (missing).
    pub async fn pttl(&self, key: &str) -> Result<i64> {
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"PTTL", key.as_bytes()])).await?;
        crate::protocol::expect_integer(frame)
    }

    /// INCRBY — atomic counter increment. Creates missing key at 0 first.
    pub async fn incr_by(&self, key: &str, delta: i64) -> Result<i64> {
        let d = delta.to_string();
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"INCRBY", key.as_bytes(), d.as_bytes()])).await?;
        crate::protocol::expect_integer(frame)
    }

    /// PERSIST key — remove the TTL. True when a TTL was removed, false
    /// when the key is missing or already had no TTL.
    pub async fn persist(&self, key: &str) -> Result<bool> {
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"PERSIST", key.as_bytes()])).await?;
        Ok(crate::protocol::expect_integer(frame)? > 0)
    }

    /// EVAL a Lua script, returning the integer reply. Routes by the
    /// first key's slot (every key must hash to the same slot — our
    /// `{app_id}:` hash-tag scoping guarantees one slot per app, so the
    /// incr-with-TTL script always stays local to one node).
    pub async fn eval(&self, script: &str, keys: &[&str], args: &[&str]) -> Result<i64> {
        if keys.is_empty() {
            return Err(Error::Unexpected(
                "EVAL with no keys cannot be slot-routed in cluster mode".to_string(),
            ));
        }
        let key_bytes: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes()).collect();
        // Validate all keys share one slot. `same_slot_or_err` derives that
        // slot from `keys[0]`, and `send_to_slot(keys[0], …)` recomputes the
        // identical slot via `redis_keyslot(keys[0])` — so routing by
        // `keys[0]` IS routing by the validated slot. We keep the call for
        // its CrossSlot guard and discard the (provably redundant) value.
        let _ = same_slot_or_err(&key_bytes)?;
        let nkeys = keys.len().to_string();
        let mut parts: Vec<&[u8]> = Vec::with_capacity(3 + keys.len() + args.len());
        parts.push(b"EVAL");
        parts.push(script.as_bytes());
        parts.push(nkeys.as_bytes());
        for k in keys { parts.push(k.as_bytes()); }
        for a in args { parts.push(a.as_bytes()); }
        let frame = self.send_to_slot(keys[0].as_bytes(), build_cmd(&parts)).await?;
        crate::protocol::expect_integer(frame)
    }

    /// DECRBY — symmetric with INCRBY for tooling visibility in MONITOR.
    pub async fn decr_by(&self, key: &str, delta: i64) -> Result<i64> {
        let d = delta.to_string();
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"DECRBY", key.as_bytes(), d.as_bytes()])).await?;
        crate::protocol::expect_integer(frame)
    }

    /// STRLEN key — byte length of the stored string (0 if missing).
    pub async fn strlen(&self, key: &str) -> Result<u64> {
        let frame = self.send_to_slot(key.as_bytes(),
            build_cmd(&[b"STRLEN", key.as_bytes()])).await?;
        Ok(crate::protocol::expect_integer(frame)?.max(0) as u64)
    }

    // -----------------------------------------------------------------
    // Multi-key + SCAN. Multi-key ops require all keys to hash to the
    // same slot (Redis/Dragonfly hard constraint). Day-1 zeroship apps
    // use `{app_id}:` hash-tagging, so all of an app's keys already
    // share a slot — callers rarely hit the CrossSlot error in practice.
    // -----------------------------------------------------------------

    /// MGET — batch fetch. All keys must hash to the same slot.
    /// Returns one `Option<Vec<u8>>` per key, preserving input order.
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

    /// MSET — atomic batch write. All keys must hash to the same slot.
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

    /// SCAN against the node owning `routing_key`'s slot. Scans ONE node;
    /// full-cluster scans require iterating nodes manually (rarely needed
    /// — hash-tag scoping keeps per-app keys on a single node).
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
}

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
        // "foo" slot 12182, "bar" slot 5061 — different.
        let keys = vec!["foo", "bar"];
        assert!(matches!(same_slot_or_err(&keys), Err(Error::CrossSlot)));
    }

    #[test]
    fn single_key_is_trivially_same_slot() {
        let keys = vec!["only"];
        same_slot_or_err(&keys).expect("single-key always ok");
    }

    #[test]
    fn empty_keys_slice_errors() {
        // No routing key → CrossSlot (matches what the mget/mset wrappers
        // would report if someone called them with a non-empty sentinel
        // that eventually resolves to zero).
        let keys: Vec<&str> = vec![];
        assert!(matches!(same_slot_or_err(&keys), Err(Error::CrossSlot)));
    }

    #[test]
    fn duplicate_keys_are_same_slot() {
        let keys = vec!["k", "k", "k"];
        same_slot_or_err(&keys).expect("identical keys trivially same slot");
    }

    #[test]
    fn empty_hash_tag_falls_back_to_full_key_hash() {
        // Per Redis spec, `{}key` has an empty tag and therefore hashes
        // the full key — so `{}:foo` and `{}:bar` land on different
        // slots.
        assert_ne!(redis_keyslot(b"{}:foo"), redis_keyslot(b"{}:bar"));
        let keys = vec!["{}:foo", "{}:bar"];
        assert!(matches!(same_slot_or_err(&keys), Err(Error::CrossSlot)));
    }

    #[test]
    fn unclosed_brace_has_no_tag_effect() {
        // No matching close brace → full-key hash.
        assert_ne!(redis_keyslot(b"{app:foo"), redis_keyslot(b"{app:bar"));
    }

    #[test]
    fn trailing_hash_tag_still_groups() {
        // The hash-tag algorithm scans for the first `{…}` pair — so
        // trailing-tag keys also group correctly.
        assert_eq!(redis_keyslot(b"prefix{app}"), redis_keyslot(b"other{app}"));
        let keys = vec!["prefix{app}", "other{app}"];
        same_slot_or_err(&keys).expect("trailing tags group");
    }
}

fn credentials_from_url(url: &str) -> Result<(Option<String>, Option<i64>)> {
    let parsed = url::Url::parse(url).map_err(|e|
        Error::Config(format!("bad seed URL '{url}': {e}")))?;
    let pw = parsed.password().map(str::to_string);
    let db = parsed.path().trim_start_matches('/').parse::<i64>().ok();
    Ok((pw, db))
}

/// Extract the bare `host:port` from a seed URL so it can be compared
/// against server-supplied redirect/topology addresses. Defaults the
/// port to Redis's 6379 when the URL omits it. Returns `None` if the URL
/// can't be parsed or has no host.
fn host_port_from_url(url: &str) -> Option<NodeAddr> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port().unwrap_or(6379);
    Some(normalize_node_addr(&format!("{host}:{port}")))
}

/// Canonicalize a `host:port` string for set membership: the host is
/// case-insensitive, so we lowercase everything up to (and including) the
/// final `:` boundary. We split on the **last** colon so bracketed IPv6
/// literals (`[::1]:6379`) survive intact.
fn normalize_node_addr(addr: &str) -> NodeAddr {
    addr.to_ascii_lowercase()
}

/// CR-CLUSTER-2 / REDIS-SSRF-1: is `addr` an operator-trusted cluster
/// node? Only addresses derived from the configured seeds or learned from
/// an authoritative `CLUSTER SLOTS` topology are trusted; a
/// server-supplied MOVED/ASK or topology entry pointing anywhere else
/// (cloud metadata, localhost ports, internal services) is rejected
/// before we ever connect — and before we replay the cluster password.
fn is_known_node(addr: &str, known: &HashSet<NodeAddr>) -> bool {
    known.contains(&normalize_node_addr(addr))
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

#[cfg(test)]
mod known_node_tests {
    use super::*;
    use std::collections::HashSet;

    fn known(addrs: &[&str]) -> HashSet<NodeAddr> {
        addrs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn known_seed_addr_is_allowed() {
        let k = known(&["127.0.0.1:7000", "127.0.0.1:7001"]);
        assert!(is_known_node("127.0.0.1:7000", &k));
        assert!(is_known_node("127.0.0.1:7001", &k));
    }

    #[test]
    fn attacker_addr_not_in_set_is_rejected() {
        // CR-CLUSTER-2 / REDIS-SSRF-1: a server-supplied MOVED/topology addr
        // pointing at the cloud metadata endpoint (or any host not derived
        // from operator seeds / verified topology) must NOT be a known node.
        let k = known(&["127.0.0.1:7000"]);
        assert!(!is_known_node("169.254.169.254:80", &k)); // cloud metadata
        assert!(!is_known_node("127.0.0.1:5432", &k));     // local Postgres
        assert!(!is_known_node("10.0.0.7:6379", &k));      // internal host
        assert!(!is_known_node("[::1]:6379", &k));         // IPv6 loopback
    }

    #[test]
    fn empty_known_set_rejects_everything() {
        let k: HashSet<NodeAddr> = HashSet::new();
        assert!(!is_known_node("127.0.0.1:7000", &k));
    }

    #[test]
    fn host_case_is_normalized() {
        // Host comparison is case-insensitive (DNS / hostnames); the addr
        // and the seed may differ only in case.
        let k = known(&["node-a:7000"]);
        assert!(is_known_node("NODE-A:7000", &k));
    }

    #[test]
    fn host_port_from_url_extracts_host_colon_port() {
        assert_eq!(host_port_from_url("redis://127.0.0.1:7000").as_deref(), Some("127.0.0.1:7000"));
        assert_eq!(host_port_from_url("redis://:pw@10.0.0.1:6379/3").as_deref(), Some("10.0.0.1:6379"));
        // No explicit port -> default Redis 6379.
        assert_eq!(host_port_from_url("redis://myhost").as_deref(), Some("myhost:6379"));
    }

    #[test]
    fn host_port_from_url_rejects_garbage() {
        assert!(host_port_from_url("").is_none());
        assert!(host_port_from_url("not a url").is_none());
    }
}

#[cfg(test)]
mod url_helper_tests {
    use super::*;

    #[test]
    fn credentials_from_url_plain() {
        let (pw, db) = credentials_from_url("redis://127.0.0.1:6379").unwrap();
        assert!(pw.is_none());
        assert!(db.is_none());
    }

    #[test]
    fn credentials_from_url_with_password() {
        let (pw, db) = credentials_from_url("redis://:secret@127.0.0.1:6379").unwrap();
        assert_eq!(pw.as_deref(), Some("secret"));
        assert!(db.is_none());
    }

    #[test]
    fn credentials_from_url_with_db() {
        let (pw, db) = credentials_from_url("redis://127.0.0.1:6379/3").unwrap();
        assert!(pw.is_none());
        assert_eq!(db, Some(3));
    }

    #[test]
    fn credentials_from_url_with_password_and_db() {
        let (pw, db) = credentials_from_url("redis://:pw@127.0.0.1:6379/7").unwrap();
        assert_eq!(pw.as_deref(), Some("pw"));
        assert_eq!(db, Some(7));
    }

    #[test]
    fn credentials_from_url_non_numeric_db_silently_dropped() {
        // A path like "/foo" isn't a valid DB index — we silently treat
        // it as "no db" rather than erroring. Matches Redis CLI laxness.
        let (pw, db) = credentials_from_url("redis://127.0.0.1/foo").unwrap();
        assert!(pw.is_none());
        assert!(db.is_none());
    }

    #[test]
    fn credentials_from_url_rejects_garbage() {
        // `url::Url::parse` is permissive but rejects strings without any
        // scheme. Empty string is a solid "no" — captures the error path.
        let err = match credentials_from_url("") {
            Ok(_) => panic!("expected parse error on empty URL"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn build_node_url_plain() {
        assert_eq!(build_node_url("host:6379", None, None), "redis://host:6379");
    }

    #[test]
    fn build_node_url_with_password_only() {
        assert_eq!(
            build_node_url("host:6379", Some("s3cret"), None),
            "redis://:s3cret@host:6379"
        );
    }

    #[test]
    fn build_node_url_with_db_only() {
        assert_eq!(
            build_node_url("host:6379", None, Some(4)),
            "redis://host:6379/4"
        );
    }

    #[test]
    fn build_node_url_full() {
        assert_eq!(
            build_node_url("host:6379", Some("pw"), Some(2)),
            "redis://:pw@host:6379/2"
        );
    }
}

#[cfg(test)]
mod connect_unit_tests {
    use super::*;

    #[compio::test]
    async fn empty_seed_list_errors_without_network() {
        let empty: [&str; 0] = [];
        let result = ClusterClient::connect(&empty, 4).await;
        // `Result<ClusterClient, _>::unwrap_err` needs `T: Debug` on the
        // Ok arm; we don't derive it, so match explicitly.
        let err = match result {
            Ok(_) => panic!("expected connect to error on empty seeds"),
            Err(e) => e,
        };
        match err {
            Error::ClusterBootstrap(msg) => assert!(msg.contains("seed")),
            other => panic!("expected ClusterBootstrap, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod set_slot_tests {
    use super::*;

    #[test]
    fn set_slot_in_range_updates_owner() {
        // Positive control: an in-range slot write lands.
        let cc = ClusterClient::for_test(ClusterTopology::empty());
        cc.set_slot(100, "127.0.0.1:7000");
        assert_eq!(cc.slot_owner(100).as_deref(), Some("127.0.0.1:7000"));
    }

    #[test]
    fn set_slot_out_of_range_does_not_panic() {
        // CR-CLUSTER-1: a MOVED with slot 40000 reaches `set_slot(40000, …)`.
        // Pre-fix the raw index `slots[40000] = …` into a 16384-long Vec
        // panics: "index out of bounds: the len is 16384 but the index is
        // 40000". Post-fix the checked write silently ignores it.
        let cc = ClusterClient::for_test(ClusterTopology::empty());
        cc.set_slot(40000, "6.6.6.6:6379"); // must NOT panic
        // The out-of-range write is dropped; no in-range slot is corrupted.
        assert!(cc.slot_owner(0).is_none());
        assert!(cc.slot_owner(16383).is_none());
    }

    #[test]
    fn set_slot_at_num_slots_boundary_is_ignored() {
        // Exactly NUM_SLOTS (16384) is the first invalid index.
        let cc = ClusterClient::for_test(ClusterTopology::empty());
        cc.set_slot(NUM_SLOTS as u16, "6.6.6.6:6379"); // 16384, must NOT panic
        assert!(cc.slot_owner(16383).is_none());
    }
}

#[cfg(test)]
mod ssrf_allowlist_tests {
    use super::*;

    /// A topology mapping slot 0 to one known node, used to seed the
    /// allowlist with a single trusted addr.
    fn topo_with(addr: &str) -> ClusterTopology {
        let mut t = ClusterTopology::empty();
        t.slots[0] = Some(addr.to_string());
        t
    }

    #[compio::test]
    async fn pool_for_unknown_addr_is_rejected_without_connecting() {
        // CR-CLUSTER-2 / REDIS-SSRF-1: pool_for must REFUSE to connect (and
        // thus refuse to replay the cluster password) to a server-supplied
        // address that isn't an operator-trusted node. We point it at the
        // cloud metadata endpoint; pre-fix it would TCP-connect there with
        // creds, post-fix it returns an Err with no connection attempt.
        //
        // The addr is unroutable/blackholed in CI, so a *missing* guard
        // would also eventually fail — but with a connect/timeout Error,
        // not the allowlist rejection we assert below. We assert the error
        // is the allowlist rejection AND that it returns essentially
        // immediately (no multi-second connect attempt).
        let known: HashSet<NodeAddr> = ["127.0.0.1:7000".to_string()].into_iter().collect();
        let cc = ClusterClient::for_test_with_known(topo_with("127.0.0.1:7000"), known);

        let t = std::time::Instant::now();
        let res = cc.pool_for("169.254.169.254:80").await;
        let elapsed = t.elapsed();

        let err = match res {
            Ok(_) => panic!("pool_for must reject an unknown (SSRF) addr, not connect to it"),
            Err(e) => e,
        };
        // The rejection must name the disallowed node, and must NOT be a
        // generic connect/IO error (which would mean we tried to connect).
        let msg = err.to_string();
        assert!(
            matches!(&err, Error::Config(_)) && msg.contains("169.254.169.254:80"),
            "expected an allowlist Config rejection naming the addr, got: {err:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "rejection must be immediate (no connect attempt); took {elapsed:?}"
        );
        // And no pool was cached for the rejected addr.
        assert!(
            cc.inner.borrow().pools.get("169.254.169.254:80").is_none(),
            "no pool may be created for a rejected addr"
        );
    }

    #[compio::test]
    async fn pool_for_known_addr_attempts_connection() {
        // Positive control: a KNOWN addr is allowed past the guard. We use
        // a closed local port so the connect itself fails fast — the point
        // is the error is a *connect/pool* error (we got past the
        // allowlist), NOT the Config allowlist rejection.
        let known: HashSet<NodeAddr> =
            ["127.0.0.1:1".to_string()].into_iter().collect();
        let cc = ClusterClient::for_test_with_known(topo_with("127.0.0.1:1"), known);

        let err = match cc.pool_for("127.0.0.1:1").await {
            Ok(_) => panic!("connect to closed port 1 should fail"),
            Err(e) => e,
        };
        // Got past the allowlist -> a Pool/IO error, not a Config rejection.
        assert!(
            matches!(err, Error::Pool(_)),
            "known addr must reach the connect path (Pool error), got: {err:?}"
        );
    }
}

// =====================================================================
// FIX 3 — CR-CLUSTER-5: confirm R2's automatic dirty barrier covers
// CLUSTER NODE connections.
//
// Audit result (documented by these regression tests): every cluster
// command runs through `send_to_slot`, which acquires a `PooledConn`
// from a per-node `Pool` (`pool_for(addr).acquire()`) and issues both
// the optional `ASKING` and the command via `Client::send_recv`. R2 sets
// `dirty=true` at the start of `send_recv` (before the write) and clears
// it only after a full clean frame; `PooledConn::drop` then refuses to
// return any conn that is dirty or has a non-empty rx. So a timed-out /
// errored / cancelled cluster-node command leaves NO reusable dirty conn
// in that node's pool — the cross-tenant desync is closed for the
// cluster path exactly as for the single-node path. The bootstrap
// `probe` is a one-shot local `Client` that is never pooled or reused,
// so it has no desync surface. NO cluster-specific fix was needed.
// =====================================================================
#[cfg(test)]
mod cluster_dirty_barrier_tests {
    use super::*;
    use compio::io::{AsyncRead, AsyncWriteExt};
    use compio::net::TcpListener;
    use std::time::Duration;

    /// Mock node that reads one command chunk per connection and NEVER
    /// replies (holding the socket open) — drives the command-timeout path
    /// that leaves a conn dirty. Returns `host:port`.
    async fn spawn_silent_node() -> NodeAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock node");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                compio::runtime::spawn(async move {
                    let buf = vec![0u8; 1024];
                    let _ = stream.read(buf).await;
                    compio::time::sleep(Duration::from_secs(30)).await;
                    drop(stream);
                })
                .detach();
            }
        })
        .detach();
        format!("{}:{}", addr.ip(), addr.port())
    }

    /// Mock node that replies to every command with a fixed RESP frame,
    /// then keeps the connection open for reuse. Positive control for the
    /// clean-conn-is-reused path. Returns `host:port`.
    async fn spawn_replying_node(reply: &'static [u8]) -> NodeAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock node");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                compio::runtime::spawn(async move {
                    loop {
                        let buf = vec![0u8; 1024];
                        let compio::BufResult(n, _b) = stream.read(buf).await;
                        match n {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let compio::BufResult(w, _r) = stream.write_all(reply).await;
                                if w.is_err() { break; }
                            }
                        }
                    }
                })
                .detach();
            }
        })
        .detach();
        format!("{}:{}", addr.ip(), addr.port())
    }

    /// Build a single-node cluster client whose only slot owner is `addr`
    /// and whose allowlist trusts `addr` — so `send_to_slot` routes there.
    fn client_routing_all_to(addr: &str) -> ClusterClient {
        let mut topo = ClusterTopology::empty();
        for s in topo.slots.iter_mut() {
            *s = Some(addr.to_string());
        }
        let known: HashSet<NodeAddr> = [addr.to_string()].into_iter().collect();
        ClusterClient::for_test_with_known(topo, known)
    }

    /// CR-CLUSTER-5 regression: a cluster-node command that TIMES OUT must
    /// leave no reusable (dirty) connection in that node's pool. This
    /// drives the exact `pool_for(addr).acquire()` → `send_recv` sequence
    /// `send_to_slot` uses (lines 627-640), then asserts the R2 barrier
    /// dropped the dirty conn.
    #[compio::test]
    async fn timed_out_cluster_node_command_leaves_no_reusable_conn() {
        let node = spawn_silent_node().await;
        let cc = client_routing_all_to(&node);

        // Same per-node pool send_to_slot would use.
        let pool = cc.pool_for(&node).await.expect("node pool opens (allowlisted)");
        {
            let mut conn = pool.acquire().await.expect("acquire node conn");
            // Short timeout so the silent node trips it fast (rather than 5 s).
            conn.set_cmd_timeout(Duration::from_millis(150));
            let err = conn.get("{app}:k").await.expect_err("silent node must time out");
            assert!(
                matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut),
                "expected TimedOut on the cluster node conn, got {err:?}"
            );
            // The conn is dirty (a reply may still be in flight on the wire).
            assert!(conn.is_dirty(), "timed-out cluster-node conn must be dirty");
            // conn drops here -> R2 barrier MUST discard it (not re-pool).
        }
        assert_eq!(
            pool.idle_len(), 0,
            "R2 barrier must discard a dirty cluster-node conn (CR-CLUSTER-5)"
        );
        assert_eq!(pool.busy_count(), 0, "busy must settle to 0 after drop");
    }

    /// Positive control: a CLEAN cluster-node command round-trips through
    /// the REAL `send_to_slot` path and leaves a reusable idle conn in the
    /// node pool (the barrier only discards dirty conns).
    #[compio::test]
    async fn clean_cluster_node_command_reuses_conn() {
        let node = spawn_replying_node(b"$5\r\nhello\r\n").await;
        let cc = client_routing_all_to(&node);

        // Drive the real end-to-end cluster command path.
        let v = cc.get("{app}:k").await.expect("cluster GET");
        assert_eq!(v.as_deref(), Some(b"hello".as_ref()));

        // The node pool must now hold the clean conn for reuse.
        let pool = cc.pool_for(&node).await.expect("node pool");
        assert_eq!(
            pool.idle_len(), 1,
            "a clean cluster-node conn must be returned to its pool for reuse"
        );
        // A second command reuses it (still works, no desync).
        let v2 = cc.get("{app}:k2").await.expect("second cluster GET");
        assert_eq!(v2.as_deref(), Some(b"hello".as_ref()));
    }
}
