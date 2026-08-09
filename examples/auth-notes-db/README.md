# auth-notes-db

Per-user notes. The example that exercises **`env.auth` + `env.db` + RPC
together**, because that combination is where the interesting bug lives and
every other example in `examples/` exercises exactly one primitive.

The property it demonstrates is **per-user row scoping**: a signed-in user
writes rows owned by them and cannot read anyone else's — *including* by
passing someone else's note id straight to the read-by-id procedure.

```
migrations/          op.* migration: notes(owner_id, title, body) + owner index
generated/zeroship/  gen-types output (committed): env.db.ts + schema.runtime.json
src/index.ts         the three RPC procedures
scripts/smoke.sh     two-user curl harness — signs in as Alice AND Bob
```

## Procedures

| id             | kind     | behaviour |
| -------------- | -------- | --------- |
| `notes.create` | mutation | Inserts a note with `owner_id` = the authenticated caller. The client never sends a user id. |
| `notes.list`   | query    | `find({ owner_id: me })` — the caller's own notes, newest first. |
| `notes.get`    | query    | `get({ id, owner_id: me })` — **both** predicates in one filter. |

`notes.get` is the one that matters. Writing it as `db.notes.get(id)` and
leaving the scoping to the list query reads fine, passes a single-user test,
and hands every row in the table to anyone who can guess an id. The owner
predicate belongs in the query.

Anonymous callers get a **401** (`UNAUTHENTICATED`); a caller asking for
somebody else's note gets a **404**, not a 403 — "that note exists but is not
yours" leaks the existence of other people's rows, and the filter cannot
distinguish the two cases anyway.

There is no `src/server/config.ts`. RPC auth is fail-closed by default
(a procedure with no policy resolves to `auth: "user"`), which is exactly what
this app wants — the starter only needs that file because it opts *out*.

## Run it

```bash
pnpm dev                 # vite + the dev runtime (SQLite + the dev-auth tier)
pnpm smoke               # the two-user ownership harness (ZEROSHIP_URL to retarget)
pnpm build               # -> dist/app.zship
pnpm gen-types           # regenerate generated/zeroship/ after a migration change
pnpm typecheck
```

`vite.config.ts` configures **two** dev users (`alice@localhost` / `alice` and
`bob@localhost` / `bob`). Two users is the entire point: a single-user dev tier
cannot express "and now somebody else asks for that row".

## Status: the ownership negative is NOT verified — the dev tier cannot run this app

`pnpm dev` currently returns **HTTP 500 for every request** in this app, from
the very first one:

```
sqlite engine: desired_snapshot failed: invalid descriptor:
collection 'notes' declares field 'created_at', which collides with
an injected policy column
```

This is not specific to this app. `examples/db-hitcounter` fails identically
(`collection 'hits' declares field 'created_at'`), which is presumably why it
ships without a `dev` script. See the report accompanying this example for the
root cause and the two candidate fix sites.

`scripts/smoke.sh` is written against the intended behaviour and is the check to
run once the dev tier can install a migration-first schema. Today it fails at
step 2. What *is* verified today: the app builds to `dist/app.zship`,
typechecks, the two dev users sign in as distinct subjects
(`pws_alice…` / `pws_bob…`), and anonymous RPC calls are refused with 401.
