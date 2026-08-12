# issue-tracker — a Bugzilla-faithful issue tracker on zeroship

A full issue tracker in the shape of Bugzilla: products and components, bugs
with a status/resolution workflow, dependencies and duplicates, attachments,
flags, keywords, saved searches, CC lists, and a complete activity history.

This example exists to exercise the platform end to end on a workload with real
shape: `env.db` (relations, indexes, transactions), `env.auth` (per-request
identity and per-product permissions), `env.storage` (attachments), and
`env.kv` (saved-search caches, unread counters). It is the heaviest DB example
in the corpus after `hr-system`, and unlike `hr-system` it is authored
migration-first with no legacy inline-schema export.

## Vocabulary (Bugzilla's, kept deliberately)

Bugzilla's nouns are load-bearing for anyone who has used it, so they are kept
verbatim rather than modernised: a **bug** (not "issue") belongs to exactly one
**component**, which belongs to exactly one **product**. A bug carries a
**status** and, once closed, a **resolution**. **Severity** describes impact;
**priority** describes scheduling. They are separate fields on purpose.

## Models

Every table gets the seven injected platform system columns (`id`,
`created_at`, `updated_at`, `created_by`, `updated_by`, `version`,
`deleted_at`) from the confined charter — they are never declared here.

### Products and structure

- `products` — name (unique), key (unique, the PARSER in PARSER-12),
  description, isActive, defaultMilestone,
  allowsUnconfirmed, classification, votesPerUser, maxVotesPerBug,
  votesToConfirm (all three default 0, which means voting is off)
- `components` — productId -> products, name, description, defaultAssigneeId
  (NOT NULL: a component always has an initial owner), defaultQaContactId,
  initialCc, isActive
- `versions` — productId -> products, name, sortKey, isActive
- `milestones` — productId -> products, name, sortKey, isActive
- `keywords` — name (unique), description
- `flagTypes` — name, description, targetType (bug/attachment), isRequestable,
  isMultiplicable, productId (nullable = global)

### Bugs

- `bugs` — productId, componentId, versionId, milestoneId, summary,
  status (UNCONFIRMED/CONFIRMED/IN_PROGRESS/RESOLVED/VERIFIED/CLOSED),
  resolution (nullable; FIXED/INVALID/WONTFIX/DUPLICATE/WORKSFORME/INCOMPLETE),
  severity (blocker/critical/major/normal/minor/trivial/enhancement),
  priority (P1..P5), assigneeId, reporterId, qaContactId, duplicateOfId,
  number (per-product sequence, 1-based; with the product key this is the
  PARSER-12 a person reads and types),
  alias (unique when set), whiteboard, opSys, platform, url, isConfirmed,
  voteCount, commentCount, estimatedTimeMinutes, remainingTimeMinutes,
  deadline, resolvedAt (stamped on resolve so reports need not mine history)
- `comments` — bugId, authorId, body, isPrivate, workTimeMinutes,
  commentNumber (0 = the original description)
- `attachments` — bugId, uploaderId, filename, contentType, sizeBytes,
  storageKey (the `env.storage` object key), description, isPatch, isObsolete
- `bugKeywords` — bugId, keywordId (join)
- `bugDependencies` — bugId, dependsOnId (a bug blocked by another bug)
- `bugCc` — bugId, userId (join)
- `bugGroups` — bugId, groupId (join). Bugzilla's bug_group_map: the table
  behind a confidential bug inside an otherwise readable product.
- `bugSeeAlso` — bugId, url
- `flags` — bugId (nullable), attachmentId (nullable), flagTypeId, setterId,
  requesteeId, status (+/-/?)
- `activities` — bugId, actorId, fieldName, oldValue, newValue, changedAt.
  Every mutating RPC writes here; this is Bugzilla's bug history table.
- `votes` — bugId, userId, count

### People and preferences

- `users` — email (unique), handle (unique), name, isAdmin, isDisabled,
  timezone, prefs (json). Bugzilla has login + realname and no third name
  field, so there is no separate `realName`
- `groups` — name (unique), description, isBugGroup
- `groupMembers` — groupId, userId
- `productGroups` — productId, groupId (per-product visibility)
- `savedSearches` — ownerId, name, queryJson, isShared
- `watchers` — watcherId, watchedId (Bugzilla's user watching)

## RPC surface

Wire ids are dotted and explicit. Auth policy lives in `src/server/config.ts`;
the default is `auth: "user"` and anything anonymous is opted in there
explicitly with `publiclyAccessible: true`.

### Bugs (core)
`bugs.create` `bugs.get` `bugs.update` `bugs.search` `bugs.changeStatus`
`bugs.resolve` `bugs.reopen` `bugs.markDuplicate` `bugs.reassign`
`bugs.setSeverity` `bugs.setPriority` `bugs.move` (product/component)

### Comments
`comments.add` `comments.list` `comments.edit` `comments.setPrivate`

### Attachments
`attachments.upload` `attachments.list` `attachments.get`
`attachments.setObsolete` `attachments.delete`

### Dependencies and duplicates
`deps.add` `deps.remove` `deps.tree` `deps.graph`
`dupes.list` (the duplicate cluster for a bug)

### Keywords, flags, CC
`keywords.list` `keywords.create` `keywords.attach` `keywords.detach`
`flags.set` `flags.clear` `flags.listRequests` (my requests / requests of me)
`flags.list` (the live flags on a bug and its attachments)
`flagTypes.create` `flagTypes.list` (admin-defined; without a type no flag can
be set, which is what made the four `flags.*` procedures unreachable)
`cc.add` `cc.remove` `cc.list` `cc.listMine` (the bugs I am CC'd on)

### Access control
`groups.create` `groups.list` `groups.addMember` `groups.removeMember`
`products.restrict` `products.unrestrict` (product-level visibility)
`bugs.restrict` `bugs.unrestrict` (Bugzilla's bug_group_map: a confidential
bug inside an otherwise readable product)

The first account to exist becomes an admin, the way Bugzilla's installer
creates one. Without a bootstrap nothing could ever set `isAdmin`, and the
whole admin surface would be unreachable.

### Voting
`votes.cast` `votes.listMine`

A vote carries a QUANTITY, so a bug's `voteCount` is the sum of vote rows rather
than a count of them. Voting is disabled until a product sets `votesPerUser`;
reaching `votesToConfirm` confirms an UNCONFIRMED bug.

### Watching and see-also
`watchers.add` `watchers.remove` `watchers.list` (Bugzilla's user watching:
you also hear about bugs the watched user is involved in)
`seeAlso.add` `seeAlso.remove` `seeAlso.list` (cross-tracker links)

### Products and admin
`products.list` `products.get` `products.create` `products.update`
`products.resolve` `users.resolve` (names for the ids a page is showing, so a
bug table need not fetch every product and user to label its rows)
`components.list` `components.create` `components.update`
`versions.list` `versions.create` `milestones.list` `milestones.create`

### Search and saved searches
`search.query` (the structured boolean query Bugzilla calls advanced search)
`search.quick` (Bugzilla's QuickSearch shorthand, e.g. `P1 @alice comp:parser`)
`savedSearches.list` `savedSearches.save` `savedSearches.delete`

### Users and notifications
`users.me` `users.list` `users.get` `users.updatePrefs`
`notifications.list` `notifications.markRead` `notifications.unreadCount`

### Reports
`reports.summary` (open by status/severity/priority)
`reports.byComponent` `reports.byAssignee` `reports.trend`
`reports.timeToResolve`

## Frontend pages

1. **Bug list** — the query result table, column picker, inline status edit
2. **Bug detail** -- the Bugzilla show_bug page: fields panel, comment stream,
   attachments, dependencies, duplicates, flags, votes, CC, security groups,
   see-also links, and the activity history tab
3. **New bug** — guided product -> component -> details
4. **Advanced search** — boolean field builder + QuickSearch box
5. **My dashboard** -- the notification inbox, assigned to me, reported by me,
   my flag requests, and bugs I am CC-d on
6. **Products admin** -- products, components, versions, target releases, plus
   group administration and product-level visibility
7. **Reports** — the summary charts

## Non-goals for this example

Charts beyond the summary set, custom fields, whining (scheduled query email),
XML-RPC/BzAPI compatibility, and the classic Bugzilla templating skin. Custom
fields in particular are deliberately out: they need runtime DDL, which is the
migration engine's job and not an app-level concern.

Email delivery is also out: notifications are in-app rows, and nothing sends
mail. Full-text search is out -- `search.quick` matches structured tokens and
an indexed prefix of the summary, not comment bodies.

## Known limits

**Reference-data lists are not paginated, and adding a limit alone would break
name resolution.** Bug listing is capped -- `bugs.search` returns 100 rows by
default and takes `limit`/`offset`. The reference lists do not: measured
against the dev database, `products.list` returned 131 rows and
`reports.byComponent` 103, both reachable anonymously and both growing with the
data.

The obvious repair is wrong. Every bug table resolves its product and assignee
columns through `useBugLookups()`, which builds id-to-name maps out of the
WHOLE of `products.list`; the table falls back to printing the raw id for
anything the map misses. Capping the list at N would therefore make every
product past the cap render as `prod_034607nk...` again -- reintroducing the
defect those maps exist to prevent, silently, and only for installations large
enough to notice.

Doing this properly means paginating the list AND giving name resolution its
own path (resolve per page, or a lookup endpoint keyed by the ids actually on
screen). That is a design change, not a parameter, so it is recorded here
rather than half-applied.

## Divergences from Bugzilla, taken deliberately

- **A group-restricted bug answers 403, not 404, and so its existence leaks.**
  Bugzilla behaves the same way ("You are not authorized to access bug #N"), and
  a tracker that pretends a restricted bug was never filed still cannot explain
  the gap its id leaves in every list. Hiding existence is the stronger property
  and this app does NOT have it: a non-member learns that the id is real and
  that it is held in some group, just not what it says. The refusal names the
  group restriction rather than the product, because the product stays fully
  accessible -- the same user can still list and file its other bugs.
- **Comments are editable.** Bugzilla comments are immutable by design; here
  `comments.edit` exists and writes an activity row. Kept because an example
  that cannot correct a typo teaches the wrong lesson about the data model.
- **Attachments can be deleted, not only obsoleted.** Bugzilla obsoletes them.
  `attachments.delete` checks bug access but NOT uploader identity, so any user
  who can edit the bug can delete another user's attachment.
- **`versions.sortKey` is a float**, where Bugzilla uses an int, so a version
  can be inserted between two others without renumbering.
- **Product structure is not admin-gated.** Bugzilla requires
  `editcomponents` to create or rename a product, component or version. Here
  any authenticated user can. The first account bootstraps as the only admin,
  so gating these would leave a second user unable to set up anything to file
  bugs against. `assertCanViewProduct` does not fill the gap: it returns true
  for any product carrying no group restriction. Gate them with
  `requireAdmin` if you copy this app for real use.
