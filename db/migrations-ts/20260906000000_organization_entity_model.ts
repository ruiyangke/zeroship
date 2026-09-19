import { grant, now, raw, t, table } from "@zeroship/migrate";

const userIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  organizations: ["personal_owner_id", "created_by"],
  projects: ["created_by"],
  organization_members: ["user_id", "added_by", "changed_by"],
  project_members: ["user_id", "added_by", "changed_by"],
  organization_invites: ["invited_by", "consumed_by"],
};

// The ownership root. An ORGANIZATION owns projects, a project owns apps, and an
// app reaches its organization through `apps.project_id -> projects.organization_id`.
// `apps.organization_id` is an FK-CONSUMED copy rather than a second answer: the
// composite key over `(project_id, organization_id)` refuses any row whose copy
// disagrees with the project's organization.
//
// THE WORD IS SPELLED IN FULL EVERYWHERE A HUMAN READS IT. Columns, constraints
// and indexes say `organization`, never `org`. The abbreviation appears only
// inside the opaque typed-id VALUE (`org_...`), where it is a prefix byte and
// not a name. The word TEAM is reserved and names nothing here.
//
// A PERSONAL ORGANIZATION IS AN ORDINARY ROW. `personal_owner_id` is a nullable
// pointer to the single user a solo creator's organization was minted for, and
// no authorization read path derives authority from it: a personal organization
// has members, projects, apps and a billing subject exactly like any other.
// Clearing the pointer IS the
// personal-to-shared conversion, which is why the foreign key is SET NULL rather
// than CASCADE -- cascading would delete the billing subject, and would not even
// succeed, since it would then abort against the RESTRICT edge from projects.
//
// AUTHORITY IS TWO INTEGERS, NOT ONE ENUM. `organization_roles` is a closed
// ladder carrying `rank` (authority over apps and the organization) and
// `billing_rank` (authority over money). They are deliberately independent:
// `viewer` and `billing` share a rank, and `billing` outranks `admin` on money
// while `admin` outranks it on everything else. An actor may act on a target
// only when `actor.rank > target.rank AND actor.billing_rank >=
// target.billing_rank`, which is integer comparison and therefore evaluable
// inside the statement that performs the effect rather than only in a preceding
// SELECT.
//
// THE LADDER IS A FOREIGN-KEY TARGET AND THE CONTROL PLANE MAY ONLY READ IT.
// Every role-bearing column references it, so an unknown role is unspellable;
// and `zeroship_control` is granted SELECT and nothing else, so the process that
// mutates membership cannot edit what a role MEANS. That is what makes rank
// escalation unrepresentable rather than merely unimplemented: privilege follows
// the process, and this process has no INSERT to reach for.
//
// PER-PROJECT NARROWING IS HERE FROM THE FIRST DAY, and it is why projects exist
// in this change at all. An organization member at admin rank or above holds
// authority over every project in the organization. A member below admin holds
// authority only where a `project_members` row exists, and their effective rank
// there is min(organization rank, project rank). A project GRANTS and CEILINGS;
// it never widens. `billing_rank` stays organization-level entirely -- there is
// no per-project invoice, so there is no per-project money authority, and
// `project_members` carries no billing dimension to be confused about.
//
// THE MIN IS COMPUTED AT READ, AND NOTHING HERE ENFORCES IT. A CHECK cannot
// subquery, and freezing the organization rank into the project row would force
// a demotion to FAIL whenever the member held a higher project role -- the
// opposite of narrowing. So the schema makes the two ranks both available and
// the authorization vocabulary takes the minimum.
//
// WHAT IS STRUCTURAL IS THE PAIR OF THINGS A PROJECT MEMBERSHIP MUST NOT BE ABLE
// TO SAY. It must not name a user who is not a member of the project's
// organization, and it must not name a project belonging to another
// organization. Both are closed by COMPOSITE foreign keys over a shared
// `organization_id` column rather than by a check anything could forget to run:
// `(project_id, organization_id)` is consumed by `projects(id, organization_id)`
// and `(organization_id, user_id)` is consumed by the
// `organization_members_natural_key` unique, so the two agree on every write to
// either side.
//
// User references use the same internal `usr` ID contract as `zeroship.users.id`.
//
// `apps.project_id` IS DECLARED WITH THE `apps` TABLE, and this migration owns
// only its bytewise collation. It is nullable there because a deleted app is
// detached from its project while keeping its organization; the composite key
// that consumes the pair lives in
// db/migrations-ts/20260906000200_apps_project_ownership_key.ts, and the
// organization copy it pairs with is declared with `apps` in
// db/migrations-ts/20260702000200_control_tables.ts.
//
// DELIBERATELY NOT HERE: the subscription move (`organizations.plan_id` and the
// composite tie to `apps.plan_id`), the project-level `sector_identifier`, and
// project-owned data resources with their capability bindings. The first two
// have no design document yet; the third is designed in
// docs/proposals/2026-08-28-app-database-decoupling.md. This header is the
// design record for what landed, and the three are named here so a later reader
// can tell a deferral from an oversight. Each carries its own consumers to
// re-plumb.
//
// `crates/zeroship-migrate-node/tests/platform_corpus/organization_authority.rs`
// applies the corpus to owned PostgreSQL databases and exercises these
// constraints, accepted controls, privileges and collations through the Rust
// driver. Run it with `cargo xtask test migrations`.
//
// EACH NEW TABLE IS ALSO REGISTERED IN policies/platform-table-owners.json, and
// that file is not optional bookkeeping: the applier refuses fail-closed on any
// op targeting a table with no ownership entry, so a table created here without
// one halts the whole corpus.
export default {
  name: "organization_entity_model",
  schema() {
    // ---- the closed authority ladder -------------------------------------
    table("organization_roles", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        role: t.text().notNull(),
        rank: t.int().notNull(),
        billing_rank: t.int().notNull(),
        label: t.text().notNull(),
      },
      primaryKey: ["id"],
    });
    table("organization_roles", { schema: "zeroship" }).unique("organization_roles_natural_key").add({ columns: ["role"] });
    // The anchor `organization_invites` consumes. An invite freezes the whole
    // triple in its own row so its escalation CHECK needs no subquery; this
    // unique is what stops the frozen copy drifting from the ladder.
    table("organization_roles", { schema: "zeroship" })
      .unique("organization_roles_rank_identity_key")
      .add({ columns: ["role", "rank", "billing_rank"] });
    // The seed is `raw` because the platform ceiling grants no DML capability
    // key at all (policies/platform.policy.toml), and `sql.raw` is the granted,
    // exercised path. The ladder must exist before the first membership row
    // references it, so it is seeded in the migration rather than by test or
    // runtime code.
    raw({
      sql:
        "INSERT INTO zeroship.organization_roles (role, rank, billing_rank, label) VALUES "
        + "('viewer',10,0,'Read apps, environment and deployments'),"
        + "('developer',20,0,'Deploy, and manage environment and secrets'),"
        + "('billing',10,20,'Manage billing and payouts; no deploy access'),"
        + "('admin',30,10,'Manage members, and read billing'),"
        + "('owner',40,20,'Full authority, including organization settings')",
      reason:
        "the closed role ladder must exist before any membership row references it, and the "
        + "platform ceiling grants no DML capability key",
    });

    // ---- organizations ----------------------------------------------------
    table("organizations", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        slug: t.text({ caseSensitive: false }).notNull(),
        name: t.text().notNull(),
        // Seeded from the minting user's address and independent thereafter.
        // The billed party and the notified party separate here for the first
        // time.
        billing_email: t.text({ caseSensitive: false }).notNull(),
        personal_owner_id: t.text(),
        created_by: t.text(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
        // The close, rather than a DELETE: the organization is the billing
        // subject and every invoice names it, while `projects.organization_id`
        // is RESTRICT. Setting the timestamp releases the slug and the personal
        // slot it was holding (the two partial indexes below) without taking the
        // counterparty out of a money record.
        dissolved_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("organizations", { schema: "zeroship" })
      .check("organizations_id_shape")
      .add({ expr: (col) => col("id").regex("^org_[0-9a-z]{25}$") });
    // Unique among LIVE organizations: a closed organization releases its
    // human-facing slug, and the identity is `id`, which nothing here touches.
    table("organizations", { schema: "zeroship" })
      .index("organizations_live_slug_key")
      .add({
        on: ["slug"],
        unique: true,
        where: (col) => col("dissolved_at").isNull(),
      });
    table("organizations", { schema: "zeroship" })
      .check("organizations_slug_grammar")
      .add({ expr: (col) => col("slug").regex("^[a-z0-9][a-z0-9-]*$") });
    // A user has at most one LIVE personal organization. Partial and unique:
    // shared organizations leave the column NULL and do not compete for the
    // slot, and a dissolved organization releases it so the creator's next
    // deploy can mint a replacement.
    table("organizations", { schema: "zeroship" })
      .index("organizations_live_personal_owner_key")
      .add({
        on: ["personal_owner_id"],
        unique: true,
        where: (col) => col("personal_owner_id").isNotNull().and(col("dissolved_at").isNull()),
      });
    table("organizations", { schema: "zeroship" })
      .foreignKey("organizations_personal_owner_id_fkey")
      .add({
        columns: ["personal_owner_id"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });
    table("organizations", { schema: "zeroship" })
      .foreignKey("organizations_created_by_fkey")
      .add({
        columns: ["created_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });

    // ---- projects ---------------------------------------------------------
    table("projects", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        organization_id: t.text().notNull(),
        slug: t.text({ caseSensitive: false }).notNull(),
        name: t.text().notNull(),
        created_by: t.text(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("projects", { schema: "zeroship" })
      .check("projects_id_shape")
      .add({ expr: (col) => col("id").regex("^prj_[0-9a-z]{25}$") });
    // A project slug is organization-local. Only `apps.name` stays globally
    // unique, because it is the Host label the gateway routes on.
    table("projects", { schema: "zeroship" })
      .unique("projects_organization_slug_key")
      .add({ columns: ["organization_id", "slug"] });
    table("projects", { schema: "zeroship" })
      .check("projects_slug_grammar")
      .add({ expr: (col) => col("slug").regex("^[a-z0-9][a-z0-9-]*$") });
    // The anchor `project_members` consumes to prove a membership's project and
    // its organization are the same organization.
    table("projects", { schema: "zeroship" })
      .unique("projects_organization_identity_key")
      .add({ columns: ["id", "organization_id"] });
    // RESTRICT, not cascade: a project will own creator data, and deleting an
    // organization must not silently take it.
    table("projects", { schema: "zeroship" })
      .foreignKey("projects_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: { table: "organizations", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    table("projects", { schema: "zeroship" })
      .foreignKey("projects_created_by_fkey")
      .add({
        columns: ["created_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });

    // ---- organization membership ------------------------------------------
    // No `revoked_at`. Removal is a DELETE, because authority is re-derived per
    // request rather than read from a tombstone every reader must remember to
    // filter.
    table("organization_members", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        organization_id: t.text().notNull(),
        user_id: t.text().notNull(),
        role: t.text().notNull(),
        added_at: t.timestamp().notNull().default(now()),
        added_by: t.text(),
        changed_at: t.timestamp().notNull().default(now()),
        changed_by: t.text(),
      },
      primaryKey: ["id"],
    });
    table("organization_members", { schema: "zeroship" }).unique("organization_members_natural_key").add({ columns: ["organization_id", "user_id"] });
    table("organization_members", { schema: "zeroship" })
      .index("organization_members_user_idx")
      .add({ on: ["user_id"] });
    // Partial and NOT unique: an organization may hold several owners. "At least
    // one owner" is not expressible as a CHECK and is not claimed here; this
    // index only makes counting them cheap for the vocabulary that enforces it.
    table("organization_members", { schema: "zeroship" })
      .index("organization_members_owner_idx")
      .add({ on: ["organization_id"], where: (col) => col("role").eq("owner") });
    table("organization_members", { schema: "zeroship" })
      .foreignKey("organization_members_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: { table: "organizations", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    table("organization_members", { schema: "zeroship" })
      .foreignKey("organization_members_user_id_fkey")
      .add({
        columns: ["user_id"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    table("organization_members", { schema: "zeroship" })
      .foreignKey("organization_members_role_fkey")
      .add({
        columns: ["role"],
        references: { table: "organization_roles", columns: ["role"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    table("organization_members", { schema: "zeroship" })
      .foreignKey("organization_members_added_by_fkey")
      .add({
        columns: ["added_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });
    table("organization_members", { schema: "zeroship" })
      .foreignKey("organization_members_changed_by_fkey")
      .add({
        columns: ["changed_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });

    // ---- project membership: THE NARROWING --------------------------------
    // `organization_id` is not a convenience copy. Two composite foreign keys
    // CONSUME it, so PostgreSQL re-checks it on every write to either side: a
    // row naming a project in one organization and a member of another cannot be
    // written, and neither can a row naming a user who is not a member of the
    // organization at all. Both are unspellable rather than checked.
    table("project_members", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        project_id: t.text().notNull(),
        organization_id: t.text().notNull(),
        user_id: t.text().notNull(),
        role: t.text().notNull(),
        added_at: t.timestamp().notNull().default(now()),
        added_by: t.text(),
        changed_at: t.timestamp().notNull().default(now()),
        changed_by: t.text(),
      },
      primaryKey: ["id"],
    });
    table("project_members", { schema: "zeroship" }).unique("project_members_natural_key").add({ columns: ["project_id", "user_id"] });
    // NO EXPLICIT INDEX ON THE TWO COMPOSITE EDGES. Neither the `id` primary
    // key nor the `(project_id, user_id)` natural key covers either foreign
    // key's column pair -- and the engine emits an index for a composite
    // foreign key of its own accord
    // (`project_members_organization_member_fkey_idx`,
    // `project_members_project_ownership_fkey_idx` on a live apply). Declaring
    // them here as well produces two identical indexes on the same columns.
    // Single-column foreign keys get no such index, which is why
    // `organization_members_user_idx` above IS declared.
    table("project_members", { schema: "zeroship" })
      .foreignKey("project_members_project_ownership_fkey")
      .add({
        columns: ["project_id", "organization_id"],
        references: { table: "projects", columns: ["id", "organization_id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    table("project_members", { schema: "zeroship" })
      .foreignKey("project_members_organization_member_fkey")
      .add({
        columns: ["organization_id", "user_id"],
        references: {
          table: "organization_members",
          columns: ["organization_id", "user_id"],
          schema: "zeroship",
        },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    table("project_members", { schema: "zeroship" })
      .foreignKey("project_members_role_fkey")
      .add({
        columns: ["role"],
        references: { table: "organization_roles", columns: ["role"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    table("project_members", { schema: "zeroship" })
      .foreignKey("project_members_added_by_fkey")
      .add({
        columns: ["added_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });
    table("project_members", { schema: "zeroship" })
      .foreignKey("project_members_changed_by_fkey")
      .add({
        columns: ["changed_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });

    // ---- invites ----------------------------------------------------------
    // THE SECRET IS NEVER STORED. `token_hash` holds the digest and carries its
    // own UNIQUE, so the token is not enumerable and a redemption is a lookup by
    // hash. The typed id exists so an audit row can name a pending invite and a
    // revoke can address one without ever handling the secret.
    //
    // The role triple and the inviter's rank pair are frozen into the row ON
    // PURPOSE, pinned to the ladder by a composite foreign key so they cannot
    // drift. That is what lets escalation-by-deferred-grant be a single-row
    // CHECK: a CHECK cannot subquery, and this shape means it does not need to.
    //
    // THE FROZEN INVITER RANK IS HALF THE FENCE, AND SAYING SO IS THE POINT. An
    // invite issued by an admin who is later demoted still names the higher role
    // at redemption. The CHECK closes escalation at ISSUE time structurally;
    // redemption must re-derive the inviter's live rank against
    // organization_members and refuse if the comparison no longer holds.
    table("organization_invites", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        token_hash: t.bytes().notNull(),
        organization_id: t.text().notNull(),
        email: t.text({ caseSensitive: false }).notNull(),
        role: t.text().notNull(),
        role_rank: t.int().notNull(),
        role_billing_rank: t.int().notNull(),
        invited_by: t.text(),
        invited_by_rank: t.int().notNull(),
        invited_by_billing_rank: t.int().notNull(),
        purpose: t.text().notNull(),
        issued_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
        consumed_at: t.timestamp(),
        consumed_by: t.text(),
        // NULL until a delivery attempt resolves. The row is written BEFORE the
        // mail is sent, so that a redemption can never arrive before the hash it
        // is matched against exists; the outcome is recorded by a later update.
        delivery: t.text(),
      },
      primaryKey: ["id"],
    });
    table("organization_invites", { schema: "zeroship" })
      .check("organization_invites_id_shape")
      .add({ expr: (col) => col("id").regex("^ivt_[0-9a-z]{25}$") });
    table("organization_invites", { schema: "zeroship" })
      .unique("organization_invites_token_key")
      .add({ columns: ["token_hash"] });
    table("organization_invites", { schema: "zeroship" })
      .check("organization_invites_purpose_check")
      .add({ expr: (col) => col("purpose").eq("organization_invite") });
    table("organization_invites", { schema: "zeroship" })
      .check("organization_invites_delivery_check")
      .add({ expr: (col) => col("delivery").in(["sent", "suppressed", "failed"]) });
    table("organization_invites", { schema: "zeroship" })
      .check("organization_invites_no_escalation")
      .add({
        expr: (col) =>
          col("invited_by_rank")
            .gt(col("role_rank"))
            .and(col("invited_by_billing_rank").ge(col("role_billing_rank"))),
      });
    // At most one live invite per (organization, email). The predicate cannot
    // also test expiry: a partial index predicate must be immutable, and `now()`
    // is not. An expired invite is therefore still occupying the slot until it
    // is consumed or deleted, which is the control plane's job.
    table("organization_invites", { schema: "zeroship" })
      .index("organization_invites_one_active")
      .add({
        on: ["organization_id", "email"],
        unique: true,
        where: (col) => col("consumed_at").isNull(),
      });
    table("organization_invites", { schema: "zeroship" })
      .index("organization_invites_expiry_idx")
      .add({ on: ["expires_at"] });
    table("organization_invites", { schema: "zeroship" })
      .foreignKey("organization_invites_role_fkey")
      .add({
        columns: ["role", "role_rank", "role_billing_rank"],
        references: {
          table: "organization_roles",
          columns: ["role", "rank", "billing_rank"],
          schema: "zeroship",
        },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    table("organization_invites", { schema: "zeroship" })
      .foreignKey("organization_invites_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: { table: "organizations", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
        onUpdate: "restrict",
      });
    // The inviter may be erased; the rank they issued under may not, because the
    // escalation CHECK reads it. SET NULL clears the pointer and leaves the
    // frozen ranks standing.
    table("organization_invites", { schema: "zeroship" })
      .foreignKey("organization_invites_invited_by_fkey")
      .add({
        columns: ["invited_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });
    table("organization_invites", { schema: "zeroship" })
      .foreignKey("organization_invites_consumed_by_fkey")
      .add({
        columns: ["consumed_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
        onUpdate: "restrict",
      });

    // ---- apps: only the project_id collation is owned here -----------------
    // `apps.project_id` and its shape check are declared with the `apps` table
    // in db/migrations-ts/20260702000200_control_tables.ts; the composite key
    // that consumes the pair lives in
    // db/migrations-ts/20260906000200_apps_project_ownership_key.ts. What this
    // migration contributes to `apps` is the bytewise collation registered in
    // the map below, without which a join from a collated id to its copy would
    // silently fall back to the locale ordering.

    // ---- sortable typed-id collations -------------------------------------
    // Every column above whose whole semantic domain is a canonical typed id,
    // plus every foreign-key copy of one. PostgreSQL's locale collation does not
    // keep the base36 alphabet in numeric order, so a sortable entity id needs
    // bytewise ordering -- and the COPIES need the identical collation even
    // though nothing orders them, because a join against the collated id cannot
    // use a copy's ordinary index when the two collations differ. A missed copy
    // is silent: it degrades a join rather than erroring, which is why they are
    // registered here in the same migration that creates them rather than left
    // for a later sweep to notice.
    //
    // The map is semantic, not name-based, exactly as
    // db/migrations-ts/20260831000001_sortable_entity_id_collations.ts states.
    // `organization_roles.role` and its copies are deliberately absent: a role is
    // a closed vocabulary word, not an identity domain. The citext slug and
    // email columns are absent for a stronger reason -- they are not `text`, and
    // altering them to a collated `text` would destroy their case-insensitive
    // comparison.
    const typedIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
      organizations: ["id", "created_by", "personal_owner_id"],
      projects: ["id", "organization_id", "created_by"],
      organization_members: ["organization_id", "user_id", "added_by", "changed_by"],
      project_members: ["project_id", "organization_id", "user_id", "added_by", "changed_by"],
      organization_invites: ["id", "organization_id", "invited_by", "consumed_by"],
      apps: ["project_id"],
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
    // The control plane owns every table here. The auth service reads two of
    // them, and only for the account reaper's ownership check; the gateway never
    // resolves creator authority, and the worker holds nothing here, so neither
    // needs a revoke.
    //
    // WHAT DENIES THE WORKER: PostgreSQL's OWNER-ONLY DEFAULT - a newly
    // created table has a null `relacl` and nobody but the owner holds anything.
    // It is NOT the `ALTER DEFAULT PRIVILEGES ... REVOKE` in
    // db/migrations-ts/20260702000900_grants.ts, whose lines
    // store nothing - revoking a privilege that was never in the default set is
    // a no-op.
    //
    // The distinction is load-bearing rather than pedantic. An ambient default
    // is not a fence: a later migration granting the worker anything on these
    // tables succeeds silently, and there is no REVOKE here to contradict it.
    // If that becomes a real risk, add an explicit revoke so the claim supports
    // itself instead of resting on what nobody has done yet.
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: [
          "organizations",
          "organization_members",
          "organization_invites",
          "projects",
          "project_members",
        ],
      },
      to: ["zeroship_control"],
    });
    // SELECT ONLY, and this is the load-bearing half of the grant. The ladder is
    // the definition of authority; a process that could INSERT into it could
    // mint a role outranking every owner, so rank escalation is unrepresentable
    // for the control plane rather than merely unimplemented.
    grant({
      privileges: ["select"],
      on: { kind: "table", schema: "zeroship", names: ["organization_roles"] },
      to: ["zeroship_control"],
    });
    // The account reaper decides the ownership rule INSIDE its erasure
    // transaction, under the same `organizations` row lock every membership
    // mutation takes, so it reads both tables and takes the lock on the
    // organization. The column grant is what makes that lock expressible: a
    // `SELECT ... FOR UPDATE` is refused with SELECT alone, while the column
    // form permits the lock and no writable column, so the reaper can serialize
    // against a departure without being able to rename, re-slug or dissolve.
    grant({
      privileges: ["select"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: ["organizations", "organization_members"],
      },
      to: ["zeroship_auth"],
    });
    raw({
      sql: "GRANT UPDATE (id) ON zeroship.organizations TO zeroship_auth",
      reason:
        "PostgreSQL requires UPDATE for a row lock; the column form grants the lock and no writable column",
    });
  },
};
