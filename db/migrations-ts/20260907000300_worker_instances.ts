import { createFunction, grant, now, raw, t, table } from "@zeroship/migrate";

// ONE ROW IS ONE LIVE WORKER PROCESS. Control writes this table; the worker
// never touches it and holds no privilege on it. The worker generates an
// Ed25519 instance keypair AT BOOT, IN MEMORY, NEVER ON DISK, and enrols the
// public half over HTTP authenticated by the `svc/worker` role key it already
// holds. An instance is a CHILD of that role: it mints under
// `svc/worker/<wkr_id>` and is addressed as `svc/worker`.
//
// WHAT THIS BUYS, STATED HONESTLY SO NOTHING HERE OVERSELLS IT. Enrolment
// authenticates with the SHARED role key, so a holder of that key can enrol
// many instances. Per-instance identity is a DISTINGUISHER against a role-key
// holder, NOT a boundary. What it buys is attribution, per-instance revocation,
// a countable and rate-limitable event, and it is what makes a per-app placement
// fence writable at all. No comment in this file, and no doc citing it, may say
// it is a boundary.
//
// THE RING KEY IS CONTROL'S, AND THE REGISTRANT CONTRIBUTES NOTHING TO IT.
// `HashRing::new` in crates/zeroship-gateway/src/proxy.rs derives ring position
// from the worker URL. If a registrant could influence its own position it would
// GRIND its address until it landed beside a target app, and the placement fence
// would become a lottery the attacker plays until it wins. Control mints these
// bytes from its own CSPRNG, and they are frozen for the row's life by the
// trigger below.
//
// THE ADDRESS IS DERIVED FROM THE ENROLMENT CONNECTION, AND THE WORKER SUPPLIES
// ONLY ITS LISTENING PORT. Control takes the host from the observed peer
// address, validates the pair against an operator-declared envelope of permitted
// CIDRs and ports held in CONTROL'S OWN config, and refuses anything outside it,
// including a peer reached through a configured proxy (without that arm the
// derivation collapses to "everything is the proxy"). Loopback is ruled on by
// the declared networks like any other address, so an operator can state a
// single-host deployment; it is not fenced above them, which would make that
// deployment inexpressible.
//
// WHY, because it is not obvious: `collect_forwarded_headers` in
// crates/zeroship-gateway/src/router/dispatch.rs strips a named header set and
// COOKIE IS NOT IN IT, and `forward_dispatch` posts the full request -- body,
// cookies, and the gateway-signed user envelope -- to whatever address the ring
// returns. A registrant-supplied address would therefore let a role-key holder
// INTERCEPT AND IMPERSONATE END-USER SESSIONS under the app's own origin, which
// is worse than the exposure the registry exists to reduce. Derivation is also
// what makes the design deployable: a per-process address setting has no
// producer, because compose replicas share one environment block and a
// Kubernetes Deployment is one pod spec for N pods, so every replica would
// present the same address.
//
// `advertise_host` IS `inet`, NOT TEXT, and that is a different call from
// `app_egress_rules.destination`. That column is text because a rule's
// destination is a DNS NAME OR a range and the native types would need a driver
// feature no workspace crate enables. This value is neither: it is copied out of
// an accepted peer socket, so it is always a literal address, and `IpAddr` is a
// codec compio-postgres carries unconditionally (its vendored postgres-types
// implements `FromSql`/`ToSql` for `IpAddr` against INET with no feature gate).
// The database therefore refuses a spelling that is not an address, instead of
// the control plane being the only thing that does.
//
// ONE TIMESTAMP, NOT A FIRST-SEEN PLUS REGISTERED PAIR. An instance id is minted
// once and never re-registered, so those would be the same instant under two
// names, and two names for one instant is how readers start disagreeing.
//
// `status` IS A CLOSED SET OVER EXACTLY WHAT A WRITER WRITES, AND READINESS IS
// NOT IN IT. Readiness is a PROBE RESULT: it is derived, it expires, and it
// belongs to whatever performs the probe. Admitting it here would make the
// column carry two kinds of fact -- what control declared and what a probe
// observed -- and readers would disagree about which one they were reading.
//
// EVERY COLUMN HAS A NAMED READER AND THERE ARE NO OTHERS. `ring_key` is read by
// the eligible-set computation; `public_key` by control's enrolment verifier;
// `advertise_host`/`advertise_port` by dispatch, by `fetch_worker_logs` in
// crates/zeroship-control/src/api.rs, and later by the health probe; `id` and
// `registered_at` by attribution and revocation; `status` by the eligible-set
// computation.
//
// NO DELETE GRANT, ON PURPOSE. `gone` is the terminal state, so an instance ends
// by being marked, not by being erased -- erasing it would take the attribution
// the row exists to provide. Reaping very old `gone` rows is not designed, and a
// capability with no reader is exactly what the privilege-follows-the-process
// rule refuses; the day a reaper is designed it brings its own grant.
//
// The table is registered in policies/platform-table-owners.json, which is not
// optional bookkeeping: the applier refuses fail-closed on any op targeting a
// table with no ownership entry.
export default {
  name: "worker_instances",
  schema() {
    table("worker_instances", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        ring_key: t.bytes().notNull(),
        public_key: t.bytes().notNull(),
        advertise_host: t.inet().notNull(),
        advertise_port: t.int().notNull(),
        registered_at: t.timestamp().notNull().default(now()),
        status: t.text().notNull(),
      },
      primaryKey: ["id"],
    });
    table("worker_instances", { schema: "zeroship" })
      .check("worker_instances_id_shape")
      .add({ expr: (col) => col("id").regex("^wkr_[0-9A-Za-z]{22}$") });
    // Ed25519 public keys are exactly 32 octets (RFC 8032 section 5.1.5), so
    // "raw Ed25519 bytes" is a shape the database can hold rather than a
    // sentence the verifier discovers is false at read time. `length(bytea)` is
    // PostgreSQL's octet count.
    table("worker_instances", { schema: "zeroship" })
      .check("worker_instances_public_key_shape")
      .add({ expr: (col) => col("public_key").length().eq(32) });
    // NOT a width. The ring key's byte count is control's minting decision, not
    // a wire constant this schema should own, so the fence is against a row that
    // carries no key at all.
    table("worker_instances", { schema: "zeroship" })
      .check("worker_instances_ring_key_present")
      .add({ expr: (col) => col("ring_key").length().gt(0) });
    table("worker_instances", { schema: "zeroship" })
      .check("worker_instances_advertise_port_range")
      .add({ expr: (col) => col("advertise_port").ge(1).and(col("advertise_port").le(65535)) });
    table("worker_instances", { schema: "zeroship" })
      .check("worker_instances_status_check")
      .add({ expr: (col) => col("status").in(["active", "draining", "gone"]) });

    // The typed-id domain needs bytewise comparison; PostgreSQL's locale
    // collation does not keep the base62 alphabet in numeric order. There are no
    // foreign-key copies of this id yet, so this is the only column to pin.
    raw({
      sql: 'ALTER TABLE "zeroship"."worker_instances" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });

    // WITHOUT THIS, "immutable" and "INSERT-ONCE" would be prose. Control holds
    // UPDATE because `status` must progress, and UPDATE is not column-selective
    // in a grant, so the identity and address columns are frozen here instead.
    // The ring key is the reason this matters most: a writer that could rotate
    // it could move an instance's ring position after placement was decided,
    // which is the grinding attack the mint exists to prevent.
    createFunction({
      schema: "zeroship",
      name: "worker_instances_reject_frozen_change",
      returns: "trigger",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  IF NEW.id <> OLD.id\n"
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
    table("worker_instances", { schema: "zeroship" })
      .trigger("worker_instances_frozen_columns")
      .create({
        timing: "before",
        events: ["update"],
        forEach: "row",
        execute: "worker_instances_reject_frozen_change",
      });

    // The control plane is the only writer and the only reader. The gateway does
    // not read this table: control publishes the eligible worker set per app on
    // the feed it already owns.
    //
    // WHY THERE IS NO REVOKE, STATED CORRECTLY - this comment named the wrong
    // mechanism until 2026-09-07. `zeroship_worker` is denied by PostgreSQL's
    // OWNER-ONLY DEFAULT: a newly created table has a null `relacl` and nobody
    // but the owner holds anything. It is NOT denied by
    // db/migrations-ts/20260818000200_worker_database_authority.ts, whose
    // `ALTER DEFAULT PRIVILEGES ... REVOKE` lines store nothing, because
    // revoking a privilege that was never in the default set is a no-op -
    // measured as an empty `pg_default_acl`, with a fresh table in this schema
    // confirming a null `relacl` and no worker privilege.
    //
    // The distinction is load-bearing rather than pedantic. An ambient default
    // is not a fence: any later migration that grants `zeroship_worker`
    // anything on this table succeeds silently, and there is no REVOKE here to
    // contradict it. If that becomes a real risk, add an explicit revoke so the
    // claim supports itself instead of resting on what nobody has done yet.
    grant({
      privileges: ["select", "insert", "update"],
      on: { kind: "table", schema: "zeroship", names: ["worker_instances"] },
      to: ["zeroship_control"],
    });
  },
};
