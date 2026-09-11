use crate::cdc::{ChangeEvent, ChangeOp};
use crate::driver::Session;
use crate::tx_lanes::TxLanes;
use std::collections::HashMap;
fn dummy_event(collection: &str) -> ChangeEvent {
    ChangeEvent {
        app_id: "app_t".into(),
        collection: collection.into(),
        op: ChangeOp::Insert,
        pk: Some("1".to_string()),
        changed_columns: Vec::new(),
        new_tuple: HashMap::new(),
        old_tuple: None,
    }
}

#[test]
fn frame_emit_marks_never_discard_an_enclosing_frames_events() {
    let mut lanes = TxLanes::new();
    // No frame is open, so there is no watermark to pop.
    assert_eq!(
        lanes.pop_frame_emit_mark("app_t"),
        None,
        "no open frame yields no watermark"
    );

    // Discarding with no watermark on the stack must leave the buffer ALONE
    // rather than truncating to zero, which would drop the enclosing
    // frame's events. Over-publishing is a bug; silently dropping a
    // committed row's event is a worse one.
    lanes.push_pending_emit(dummy_event("c1"));
    lanes.discard_frame_effects("app_t");
    assert_eq!(
        lanes
            .by_app()
            .get("app_t")
            .map(|lane| lane.pending_emits().len()),
        Some(1),
        "a missing watermark must not discard the enclosing frame's events"
    );

    // A real watermark discards exactly the frame's own events: the mark is
    // taken when the frame opens, so everything queued after it is the
    // frame's and everything before it is the parent's.
    lanes.push_frame_emit_mark("app_t");
    lanes.push_pending_emit(dummy_event("c2"));
    lanes.discard_frame_effects("app_t");
    let kept = lanes.by_app().get("app_t").expect("lane").pending_emits();
    assert_eq!(kept.len(), 1, "truncate to the frame's watermark");
    assert_eq!(
        kept[0].collection, "c1",
        "the enclosing frame's event survives"
    );

    // `discard_frame_effects` does NOT pop: a rolled-back frame is not
    // closed until its RELEASE lands, and that is the call that pops.
    assert_eq!(lanes.pop_frame_emit_mark("app_t"), Some(1));
    assert_eq!(lanes.pop_frame_emit_mark("app_t"), None);
}

/// `retire_transaction` drops the watermarks that died with the frames.
///
/// Leaving them would let the next transaction's first savepoint pop a
/// stale mark and truncate that transaction's buffer to an unrelated
/// length.
#[test]
fn retiring_a_transaction_drops_its_frame_watermarks() {
    let mut lanes = TxLanes::new();
    lanes.push_pending_emit(dummy_event("c1"));
    lanes.push_frame_emit_mark("app_t");
    lanes.retire_transaction("app_t");
    assert_eq!(
        lanes.pop_frame_emit_mark("app_t"),
        None,
        "a retired transaction leaves no watermark behind"
    );
}

// ----- PENDING_EMITS state machine -----------------------------------

#[test]
fn pending_emits_start_empty() {
    let lanes = TxLanes::new();
    assert!(
        lanes
            .by_app()
            .values()
            .all(|lane| lane.pending_emits().is_empty())
    );
}

#[test]
fn push_pending_emit_allocates_slot_lazily() {
    // dummy_event tags app_id "app_t"; the queue keys on that.
    let mut lanes = TxLanes::new();
    assert!(
        lanes
            .by_app()
            .values()
            .all(|lane| lane.pending_emits().is_empty())
    );
    lanes.push_pending_emit(dummy_event("c1"));
    assert!(lanes.by_app().contains_key("app_t"));
    assert_eq!(
        lanes.by_app().get("app_t").unwrap().pending_emits().len(),
        1
    );
}

#[test]
fn push_pending_emit_accumulates() {
    let mut lanes = TxLanes::new();
    lanes.push_pending_emit(dummy_event("c1"));
    lanes.push_pending_emit(dummy_event("c2"));
    lanes.push_pending_emit(dummy_event("c3"));
    let evs = lanes.by_app().get("app_t").unwrap().pending_emits();
    assert_eq!(evs.len(), 3);
    assert_eq!(evs[0].collection, "c1");
    assert_eq!(evs[1].collection, "c2");
    assert_eq!(evs[2].collection, "c3");
}

#[test]
fn drain_pending_emits_returns_and_clears() {
    let mut lanes = TxLanes::new();
    lanes.push_pending_emit(dummy_event("c1"));
    lanes.push_pending_emit(dummy_event("c2"));
    let drained = lanes.drain_pending_emits_for("app_t");
    assert_eq!(drained.len(), 2);
    // After drain the app's queue is cleared — subsequent pushes
    // re-allocate.
    assert!(
        lanes
            .by_app()
            .get("app_t")
            .is_none_or(|lane| lane.pending_emits().is_empty())
    );
}

#[test]
fn drain_pending_emits_on_empty_returns_empty_vec() {
    let mut lanes = TxLanes::new();
    let drained = lanes.drain_pending_emits_for("app_t");
    assert!(drained.is_empty());
    assert!(
        lanes
            .by_app()
            .values()
            .all(|lane| lane.pending_emits().is_empty())
    );
}

#[test]
fn drain_then_push_starts_fresh() {
    let mut lanes = TxLanes::new();
    lanes.push_pending_emit(dummy_event("c1"));
    let _ = lanes.drain_pending_emits_for("app_t");
    lanes.push_pending_emit(dummy_event("c2"));
    let evs = lanes.by_app().get("app_t").unwrap().pending_emits();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].collection, "c2");
}

#[test]
fn clear_pending_emits_drops_without_returning() {
    let mut lanes = TxLanes::new();
    lanes.push_pending_emit(dummy_event("c1"));
    lanes.push_pending_emit(dummy_event("c2"));
    lanes.clear_pending_emits_for("app_t");
    assert!(
        lanes
            .by_app()
            .get("app_t")
            .is_none_or(|lane| lane.pending_emits().is_empty())
    );
    // A subsequent drain returns empty (queue is gone).
    assert!(lanes.drain_pending_emits_for("app_t").is_empty());
}

#[test]
fn clear_pending_emits_on_empty_is_idempotent() {
    let mut lanes = TxLanes::new();
    lanes.clear_pending_emits_for("app_t");
    lanes.clear_pending_emits_for("app_t");
    assert!(
        lanes
            .by_app()
            .values()
            .all(|lane| lane.pending_emits().is_empty())
    );
}

// ----- SEC-1: per-app scoping of the tx / savepoint / emit slots

// A worker thread multiplexes up to ~200 isolates (one per app).
// Every slot below used to be a single per-OS-thread cell shared by
// ALL co-resident apps: app B could observe and drain app A's
// parked transaction client (running B's SQL inside A's
// transaction, snapshot, and per-app role), corrupt A's savepoint
// bookkeeping, and drain A's pre-commit broker queue. These tests
// pin the per-app ownership contract.

fn run_async<F: std::future::Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime build")
        .block_on(f)
}

/// Build a parkable [`Session`] without a live Postgres: the
/// embedded SQLite backend hands out a real session handle from a
/// tempdir-backed store. No SQL is executed on it — these tests
/// exercise the slot state machine only.
async fn sqlite_tx_conn(dir: &tempfile::TempDir) -> Session {
    use crate::tests::fixtures::DatabaseFixture;
    let backend = crate::backend_selection::new_sqlite_backend(
        std::path::PathBuf::from(dir.path()),
        crate::encryption::ProjectKeySource::unavailable(),
    )
    .expect("open sqlite backend");
    let client = backend
        .fixture_session("slot_state_probe")
        .await
        .expect("acquire sqlite client");
    Session::new(client)
}

#[test]
fn sec1_tx_parked_by_app_a_is_invisible_and_untakable_for_app_b() {
    run_async(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut lanes = TxLanes::new();
        let prev = lanes.install_tx_client("app_a", sqlite_tx_conn(&dir).await);
        assert!(prev.is_none(), "tx slot must start empty");

        assert!(
            lanes.has_tx_for("app_a"),
            "the owning app must see its own parked tx",
        );
        assert!(
            !lanes.has_tx_for("app_b"),
            "SEC-1: app_b must NOT observe app_a's parked tx \
                 (a hit here routes app_b's SQL onto app_a's tx connection)",
        );
        assert!(
            lanes.take_tx_client_for("app_b").is_none(),
            "SEC-1: app_b must NOT be able to drain app_a's tx client",
        );
        assert!(
            lanes.has_tx_for("app_a"),
            "app_a's parked tx must survive app_b's probe unmodified",
        );
        // The owner can still take its own client back out.
        assert!(
            lanes.take_tx_client_for("app_a").is_some(),
            "the owner must still be able to take its own tx client",
        );
    });
}

#[test]
fn sec1_frame_watermarks_are_scoped_per_app() {
    run_async(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut lanes = TxLanes::new();
        lanes.install_tx_client("app_a", sqlite_tx_conn(&dir).await);

        lanes.push_frame_emit_mark("app_a");
        lanes.push_frame_emit_mark("app_a");
        assert_eq!(
            lanes.pop_frame_emit_mark("app_b"),
            None,
            "SEC-1: app_b must not inherit app_a's frame watermarks \
                 (a shared stack lets one app truncate the other's queue)",
        );

        // app_b settling its own (nonexistent) transaction must not clobber
        // app_a's live frame bookkeeping.
        lanes.retire_transaction("app_b");
        assert_eq!(
            lanes.pop_frame_emit_mark("app_a"),
            Some(0),
            "SEC-1: app_b's settle must not drop app_a's frame watermarks",
        );
        assert_eq!(lanes.pop_frame_emit_mark("app_a"), Some(0));
        assert_eq!(lanes.pop_frame_emit_mark("app_a"), None);
    });
}

#[test]
fn sec1_pending_emits_drain_is_scoped_per_app() {
    let mut lanes = TxLanes::new();
    let mut ev_a = dummy_event("orders");
    ev_a.app_id = "app_a".to_string();
    let mut ev_b = dummy_event("messages");
    ev_b.app_id = "app_b".to_string();
    lanes.push_pending_emit(ev_a);
    lanes.push_pending_emit(ev_b);

    let drained_b = lanes.drain_pending_emits_for("app_b");
    assert_eq!(
        drained_b.len(),
        1,
        "SEC-1: app_b's commit drain must only fire app_b's queued events",
    );
    assert_eq!(drained_b[0].app_id, "app_b");

    let drained_a = lanes.drain_pending_emits_for("app_a");
    assert_eq!(
        drained_a.len(),
        1,
        "SEC-1: app_a's queued events must survive app_b's drain \
             (firing them early breaks the Gap-B pre-commit fence)",
    );
    assert_eq!(drained_a[0].app_id, "app_a");
}
