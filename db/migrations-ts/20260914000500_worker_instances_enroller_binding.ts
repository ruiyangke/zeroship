import { createFunction, raw, t, table } from "@zeroship/migrate";

// The other half of 20260914000400_execution_zones_and_worker_enrollers.ts,
// split into its own file for the same reason that one documents for
// `worker_enrollers.execution_zone_id`: a foreign key authored in the SAME
// file as the COLLATE "C" fix on its target is checked against a pre-file
// catalog snapshot and refused. `worker_enrollers.id` is fixed in the previous
// migration, so this file's foreign key to it is safe.
//
// This file spends the enroller table: it binds each worker instance to the
// enroller that admitted it, makes enrolment idempotent on the instance's
// public key, and adds the two functions that ARE the option-1A mechanism --
// crates/zeroship-control/src/worker_enrolment.rs calls the first on every
// enrolment, and an operator calls the second, by hand, to revoke a unit.
export default {
  name: "worker_instances_enroller_binding",
  schema() {
    table("worker_instances", { schema: "zeroship" })
      .column("enroller_id")
      .add({ type: t.text().notNull() });

    table("worker_instances", { schema: "zeroship" })
      .foreignKey("worker_instances_enroller_fk")
      .add({
        columns: ["enroller_id"],
        references: { table: "worker_enrollers", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
      });

    raw({
      sql: 'ALTER TABLE "zeroship"."worker_instances" ALTER COLUMN "enroller_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });

    // Closes the double-row-per-lost-reply gap named in the module header of
    // worker_enrolment.rs: a retried enrolment presenting the SAME instance
    // public key must return the SAME instance id rather than minting a
    // second row. zeroship.enrol_worker_instance below is what makes that
    // idempotent on this index rather than racy.
    table("worker_instances", { schema: "zeroship" })
      .unique("worker_instances_public_key_uq")
      .add({ columns: ["public_key"] });

    // Re-issue the frozen-columns trigger body so `enroller_id` freezes with
    // everything else an instance enrols with. `replace: true` because this
    // function already exists (20260907000300_worker_instances.ts) -- a
    // stored plpgsql body is a string PostgreSQL never rewrites when a column
    // is added, so the un-replaced function would silently let `enroller_id`
    // move after enrolment.
    createFunction({
      schema: "zeroship",
      name: "worker_instances_reject_frozen_change",
      returns: "trigger",
      language: "procedural",
      replace: true,
      body:
        "BEGIN\n"
        + "  IF NEW.id <> OLD.id\n"
        + "     OR NEW.enroller_id <> OLD.enroller_id\n"
        + "     OR NEW.ring_key <> OLD.ring_key\n"
        + "     OR NEW.public_key <> OLD.public_key\n"
        + "     OR NEW.advertise_host <> OLD.advertise_host\n"
        + "     OR NEW.advertise_port <> OLD.advertise_port\n"
        + "     OR NEW.registered_at <> OLD.registered_at THEN\n"
        + "    RAISE EXCEPTION 'worker_instances identity and address are frozen at enrolment; "
        + "only status may change'\n"
        + "      USING ERRCODE = 'check_violation';\n"
        + "  END IF;\n"
        + "  RETURN NEW;\n"
        + "END;",
    });

    // THE OPTION-1A CRITICAL SECTION. One statement, so one server-side
    // transaction: `crates/zeroship-control/src/worker_enrolment.rs::enrol`
    // calls this over the SAME shared, single-session `control_pg` client
    // every other control-plane write uses (compio-postgres's `Client::
    // transaction` needs `&mut self`, and that client is a `&self`-shared
    // `Arc` reached from concurrent requests, so the lock-then-insert sequence
    // is expressed here, server-side, as ONE round trip rather than a
    // client-driven multi-statement transaction).
    //
    // 1. Lock the enroller row with a guarded no-op update, conditioned on
    //    `status = 'active'` -- the `lock_scope` pattern in
    //    crates/zeroship-workflow-manager/src/queue.rs. A concurrent
    //    zeroship.revoke_worker_enroller call's own first UPDATE targets this
    //    same row and queues behind this lock, so it always sees whatever
    //    this call committed before it, never a half-finished enrolment.
    // 2. If the enroller is not active (revoked between when Control verified
    //    the caller's assertion and when this statement runs -- the race
    //    success criterion 4 in the enrolment PoC exercises), raise and
    //    refuse. No row is written.
    // 3. Insert the instance, conflicting on the SAME public key. A retry
    //    after a lost reply hits the conflict and this call returns the
    //    EXISTING row's id instead of minting a second one. The same key
    //    presented under a DIFFERENT enroller also hits the conflict and is
    //    refused -- a public key names exactly one enroller for its life.
    createFunction({
      schema: "zeroship",
      name: "enrol_worker_instance",
      args: [
        { name: "p_enroller_id", type: "text" },
        { name: "p_id", type: "text" },
        { name: "p_ring_key", type: "bytea" },
        { name: "p_public_key", type: "bytea" },
        { name: "p_advertise_host", type: "inet" },
        { name: "p_advertise_port", type: "int" },
      ],
      returns: "text",
      language: "procedural",
      body:
        "DECLARE\n"
        + "  v_locked int;\n"
        + "  v_existing_id text;\n"
        + "  v_existing_enroller text;\n"
        + "BEGIN\n"
        + "  UPDATE zeroship.worker_enrollers\n"
        + "     SET lock_version = lock_version\n"
        + "   WHERE id = p_enroller_id AND status = 'active';\n"
        + "  GET DIAGNOSTICS v_locked = ROW_COUNT;\n"
        + "  IF v_locked = 0 THEN\n"
        + "    RAISE EXCEPTION 'worker enroller % is not active', p_enroller_id\n"
        + "      USING ERRCODE = 'insufficient_privilege';\n"
        + "  END IF;\n"
        + "\n"
        + "  INSERT INTO zeroship.worker_instances\n"
        + "      (id, enroller_id, ring_key, public_key, advertise_host, advertise_port, status)\n"
        + "  VALUES (p_id, p_enroller_id, p_ring_key, p_public_key, p_advertise_host, p_advertise_port, 'active')\n"
        + "  ON CONFLICT (public_key) DO NOTHING;\n"
        + "\n"
        + "  IF FOUND THEN\n"
        + "    RETURN p_id;\n"
        + "  END IF;\n"
        + "\n"
        + "  SELECT worker_instances.id, worker_instances.enroller_id\n"
        + "    INTO v_existing_id, v_existing_enroller\n"
        + "    FROM zeroship.worker_instances\n"
        + "   WHERE public_key = p_public_key;\n"
        + "\n"
        + "  IF v_existing_enroller = p_enroller_id THEN\n"
        + "    RETURN v_existing_id;\n"
        + "  END IF;\n"
        + "\n"
        + "  RAISE EXCEPTION 'public key already enrolled under a different enroller'\n"
        + "    USING ERRCODE = 'unique_violation';\n"
        + "END;",
    });

    // THE REVOCATION. Two ordered updates, one statement, so one transaction:
    // the enroller moves to `revoked` first (waiting on any enrolment already
    // holding the row lock), then every instance it enrolled that is still
    // `active` moves to `gone`. Because the second update runs after the
    // first's lock is granted, it always sees an enrolment that won the race
    // and committed first -- there is no ordering of these two operations
    // under which an active instance of a revoked enroller survives both.
    //
    // NO RUNTIME ROLE RECEIVES EXECUTE (enforced below): this is an explicit
    // operator database operation, run by hand against the platform database,
    // exactly as `zeroship dev init` writes key material today. Control never
    // calls it and holds no grant to.
    createFunction({
      schema: "zeroship",
      name: "revoke_worker_enroller",
      args: [{ name: "p_enroller_id", type: "text" }],
      returns: "void",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  UPDATE zeroship.worker_enrollers\n"
        + "     SET status = 'revoked'\n"
        + "   WHERE id = p_enroller_id AND status = 'active';\n"
        + "\n"
        + "  UPDATE zeroship.worker_instances\n"
        + "     SET status = 'gone'\n"
        + "   WHERE enroller_id = p_enroller_id AND status = 'active';\n"
        + "END;",
    });

    // PostgreSQL grants EXECUTE on a new function to PUBLIC by default -- the
    // one place a function's default ACL is the opposite of a table's. Both
    // grants below undo that; the explicit GRANT restores exactly what
    // Control needs and nothing else.
    raw({
      sql: "REVOKE EXECUTE ON FUNCTION zeroship.enrol_worker_instance(text, text, bytea, bytea, inet, int) FROM PUBLIC",
      reason: "only the control plane may lock an enroller and enrol an instance",
    });
    raw({
      sql: "GRANT EXECUTE ON FUNCTION zeroship.enrol_worker_instance(text, text, bytea, bytea, inet, int) TO zeroship_control",
      reason: "control's enrolment handler calls this to lock the enroller and insert the instance",
    });
    raw({
      sql: "REVOKE EXECUTE ON FUNCTION zeroship.revoke_worker_enroller(text) FROM PUBLIC",
      reason: "revocation is an explicit operator database operation; no runtime role may call it",
    });
  },
};
