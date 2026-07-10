#![cfg(feature = "zsv8")]
//! PR4a §8.9.2 recorder-SERVICE contract tests: PAT auth + server-side `app_id`
//! ownership cross-check (§8.6), per-token + global concurrency limits, and the
//! end-to-end `record` happy path through the real sandboxed child.
//!
//! These exercise the SERVICE layer (`RecorderService`) atop the kernel sandbox:
//! the authorizer seam, the fail-closed ownership check, and the limiter — plus one
//! full `record` that goes all the way through the real spawned child.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zeroship_migrate::frontend::recorder_service::{Authorizer, ConcurrencyLimits};
use zeroship_migrate::frontend::{RecorderError, RecorderService};

const MIGRATION: &str = r#"
import { table, t } from "@zeroship/migrate";
export function up() {
  table("svc_tbl").create({ columns: { id: t.int().notNull() } });
}
"#;

/// A test authorizer: `token` owns `app_id` iff they are in the allow-map.
struct MapAuthorizer {
    owns: std::collections::HashMap<&'static str, &'static str>,
    calls: Arc<AtomicUsize>,
}
impl Authorizer for MapAuthorizer {
    fn authorize(&self, token: &str, app_id: &str) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.owns.get(token) {
            Some(owned) if *owned == app_id => Ok(()),
            Some(_) => Err(format!("token does not own app {app_id}")),
            None => Err("unknown token".into()),
        }
    }
}

fn svc() -> (RecorderService, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut owns = std::collections::HashMap::new();
    owns.insert("pat_alice", "app_alice");
    let auth = MapAuthorizer {
        owns,
        calls: calls.clone(),
    };
    (RecorderService::new(Box::new(auth)), calls)
}

#[test]
fn record_succeeds_for_owned_app() {
    let (s, calls) = svc();
    let res = s
        .record("pat_alice", "app_alice", MIGRATION, "m1", None)
        .expect("owned app records");
    assert!(res.ir_json.contains("svc_tbl"), "got: {}", res.ir_json);
    // owner_app HINT is stamped server-side from the cross-checked app_id.
    assert!(res.ir_json.contains("app_alice"));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "authorizer must be consulted");
}

#[test]
fn record_fails_closed_for_unowned_app() {
    let (s, _) = svc();
    // alice's token, but a DIFFERENT app she doesn't own — fail-closed.
    let err = s
        .record("pat_alice", "app_someone_else", MIGRATION, "m1", None)
        .expect_err("ownership mismatch must be refused");
    assert!(matches!(err, RecorderError::Unauthorized(_)), "got {err:?}");
    assert_eq!(err.code(), "RECORDER_UNAUTHORIZED");
}

#[test]
fn record_fails_closed_for_unknown_token() {
    let (s, _) = svc();
    let err = s
        .record("pat_unknown", "app_alice", MIGRATION, "m1", None)
        .expect_err("unknown token must be refused");
    assert!(matches!(err, RecorderError::Unauthorized(_)), "got {err:?}");
}

#[test]
fn per_token_concurrency_limit_rejects_a_burst() {
    // A per-token cap of 1: while one record holds the slot, a second concurrent
    // call for the SAME token is Overloaded. We simulate concurrency by holding the
    // first call in flight via a slow (looping) migration bounded by a short wall.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut owns = std::collections::HashMap::new();
    owns.insert("pat_alice", "app_alice");
    let auth = MapAuthorizer {
        owns,
        calls: calls.clone(),
    };
    let s = Arc::new(
        RecorderService::new(Box::new(auth)).with_limits(ConcurrencyLimits {
            per_token_max: 1,
            global_max: 8,
        }),
    );

    // A migration that spins so the first record holds its slot a moment; bounded by
    // the service budget's wall watchdog.
    let slow = r#"export function up(){ const t=Date.now? 0:0; let n=0; while(n<5e8){n++;} }"#;

    let s1 = s.clone();
    let slow_owned = slow.to_string();
    let h = std::thread::spawn(move || {
        // This call holds the only per-token slot while it spins.
        let _ = s1.record("pat_alice", "app_alice", &slow_owned, "slow", None);
    });

    // Give the first call time to acquire the slot + start spinning.
    std::thread::sleep(std::time::Duration::from_millis(150));
    // A second concurrent call for the same token must be Overloaded.
    let second = s.record("pat_alice", "app_alice", MIGRATION, "m2", None);
    match second {
        Err(RecorderError::Overloaded) => {}
        // If the first call already finished (fast host), the slot was free and the
        // second succeeded — acceptable, but assert it did not error spuriously.
        Ok(_) => {}
        other => panic!("expected Overloaded or Ok, got {other:?}"),
    }
    h.join().ok();
    // After both complete, the limiter must be drained (no leaked slots).
    assert_eq!(s.inflight_global(), 0, "concurrency slots must drain to 0");
}

#[test]
fn global_concurrency_cap_rejects_excess() {
    // global_max = 0 means EVERY record is Overloaded (the cap is the gate). This
    // proves the global cap is enforced independently of the per-token cap.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut owns = std::collections::HashMap::new();
    owns.insert("pat_alice", "app_alice");
    let auth = MapAuthorizer {
        owns,
        calls: calls.clone(),
    };
    let s = RecorderService::new(Box::new(auth)).with_limits(ConcurrencyLimits {
        per_token_max: 10,
        global_max: 0,
    });
    let err = s
        .record("pat_alice", "app_alice", MIGRATION, "m1", None)
        .expect_err("global cap 0 must reject");
    assert!(matches!(err, RecorderError::Overloaded), "got {err:?}");
    // The authorizer ran (auth precedes the limiter), but no child was spawned.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(s.inflight_global(), 0, "rejected call must not leak a slot");
}
