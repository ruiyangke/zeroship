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

- `products` — name (unique), description, isActive, defaultMilestone,
  allowsUnconfirmed, classification
- `components` — productId -> products, name, description, defaultAssignee,
  defaultQaContact, isActive
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
  whiteboard, opSys, platform, url, isConfirmed, votes, deadline
- `comments` — bugId, authorId, body, isPrivate, workTimeMinutes,
  commentNumber (0 = the original description)
- `attachments` — bugId, uploaderId, filename, contentType, sizeBytes,
  storageKey (the `env.storage` object key), description, isPatch, isObsolete
- `bugKeywords` — bugId, keywordId (join)
- `bugDependencies` — bugId, dependsOnId (a bug blocked by another bug)
- `bugCc` — bugId, userId (join)
- `bugSeeAlso` — bugId, url
- `flags` — bugId (nullable), attachmentId (nullable), flagTypeId, setterId,
  requesteeId, status (+/-/?)
- `activities` — bugId, actorId, fieldName, oldValue, newValue, changedAt.
  Every mutating RPC writes here; this is Bugzilla's bug history table.
- `votes` — bugId, userId, count

### People and preferences

- `users` — email (unique), name, handle (unique), isAdmin, isDisabled,
  realName, timezone
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
`cc.add` `cc.remove` `cc.list`

### Products and admin
`products.list` `products.get` `products.create` `products.update`
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
2. **Bug detail** — the Bugzilla show_bug page: fields panel, comment stream,
   attachments, dependencies, flags, activity history
3. **New bug** — guided product -> component -> details
4. **Advanced search** — boolean field builder + QuickSearch box
5. **My dashboard** — assigned to me, reported by me, my flag requests, CC'd
6. **Products admin** — products, components, versions, milestones
7. **Reports** — the summary charts

## Non-goals for this example

Charts beyond the summary set, custom fields, whining (scheduled query email),
XML-RPC/BzAPI compatibility, and the classic Bugzilla templating skin. Custom
fields in particular are deliberately out: they need runtime DDL, which is the
migration engine's job and not an app-level concern.
