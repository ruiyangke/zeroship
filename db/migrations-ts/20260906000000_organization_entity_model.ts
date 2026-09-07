import { grant, now, raw, t, table } from "@zeroship/migrate";

// The ownership root. An ORGANIZATION owns projects, a project owns apps, and an
// app reaches its organization through exactly ONE path: apps.project_id ->
// projects.organization_id. There is no second edge to keep in agreement,
// because a second edge is a second answer.
//
// THE WORD IS SPELLED IN FULL EVERYWHERE A HUMAN READS IT. Columns, constraints
// and indexes say `organization`, never `org`. The abbreviation appears only
// inside the opaque typed-id VALUE (`org_...`), where it is a prefix byte and
// not a name. The word TEAM is reserved and names nothing here.
//
// A PERSONAL ORGANIZATION IS AN ORDINARY ROW. `personal_owner_id` is a nullable
// pointer to the single user a solo creator's organization was minted for, and
// no read path branches on it: a personal organization has members, projects,
// apps and a billing subject exactly like any other. Clearing the pointer IS the
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
// and `(organization_id, user_id)` is consumed by the `organization_members`
// primary key, so the two agree on every write to either side.
//
// USER AND APP FOREIGN KEYS POINT AT uuid COLUMNS, ON PURPOSE. `zeroship.users`
// and `zeroship.apps` still key on uuid; converting them to typed ids is a
// separate sweep with its own consumers. The new tables use typed-id TEXT for
// their OWN ids and keep uuid where they reference those two.
//
// `apps.project_id` IS ADDED NOT NULL WITH NO BACKFILL. Pre-launch, no deployed
// database holds app rows to carry across, so a default-less NOT NULL add is the
// end state rather than a step toward it. `zeroship.app_members` is DELETED in
// the same change, not demoted: app-level membership is replaced by organization
// membership narrowed per project, and leaving both would leave two answers.
//
// DELIBERATELY NOT HERE: the subscription move (`organizations.plan_id` and the
// composite tie to `apps.plan_id`), the project-level `sector_identifier`, and
// project-owned data resources with their capability bindings. NO COMMITTED
// DOCUMENT DESIGNS ANY OF THE THREE. This header is the design record for what
// landed, and the three are named here so a later reader can tell a deferral
// from an oversight -- not as a pointer to a design that exists somewhere else.
// Each carries its own consumers to re-plumb, and each needs its design written
// before it is built.
//
// EVERY CLAIM ABOVE ABOUT WHAT CANNOT BE WRITTEN IS RE-MEASURED, not asserted:
// `tests/organization_authority_gate.sh` applies this corpus to an empty
// database and drives each refusal, each paired control, and the collations
// below, requiring PostgreSQL to name the exact constraint it refused on.
//
// EACH NEW TABLE IS ALSO REGISTERED IN policies/platform-table-owners.json, and
// that file is not optional bookkeeping: the applier refuses fail-closed on any
// op targeting a table with no ownership entry, so a table created here without
// one halts the whole corpus. Its entries also outlive the tables they name -
// `app_members` stays listed after the drop below, because the earlier
// migrations that create and constrain it still run on a fresh database.
export default {
  name: "organization_entity_model",
  schema() {
    // ---- the closed authority ladder -------------------------------------
    table("organization_roles", { schema: "zeroship" }).create({
      columns: {
        role: t.text().notNull(),
        rank: t.int().notNull(),
        billing_rank: t.int().notNull(),
        label: t.text().notNull(),
      },
      primaryKey: ["role"],
    });
    // The anchor `organization_invites` consumes. An invite freezes the whole
    // triple in its own row so its escalation CHECK needs no subquery; this
    // unique is what stops the frozen copy drifting from the ladder.
    table("organization_roles", { schema: "zeroship" })
      .unique("organization_roles_rank_identity_key")
      .add({ columns: ["role", "rank", "billing_rank"] });
    // The seed is `raw` because the platform ceiling grants no DML capability
    // key at all (policies/platform.policy.toml), and `sql.raw` is the granted,
    // exercised path. Every other catalog row in this corpus is inserted by test
    // or runtime code; this one cannot be, because the ladder must exist before
    // the first membership row references it.
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
        personal_owner_id: t.uuid(),
        created_by: t.uuid(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("organizations", { schema: "zeroship" })
      .check("organizations_id_shape")
      .add({ expr: (col) => col("id").regex("^org_[0-9A-Za-z]{22}$") });
    table("organizations", { schema: "zeroship" })
      .unique("organizations_slug_key")
      .add({ columns: ["slug"] });
    table("organizations", { schema: "zeroship" })
      .check("organizations_slug_grammar")
      .add({ expr: (col) => col("slug").regex("^[a-z0-9][a-z0-9-]*$") });
    // A user has at most one personal organization. Partial and unique: shared
    // organizations leave the column NULL and do not compete for the slot.
    table("organizations", { schema: "zeroship" })
      .index("organizations_personal_owner_key")
      .add({
        on: ["personal_owner_id"],
        unique: true,
        where: (col) => col("personal_owner_id").isNotNull(),
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
        created_by: t.uuid(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("projects", { schema: "zeroship" })
      .check("projects_id_shape")
      .add({ expr: (col) => col("id").regex("^prj_[0-9A-Za-z]{22}$") });
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
        organization_id: t.text().notNull(),
        user_id: t.uuid().notNull(),
        role: t.text().notNull(),
        added_at: t.timestamp().notNull().default(now()),
        added_by: t.uuid(),
        changed_at: t.timestamp().notNull().default(now()),
        changed_by: t.uuid(),
      },
      primaryKey: ["organization_id", "user_id"],
    });
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
        project_id: t.text().notNull(),
        organization_id: t.text().notNull(),
        user_id: t.uuid().notNull(),
        role: t.text().notNull(),
        added_at: t.timestamp().notNull().default(now()),
        added_by: t.uuid(),
        changed_at: t.timestamp().notNull().default(now()),
        changed_by: t.uuid(),
      },
      primaryKey: ["project_id", "user_id"],
    });
    // NO EXPLICIT INDEX ON THE TWO COMPOSITE EDGES. The primary key leads on
    // project_id, so neither composite foreign key is covered by it -- and the
    // engine emits an index for a composite foreign key of its own accord
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
        invited_by: t.uuid(),
        invited_by_rank: t.int().notNull(),
        invited_by_billing_rank: t.int().notNull(),
        purpose: t.text().notNull(),
        issued_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
        consumed_at: t.timestamp(),
        consumed_by: t.uuid(),
        // NULL until a delivery attempt resolves. The row is written BEFORE the
        // mail is sent, so that a redemption can never arrive before the hash it
        // is matched against exists; the outcome is recorded by a later update.
        delivery: t.text(),
      },
      primaryKey: ["id"],
    });
    table("organization_invites", { schema: "zeroship" })
      .check("organization_invites_id_shape")
      .add({ expr: (col) => col("id").regex("^ivt_[0-9A-Za-z]{22}$") });
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

    // ---- apps: re-rooted onto a project -----------------------------------
    // NOT NULL with no default and no backfill. Pre-launch there is no deployed
    // database holding app rows, so this is the end state rather than a step
    // toward one; a development database with rows is recreated rather than
    // migrated.
    table("apps", { schema: "zeroship" })
      .column("project_id")
      .add({ type: t.text().notNull() });
    table("apps", { schema: "zeroship" })
      .check("apps_project_id_shape")
      .add({ expr: (col) => col("project_id").regex("^prj_[0-9A-Za-z]{22}$") });
    // RESTRICT: an app is the deployable unit and a project delete must not take
    // one silently. There is no `apps.organization_id` -- the organization is
    // reached through the project, and one path cannot disagree with itself.
    table("apps", { schema: "zeroship" })
      .foreignKey("apps_project_id_fkey")
      .add({
        columns: ["project_id"],
        references: { table: "projects", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    table("apps", { schema: "zeroship" })
      .index("apps_project_id_idx")
      .add({ on: ["project_id"] });

    // App-level membership is replaced, not demoted. Organization membership
    // narrowed per project is the one answer to "who may act on this app", and
    // leaving the old table would leave a second one.
    table("app_members", { schema: "zeroship" }).drop({ ifExists: true });

    // ---- sortable typed-id collations -------------------------------------
    // Every column above whose whole semantic domain is a canonical typed id,
    // plus every foreign-key copy of one. PostgreSQL's locale collation does not
    // keep the base62 alphabet in numeric order, so a sortable entity id needs
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
      organizations: ["id"],
      projects: ["id", "organization_id"],
      organization_members: ["organization_id"],
      project_members: ["project_id", "organization_id"],
      organization_invites: ["id", "organization_id"],
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

    // ---- grants ------------------------------------------------------------
    // The control plane is the only service that reaches any of these. The
    // gateway and the auth service never resolve creator authority, and the
    // worker is denied by default in the platform schema
    // (db/migrations-ts/20260818000200_worker_database_authority.ts revokes
    // future tables through ALTER DEFAULT PRIVILEGES), so neither needs a
    // revoke here.
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
  },
};
