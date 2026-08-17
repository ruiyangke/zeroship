//! Live PostgreSQL: an out-of-band change to a function, a policy or a trigger is
//! reported as drift.
//!
//! The unit half of this lives in `vendor_object_drift.rs`, which builds both
//! snapshots by hand and can therefore prove what the DIFF does but nothing about
//! what the CATALOG READ returns. This file is the other half: every snapshot here
//! comes from `snapshot_schema` against a real server, so it fails if the three
//! queries stop finding the objects, if the `polcmd` / `tgtype` decoding is wrong,
//! or if the internal-trigger and extension-function exclusions stop holding.
//!
//! WHY IT MATTERS MOST FOR POLICIES. `setRls` turning row-level security on is
//! already visible to drift, but a table with RLS enabled and no policy denies
//! everything, and a table whose policy was widened by hand allows everything. The
//! enforcement lives in `pg_policy`, and until this landed nothing read it.
//!
//! The false-drift control is NOT here - it is `fold_roundtrip_pg`, which folds the
//! authored ops offline and diffs them against this same live read. That is the
//! comparison PostgreSQL's normalisation would break, and it is the one that has to
//! stay clean.

mod support;

use support::PgDevSession;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{PolicyCmd, TriggerEvent, TriggerTiming};
use zero_migrate::{diff_snapshots, snapshot_schema};

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "vendor_drift_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

#[compio::test]
async fn live_postgres_reports_a_hand_dropped_function_policy_and_trigger() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    let result: Result<(), String> = async {
        session
            .batch(&format!(
                "CREATE SCHEMA \"{schema}\"; \
                 CREATE TABLE \"{schema}\".orders ( \
                    id int PRIMARY KEY, owner text, total int); \
                 CREATE TABLE \"{schema}\".lines ( \
                    id int PRIMARY KEY, order_id int REFERENCES \"{schema}\".orders(id)); \
                 ALTER TABLE \"{schema}\".orders ENABLE ROW LEVEL SECURITY; \
                 CREATE POLICY orders_owner ON \"{schema}\".orders FOR SELECT \
                    USING (owner = current_user AND total > 0); \
                 CREATE FUNCTION \"{schema}\".audit(x int) RETURNS int \
                    LANGUAGE sql AS $$ SELECT x $$; \
                 CREATE FUNCTION \"{schema}\".stamp() RETURNS trigger \
                    LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$; \
                 CREATE TRIGGER orders_stamp AFTER INSERT OR UPDATE ON \"{schema}\".orders \
                    FOR EACH ROW WHEN (NEW.total > 0) \
                    EXECUTE FUNCTION \"{schema}\".stamp()"
            ))
            .await
            .map_err(|error| format!("create the fixture schema: {error}"))?;

        let before = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("snapshot the live schema: {error}"))?;
        let vendor = before
            .vendor_objects
            .as_ref()
            .ok_or("a PostgreSQL snapshot must speak about vendor objects")?;

        // (1) The catalog read found them, decoded them, and canonicalised the
        //     signature. `audit(x int)` is stored as `integer`; a snapshot that kept
        //     the catalog spelling would compare unequal to the authored `int`
        //     forever, so this asserts the FOLDED form, not the catalog's.
        let functions: Vec<String> = vendor
            .functions
            .iter()
            .map(|key| format!("{}({})", key.name, key.arg_types.join(",")))
            .collect();
        if functions != vec!["audit(int4)".to_string(), "stamp()".to_string()] {
            return Err(format!("unexpected function set: {functions:?}"));
        }

        let policy = vendor
            .policies
            .values()
            .next()
            .ok_or("the live read must find the policy")?;
        if vendor.policies.len() != 1
            || policy.for_cmd != PolicyCmd::Select
            || !policy.to.is_empty()
            || !policy.permissive
        {
            return Err(format!("unexpected policy identity: {policy:?}"));
        }

        // (2) The internal-trigger exclusion holds. `lines.order_id REFERENCES
        //     orders(id)` makes PostgreSQL create four `RI_ConstraintTrigger_*` rows;
        //     without the `tgisinternal` filter every drift report would name them.
        if vendor.triggers.len() != 1 {
            return Err(format!(
                "only the authored trigger may be reported, got {:?}",
                vendor.triggers.keys().collect::<Vec<_>>()
            ));
        }
        let trigger = vendor
            .triggers
            .values()
            .next()
            .ok_or("the live read must find the trigger")?;
        if trigger.timing != TriggerTiming::After
            || trigger.events != vec![TriggerEvent::Insert, TriggerEvent::Update]
        {
            return Err(format!("unexpected trigger identity: {trigger:?}"));
        }

        // (3) A second read of an UNCHANGED schema is clean. Two catalog reads agree
        //     trivially on text, but not on anything the decoding does per row, so
        //     this catches a non-deterministic role aggregate or event ordering.
        let unchanged = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("re-snapshot the live schema: {error}"))?;
        let clean = diff_snapshots(&before, &unchanged);
        if !clean.is_clean() {
            return Err(format!("an untouched schema drifted: {clean:#?}"));
        }

        // (4) THE DEFECT. Drop all three by hand, exactly as an operator would at a
        //     psql prompt, and re-read. Before this work the report was empty and the
        //     table was left with row-level security on and nothing enforcing it.
        session
            .batch(&format!(
                "DROP POLICY orders_owner ON \"{schema}\".orders; \
                 DROP TRIGGER orders_stamp ON \"{schema}\".orders; \
                 DROP FUNCTION \"{schema}\".audit(int)"
            ))
            .await
            .map_err(|error| format!("drop the objects out of band: {error}"))?;

        let after = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("snapshot after the out-of-band drops: {error}"))?;
        let drift = diff_snapshots(&before, &after);
        let mut missing = drift.missing_objects.clone();
        missing.sort();
        if missing
            != vec![
                format!("function {schema}.audit(int4)"),
                format!("policy orders_owner on {schema}.orders"),
                format!("trigger orders_stamp on {schema}.orders"),
            ]
        {
            return Err(format!(
                "all three hand-dropped objects must be reported: {drift:#?}"
            ));
        }

        // (5) And the mirror: reading the pre-drop snapshot as the LIVE side reports
        //     the same three as out-of-band creations.
        let mut unexpected = diff_snapshots(&after, &before).unexpected_objects;
        unexpected.sort();
        if unexpected
            != vec![
                format!("function {schema}.audit(int4)"),
                format!("policy orders_owner on {schema}.orders"),
                format!("trigger orders_stamp on {schema}.orders"),
            ]
        {
            return Err(format!(
                "out-of-band creation must be reported: {unexpected:?}"
            ));
        }

        Ok(())
    }
    .await;

    // The guard drops the schema on the way out, including on an unwind. Assert
    // afterwards so a failure message survives.
    if let Err(error) = result {
        panic!("{error}");
    }
}

#[compio::test]
async fn live_postgres_reports_a_policy_narrowed_out_of_band() {
    // The altered bucket, which the missing/unexpected buckets cannot see: the
    // policy still exists under the same name, but no longer applies to the same
    // command or the same roles. The predicate is NOT part of this - PostgreSQL
    // re-deparses it, so comparing it would report drift on every project.
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    let result: Result<(), String> = async {
        session
            .batch(&format!(
                "CREATE SCHEMA \"{schema}\"; \
                 CREATE TABLE \"{schema}\".orders (id int PRIMARY KEY, owner text); \
                 ALTER TABLE \"{schema}\".orders ENABLE ROW LEVEL SECURITY; \
                 CREATE POLICY orders_owner ON \"{schema}\".orders \
                    FOR ALL TO PUBLIC USING (owner = current_user)"
            ))
            .await
            .map_err(|error| format!("create the fixture schema: {error}"))?;
        let before = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("snapshot the live schema: {error}"))?;

        session
            .batch(&format!(
                "DROP POLICY orders_owner ON \"{schema}\".orders; \
                 CREATE POLICY orders_owner ON \"{schema}\".orders AS RESTRICTIVE \
                    FOR SELECT TO postgres USING (owner = current_user)"
            ))
            .await
            .map_err(|error| format!("replace the policy out of band: {error}"))?;
        let after = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("re-snapshot the live schema: {error}"))?;

        let drift = diff_snapshots(&before, &after);
        let reported: Vec<(String, String, String)> = drift
            .altered_objects
            .iter()
            .map(|a| (a.field.clone(), a.expected.clone(), a.actual.clone()))
            .collect();
        if reported
            != vec![
                (
                    "command".to_string(),
                    "ALL".to_string(),
                    "SELECT".to_string(),
                ),
                (
                    "permissive".to_string(),
                    "true".to_string(),
                    "false".to_string(),
                ),
                (
                    "roles".to_string(),
                    "PUBLIC".to_string(),
                    "postgres".to_string(),
                ),
            ]
        {
            return Err(format!(
                "the narrowed policy must report command, permissive and roles: {drift:#?}"
            ));
        }
        Ok(())
    }
    .await;

    if let Err(error) = result {
        panic!("{error}");
    }
}
