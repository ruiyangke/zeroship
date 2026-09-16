import { grant, now, raw, t, table } from "@zeroship/migrate";

// The worker JOIN trust anchor. A JOIN SIGNER is the authority that decides a
// worker should exist: one Ed25519 keypair whose PRIVATE half stays with
// whoever provisions workers -- the operator, or on a single host the control
// plane minting for its own zone -- and whose public half Control records here,
// in advance, along with the execution zones that signer may mint for.
//
// The signing key therefore never reaches a worker: what a worker holds is a
// short-lived, use-capped TOKEN it cannot mint anything with. The document
// recorded here carries PUBLIC keys only, so possession of it admits nobody.
// One signer covers as many deployment units as its zones do, so bringing up
// another unit is not a Control-side operation at all.
//
// See crates/zeroship-control/src/worker_join.rs for the verification and the
// import, crates/zeroship-core/src/worker_join.rs for the token and the
// documents, and docs/proposals/2026-09-11-workflow-worker.md ("Enrollment
// bootstrap and revocation") for the contract.
//
// THE ZONES TABLE IS SHARED WITH DECISION 2 (placement eligibility and
// capacity), which is NOT built yet. An execution zone is an operator-declared
// set of worker deployment units that share creator-side connectivity;
// decision 2 adds `apps.execution_zone_id` against this same table so placement
// can match an app's zone to a worker's. A single-VPS deployment has exactly
// one zone, seeded by the next migration.
export default {
  name: "execution_zones_and_join_signers",
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

    // Control's signer import (crates/zeroship-control/src/worker_join.rs,
    // `import_join_signers`) resolves each permitted zone NAME to its id here,
    // and refuses a file naming a zone this deployment does not declare. Read
    // only: zones are declared by migrations, never by Control.
    grant({
      privileges: ["select"],
      on: { kind: "table", schema: "zeroship", names: ["execution_zones"] },
      to: ["zeroship_control"],
    });

    // The deployment's single execution zone is seeded by the next migration,
    // 20260914000450_execution_zones_default_zone.ts, as DATA rather than
    // here: `schema()` accepts DDL only, and the host recorder refuses a
    // recorded DML operation inside it.

    // ---- worker_join_signers ------------------------------------------------
    table("worker_join_signers", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        public_key: t.bytes().notNull(),
        // `active` | `revoked`. Revoked is terminal: there is no way back to
        // `active` for a given id, only a newly provisioned signer. Two
        // operator verbs reach it and they differ in what ELSE they do --
        // zeroship.rotate_worker_join_signer leaves the fleet running, and
        // zeroship.purge_worker_join_signer retires every instance the signer
        // admitted. Both are in 20260914000500_worker_join_bindings.ts.
        status: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        // The guarded-no-op-update lock target: a join holds this row's lock
        // for the span of its own insert, and a concurrent revocation's first
        // UPDATE waits on the same lock, so revocation always observes every
        // join that committed before it.
        lock_version: t.int().notNull().default(0),
      },
      primaryKey: ["id"],
    });
    table("worker_join_signers", { schema: "zeroship" })
      .check("worker_join_signers_id_shape")
      .add({ expr: (col) => col("id").regex("^wjs_[0-9a-z]{25}$") });
    // Ed25519 public keys are exactly 32 octets (RFC 8032 section 5.1.5).
    table("worker_join_signers", { schema: "zeroship" })
      .check("worker_join_signers_public_key_shape")
      .add({ expr: (col) => col("public_key").length().eq(32) });
    // A key names exactly one signer for its life. Without this, two signer
    // rows could share a key and revoking one would leave the other minting.
    table("worker_join_signers", { schema: "zeroship" })
      .unique("worker_join_signers_public_key_uq")
      .add({ columns: ["public_key"] });
    table("worker_join_signers", { schema: "zeroship" })
      .check("worker_join_signers_status_check")
      .add({ expr: (col) => col("status").in(["active", "revoked"]) });

    raw({
      sql: 'ALTER TABLE "zeroship"."worker_join_signers" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });

    // Control resolves a signer's active key and permitted zones on every join
    // (crate::worker_join::trusted_join_signer), imports signers at startup,
    // and locks this row inside zeroship.join_worker_instance. No DELETE: a
    // revoked row is retained, never erased, the same rule worker_instances
    // already follows.
    grant({
      privileges: ["select", "insert", "update"],
      on: { kind: "table", schema: "zeroship", names: ["worker_join_signers"] },
      to: ["zeroship_control"],
    });
  },
};
