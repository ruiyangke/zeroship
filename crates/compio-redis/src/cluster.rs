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
}

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
