import { grant, now, raw, t, table } from "@zeroship/migrate";

// ONE ROW IS ONE LIVE WORKER PROCESS. Control writes this table; the worker
// never touches it and holds no privilege on it. The worker generates an
// Ed25519 instance keypair AT BOOT, IN MEMORY, NEVER ON DISK, and presents the
// public half with a JOIN TOKEN a trusted signer minted, plus a signature over
// the request made by that very key. An instance is a CHILD of the worker role:
// it mints under `svc/worker/<wkr_id>` and is addressed as `svc/worker`.
//
// WHAT THIS BUYS, STATED SO NOTHING HERE OVERSELLS IT. The instance private
// half exists in one process's memory and nowhere else, so retiring one row
// takes a capability away rather than only removing an attribution. A captured
// join token admits workers the captor controls and nothing more, because the
// possession proof binds the key being registered. What bounds a token is its
// own budget: the uses it was minted with, until its expiry, in the one zone it
// names.
//
// NOTHING RATE-LIMITS JOINING beyond that budget, and the budget is the whole
// of it: a signer that mints generously mints generously. Say "could" in a
// design and "does" only where something does.
//
// THE RING KEY IS CONTROL'S, AND THE REGISTRANT CONTRIBUTES NOTHING TO IT.
// The registry's ring order is defined in
// `zeroship_core::worker_ring::vnode_position`, which orders by this column's
// bytes; the gateway's `HashRing` still derives worker positions from worker
// URLs and must be replaced by that ordering. If a registrant could influence
// its own position it would GRIND its address until
// it landed beside a target app, and the placement fence would become a lottery
// the attacker plays until it wins. Control mints these bytes from its own
// CSPRNG, and they are frozen for the row's life by the frozen-columns trigger
// (20260914000500_worker_join_bindings.ts).
//
// THE ADDRESS IS DERIVED FROM THE JOIN CONNECTION, AND THE WORKER SUPPLIES
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
// returns. A registrant-supplied address would therefore let a token holder
// INTERCEPT AND IMPERSONATE END-USER SESSIONS under the app's own origin, which
// is worse than the exposure the registry exists to reduce. Derivation is also
// what makes the design deployable: a per-process address setting has no
// producer, because compose replicas share one environment block and a
// Kubernetes Deployment is one pod spec for N pods, so every replica would
// present the same address.
//
// `advertise_host` IS `inet`, NOT TEXT, and that is a different call from
// `app_egress_rules.destination`. That column is text because a rule's
// destination is a DNS NAME or a range and no address type holds a name. This
// value is neither: it is copied out of
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
// `status` IS A CLOSED SET, AND READINESS IS NOT IN IT. `draining` is reserved
// for a drain path that has no writer yet. Readiness is a PROBE RESULT: it is
// derived, it expires, and it belongs to whatever performs the probe. Admitting
// it here would make the
// column carry two kinds of fact -- what control declared and what a probe
// observed -- and readers would disagree about which one they were reading.
//
// WHAT READS THESE COLUMNS. `public_key` is read by control's enrolment
// verifier (`worker_join::active_instance_public_key`); `advertise_host`/
// `advertise_port` by `worker_health::enrolled_targets`, the health probe's
// query; `id`/`status` by `worker_join::instance_serves_app` and the probe.
// `ring_key` is ordered by `zeroship_core::worker_ring::vnode_position`, whose
// per-app eligible-set caller is not built yet. The join columns declared above
// carry their own readers: control's join functions read
// `join_signer_id`/`join_token_id`, and workflow placement reads
// `execution_zone_id`/`expires_at`.
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
        // The signer and the token that admitted this instance. Recorded so
        // "who vouched for this worker" is a stored fact rather than an
        // inference, and so purging a leaked signer can enumerate exactly what
        // it admitted. Both are frozen for the row's life.
        join_signer_id: t.text().notNull(),
        join_token_id: t.text().notNull(),
        // The token's `zone` claim, resolved to an id by Control. Nothing a
        // worker sends reaches it.
        execution_zone_id: t.text().notNull(),
        // THE LEASE. An instance identity expires and the worker renews it, so
        // revocation stops being the only way a credential ever stops working:
        // a crashed or abandoned worker's row stops satisfying Control's
        // instance read on its own, with nothing observing liveness to make it
        // happen.
        expires_at: t.timestamp().notNull(),
      },
      primaryKey: ["id"],
    });
    table("worker_instances", { schema: "zeroship" })
      .check("worker_instances_id_shape")
      .add({ expr: (col) => col("id").regex("^wkr_[0-9a-z]{25}$") });
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

    // Two Control replicas racing a lost-reply retry must converge on ONE
    // instance row rather than minting two identities for one key.
    table("worker_instances", { schema: "zeroship" })
      .unique("worker_instances_public_key_uq")
      .add({ columns: ["public_key"] });

    // The typed-id domain needs bytewise comparison; PostgreSQL's locale
    // collation does not keep the base36 alphabet in numeric order. There are no
    // foreign-key copies of this id, so this is the only column to pin.
    raw({
      sql: 'ALTER TABLE "zeroship"."worker_instances" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });

    // The control plane is the only runtime writer; the operator's
    // `purge_worker_join_signer` is the other writer, and it only marks rows
    // `gone`. Readers are column-scoped:
    // `zeroship_cdc` verifies enrolled worker identity (granted below), and
    // `zeroship_workflow` reads identity and placement facts (granted in
    // 20260911000000_workflow_coordination.ts and
    // 20260914000600_placement_eligibility.ts). The gateway does not read this
    // table: the per-app eligible set it would consume is not built yet.
    //
    // WHY THERE IS NO REVOKE. `zeroship_worker` is denied by PostgreSQL's
    // OWNER-ONLY DEFAULT: a newly created table has a null `relacl` and nobody
    // but the owner holds anything. It is NOT denied by
    // db/migrations-ts/20260702000900_grants.ts, whose
    // `ALTER DEFAULT PRIVILEGES ... REVOKE` lines store nothing, because
    // revoking a privilege that was never in the default set is a no-op.
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

    // The CDC relay verifies an enrolled worker's identity and observes its
    // revocation; column-scoped because it needs nothing else on the row.
    raw({
      sql: "GRANT SELECT (id, status, public_key) ON zeroship.worker_instances TO zeroship_cdc",
      reason: "the relay verifies enrolled worker identities and observes revocation",
    });
  },
};
