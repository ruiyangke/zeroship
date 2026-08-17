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

## Vocabulary (Bugzilla's, with one noun changed)

Bugzilla's nouns are load-bearing for anyone who has used it, so they are kept
verbatim rather than modernised -- with one exception, reversed deliberately and
recorded here because this section previously said the opposite.

An **issue** belongs to exactly one **component**, which belongs to exactly one
**product**. An issue carries a **kind**, a **status** and, once closed, a
**resolution**. **Severity** describes impact; **priority** describes
scheduling; **kind** describes what the record is. All three are separate
fields on purpose.

This file used to read "a **bug** (not "issue")", on the argument that
Bugzilla's vocabulary is what makes the example legible to anyone who has used
Bugzilla. That argument is still sound and is why every other noun here is
untouched -- product, component, milestone, QA contact, whiteboard, flag,
keyword, see-also, resolution and the UNCONFIRMED/VERIFIED lifecycle all keep
Bugzilla's spelling.

What changed is that the tracker has to hold FEATURE REQUESTS, and "bug" is a
claim that something is broken. Bugzilla's own answer is `severity:
enhancement`, which puts "what kind of thing is this" on the axis that means
"how bad is it" -- so a critical feature request is unsayable and every severity
distribution is polluted by records that have no severity at all. Bugzilla's
flagship deployment reached the same conclusion: bugzilla.mozilla.org dropped
the `enhancement` severity in favour of a Type field of
defect/enhancement/task, after which Mozilla's own severity guide reads "the
severity of most bugs of type task and enhancement will be N/A".

So the fix is the **kind** field, and the rename follows it rather than leading
it. The two are separable -- GitHub calls the record an issue and still offers
"bug" as a type -- and the field is the half that makes feature requests
representable. The noun changed as well because the product already called
itself an Issue Tracker in its title, its directory, its nav and this file's
own heading, while the model underneath said `bug`: a page listing feature
requests under a heading that said "Bugs", filed with a button that said "New
bug".

## Models

Every table gets the seven injected platform system columns (`id`,
`created_at`, `updated_at`, `created_by`, `updated_by`, `version`,
`deleted_at`) from the confined charter — they are never declared here.

### Products and structure

- `products` — name (unique), key (unique, the PARSER in PARSER-12),
  description, isActive, defaultMilestone,
  allowsUnconfirmed, classification, votesPerUser, maxVotesPerIssue,
  votesToConfirm (all three default 0, which means voting is off)
- `components` — productId -> products, name, description, defaultAssigneeId
  (NOT NULL: a component always has an initial owner), defaultQaContactId,
  isActive
- `versions` — productId -> products, name, sortKey, isActive
- `milestones` — productId -> products, name, sortKey, isActive
- `keywords` — name (unique), description
- `flagTypes` — name, description, targetType (issue/attachment), isRequestable,
  isMultiplicable, productId (nullable = global)

### Issues

- `issues` — productId, componentId, versionId (nullable: a feature request
  is not found in a version), milestoneId, summary,
  kind (defect/enhancement/task; what the record IS, kept apart from how bad
  it is),
  status (UNCONFIRMED/CONFIRMED/IN_PROGRESS/RESOLVED/VERIFIED/CLOSED),
  resolution (nullable; FIXED/INVALID/WONTFIX/DUPLICATE/WORKSFORME/INCOMPLETE),
  severity (blocker/critical/major/normal/minor/trivial; `enhancement` is a
  kind, not a severity),
  priority (P1..P5), assigneeId, reporterId, qaContactId, duplicateOfId,
  number (per-product sequence, 1-based; with the product key this is the
  PARSER-12 a person reads and types),
  alias (unique when set), whiteboard, opSys, platform, url, isConfirmed,
  voteCount, commentCount,
  deadline, resolvedAt (stamped on resolve so reports need not mine history)
- `comments` — issueId, authorId, body, isPrivate,
  commentNumber (0 = the original description)
- `attachments` — issueId, uploaderId, filename, contentType, sizeBytes,
  storageKey (the `env.storage` object key), description, isPatch, isObsolete
- `issueKeywords` — issueId, keywordId (join)
- `issueDependencies` — issueId, dependsOnId (an issue blocked by another)
- `issueCc` — issueId, userId (join)
- `issueGroups` — issueId, groupId (join). Bugzilla's bug_group_map: the table
  behind a confidential issue inside an otherwise readable product.
- `issueSeeAlso` — issueId, url
- `flags` — issueId (nullable), attachmentId (nullable), flagTypeId, setterId,
  requesteeId, status (+/-/?)
- `activities` — issueId, actorId, fieldName, oldValue, newValue, changedAt.
  Every mutating RPC writes here; this is Bugzilla's bug history table.
- `votes` — issueId, userId, count

### People and preferences

- `users` — email (unique), handle (unique), name, isAdmin, isDisabled,
  timezone, prefs (json). Bugzilla has login + realname and no third name
  field, so there is no separate `realName`
- `groups` — name (unique), description
- `groupMembers` — groupId, userId
- `productGroups` — productId, groupId (per-product visibility)
- `savedSearches` — ownerId, name, queryJson, isShared
- `watchers` — watcherId, watchedId (Bugzilla's user watching)

## RPC surface

Wire ids are dotted and explicit. Auth policy lives in `src/server/config.ts`;
the default is `auth: "user"` and anything anonymous is opted in there
explicitly with `publiclyAccessible: true`.

### Issues (core)
`issues.create` `issues.get` `issues.update` `issues.search`
`issues.changeStatus` `issues.resolve` `issues.reopen`
`issues.markDuplicate` `issues.reassign` `issues.setKind` `issues.setSeverity`
`issues.setPriority` `issues.move` (product/component)

### Comments
`comments.add` `comments.list` `comments.edit` `comments.setPrivate`

### Attachments
`attachments.upload` `attachments.list` `attachments.get`
`attachments.setObsolete` `attachments.delete`

### Dependencies and duplicates
`deps.add` `deps.remove` `deps.tree` `deps.graph`
`dupes.list` (the duplicate cluster for an issue)

### Keywords, flags, CC
`keywords.list` `keywords.create` `keywords.attach` `keywords.detach`
`flags.set` `flags.clear` `flags.listRequests` (my requests / requests of me)
`flags.list` (the live flags on an issue and its attachments)
`flagTypes.create` `flagTypes.list` (admin-defined; without a type no flag can
be set, which is what made the four `flags.*` procedures unreachable)
`cc.add` `cc.remove` `cc.list` `cc.listMine` (the issues I am CC'd on)

### Access control
`groups.create` `groups.delete` `groups.list` `groups.members`
`groups.addMember` `groups.removeMember`
`products.restrict` `products.unrestrict` (product-level visibility)
`issues.restrict` `issues.unrestrict` (Bugzilla's bug_group_map: a confidential
issue inside an otherwise readable product)

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
`products.delete` (admin only, and it refuses until you confirm the issue count
it would take with it)
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

## Procedures with no UI, on purpose

`users.get` and `users.updatePrefs` have no screen. This example has no
profile page: identity comes from the platform, and the app stores a name,
timezone and a prefs blob it never asks anyone to edit. They stay in the
surface because the data model has the columns and an example that models a
field it cannot write is worse than one that documents the gap.

Everything else IS reachable. That is checked, not asserted: a sweep over the
source fails when a component is defined and rendered nowhere, which is how
`QuickSearchBox` and the old `Nav` were found after the pages that
hosted them were deleted. The same shape one layer down -- a procedure no
client code calls -- is how the flag types, group membership, user watching
and vote list were found; all four now have UI.

## Known limits

**Reference-data lists are not paginated, and adding a limit alone would break
name resolution.** Bug listing is capped -- `issues.search` returns 100 rows by
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

- **The record is an `issue` with a `kind`, not a `bug` with `severity:
  enhancement`.** The largest divergence in this file, argued in full under
  Vocabulary above. `kind` is `defect | enhancement | task`, `severity` means
  only impact, and `enhancement` is gone from the severity vocabulary. This
  follows bugzilla.mozilla.org, which made the same split; upstream Bugzilla
  still ships the overloaded severity.
- **`versionId` is nullable.** It was NOT NULL, matching Bugzilla, where every
  bug is found in some version. A feature request is not found in a version at
  all, so requiring one forced a lie. The column stays for defects and is
  simply absent on the kinds that have no answer.
- **A group-restricted issue answers 403, not 404, and so its existence leaks.**
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
