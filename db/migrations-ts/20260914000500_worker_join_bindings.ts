import { createFunction, grant, raw, t, table } from "@zeroship/migrate";

// The other half of 20260914000400_execution_zones_and_join_signers.ts, split
// into its own file because a foreign key authored in the SAME file as the
// COLLATE "C" fix on its target is checked against a pre-file catalog snapshot
// and refused. Both target columns are fixed in
// 20260914000400_execution_zones_and_join_signers.ts, so this file's foreign
// keys to them are safe.
//
// This file spends the signer table. It records which zones each signer may
// mint for, accounts for a token's uses, binds each worker instance to the
// signer and token that admitted it, and adds the functions that are the
// mechanism. The instance's expiry column is declared with the table itself
// (20260907000300_worker_instances.ts).
export default {
  name: "worker_join_bindings",
  schema() {
    // ---- which zones a signer may mint for ---------------------------------
    //
    // A junction rather than an array column: the zone is a foreign key to a
    // row this deployment declares, so a signer trusted for a zone that does
    // not exist is refused by the database rather than by whoever reads it.
    table("worker_join_signer_zones", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        signer_id: t.text().required(),
        execution_zone_id: t.text().required(),
      },
      primaryKey: ["id"],
    });
    table("worker_join_signer_zones", { schema: "zeroship" })
      .unique("worker_join_signer_zones_natural_key")
      .add({ columns: ["signer_id", "execution_zone_id"] });
    table("worker_join_signer_zones", { schema: "zeroship" })
      .foreignKey("worker_join_signer_zones_signer_fk")
      .add({
        columns: ["signer_id"],
        references: { table: "worker_join_signers", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
      });
    table("worker_join_signer_zones", { schema: "zeroship" })
      .foreignKey("worker_join_signer_zones_zone_fk")
      .add({
        columns: ["execution_zone_id"],
        references: { table: "execution_zones", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
      });
    grant({
      privileges: ["select", "insert"],
      on: { kind: "table", schema: "zeroship", names: ["worker_join_signer_zones"] },
      to: ["zeroship_control"],
    });

    // ---- a token's use accounting -------------------------------------------
    //
    // TWO TABLES, and the split is what makes the accounting both EXACT and
    // RETRY-SAFE under several Control replicas.
    //
    // `worker_join_tokens` holds the counter. Consuming a use is
    // `UPDATE ... WHERE uses_consumed < uses_allowed`, one guarded statement
    // whose row lock serializes every concurrent claim of one token, so N uses
    // admit exactly N workers however many present the token at once. A read
    // followed by a write would be a race a concurrent joiner wins outright.
    //
    // `worker_join_token_claims` records WHICH joining key each use went to,
    // unique on the pair. A retried join after a lost reply presents the same
    // key, hits the conflict, and is answered without consuming a second use --
    // otherwise a single-use token could not survive one dropped response.
    //
    // `token_key` is `<signer issuer>|<jti>`, scoped by issuer for the same
    // reason the service-assertion replay key is: one signer cannot burn
    // another's token id. Both halves are character-restricted by the verifier
    // before they reach here.
    table("worker_join_tokens", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        token_key: t.text().required(),
        uses_allowed: t.int().required(),
        uses_consumed: t.int().required(),
        // `exp` plus the verifier's skew tolerance: the instant after which no
        // presentation of this token can be accepted, and therefore the instant
        // its accounting may be reclaimed. Deleting a row before then would make
        // a spent token spendable again while it is still valid.
        expires_at: t.timestamp().required(),
      },
      primaryKey: ["id"],
    });
    table("worker_join_tokens", { schema: "zeroship" })
      .unique("worker_join_tokens_natural_key")
      .add({ columns: ["token_key"] });
    table("worker_join_tokens", { schema: "zeroship" })
      .index("worker_join_tokens_expiry_idx")
      .add({ on: ["expires_at"] });
    table("worker_join_tokens", { schema: "zeroship" })
      .check("worker_join_tokens_uses_range")
      .add({
        expr: (col) => col("uses_allowed").ge(1).and(col("uses_consumed").ge(0)),
      });

    table("worker_join_token_claims", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        token_key: t.text().required(),
        // The RFC 7638 thumbprint of the joining public key, not the key
        // itself: the claim exists to recognise a retry, and a thumbprint is
        // bounded, printable and already computed by the verifier.
        joining_key: t.text().required(),
        expires_at: t.timestamp().required(),
      },
      primaryKey: ["id"],
    });
    table("worker_join_token_claims", { schema: "zeroship" })
      .unique("worker_join_token_claims_natural_key")
      .add({ columns: ["token_key", "joining_key"] });
    table("worker_join_token_claims", { schema: "zeroship" })
      .index("worker_join_token_claims_expiry_idx")
      .add({ on: ["expires_at"] });

    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: ["worker_join_tokens", "worker_join_token_claims"],
      },
      to: ["zeroship_control"],
    });

    // ---- the instance's binding and zone ------------------------------------
    // The instance columns are declared in 20260907000300_worker_instances.ts;
    // this file adds the foreign keys and the bytewise collations they need.
    table("worker_instances", { schema: "zeroship" })
      .foreignKey("worker_instances_join_signer_fk")
      .add({
        columns: ["join_signer_id"],
        references: { table: "worker_join_signers", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
      });
    table("worker_instances", { schema: "zeroship" })
      .foreignKey("worker_instances_zone_fk")
      .add({
        columns: ["execution_zone_id"],
        references: { table: "execution_zones", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
      });

    raw({
      sql: 'ALTER TABLE "zeroship"."worker_instances" ALTER COLUMN "join_signer_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."worker_instances" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });

    // WITHOUT THIS, "immutable" and "INSERT-ONCE" would be prose. Control holds
    // UPDATE because `status` must progress and `expires_at` must renew, and
    // its table-wide UPDATE grant also covers the identity and address columns,
    // so those are frozen here instead. The ring key is the reason this matters
    // most: a writer that could rotate it could move an instance's ring position
    // after placement was decided, which is the grinding attack the mint exists
    // to prevent.
    createFunction({
      schema: "zeroship",
      name: "worker_instances_reject_frozen_change",
      returns: "trigger",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  IF NEW.id <> OLD.id\n"
        + "     OR NEW.join_signer_id <> OLD.join_signer_id\n"
        + "     OR NEW.join_token_id <> OLD.join_token_id\n"
        + "     OR NEW.execution_zone_id <> OLD.execution_zone_id\n"
        + "     OR NEW.ring_key <> OLD.ring_key\n"
        + "     OR NEW.public_key <> OLD.public_key\n"
        + "     OR NEW.advertise_host <> OLD.advertise_host\n"
        + "     OR NEW.advertise_port <> OLD.advertise_port\n"
        + "     OR NEW.registered_at <> OLD.registered_at THEN\n"
        + "    RAISE EXCEPTION 'worker_instances identity and address are frozen at join; "
        + "only status and expires_at may change'\n"
        + "      USING ERRCODE = 'check_violation';\n"
        + "  END IF;\n"
        + "  RETURN NEW;\n"
        + "END;",
    });
    table("worker_instances", { schema: "zeroship" })
      .trigger("worker_instances_frozen_columns")
      .create({
        timing: "before",
        events: ["update"],
        forEach: "row",
        execute: "worker_instances_reject_frozen_change",
      });

    // ---- consuming one use of a join token ----------------------------------
    //
    // One statement from the client, so one server-side transaction: Control
    // calls this over the SAME shared, single-session `control_pg` client every
    // other control-plane write uses, and compio-postgres's `Client::
    // transaction` needs `&mut self` that call site does not have.
    //
    // 1. Record the claim for this joining key, guarded by the natural key. A
    //    repeat -- the retry after a lost reply -- takes no use and says so.
    //    Concurrent presentations of the SAME key block on the uncommitted row
    //    and the loser sees the repeat, so two replicas cannot both consume.
    // 2. Create the counter row if this is the token's first presentation.
    // 3. Consume, guarded on `uses_consumed < uses_allowed`. The row lock that
    //    UPDATE takes is what serializes concurrent claims of DIFFERENT keys,
    //    so exactly `uses_allowed` of them succeed.
    // 4. An exhausted token raises, which rolls back the claim recorded in
    //    step 1 -- a claim row left behind without a consumed use would let the
    //    same key back in later for free.
    createFunction({
      schema: "zeroship",
      name: "claim_worker_join_use",
      args: [
        { name: "p_token_key", type: "text" },
        { name: "p_joining_key", type: "text" },
        { name: "p_uses", type: "int" },
        { name: "p_expires_at", type: "timestamptz" },
      ],
      returns: "text",
      language: "procedural",
      body:
        "DECLARE\n"
        + "  v_rows int;\n"
        + "BEGIN\n"
        + "  INSERT INTO zeroship.worker_join_token_claims (token_key, joining_key, expires_at)\n"
        + "  VALUES (p_token_key, p_joining_key, p_expires_at)\n"
        + "  ON CONFLICT (token_key, joining_key) DO NOTHING;\n"
        + "  GET DIAGNOSTICS v_rows = ROW_COUNT;\n"
        + "  IF v_rows = 0 THEN\n"
        + "    RETURN 'repeat';\n"
        + "  END IF;\n"
        + "\n"
        + "  INSERT INTO zeroship.worker_join_tokens\n"
        + "      (token_key, uses_allowed, uses_consumed, expires_at)\n"
        + "  VALUES (p_token_key, p_uses, 0, p_expires_at)\n"
        + "  ON CONFLICT (token_key) DO NOTHING;\n"
        + "\n"
        + "  UPDATE zeroship.worker_join_tokens\n"
        + "     SET uses_consumed = uses_consumed + 1\n"
        + "   WHERE token_key = p_token_key AND uses_consumed < uses_allowed;\n"
        + "  GET DIAGNOSTICS v_rows = ROW_COUNT;\n"
        + "  IF v_rows = 0 THEN\n"
        + "    RAISE EXCEPTION 'join token % has no uses left', p_token_key\n"
        + "      USING ERRCODE = 'insufficient_resources';\n"
        + "  END IF;\n"
        + "  RETURN 'consumed';\n"
        + "END;",
    });

    // ---- admitting one instance ---------------------------------------------
    //
    // 1. Lock the signer row with a guarded no-op update, conditioned on
    //    `status = 'active'`. A concurrent rotate/purge call's own first UPDATE
    //    targets this same row and queues behind this lock, so it always sees
    //    whatever this call committed, never a half-finished join.
    // 2. Refuse a zone the signer may not mint for. Control already checked
    //    this against the recorded set; checking it again HERE, under the lock,
    //    is what makes "the zone comes from the token and the signer must be
    //    permitted it" a property of the database rather than of one caller.
    //    Its SQLSTATE differs from the inactive-signer one so the two refusals
    //    stay distinguishable.
    // 3. Insert the instance, conflicting on the public key. A retry after a
    //    lost reply, or a racing replica, hits the conflict and gets the
    //    EXISTING row's id. The same key presented under a DIFFERENT signer or
    //    token is refused -- a public key names exactly one join for its life.
    createFunction({
      schema: "zeroship",
      name: "join_worker_instance",
      args: [
        { name: "p_signer_id", type: "text" },
        { name: "p_token_id", type: "text" },
        { name: "p_zone_id", type: "text" },
        { name: "p_id", type: "text" },
        { name: "p_ring_key", type: "bytea" },
        { name: "p_public_key", type: "bytea" },
        { name: "p_advertise_host", type: "inet" },
        { name: "p_advertise_port", type: "int" },
        { name: "p_lease_seconds", type: "int" },
      ],
      returns: "text",
      language: "procedural",
      body:
        "DECLARE\n"
        + "  v_locked int;\n"
        + "  v_permitted int;\n"
        + "  v_existing_id text;\n"
        + "  v_existing_signer text;\n"
        + "  v_existing_token text;\n"
        + "BEGIN\n"
        + "  UPDATE zeroship.worker_join_signers\n"
        + "     SET lock_version = lock_version\n"
        + "   WHERE id = p_signer_id AND status = 'active';\n"
        + "  GET DIAGNOSTICS v_locked = ROW_COUNT;\n"
        + "  IF v_locked = 0 THEN\n"
        + "    RAISE EXCEPTION 'join signer % is not active', p_signer_id\n"
        + "      USING ERRCODE = 'insufficient_privilege';\n"
        + "  END IF;\n"
        + "\n"
        + "  SELECT count(*) INTO v_permitted\n"
        + "    FROM zeroship.worker_join_signer_zones\n"
        + "   WHERE signer_id = p_signer_id AND execution_zone_id = p_zone_id;\n"
        + "  IF v_permitted = 0 THEN\n"
        + "    RAISE EXCEPTION 'join signer % may not mint for zone %', p_signer_id, p_zone_id\n"
        + "      USING ERRCODE = 'invalid_parameter_value';\n"
        + "  END IF;\n"
        + "\n"
        + "  INSERT INTO zeroship.worker_instances\n"
        + "      (id, join_signer_id, join_token_id, execution_zone_id, ring_key, public_key,\n"
        + "       advertise_host, advertise_port, status, expires_at)\n"
        + "  VALUES (p_id, p_signer_id, p_token_id, p_zone_id, p_ring_key, p_public_key,\n"
        + "          p_advertise_host, p_advertise_port, 'active',\n"
        + "          now() + make_interval(secs => p_lease_seconds))\n"
        + "  ON CONFLICT (public_key) DO NOTHING;\n"
        + "\n"
        + "  IF FOUND THEN\n"
        + "    RETURN p_id;\n"
        + "  END IF;\n"
        + "\n"
        + "  SELECT worker_instances.id, worker_instances.join_signer_id,\n"
        + "         worker_instances.join_token_id\n"
        + "    INTO v_existing_id, v_existing_signer, v_existing_token\n"
        + "    FROM zeroship.worker_instances\n"
        + "   WHERE public_key = p_public_key;\n"
        + "\n"
        + "  IF v_existing_signer = p_signer_id AND v_existing_token = p_token_id THEN\n"
        + "    RETURN v_existing_id;\n"
        + "  END IF;\n"
        + "\n"
        + "  RAISE EXCEPTION 'public key already joined under a different signer or token'\n"
        + "    USING ERRCODE = 'unique_violation';\n"
        + "END;",
    });

    // ---- the two signer verbs, side by side ---------------------------------
    //
    // ROTATE is the hygiene path: the key is merely old. It stops every future
    // token minted under it and leaves the fleet running, because one signer
    // covers many units and retiring a key must not take all of them down.
    // Outstanding tokens are refused immediately, because verification resolves
    // only an active signer.
    //
    // PURGE is the incident path: the key is believed to have leaked. It does
    // what rotate does AND retires every instance the signer admitted, in one
    // transaction, so an operator is not retiring instances one at a time while
    // an attacker's workers keep serving. The second update runs after the
    // first's row lock is granted, so it always sees a join that won the race
    // and committed first.
    //
    // NO RUNTIME ROLE RECEIVES EXECUTE on either (enforced below): both are
    // explicit operator database operations, run by hand against the platform
    // database. Control never calls them and holds no grant to.
    createFunction({
      schema: "zeroship",
      name: "rotate_worker_join_signer",
      args: [{ name: "p_signer_id", type: "text" }],
      returns: "void",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  UPDATE zeroship.worker_join_signers\n"
        + "     SET status = 'revoked'\n"
        + "   WHERE id = p_signer_id AND status = 'active';\n"
        + "END;",
    });

    createFunction({
      schema: "zeroship",
      name: "purge_worker_join_signer",
      args: [{ name: "p_signer_id", type: "text" }],
      returns: "void",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  UPDATE zeroship.worker_join_signers\n"
        + "     SET status = 'revoked'\n"
        + "   WHERE id = p_signer_id AND status = 'active';\n"
        + "\n"
        + "  UPDATE zeroship.worker_instances\n"
        + "     SET status = 'gone'\n"
        + "   WHERE join_signer_id = p_signer_id AND status = 'active';\n"
        + "END;",
    });

    // PostgreSQL grants EXECUTE on a new function to PUBLIC by default -- the
    // one place a function's default ACL is the opposite of a table's. Every
    // revoke below undoes that; the explicit grants restore exactly what
    // Control needs and nothing else.
    raw({
      sql: "REVOKE EXECUTE ON FUNCTION zeroship.claim_worker_join_use(text, text, int, timestamptz) FROM PUBLIC",
      reason: "only the control plane may consume a join token use",
    });
    raw({
      sql: "GRANT EXECUTE ON FUNCTION zeroship.claim_worker_join_use(text, text, int, timestamptz) TO zeroship_control",
      reason: "control's join handler consumes one use before admitting a worker",
    });
    raw({
      sql: "REVOKE EXECUTE ON FUNCTION zeroship.join_worker_instance(text, text, text, text, bytea, bytea, inet, int, int) FROM PUBLIC",
      reason: "only the control plane may lock a signer and admit an instance",
    });
    raw({
      sql: "GRANT EXECUTE ON FUNCTION zeroship.join_worker_instance(text, text, text, text, bytea, bytea, inet, int, int) TO zeroship_control",
      reason: "control's join handler calls this to lock the signer and insert the instance",
    });
    raw({
      sql: "REVOKE EXECUTE ON FUNCTION zeroship.rotate_worker_join_signer(text) FROM PUBLIC",
      reason: "signer rotation is an explicit operator database operation; no runtime role may call it",
    });
    raw({
      sql: "REVOKE EXECUTE ON FUNCTION zeroship.purge_worker_join_signer(text) FROM PUBLIC",
      reason: "signer purge is an explicit operator database operation; no runtime role may call it",
    });
  },
};
