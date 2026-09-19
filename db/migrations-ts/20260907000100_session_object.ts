import { grant, now, raw, t, table } from "@zeroship/migrate";

const userIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  grants: ["person_id"],
  sessions: ["person_id"],
};

// The session object and the grant it hangs off.
//
// ONE ROW IS THE WHOLE CREDENTIAL. A session carries its own rotating secret
// (`secret_hash` + `secret_key_version`), the predecessor that secret replaced
// (`prev_secret_hash`), and the sealed response that predecessor is allowed to
// replay (`idem_response_enc` + `idem_expires_at`). The refresh FAMILY becomes
// a single row whose secret rotates in place, so "kill the family" is a write
// to the row that mints, not to a sibling table a reader has to remember to
// consult. That is the whole content of MINT-READS-ROW: the statement that
// issues a credential is the statement that enforces liveness, expiry, the
// session epoch and the person's credential epoch, because it is one UPDATE.
//
// THE AUDIENCE KEEPS TODAY'S SCOPE AND THIS IS DELIBERATE. `audience_kind` is
// `platform` or `app`, and the app arm is keyed by the OAuth `client_id` -
// exactly what `zeroship.oauth_grants` and `zeroship.app_user_identities` are
// keyed by now. The redesign's end state scopes the audience to the PROJECT,
// and that move is its own step: it adds `project_id` to both tables under the
// add-and-collate-then-key ordering the entity migration's own header states,
// and it re-homes the sector identifier. Nothing here anticipates it. Whether
// the organization and project entities change what a SUBJECT or a SUSPENSION
// is scoped to is an open operator question, so this file changes neither.
//
// `grant_id` IS NOT NULL, INCLUDING FOR THE PLATFORM AUDIENCE. Platform is an
// audience like any other, so the platform session hangs off a platform grant
// exactly as an app session hangs off an app grant. A nullable column here
// would cost the design the property every other row buys: the suspension
// predicate rides the statement that already resolves the grant, and a NULL
// leaves that predicate vacuous for precisely the audience that governs
// deploying.
//
// THE GRANT HAS NO `revoked_at`. Revocation is DELETE and sessions cascade off
// it. A revocation column invites the failure mode this avoids: clearers and
// setters drift apart, and readers disagree about what a set value means.
//
// NO ROW-LEVEL SECURITY, AND THAT IS AN ARGUMENT RATHER THAN AN OMISSION. The
// live RLS fences in db/migrations-ts/20260702000800_policies_rls.ts bind
// because a SECOND role reaches those tables - `zeroship_gateway` holds grants
// on `app_user_identities`, `gateway_sessions` and `app_session_anchors` and
// sets the matching GUCs. These two tables are granted to `zeroship_auth` and
// to nothing else, so there is no second tenant to isolate from and a policy
// here would bind nothing. Role-scoped PostgreSQL behavior tests must prove any
// future policy and second role form an actual tenant boundary.
//
// THE SECRET IS NEVER STORED. `secret_hash` is a keyed HMAC under a versioned
// keyring the auth service holds, so a database copy yields no presentable
// credential. `prev_secret_hash` is the same shape under its own version,
// because a rotation that crosses a keyring rotation leaves the two halves
// under different keys.
export default {
  name: "session_object",
  schema() {
    // ---- grants ------------------------------------------------------------
    table("grants", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        person_id: t.text().required(),
        audience_kind: t.text().required(),
        client_id: t.text(),
        // The subject this person presents to this audience. Derived, never
        // supplied: the platform audience stores the person's own id, an app
        // audience stores the pairwise subject computed over the client's
        // sector identifier. Storing it is what removes the reverse-lookup
        // table.
        subject: t.text().required(),
        scopes: t.array(t.text(), { storage: "native" }).required().default([]),
        relay_email: t.text(),
        subject_status: t.text().required().default("active"),
        suspended_at: t.timestamp(),
        suspended_cause: t.text(),
        first_consented_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("grants", { schema: "zeroship" })
      .check("grants_id_shape")
      .add({ expr: (col) => col("id").regex("^grt_[0-9a-z]{25}$") });
    table("grants", { schema: "zeroship" })
      .check("grants_audience_kind_check")
      .add({ expr: (col) => col("audience_kind").in(["platform", "app"]) });
    // The audience discriminant and its payload agree or the row is refused.
    // Without this a platform grant could carry a client id and an app grant
    // could carry none, and both unique indexes below would then rule on a
    // shape that cannot be read back.
    table("grants", { schema: "zeroship" })
      .check("grants_audience_shape")
      .add({
        expr: (col) =>
          col("audience_kind").eq("platform").and(col("client_id").isNull())
            .or(col("audience_kind").eq("app").and(col("client_id").isNotNull())),
      });
    table("grants", { schema: "zeroship" })
      .check("grants_subject_status_check")
      .add({ expr: (col) => col("subject_status").in(["active", "suspended"]) });
    // A suspension carries its timestamp or it is not a suspension. The pair is
    // what an audit reads; a status with no `suspended_at` is a state nobody can
    // date.
    table("grants", { schema: "zeroship" })
      .check("grants_suspension_shape")
      .add({
        expr: (col) =>
          col("subject_status").eq("suspended").and(col("suspended_at").isNotNull())
            .or(col("subject_status").eq("active").and(col("suspended_at").isNull())),
      });
    // One row per (person, audience), expressed as two PARTIAL uniques rather
    // than one three-column unique. A plain `UNIQUE (person_id, audience_kind,
    // client_id)` does not constrain the platform arm at all, because
    // PostgreSQL treats NULLs as distinct and would admit any number of
    // platform grants for one person.
    table("grants", { schema: "zeroship" })
      .index("grants_platform_audience_key")
      .add({
        on: ["person_id"],
        unique: true,
        where: (col) => col("audience_kind").eq("platform"),
      });
    table("grants", { schema: "zeroship" })
      .index("grants_app_audience_key")
      .add({
        on: ["person_id", "client_id"],
        unique: true,
        where: (col) => col("audience_kind").eq("app"),
      });
    table("grants", { schema: "zeroship" })
      .index("grants_person_idx")
      .add({ on: ["person_id"] });
    table("grants", { schema: "zeroship" })
      .foreignKey("grants_person_id_fkey")
      .add({
        columns: ["person_id"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    // Keyed to the client registry, cascading, exactly as
    // `app_user_identities_app_client_id_fkey` and
    // `oauth_grants_client_id_fkey` are. A grant naming a client
    // that does not exist is a subject nobody can present. The key goes when
    // the audience moves to the project; until then the referent is the same
    // one every other audience-scoped table uses.
    table("grants", { schema: "zeroship" })
      .foreignKey("grants_client_id_fkey")
      .add({
        columns: ["client_id"],
        references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });

    // ---- sessions ----------------------------------------------------------
    table("sessions", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        person_id: t.text().required(),
        audience_kind: t.text().required(),
        client_id: t.text(),
        grant_id: t.text().required(),
        parent_session_id: t.text(),
        kind: t.text().required(),
        // Per-session. Narrowing this session's scopes bumps `epoch`; ending
        // the session sets `revoked_at`. Without the separation those are the
        // same operation and consent narrowing has to log the human out.
        epoch: t.bigInt().required().default(0),
        // Copied from `users.credential_version` at creation and compared on
        // every validating read, so a password change kills every session as a
        // data dependency rather than as an enumeration.
        credential_epoch: t.bigInt().required(),
        // NULLABLE, and the null case is a real one rather than a slack
        // constraint. A token exchange that was not granted `offline_access`
        // issues no rotating credential, so its session has no secret anybody
        // can present: it is minted once, from the statement that created it,
        // and can never be validated again. Storing a random hash nobody holds
        // would put an unpresentable entry in the unique index below and would
        // say a credential exists where none does.
        secret_hash: t.bytes(),
        secret_key_version: t.int(),
        prev_secret_hash: t.bytes(),
        prev_secret_key_version: t.int(),
        rotated_at: t.timestamp(),
        idem_response_enc: t.bytes(),
        idem_expires_at: t.timestamp(),
        amr: t.array(t.text(), { storage: "native" }).required().default([]),
        acr: t.text(),
        auth_time: t.timestamp().required().default(now()),
        scopes: t.array(t.text(), { storage: "native" }).required().default([]),
        label: t.text(),
        created_at: t.timestamp().required().default(now()),
        idle_expires_at: t.timestamp().required(),
        absolute_expires_at: t.timestamp().required(),
        revoked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("sessions", { schema: "zeroship" })
      .check("sessions_id_shape")
      .add({ expr: (col) => col("id").regex("^ses_[0-9a-z]{25}$") });
    table("sessions", { schema: "zeroship" })
      .check("sessions_audience_kind_check")
      .add({ expr: (col) => col("audience_kind").in(["platform", "app"]) });
    table("sessions", { schema: "zeroship" })
      .check("sessions_audience_shape")
      .add({
        expr: (col) =>
          col("audience_kind").eq("platform").and(col("client_id").isNull())
            .or(col("audience_kind").eq("app").and(col("client_id").isNotNull())),
      });
    table("sessions", { schema: "zeroship" })
      .check("sessions_kind_check")
      .add({ expr: (col) => col("kind").in(["browser", "cli", "device_pending"]) });
    // The sliding idle window may never outlive the absolute ceiling.
    table("sessions", { schema: "zeroship" })
      .check("sessions_idle_le_ceiling")
      .add({ expr: (col) => col("idle_expires_at").le(col("absolute_expires_at")) });
    // A sealed idempotent response with no expiry would be replayable forever.
    table("sessions", { schema: "zeroship" })
      .check("sessions_idem_window_shape")
      .add({
        expr: (col) =>
          col("idem_response_enc").isNull().or(col("idem_expires_at").isNotNull()),
      });
    // A hash and the keyring version that produced it travel together, in both
    // the current and the superseded slot. Without the pair a rotation that
    // crossed a keyring rotation would leave a hash nothing can re-derive.
    table("sessions", { schema: "zeroship" })
      .check("sessions_secret_version_shape")
      .add({
        expr: (col) =>
          col("secret_hash").isNull().and(col("secret_key_version").isNull())
            .or(col("secret_hash").isNotNull().and(col("secret_key_version").isNotNull())),
      });
    table("sessions", { schema: "zeroship" })
      .check("sessions_prev_secret_version_shape")
      .add({
        expr: (col) =>
          col("prev_secret_hash").isNull().and(col("prev_secret_key_version").isNull())
            .or(col("prev_secret_hash").isNotNull().and(col("prev_secret_key_version").isNotNull())),
      });
    // The presented secret is looked up by hash, so two rows may not share one.
    table("sessions", { schema: "zeroship" })
      .index("sessions_secret_hash_key")
      .add({
        on: ["secret_hash"],
        unique: true,
        where: (col) => col("secret_hash").isNotNull(),
      });
    // The predecessor lookup is what tells a replay inside the idempotency
    // window from a reuse outside it. Unique for the same reason the current
    // hash is: an ambiguous predecessor is an unanswerable question.
    table("sessions", { schema: "zeroship" })
      .index("sessions_prev_secret_hash_key")
      .add({
        on: ["prev_secret_hash"],
        unique: true,
        where: (col) => col("prev_secret_hash").isNotNull(),
      });
    table("sessions", { schema: "zeroship" })
      .index("sessions_person_idx")
      .add({ on: ["person_id"] });
    table("sessions", { schema: "zeroship" })
      .index("sessions_grant_idx")
      .add({ on: ["grant_id"] });
    // "Log this human out of everything they reached from this login" is a tree
    // delete over this edge rather than an HTTP fan-out.
    table("sessions", { schema: "zeroship" })
      .index("sessions_parent_idx")
      .add({
        on: ["parent_session_id"],
        where: (col) => col("parent_session_id").isNotNull(),
      });
    // The sweeper's driving predicate.
    table("sessions", { schema: "zeroship" })
      .index("sessions_absolute_expiry_idx")
      .add({ on: ["absolute_expires_at"] });
    table("sessions", { schema: "zeroship" })
      .foreignKey("sessions_person_id_fkey")
      .add({
        columns: ["person_id"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    table("sessions", { schema: "zeroship" })
      .foreignKey("sessions_grant_id_fkey")
      .add({
        columns: ["grant_id"],
        references: { table: "grants", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    table("sessions", { schema: "zeroship" })
      .foreignKey("sessions_parent_session_id_fkey")
      .add({
        columns: ["parent_session_id"],
        references: { table: "sessions", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });

    // ---- sortable typed-id collations --------------------------------------
    // Bytewise ordering for every typed-id column and every copy of one, for
    // the reason db/migrations-ts/20260831000001_sortable_entity_id_collations.ts
    // states: PostgreSQL's locale collation does not keep the base36 alphabet
    // in numeric order, and a copy under a different collation cannot serve an
    // indexed join against the collated original.
    //
    // The grant and session identifiers and their user references use the
    // text-plus-collation pattern. A LATER
    // file adding a key against these columns must author its own column
    // collated, because the engine's pre-migration catalog snapshot cannot see
    // a `raw` collation island - that is why
    // db/migrations-ts/20260906000200_apps_project_ownership_key.ts exists as
    // its own file.
    //
    // `client_id` is absent on purpose: an OAuth client id is not a typed id
    // and the collation map excludes that whole family. `subject` is absent for
    // the same reason - it is a pairwise derivation or a rendered user id, not
    // an identity domain this corpus mints.
    const typedIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
      grants: ["id", "person_id"],
      sessions: ["id", "grant_id", "parent_session_id", "person_id"],
    };
    for (const [tableName, columns] of Object.entries(typedIdColumnsByTable)) {
      const alterations = columns
        .map((column) => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`)
        .join(", ");
      raw({
        sql: `ALTER TABLE "zeroship"."${tableName}" ${alterations}`,
        reason:
          "typed-id text domains need bytewise comparison, including matching copies used by "
          + "indexed joins",
      });
    }
    for (const [tableName, columns] of Object.entries(userIdColumnsByTable)) {
      for (const column of columns) {
        table(tableName, { schema: "zeroship" })
          .check(`${tableName}_${column}_usr_shape`)
          .add({ expr: (col) => col(column).regex("^usr_[0-9a-z]{25}$") });
      }
    }

    // ---- grants ------------------------------------------------------------
    // The auth service and nothing else. Every other process reaches identity
    // through a credential auth mints, never through these rows, and the
    // single-writer property is what makes the absent RLS policy an argument
    // rather than an omission - see this file's header.
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "zeroship", names: ["grants", "sessions"] },
      to: ["zeroship_auth"],
    });
  },
};
