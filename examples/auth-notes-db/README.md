# auth-notes-db

Per-user notes. The example that exercises **`env.auth` + `env.db` + RPC
together**, because that combination is where the interesting bug lives and
every other example in `examples/` exercises exactly one primitive.

The property it demonstrates is **per-user row scoping**: a signed-in user
writes rows owned by them and cannot read anyone else's - *including* by
passing someone else's note id straight to the read-by-id procedure.

```
migrations/          op.* migration: notes(owner_id, title, body) + owner index
generated/zeroship/  gen-types output (committed): env.db.ts + schema.runtime.json
src/index.ts         the three RPC procedures
scripts/smoke.sh     two-user curl harness - signs in as Alice AND Bob
```

## Procedures

| id             | kind     | behaviour |
| -------------- | -------- | --------- |
| `notes.create` | mutation | Inserts a note with `owner_id` = the authenticated caller. The client never sends a user id. |
| `notes.list`   | query    | `find({ owner_id: me })` - the caller's own notes, newest first. |
| `notes.get`    | query    | `get({ id, owner_id: me })` - **both** predicates in one filter. |

`notes.get` is the one that matters. Writing it as `db.notes.get(id)` and
leaving the scoping to the list query reads fine, passes a single-user test,
and hands every row in the table to anyone who can guess an id. The owner
predicate belongs in the query.

Anonymous callers get a **401** (`UNAUTHENTICATED`); a caller asking for
somebody else's note gets a **404**, not a 403 - "that note exists but is not
yours" leaks the existence of other people's rows, and the filter cannot
distinguish the two cases anyway.

There is no `src/server/config.ts`. RPC auth is fail-closed by default
(a procedure with no policy resolves to `auth: "user"`), which is exactly what
this app wants - the starter only needs that file because it opts *out*.

## Run it

```bash
pnpm migrate             # apply migrations to the dev SQLite database - DO THIS FIRST
pnpm dev                 # vite + the dev runtime (SQLite + the dev-auth tier)
pnpm smoke               # the two-user ownership harness (ZEROSHIP_URL to retarget)
pnpm build               # -> dist/app.zship
pnpm gen-types           # regenerate generated/zeroship/ after a migration change
pnpm typecheck
```

`pnpm migrate` is not optional and is not folded into `pnpm dev`: on the dev
tier the database is migrated by a separate, explicit step, exactly as a deploy
migrates Postgres before the worker serves. Skip it and every `env.db` call
returns HTTP 500 with a bare `"internal error"` on the wire; the reason is only
visible in the dev server's own boot output (`dev schema NOT applied - env.db
will fail for: notes`) and its request log (`db: no such table: default.notes`).

`vite.config.ts` configures **two** dev users (`alice@localhost` / `alice` and
`bob@localhost` / `bob`). Two users is the entire point: a single-user dev tier
cannot express "and now somebody else asks for that row".

## Status: the ownership negative IS verified on the dev tier (2026-08-12)

`pnpm migrate && pnpm dev && pnpm smoke` reports **all checks passed**, including
the by-id negative that is the whole reason this example exists:

```
notes.create (alice)      -> 200  note_0345gdZEzQgim9IzfqMP4P
notes.list   (alice)      -> 200  [that note]
notes.get    (alice, own) -> 200
notes.list   (bob)        -> 200  []
notes.get    (bob, ALICE'S id) -> 404 {"message":"Note not found","code":"NOT_FOUND"}
```

### What this section used to say, and why it was wrong

It reported HTTP 500 on every request and attributed it to a descriptor
collision:

```
sqlite engine: desired_snapshot failed: invalid descriptor:
collection 'notes' declares field 'created_at', which collides with
an injected policy column
```

That cause is stale twice over. `migrations/20260808000000_create_notes.ts` does
not declare `created_at` at all any more (it defers to the seven injected system
columns, and says so), and the failure measured on 2026-08-12 was a different
one: `db: no such table: default.notes`. The table did not exist because
`pnpm migrate` had never run - and it had never run because **this package.json
did not define a `migrate` script**, while the five other `env.db` examples
(`db-chat`, `db-e2e`, `db-todos`, `hr-system`, `scaffold-app`) all do. The dev
server was printing `pnpm migrate` as the instruction, and running it gave
`Command "migrate" not found`.

Adding that one line is the entire fix. Nothing in the app, the runtime, the
descriptor or the engine changed. The lesson worth keeping is that a stale
diagnosis is more expensive than no diagnosis: the collision text above is
specific and quotable, so it was believed and re-cited for days after it stopped
being true.
