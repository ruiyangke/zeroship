import { grant, now, raw, t, table } from "@zeroship/migrate";

// Decision 1 (worker enrollment bootstrap and revocation, option 1A) from the
// workflow-refactor design. An ENROLLER is a deployment unit's bootstrap
// identity: the operator generates one Ed25519 keypair per host or pool and
// mounts the private half into that unit's worker containers; no worker holds a
// `svc/worker` role key. Control records the public half here. Revoking
// an enroller is the boundary against a compromised or decommissioned unit: a
// revoked unit cannot regain equivalent authority by enrolling a fresh
// instance identity, because CONTROL_WORKER_ENROL belongs to the enroller
// role, not to any instance it enrols. See
// crates/zeroship-control/src/worker_enrolment.rs for the enrolment and
// revocation mechanics, and crates/zeroship-core/src/service_identity.rs for
// the endpoint grant.
//
// THIS TABLE IS SHARED WITH DECISION 2 (placement eligibility and capacity),
// which is NOT built yet. An execution zone is an operator-declared set of
// worker deployment units that share creator-side connectivity; decision 2
// adds `apps.execution_zone_id` against this same table so placement can match
// an app's zone to an enroller's zone. A single-VPS deployment has exactly one
// zone, seeded below. Decision 1 needs the column on `worker_enrollers` to
// exist now (the enroller table is decision 1's own surface), so the zones
// table it depends on is created here rather than deferred to decision 2's own
// migration.
export default {
  name: "execution_zones_and_worker_enrollers",
  schema() {
    // ---- execution_zones ---------------------------------------------------
    table("execution_zones", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        name: t.text().notNull(),
        status: t.text().notNull().default("active"),
      },
      primaryKey: ["id"],
    });
    table("execution_zones", { schema: "zeroship" })
      .check("execution_zones_id_shape")
      .add({ expr: (col) => col("id").regex("^ezn_[0-9a-z]{25}$") });
    table("execution_zones", { schema: "zeroship" })
      .unique("execution_zones_name_uq")
      .add({ columns: ["name"] });
    // Decision 2 has not defined a lifecycle beyond "active" yet. The set is
    // closed and singleton-valued on purpose: widening it is that decision's
    // call to make, not an accident of this migration leaving it open.
    table("execution_zones", { schema: "zeroship" })
      .check("execution_zones_status_check")
      .add({ expr: (col) => col("status").in(["active"]) });

    raw({
      sql: 'ALTER TABLE "zeroship"."execution_zones" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });

    // Control's enroller import (crates/zeroship-control/src/worker_enrolment.rs,
    // `import_enrollers`) resolves each enroller's zone NAME to its id here, and
    // refuses a file naming a zone this deployment does not declare. Read only:
    // zones are declared by migrations, never by Control.
    grant({
      privileges: ["select"],
      on: { kind: "table", schema: "zeroship", names: ["execution_zones"] },
      to: ["zeroship_control"],
    });

    // The deployment's single execution zone is seeded by the next migration,
    // 20260914000050_execution_zones_default_zone.ts, as DATA rather than
    // here: `schema()` accepts DDL only, and the host recorder refuses a
    // recorded DML operation inside it (`zero-migrate: host recorder: schema()
    // recorded the DML operation insert; move this operation to data() and
    // declare inverse() or irreversible`, measured against this exact file).

    // ---- worker_enrollers ---------------------------------------------------
    table("worker_enrollers", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        public_key: t.bytes().notNull(),
        // PLAIN text here, deliberately with no `.references()`. A same-file
        // foreign key to `execution_zones.id` would be checked by the engine
        // against a catalog snapshot taken before this file's own COLLATE "C"
        // fix on that column runs, and the lowering refuses the pair -- the
        // exact constraint `20260906000200_apps_project_ownership_key.ts`
        // documents and works around by splitting across files. Decision 2
        // owns turning this into a real foreign key when it lands; until
        // then the column is intent, enforced by nothing but the writer.
        execution_zone_id: t.text().notNull(),
        // `active` | `revoked`. Revoked is terminal: there is no way back to
        // `active` for a given id, only a newly provisioned enroller.
        status: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        // The guarded-no-op-update lock target (the `lock_scope` pattern in
        // crates/zeroship-workflow-manager/src/queue.rs::lock_scope): an
        // enrolment holds this row's lock for the span of its own insert, and
        // a concurrent revocation's first UPDATE waits on the same lock, so
        // revocation always observes every enrolment that committed before it.
        lock_version: t.int().notNull().default(0),
      },
      primaryKey: ["id"],
    });
    table("worker_enrollers", { schema: "zeroship" })
      .check("worker_enrollers_id_shape")
      .add({ expr: (col) => col("id").regex("^wen_[0-9a-z]{25}$") });
    table("worker_enrollers", { schema: "zeroship" })
      .check("worker_enrollers_public_key_shape")
      .add({ expr: (col) => col("public_key").length().eq(32) });
    table("worker_enrollers", { schema: "zeroship" })
      .unique("worker_enrollers_public_key_uq")
      .add({ columns: ["public_key"] });
    table("worker_enrollers", { schema: "zeroship" })
      .check("worker_enrollers_status_check")
      .add({ expr: (col) => col("status").in(["active", "revoked"]) });

    raw({
      sql: 'ALTER TABLE "zeroship"."worker_enrollers" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."worker_enrollers" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "a future foreign key to execution_zones.id needs matching bytewise collation",
    });

    // The control plane resolves an enroller's active key on every enrolment
    // request (crate::worker_enrolment::active_enroller_public_key) and locks
    // and updates this row inside zeroship.enrol_worker_instance
    // (db/migrations-ts/20260914000100_worker_instances_enroller_binding.ts).
    // No DELETE: a revoked row is retained, never erased, the same rule
    // worker_instances already follows.
    grant({
      privileges: ["select", "insert", "update"],
      on: { kind: "table", schema: "zeroship", names: ["worker_enrollers"] },
      to: ["zeroship_control"],
    });
  },
};
