import { enumType, table, t } from "@zeroship/migrate";

// issue-tracker's schema, authored migration-first. Committed migrations are
// the sole source for the generated runtime descriptor that `env.db` is
// installed from; there is deliberately no inline `schema` export anywhere in
// this example (that is the #209/#174 mechanism that left hr-system's
// procedures throwing "Cannot read properties of undefined (reading 'find')").
//
// Two spelling rules the types do not enforce:
//
//   1. The seven platform system columns (id, created_at, updated_at,
//      created_by, updated_by, version, deleted_at) are INJECTED by the
//      confined charter. Declaring `id` here collides with the injected
//      column and the descriptor is refused.
//
//   2. `t.ref()` is a column TYPE that names a table only. The column FACET
//      `.references(table, column)` is the stronger construct: it names a
//      target table AND column and carries ON DELETE / ON UPDATE. Emitting the
//      type where the facet was meant silently narrows the declaration
//      (zero-migrate-ir/src/ir.rs, ColType::Ref). Every cross-table link below
//      uses the facet.
//
// UNIQUENESS IS SPELLED AS A UNIQUE INDEX, NOT AS `uniques`. A table-level
// `uniques` entry is a HARD authoring error on SQLite -- the SQLite CREATE
// renders from the column descriptor and a table-level constraint is never
// threaded into the emitter, so it is refused fail-closed rather than silently
// dropped (sdks/migrate/src/types.ts:1091-1094). `indexes` lower on both
// backends, and SQLite is the dev backend, so `indexes[{ unique: true }]` is
// the only portable spelling. Every join table below carries one; without it
// `cc.add` and `keywords.attach` are duplicate-row generators under retry.
//
// AN EARLIER VERSION OF THIS FILE WAS WRITTEN AGAINST AN IMAGINED, POORER DSL:
// it claimed "the engine has no CHECK-constraint spelling here" and stored
// every integer in `t.double()`. Both were false. The lexicon carries
// `t.int()`, `t.smallInt()`, `t.bigInt()`, `t.timestamp()`, `t.uuid()`,
// `t.numeric()` and `t.enum()` (types.ts:262-307), the engine's closed
// `ColType` accepts them (ir.rs:546+), and the runtime bridge maps `int` to
// `int` rather than widening it to `double` (db-lexicon.ts:124-137).
// `CreateTableArgs` additionally carries `checks`, `uniques`, `indexes[].unique`,
// partial-index `where`, and FK `onDelete`/`onUpdate` (types.ts:1085-1140).
// Note `t.date()` is admitted ONLY as a PostgreSQL domain base type
// (ir.rs, ColType::Date), so a plain calendar column is `t.timestamp()`.
//
// Platform ids are typed-id TEXT (`usr_...`), so every FK column is t.text().
export default {
  name: "create_issue_tracker",
  up() {
    // -----------------------------------------------------------------------
    // Closed vocabularies
    // -----------------------------------------------------------------------
    //
    // Each of these is also declared in TypeScript (src/lib/workflow.ts,
    // src/lib/quicksearch.ts, src/lib/flags.ts). The two declarations must
    // agree; the database one is the one a direct write cannot talk past.
    enumType("issue_kind").create({ values: ["defect", "enhancement", "task"] });
    enumType("issue_status").create({
      values: ["UNCONFIRMED", "CONFIRMED", "IN_PROGRESS", "RESOLVED", "VERIFIED", "CLOSED"],
    });
    enumType("issue_resolution").create({
      values: ["FIXED", "INVALID", "WONTFIX", "DUPLICATE", "WORKSFORME", "INCOMPLETE"],
    });
    enumType("issue_severity").create({
      values: ["blocker", "critical", "major", "normal", "minor", "trivial"],
    });
    enumType("issue_priority").create({ values: ["P1", "P2", "P3", "P4", "P5"] });
    enumType("flag_target_type").create({ values: ["issue", "attachment"] });
    enumType("flag_status").create({ values: ["+", "-", "?"] });
    // One member, and that is the point: the fanout in `notifyIssueChange` is
    // the only producer, so the set is closed at one until a second notifier
    // exists. Spelling it as an enum is what makes adding the second one a
    // schema change rather than a free-text drift.
    enumType("notification_kind").create({ values: ["issue_changed"] });

    // -----------------------------------------------------------------------
    // People and groups
    // -----------------------------------------------------------------------
    table("users").create({
      columns: {
        email: t.text().notNull().unique(),
        handle: t.text().notNull().unique(),
        // Bugzilla has login + realname and no third name field. `name` is the
        // display name; `realName` was a duplicate of it and is gone.
        name: t.text().notNull(),
        timezone: t.text().notNull().default("UTC"),
        isAdmin: t.boolean().notNull().default(false),
        isDisabled: t.boolean().notNull().default(false),
        // `users.updatePrefs` had no storage at all until this column.
        prefs: t.json(),
      },
    });

    table("groups").create({
      columns: {
        name: t.text().notNull().unique(),
        description: t.text(),
        isIssueGroup: t.boolean().notNull().default(true),
      },
    });

    table("groupMembers").create({
      columns: {
        groupId: t.text().notNull().references("groups", "id"),
        userId: t.text().notNull().references("users", "id"),
      },
      indexes: [
        { name: "group_members_pair_uniq", on: ["groupId", "userId"], unique: true },
        // Every permission check resolves a user's groups, so the reverse
        // direction is a read path, not a maintenance convenience.
        { name: "group_members_user_idx", on: ["userId"] },
      ],
    });

    // -----------------------------------------------------------------------
    // Product structure
    // -----------------------------------------------------------------------
    table("products").create({
      columns: {
        name: t.text().notNull().unique(),
        // The short prefix a bug is referred to by: PARSER-12, RUNTIME-3.
        // Unique, because it is half of every bug identifier a human types.
        key: t.text().notNull().unique(),
        description: t.text(),
        classification: t.text().notNull().default("Unclassified"),
        defaultMilestone: t.text(),
        allowsUnconfirmed: t.boolean().notNull().default(true),
        isActive: t.boolean().notNull().default(true),
        // Bugzilla's voting mechanics. Without these three the classic
        // "votes auto-confirm an UNCONFIRMED bug" flow is unrepresentable.
        votesPerUser: t.int().notNull().default(0),
        maxVotesPerIssue: t.int().notNull().default(0),
        votesToConfirm: t.int().notNull().default(0),
      },
    });

    table("components").create({
      columns: {
        productId: t.text().notNull().references("products", "id"),
        name: t.text().notNull(),
        description: t.text(),
        // Bugzilla requires `initialowner` per component. Nullable here left
        // `bugs.create` with no guaranteed assignment target.
        defaultAssigneeId: t.text().notNull().references("users", "id"),
        defaultQaContactId: t.text().references("users", "id"),
        // Bugzilla's `initialcc`: users CC'd onto every new bug in this
        // component. Stored as a JSON array of user ids.
        initialCc: t.json(),
        isActive: t.boolean().notNull().default(true),
      },
      indexes: [
        { name: "components_product_idx", on: ["productId"] },
        // Two "General" components in one product breaks QuickSearch's
        // `comp:parser` addressing.
        { name: "components_product_name_uniq", on: ["productId", "name"], unique: true },
      ],
    });

    table("versions").create({
      columns: {
        productId: t.text().notNull().references("products", "id"),
        name: t.text().notNull(),
        // Deliberately a float, not an int: fractional keys let a version be
        // inserted between two existing ones without renumbering the rest.
        // Bugzilla's own sortkeys are ints; this is a considered divergence.
        sortKey: t.double().notNull().default(0),
        isActive: t.boolean().notNull().default(true),
      },
      indexes: [
        { name: "versions_product_idx", on: ["productId"] },
        { name: "versions_product_name_uniq", on: ["productId", "name"], unique: true },
      ],
    });

    table("milestones").create({
      columns: {
        productId: t.text().notNull().references("products", "id"),
        name: t.text().notNull(),
        sortKey: t.double().notNull().default(0),
        isActive: t.boolean().notNull().default(true),
      },
      indexes: [
        { name: "milestones_product_idx", on: ["productId"] },
        { name: "milestones_product_name_uniq", on: ["productId", "name"], unique: true },
      ],
    });

    table("productGroups").create({
      columns: {
        productId: t.text().notNull().references("products", "id"),
        groupId: t.text().notNull().references("groups", "id"),
      },
      indexes: [
        { name: "product_groups_pair_uniq", on: ["productId", "groupId"], unique: true },
        { name: "product_groups_group_idx", on: ["groupId"] },
      ],
    });

    // -----------------------------------------------------------------------
    // Issues
    // -----------------------------------------------------------------------
    //
    // `status` and `resolution` are separate columns because Bugzilla treats
    // them as separate: a bug is RESOLVED *as* FIXED. Collapsing them loses
    // the distinction between "closed and fixed" and "closed as invalid",
    // which every triage query in this app depends on.
    //
    // `severity` (impact) and `priority` (scheduling) are likewise distinct
    // fields, not two names for one axis.
    table("issues").create({
      columns: {
        productId: t.text().notNull().references("products", "id"),
        componentId: t.text().notNull().references("components", "id"),
        // NULLABLE, and this reversed an earlier decision that read "version is
        // mandatory on a Bugzilla bug; nullable here was wrong". That was true
        // while every row WAS a bug: "version found in" is a defect concept.
        // An enhancement has no version it was found in, so requiring one meant
        // a feature request had to name a version it has nothing to do with.
        versionId: t.text().references("versions", "id"),
        milestoneId: t.text().references("milestones", "id"),
        // Bugzilla users address bugs by alias constantly. Unique when set,
        // via the plain unique index below: NULLs compare distinct in both
        // PostgreSQL and SQLite, so any number of alias-less bugs coexist
        // without needing a partial predicate.
        alias: t.text(),
        // Per-product sequence, 1-based. Paired with the product key this
        // gives PARSER-12: short, ordered, and quotable, which a UUID is not.
        // Uniqueness is enforced by the (productId, number) index below, not
        // by the allocator, so two concurrent files cannot both take 12.
        number: t.int().notNull(),
        summary: t.text().notNull(),
        // WHAT this row is, kept strictly apart from how bad it is.
        //
        // Bugzilla answers both with one field: a feature request is
        // `severity: enhancement`. That makes "a critical feature request"
        // unsayable and pollutes every severity distribution, and Bugzilla's
        // own flagship deployment abandoned it -- bugzilla.mozilla.org dropped
        // the `enhancement` severity for a Type field of defect/enhancement/
        // task (bugzilla.mozilla.org bug 1522340), after which Mozilla's
        // severity guide reads "the severity of most bugs of type task and
        // enhancement will be N/A".
        //
        // So `enhancement` is gone from severity (see BUG_SEVERITIES) and
        // lives here instead. `defect` is the default because an unclassified
        // report is more often something broken than something wished for.
        kind: t.enum("issue_kind").notNull().default("defect"),
        status: t.enum("issue_status").notNull().default("UNCONFIRMED"),
        resolution: t.enum("issue_resolution"),
        severity: t.enum("issue_severity").notNull().default("normal"),
        priority: t.enum("issue_priority").notNull().default("P3"),
        reporterId: t.text().notNull().references("users", "id"),
        assigneeId: t.text().references("users", "id"),
        qaContactId: t.text().references("users", "id"),
        // Was a bare t.text() with no FK and no index, which made
        // `dupes.list` an unindexed scan against an unconstrained column.
        duplicateOfId: t.text().references("issues", "id"),
        whiteboard: t.text(),
        opSys: t.text().notNull().default("Unspecified"),
        platform: t.text().notNull().default("Unspecified"),
        url: t.text(),
        isConfirmed: t.boolean().notNull().default(false),
        // Counters are integers. They were doubles.
        voteCount: t.int().notNull().default(0),
        commentCount: t.int().notNull().default(0),
        // A date to finish BY -- not elapsed effort. Bugzilla's time
        // tracking (estimated / actual / remaining) is deliberately absent:
        // see the note on the comments table.
        deadline: t.timestamp(),
        // Without this, reports.timeToResolve and reports.trend can only be
        // answered by mining `activities` for fieldName = 'status' -- a full
        // scan with string matching, and env.db has no raw SQL to rescue it.
        resolvedAt: t.timestamp(),
      },
      indexes: [
        { name: "issues_product_idx", on: ["productId"] },
        // The allocator reads max(number) for a product and writes max+1. This
        // is what makes that safe: two transactions that both read 11 cannot
        // both commit 12, the loser is rejected and retries.
        { name: "issues_product_number_uniq", on: ["productId", "number"], unique: true },
        { name: "issues_component_idx", on: ["componentId"] },
        { name: "issues_status_idx", on: ["status"] },
        // Every "show me the feature requests" view is this index.
        { name: "issues_kind_idx", on: ["kind"] },
        { name: "issues_assignee_idx", on: ["assigneeId"] },
        { name: "issues_reporter_idx", on: ["reporterId"] },
        { name: "issues_duplicate_of_idx", on: ["duplicateOfId"] },
        { name: "issues_resolved_at_idx", on: ["resolvedAt"] },
        { name: "issues_alias_uniq", on: ["alias"], unique: true },
      ],
    });

    // Bugzilla's bug_group_map: the feature Bugzilla is most famous for, a
    // confidential security bug inside an otherwise public product. `groups`
    // without this buys almost nothing -- product-level visibility alone
    // cannot express "this one bug is restricted".
    table("issueGroups").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        groupId: t.text().notNull().references("groups", "id"),
      },
      indexes: [
        { name: "issue_groups_pair_uniq", on: ["issueId", "groupId"], unique: true },
        { name: "issue_groups_group_idx", on: ["groupId"] },
      ],
    });

    // commentNumber 0 is the original description, exactly as in Bugzilla.
    //
    // No workTimeMinutes, and no estimated/remaining on bugs. Bugzilla logs
    // effort per comment and reads it per bug, and only the whole set means
    // anything: minutes logged against one comment answers a question nobody
    // asks. We shipped the logging without the totals, so the composer spent
    // a full row collecting a number that was never added up. Removed rather
    // than completed -- time tracking is a niche most trackers have dropped,
    // and a dead control teaches the wrong thing in an example app.
    table("comments").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        authorId: t.text().notNull().references("users", "id"),
        body: t.text().notNull(),
        commentNumber: t.int().notNull().default(0),
        isPrivate: t.boolean().notNull().default(false),
      },
      indexes: [
        // Also the ordering index for comments.list, and the backstop for the
        // max+1 race in the comment-number assignment.
        { name: "comments_issue_number_uniq", on: ["issueId", "commentNumber"], unique: true },
      ],
    });

    // `storageKey` is the env.storage object key; the bytes never live in the
    // database. `sizeBytes` and `contentType` are recorded at upload so the
    // list view never has to touch object storage.
    table("attachments").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        // The comment a file arrived with, when it arrived with one.
        // Files are attached BY commenting, the way anyone who has used a
        // tracker this decade expects, so an attachment usually has a
        // sentence next to it explaining why it is there. Nullable because
        // the bug can still hold files with no comment of their own, and
        // because bugId stays the thing visibility is decided by -- a file
        // is reachable exactly when its bug is.
        commentId: t.text().references("comments", "id"),
        uploaderId: t.text().notNull().references("users", "id"),
        filename: t.text().notNull(),
        contentType: t.text().notNull().default("application/octet-stream"),
        // A file size is not a float.
        sizeBytes: t.bigInt().notNull().default(0),
        storageKey: t.text().notNull(),
        description: t.text(),
        isPatch: t.boolean().notNull().default(false),
        isObsolete: t.boolean().notNull().default(false),
      },
      indexes: [
        { name: "attachments_issue_idx", on: ["issueId"] },
        { name: "attachments_comment_idx", on: ["commentId"] },
      ],
    });

    table("keywords").create({
      columns: {
        name: t.text().notNull().unique(),
        description: t.text(),
      },
    });

    table("issueKeywords").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        keywordId: t.text().notNull().references("keywords", "id"),
      },
      indexes: [
        { name: "issue_keywords_pair_uniq", on: ["issueId", "keywordId"], unique: true },
        // Advanced search filters by keyword, which reads this direction.
        { name: "issue_keywords_keyword_idx", on: ["keywordId"] },
      ],
    });

    // A row means `bugId` is blocked by `dependsOnId`. The inverse direction
    // ("blocks") is the same row read the other way; storing both would let
    // the two halves disagree. Acyclicity is not expressible here and is
    // enforced in `deps.add`.
    table("issueDependencies").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        dependsOnId: t.text().notNull().references("issues", "id"),
      },
      indexes: [
        { name: "issue_deps_pair_uniq", on: ["issueId", "dependsOnId"], unique: true },
        { name: "issue_deps_depends_idx", on: ["dependsOnId"] },
      ],
    });

    table("issueCc").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        userId: t.text().notNull().references("users", "id"),
      },
      indexes: [
        { name: "issue_cc_pair_uniq", on: ["issueId", "userId"], unique: true },
        // "CC'd to me" on the dashboard reads this direction.
        { name: "issue_cc_user_idx", on: ["userId"] },
      ],
    });

    table("issueSeeAlso").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        url: t.text().notNull(),
      },
      indexes: [
        { name: "issue_see_also_pair_uniq", on: ["issueId", "url"], unique: true },
      ],
    });

    table("votes").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        userId: t.text().notNull().references("users", "id"),
        // A vote quantity, so bugs.voteCount is SUM(count), not COUNT(*).
        count: t.int().notNull().default(1),
      },
      indexes: [
        { name: "votes_pair_uniq", on: ["issueId", "userId"], unique: true },
        { name: "votes_user_idx", on: ["userId"] },
      ],
    });

    // -----------------------------------------------------------------------
    // Flags
    // -----------------------------------------------------------------------
    table("flagTypes").create({
      columns: {
        name: t.text().notNull(),
        description: t.text(),
        targetType: t.enum("flag_target_type").notNull().default("issue"),
        isRequestable: t.boolean().notNull().default(true),
        isMultiplicable: t.boolean().notNull().default(false),
        productId: t.text().references("products", "id"),
      },
      indexes: [
        { name: "flag_types_name_target_uniq", on: ["name", "targetType"], unique: true },
      ],
    });

    // Exactly one of bugId / attachmentId is set, matched to the flag type's
    // targetType. That is a cross-column implication over two nullable
    // columns; it is enforced in `flags.set` and asserted in the test suite.
    table("flags").create({
      columns: {
        flagTypeId: t.text().notNull().references("flagTypes", "id"),
        issueId: t.text().references("issues", "id"),
        attachmentId: t.text().references("attachments", "id"),
        setterId: t.text().notNull().references("users", "id"),
        requesteeId: t.text().references("users", "id"),
        status: t.enum("flag_status").notNull().default("?"),
      },
      indexes: [
        { name: "flags_issue_idx", on: ["issueId"] },
        { name: "flags_attachment_idx", on: ["attachmentId"] },
        // flags.listRequests answers both "requests OF me" and "requests BY
        // me"; only the first was indexed.
        { name: "flags_requestee_idx", on: ["requesteeId"] },
        { name: "flags_setter_idx", on: ["setterId"] },
      ],
    });

    // -----------------------------------------------------------------------
    // History, saved searches, watching
    // -----------------------------------------------------------------------
    //
    // Bugzilla's bugs_activity: one row per changed field per edit. Every
    // mutating RPC writes here in the same transaction as the mutation, which
    // is what makes the detail page's History tab reconstructible without a
    // separate event log.
    table("activities").create({
      columns: {
        issueId: t.text().notNull().references("issues", "id"),
        actorId: t.text().notNull().references("users", "id"),
        // Bugzilla's bugs_activity carries attach_id. Without it, "attachment
        // 12 marked obsolete" renders in the history as a bare field name.
        attachmentId: t.text().references("attachments", "id"),
        fieldName: t.text().notNull(),
        oldValue: t.text(),
        newValue: t.text(),
      },
      indexes: [
        { name: "activities_issue_idx", on: ["issueId"] },
        // reports.trend mines history by field; bug-only indexing made that a
        // full scan.
        { name: "activities_issue_field_idx", on: ["issueId", "fieldName"] },
      ],
    });

    table("savedSearches").create({
      columns: {
        ownerId: t.text().notNull().references("users", "id"),
        name: t.text().notNull(),
        // Executed on behalf of others when shared, so it is validated against
        // a closed field/operator whitelist before execution.
        queryJson: t.json().notNull(),
        isShared: t.boolean().notNull().default(false),
      },
      indexes: [
        { name: "saved_searches_owner_name_uniq", on: ["ownerId", "name"], unique: true },
      ],
    });

    table("watchers").create({
      columns: {
        watcherId: t.text().notNull().references("users", "id"),
        watchedId: t.text().notNull().references("users", "id"),
      },
      indexes: [
        { name: "watchers_pair_uniq", on: ["watcherId", "watchedId"], unique: true },
        // Notification fanout asks "who watches user X", which is this
        // direction.
        { name: "watchers_watched_idx", on: ["watchedId"] },
      ],
    });

    table("notifications").create({
      columns: {
        userId: t.text().notNull().references("users", "id"),
        issueId: t.text().references("issues", "id"),
        kind: t.enum("notification_kind").notNull().default("issue_changed"),
        title: t.text().notNull(),
        body: t.text(),
        isRead: t.boolean().notNull().default(false),
      },
      indexes: [{ name: "notifications_user_idx", on: ["userId"] }],
    });
  },
};
