# Error message quality: census, audience taxonomy, and options

Status: proposal (investigation only; nothing implemented)
Date: 2026-08-14
Scope: read-only investigation of user-reachable error text in `crates/`, `sdks/`, `libs/`

This document is ASCII-only, deliberately. Non-ASCII characters found in the
codebase are transcribed by codepoint (`U+2014`) or by name (em-dash), never
pasted. A document about non-ASCII leakage that itself leaked would be its own
punchline.

---

## 0. The one string, decomposed

The investigation is anchored on a real production failure, not a hypothetical.
A creator's deployed app returned this to a browser:

```
{"message":"internal error","name":"Error","request_id":"1"}
```

while the worker logged (non-ASCII transcribed):

```
db: autocommit session setup (per-app <U+00A7>17.5 + DB-1 guards): db: db error <U+2014> caused by: ERROR: role "app_<uuid>_role" does not exist
```

Every fragment has an owner. All of the following are VERIFIED by reading the
cited line:

| Fragment | Origin | Layer |
| --- | --- | --- |
| `db: autocommit session setup (per-app <U+00A7>17.5 + DB-1 guards): ` | `crates/zeroship-data-engine/src/exec.rs:326` (literal), applied at `exec.rs:322-329` | plugin-db call site |
| inner `db: ` | `crates/zeroship-plugin-db/src/error.rs:721` -- `format!("db: {e}")` in `walk_pg_chain` | plugin-db classifier |
| `db error` | `libs/compio-postgres/src/error/mod.rs:395` -- `Kind::Db => fmt.write_str("db error")` | driver Display |
| ` <U+2014> caused by: ` | `crates/zeroship-plugin-db/src/error.rs:724` -- `msg.push_str(&format!(" <U+2014> caused by: {src}"))` | plugin-db source-chain walk |
| `ERROR: role "app_<uuid>_role" does not exist` | `libs/compio-postgres/src/error/mod.rs:313-315` -- `write!(fmt, "{}: {}", self.severity, self.message)` | PostgreSQL server text, verbatim |

The user-facing body is built by `build_internal_error_body`
(`crates/zeroship-runtime/src/core/dispatch.rs:349-361`), reached from `build_error_body`
(`dispatch.rs:148-226`): at `dispatch.rs:155` the status is in `500..=599`, at
`dispatch.rs:179` the error's code fails `is_public_error_code`
(`dispatch.rs:276-305`), so the blanked body is returned at `dispatch.rs:183-186`.

`request_id` is `"1"` because it is a per-isolate `u64` counter, not a trace id:
declared at `crates/zeroship-runtime/src/core/runtime.rs:882`, initialised to `1` at
`runtime.rs:1197`, incremented per dispatch at `runtime.rs:2130-2131`. A cold
app whose first request fails always yields `"1"`.

Two findings fall directly out of this decomposition and shape everything below.

**Finding A: the worst leak is not a string in this repository.** The text
`role "app_<uuid>_role" does not exist` appears in this tree only inside
comments (`crates/zeroship-plugin-db/src/register_model/bootstrap.rs:201-202`,
`crates/zeroship-cli/src/migrate.rs:23`). The runtime value is PostgreSQL's own message,
copied verbatim by `walk_pg_chain` (`crates/zeroship-plugin-db/src/error.rs:720-728`). No
scanner over our source literals can ever find this class of leak. Any proposal
that consists of "grep for bad strings and fix them" is structurally incapable
of preventing the failure that prompted it.

**Finding B: the message was not merely ugly, it was misclassified.** This is a
creator configuration error with a known, documented, one-command fix. The
in-tree documentation already says so:
`crates/zeroship-cli/src/migrate.rs:16-25` states that deploying an `env.db` app without
running `zeroship migrate` makes the first DB call fail with
`role "app_..._role" does not exist`, "which reaches the end user as
`{"message":"internal error"}`". `DbError::from_pg`
(`crates/zeroship-plugin-db/src/error.rs:189-230`) has no arm for this SQLSTATE, so it
falls through to the catch-all `DbError::Internal` at `error.rs:228`, which
stamps the code `internal` (`error.rs:287-289`). `internal` is on neither
allow-list, so the rail correctly blanks a message it has been told is internal.
The sanitiser did its job. The classifier lied to it.

---

## 1. Census

### 1.1 Method, and what it cannot see

Greping lines does not work here: a comment mentioning `internal error` is
indistinguishable from an error that says it, and this repository is unusually
comment-dense. So the census tokenizes.

A ~150-line scanner (Node, run from the repo root; kept in scratch, not
committed) walks `crates/`, `sdks/`, `libs/`, skipping `target`, `node_modules`,
`dist`, `.git`, `wpt`, `coverage`, `build`. For Rust it handles `//`, nested
`/* */`, `"..."` with escapes, `r"..."` / `r#"..."#`, and skips char literals and
lifetimes; for TS/JS it handles `//`, `/* */`, and all three quote forms.
**Comments are dropped.** It then records, for each literal, the 160 preceding
source characters, so a literal can be classified by whether it sits in an
error-constructing position.

Denominator: **1596 files scanned (1043 Rust, 553 TS)**.

**The instrument was validated against ground truth before its numbers were
trusted, and it failed the first time.** The first classifier used a guessed
list of error constructors (`format!`, `anyhow!`, `panic!`, `.context(`, ...).
It found the `exec.rs:326` literal but marked it `err=false`, because this
codebase builds that error through its own helper, `prefix_message`
(`crates/zeroship-plugin-db/src/error.rs:521-539`), which is in no such list. The
vocabulary was then **derived from the data** -- ranking the identifier
preceding every literal in the tree -- rather than guessed, and re-run. After
widening, the ground-truth literal classifies as `err=true` and the guard-token
count in error positions rose from 1 to 5. Both numbers below are reported from
the validated run.

**What this census structurally CANNOT see:**

1. **Runtime-composed text.** `format!` arguments, template-literal `${}` spans.
   The observed production string is 60% composed of values the scanner never
   sees.
2. **Text owned by dependencies.** PostgreSQL server messages, Stripe API
   errors, `io::Error` text. This is where the actual leak in the anchor case
   lives (Finding A).
3. **Macro-generated text.** `#[v8_class]` and the config macros synthesise
   messages.
4. **Text in data.** DB rows, JSON fixtures, template files.
5. **Reachability.** The scanner says where a string is written, never who reads
   it. Every audience claim in Section 2 was traced by hand.

Consequently the counts below are a **lower bound on the problem and an upper
bound on what a scanner could enforce.**

### 1.2 Non-ASCII

3326 literals carry at least one flag. Of these:

| Measure | Count |
| --- | --- |
| Literals containing non-ASCII | **1642** |
| ... in test/bench files | 690 |
| ... in non-test files | **952** |
| ... in an error/log construction position | 242 |
| ... in an error/log position, non-test | **196** |

Codepoints actually used (all flagged literals; err/log non-test in brackets):

| Codepoint | Character | All | err/log non-test |
| --- | --- | --- | --- |
| `U+2014` | em-dash | 1076 | 162 |
| `U+2192` | rightwards arrow | 244 | 17 |
| `U+2026` | ellipsis | 65 | 4 |
| `U+2212` | minus sign | 36 | 0 |
| `U+00D7` | multiplication sign | 35 | 2 |
| `U+21D2` | double arrow | 33 | 0 |
| `U+00A7` | section sign | 29 | 9 |
| `U+2013` | en-dash | 11 | 2 |
| `U+2265` | greater-or-equal | 15 | 2 |
| `U+2713` | check mark | - | 1 |

The em-dash is 65% of all occurrences and dominates the error surface. Worst
files, non-test:

| Count | File |
| --- | --- |
| 44 | `crates/zeroship-control/src/stripe_handlers.rs` |
| 26 | `crates/zeroship-control/src/proration.rs` |
| 24 | `sdks/ui/src/stories/Input.stories.tsx` |
| 21 | `crates/zeroship-control/src/pricing.rs` |
| 19 | `sdks/ui/src/stories/NavigationMenu.stories.tsx` |
| 17 | `crates/zeroship-control/src/cron/billing_reconcile.rs` |
| 17 | `crates/zeroship-control/src/notify.rs` |

By crate, non-test: `sdks/ui` 425, `crates/control` 217, `crates/plugin-db` 59,
`sdks/vite-plugin` 38, `crates/runtime` 27, `crates/auth` 25,
`crates/runtime-macros` 24, `sdks/db` 24, `crates/gateway` 23.

Note the shape: `sdks/ui` leads on raw count but those are Storybook demo
strings, not errors. `crates/control` is the real error-surface concentration.

**The single highest-value non-ASCII site is one line:**
`crates/zeroship-plugin-db/src/error.rs:724`. It is the em-dash in the anchor string, and
because it sits in the source-chain walk it stamps an em-dash into *every*
multi-layer database error the platform produces.

### 1.3 Doc markers, guard identifiers, ticket IDs

| Tag | Total | Non-test | In err/log position, non-test |
| --- | --- | --- | --- |
| guard IDs (`DB-1`, `SEC-4`, `ISS-29`) | 56 | 34 | **5** |
| section refs (`U+00A7`17.5, `section 4.3`) | 28 | 13 | **8** |
| doc refs (`.md`, `proposal`, `see docs/`) | 25 | 21 | 1 |

This is the finding most improved by classification. **The majority of guard
tokens are legitimate.** `SEC-1` and `SEC-4` appear in `assert!` messages
(`crates/zeroship-plugin-db/src/context.rs:1206-1275`,
`crates/zeroship-data-engine/src/crud/mask_pass.rs:1055-1065`) -- a DEVELOPER audience,
where naming the guard being tested is exactly right. A blanket ban on guard
tokens in strings would delete these correctly-written test assertions.

The runtime offenders are a small, tightly-scoped set:

- `crates/zeroship-data-engine/src/exec.rs:316` -- `db: autocommit BEGIN (per-app U+00A7 17.5 + DB-1 guards): `
- `crates/zeroship-data-engine/src/exec.rs:326` -- `db: autocommit session setup (...)` (the anchor)
- `crates/zeroship-data-engine/src/exec.rs:344` -- `db: autocommit COMMIT (...)`
- `crates/zeroship-data-engine/src/transaction/mod.rs:171` -- `db: tx session setup (...)`
- `crates/zeroship-data-engine/src/auth/bootstrap.rs:1786` -- `per-app role MUST be NOREPLICATION (U+00A7 17.5 slot-ownership-stays-platform)`
- `crates/zeroship-core/src/logout_token.rs:195,204` -- `(forbidden by OIDC BCL U+00A7 2.4)` (arguably legitimate: a public RFC citation, not an internal doc)
- `crates/zeroship-runtime/src/web/streams/readable_default_controller.rs:1230` -- `see streams-native.md U+00A7 VII`
- `crates/zeroship-schema/src/query.rs:8728` -- `(was INTEGER pre-PR 3 -- see proposal U+00A7 9 PR 3)`

**Four lines in `plugin-db` account for the entire observed problem.** That is a
mechanical fix, not a program.

### 1.4 Opaque messages

The `opaque` regex tag returned 792 hits and is **the least trustworthy number
in this document**, so it is reported with its false positives rather than as a
headline. Inspection showed most hits in `crates/zeroship-auth/src/oidc/` are the JSON
*field name* `"error"` or RFC 6749 error *codes* (`invalid_scope`,
`invalid_principal_id` at `crates/zeroship-auth/src/oidc/device_token.rs:573-592`) --
which are stable machine-readable codes, i.e. a good pattern, tagged as a bad
one.

Hand-verified counts of actual opaque phrases (non-test, excluding `.md`):

| Count | Phrase |
| --- | --- |
| **48** | `"internal error"` |
| 9 | `"invalid request"` |
| 5 | `"unknown error"` |
| 5 | `"not found"` |
| 2 | `"something went wrong"` |
| 2 | `"bad request"` |

`"internal error"` concentration: `crates/zeroship-control/src/stripe_handlers.rs` 17,
`crates/zeroship-runtime/src/core/dispatch.rs` 11, `crates/zeroship-control/src/api.rs` 5,
`crates/zeroship-auth/src/ui/reset.rs` 5, `sdks/bootstrap/src/fetch-handler.ts` 2,
`crates/zeroship-control/src/env_handlers.rs` 2.

The `invalid request` observation holds: **5 Rust UI handlers** produce it with
different causes -- `crates/zeroship-auth/src/ui/device.rs:154`,
`crates/zeroship-auth/src/ui/signup.rs:110`, `crates/zeroship-auth/src/ui/reset.rs:87`,
`crates/zeroship-auth/src/ui/login.rs:172`, `crates/zeroship-auth/src/ui/forgot.rs:70` -- plus
`sdks/bootstrap/src/dev-auth.ts:697,710` and the enum arm at
`crates/zeroship-auth/src/ui/mod.rs:95`. Undiagnosable from the outside, as stated.

However, this surface is **already half-fixed** and that matters for the
recommendation. `PublicErrorMessage` (`crates/zeroship-auth/src/ui/mod.rs:82-113`) is a
closed enum yielding both `as_str()` (human) and `error_code()` (stable slug),
and `ErrorPage` (`mod.rs:76-80`) carries `error_code` into
`templates/error.html`. The five handlers are distinguishable *if* they stop
sharing one variant. The mechanism exists; the variants are too coarse.

### 1.5 Internal-detail leaks

The `leak` regex tag returned 833 hits, 52 in err/log positions in non-test
files. **This tag is heavily false-positive and its raw count should not be
quoted.** Inspection showed the bulk are SQL DDL strings matching on `_role` or
`information_schema` -- e.g. `crates/zeroship-data-engine/src/auth/bootstrap.rs:147,160,728`
(`CREATE SCHEMA`, `GRANT EXECUTE`), `crates/zeroship-migrate-server/src/provisioning.rs:123`.
These are SQL being *built*, not messages being *shown*.

The real leak risk is not literal, it is passthrough (Finding A).

**The correct framing, after tracing reachability, is that leaks are CONTAINED,
not exposed.** The platform has a working, fail-closed sanitising rail and it
holds. A large volume of internal detail reaches app JS and the creator; almost
none of it reaches an anonymous HTTP caller. Six redaction mechanisms exist
(VERIFIED, each read):

| Helper | Location | Coverage |
| --- | --- | --- |
| `build_error_body` 5xx rail | `crates/zeroship-runtime/src/core/dispatch.rs:148` | the main boundary |
| `infrastructure_error_response` | `crates/zeroship-control/src/api.rs:140` | 15 sites in `api.rs`, 4 in `workflow_instance_api.rs` |
| `redact_url` | `crates/zeroship-plugin-kv/src/error.rs:178-189` | 3 sites, all URL-bearing plugin-kv messages |
| `scrub_constraint_detail` | `crates/zeroship-plugin-db/src/error.rs:713-718` | strips PG `DETAIL:` (the conflicting value) |
| `PublicErrorMessage` | `crates/zeroship-auth/src/ui/mod.rs:83-113` | closed enum, 5 strings, end-user login UI |
| `oidc_callback_public_error` | `crates/zeroship-gateway/src/router/dispatch.rs:2909` | returns `&'static str` -- leak-proof by type |

`scrub_constraint_detail` fires for only four SQLSTATEs (`error.rs:201-207`:
23505/23503/23502/23514); for 42P01, 42501, 28000 the `DETAIL` and `HINT` lines
survive into the message. They are then blanked at the HTTP boundary, because
those codes are not allow-listed. **83 distinct plugin-db error codes exist; 13
are allow-listed.**

Specific leak sites worth fixing, all traced to OPERATOR-ONLY over HTTP:

- **`crates/zeroship-control/src/cron/workflow_engine.rs:1084`** --
  `format!("workflow scheduler store: {error:?}")`. The `Debug` (not `Display`)
  formatter on `compio_postgres::Error` prints the full PG payload: schema,
  table, column, constraint, SQLSTATE, detail, hint, position, file, line,
  routine. **The sibling function four lines above (`:1080`) uses `{error}`**, so
  this reads as an oversight rather than intent. Routes only to
  `infrastructure_error_response`, so it is logged and blanked.
- **Redis credentials bypassing the helper built to stop them.**
  `crates/zeroship-plugin-kv/src/backend/redis.rs:170` applies `redact_url` to the outer
  URL but interpolates the inner error raw, and
  `libs/compio-redis/src/client.rs:94` builds `bad URL '{url_str}': {e}` with the
  password intact. Separately, `redact_url` passes unparseable input through
  verbatim (`crates/zeroship-plugin-kv/src/error.rs:187`, `Err(_) => raw.to_string()`) --
  and a malformed URL can still contain a password. Reaches app JS; requires
  operator misconfiguration.
- **`crates/zeroship-data-postgres/src/postgres.rs:976-1035`** -- pg_dump/pg_restore
  stderr passthrough carrying internal hostname, IP, port and role name; the
  catch-all arm keeps 4096 bytes of raw stderr.

Negative results worth recording (each traced, not assumed): the gateway's 502
`worker error: {e}` (`crates/zeroship-gateway/src/router/dispatch.rs:2564`) wraps a bare
`io::Error` and carries no host, IP or port; `compio_postgres::Error`'s `Display`
is 17 fixed literals with no DSN, and `Config`'s `Debug` redacts the password
(`libs/compio-postgres/src/config.rs:835`); no site in scope splices raw SQL into
an error message; all ~20 `expose_secret()` uses build outbound headers, none
appear in an error.

---

## 2. Audience taxonomy

### 2.1 The four audiences

| Audience | Who | Requirement |
| --- | --- | --- |
| END USER | a member of the public using a creator's deployed app | nothing internal, ever |
| CREATOR | using the CLI or console | actionable text naming what THEY can fix |
| OPERATOR | running the platform, reading logs | maximum detail; `DB-1` is legitimate here |
| DEVELOPER | a test assertion or panic | maximum detail; guard IDs are correct |

### 2.2 Does the code distinguish them today? Partly, and unevenly.

VERIFIED, the code has genuine audience awareness in four places:

1. **`build_error_body`** (`crates/zeroship-runtime/src/core/dispatch.rs:148-226`) is an
   explicit END-USER/OPERATOR split: log everything at `dispatch.rs:156-166`,
   blank the body unless the code is allow-listed.
2. **`PublicErrorMessage`** (`crates/zeroship-auth/src/ui/mod.rs:82-113`) is named for the
   audience boundary it enforces.
3. **`infrastructure_error_response`** (`crates/zeroship-control/src/api.rs:139-152`)
   logs detail, returns generic.
4. **`scrub_constraint_detail`** (`crates/zeroship-plugin-db/src/error.rs:713-718`)
   redacts a value while keeping a classification.

The failure is not absence of the concept. It is that the boundary is
**re-implemented per crate, with no shared type**, and several paths miss it.

### 2.3 Where strings cross the boundary

**Direction A -- internal text reaching an END USER.** These are the real
exposures (VERIFIED by the tracing agent, spot-checked by me):

- `errorResponse()` at `crates/zeroship-runtime/src/core/init.rs:1723-1744` forwards
  `err.message` verbatim **at any status including 500**, with no rail at all.
  Reachable from the `user.index()` catch at `init.rs:2160-2162`. Had the anchor
  app rendered its homepage through `index()` rather than RPC, the browser would
  have received the full `db: ... role "app_<uuid>_role" does not exist` string.
  `dispatch.rs:106-115` documents this divergence explicitly.
- `settle_rpc_promise` at `crates/zeroship-runtime/src/core/runtime.rs:3716-3725` takes an
  Error *returned* (not thrown) by a procedure and emits the raw message to
  `make_error` (`crates/zeroship-worker/src/handler.rs:1363-1373`), producing a 500 body
  with no blanking and no request_id. `crates/zeroship-runtime/src/transport/handler.rs:373-376`
  contains a comment warning against exactly this shape.
- `_zsSubError` at `crates/zeroship-runtime/src/core/init.rs:1771-1779` forwards
  `err.message` verbatim into the WebSocket `{"t":"error"}` frame.

**Direction B -- useful cause discarded.** The anchor case, plus:

- **`hint` never reaches the wire.** `OpError::coded` carries a remediation hint
  set at `crates/zeroship-runtime/src/core/state.rs:230-234` and populated with genuinely
  useful text (`crates/zeroship-plugin-db/src/error.rs:266,271,276`, e.g. "retry the
  transaction"). I verified independently that `build_verbose_error_body`
  (`dispatch.rs:363-396`) emits `message`, `name`, `stack`, `code`, `details`,
  `retryable` -- and that the token `hint` appears nowhere in `dispatch.rs`
  except two unrelated comments. **The field is populated and then dropped at
  every status.**
- **The control helper loop is closed; the app-dispatch loop is not.**
  `infrastructure_error_response` now returns the UUID as `trace_id`, logs the
  same value under the same key, and `@zeroship/control` lifts it. That statement
  covers only responses routed through this helper; direct control 5xx bodies
  still exist outside it and remain id-less.
- **Three unrelated request-id concepts still exist.** The gateway mints a UUID
  as `X-Request-Id` (`crates/zeroship-gateway/src/proxy.rs:558`); the worker uses it only
  to bind the `ZeroShip-User` HMAC (`crates/zeroship-worker/src/handler.rs:96-104`) and
  never forwards it; the runtime invents its own per-isolate `u64`.
  Generic sanitized app 5xx bodies carry a per-isolate `request_id` that cannot
  be joined to the gateway trace. Public-code app 5xx bodies carry no id.
- **`@zeroship/rpc` lifts `trace_id`, but app dispatch emits none.** It does not
  lift the runtime's local `request_id`, and relabeling that counter would not
  make it a joinable trace identity.

**The cleanest existing model** is `crates/zeroship-migrate-server/src/api.rs:321-345`: stable
slug in `"error"`, human text in `"detail"`, blanked at 5xx and verbatim at 4xx,
raw error to `tracing`. It is the only place in the tree that gets the split
right in one shape.

### 2.4 The structural gap: the 4xx path has no rail

`build_error_body` applies its sanitising rail **only** in `500..=599`
(`crates/zeroship-runtime/src/core/dispatch.rs:155`). At 4xx it skips the rail entirely
and emits `message`, `code`, `details` and `retryable` verbatim; only `stack` is
stripped, at every status (`dispatch.rs:223`). The TS twin
(`sdks/bootstrap/src/fetch-handler.ts:313`) behaves identically.

Today this is safe, and the reason is worth writing down because it is load-
bearing and accidental: **no platform component mints a 4xx carrying internals.**
`OpErrorKind::CodedError` sets only `.code` and `.hint`, never `.status`
(`crates/zeroship-runtime/src/core/state.rs:224-237`); `reject_op` builds a bare exception
with neither (`dispatch.rs:627`); `@zeroship/db` re-stamps `.code` but never
`.status` (`sdks/db/src/errors.ts`). So every `env.db` / `env.kv` / `env.storage`
error lands at 500 and is blanked -- which is precisely why the anchor failure
produced "internal error" rather than a leak.

The exposure is one creator idiom away. The extremely common pattern

```js
catch (e) { throw new HttpError(400, e.message) }
```

sets a 4xx status on an error whose message is the full internal chain, and every
leak catalogued in Section 1.5 goes straight to an anonymous caller. This is a
documentation and lint concern rather than a platform defect, but it is the
single thing that would turn this census from "contained" to "exposed", and it
deserves an explicit note in the creator-facing docs.

---

## 3. i18n

### 3.1 Verdict: no support exists, and none should be built now

VERIFIED absent, with the patterns tried (all counts exclude the vendored
`crates/zeroship-runtime/tests/wpt/`, `refs/`, `third_party/` trees, which dominate naive
greps -- my own `Accept-Language` search returned hits **only** in WPT):

| Probe | Patterns tried | Hits |
| --- | --- | --- |
| Header negotiation | `accept.language`, case-insensitive | 0 |
| Libraries | `fluent`, `gettext`, `rust-i18n`, `i18next`, `react-intl`, `@lingui`, `formatjs`, `icu4x`, `unic-langid`, `next-intl`, `vue-i18n` | 0 |
| Catalog files | `*.po *.pot *.mo *.ftl *.xliff *.xlf *.arb` | 0 |
| Catalog dirs | `locales/ locale/ translations/ i18n/ lang/ messages/` | 0 |
| Call conventions | `useTranslation`, `useIntl`, `<Trans`, `FormattedMessage`, `gettext(`, `i18n.t(`, `$t(` | 0 |
| Locale as a field | `locale`/`lang`/`language` on user, session, or app config | 0 (every `language` hit is plpgsql `LANGUAGE` or an FTS dictionary) |

Two adjacent capabilities do exist and should not be oversold: ICU data is
loaded into V8 (`crates/zeroship-runtime/src/core/init.rs:59-64`) so app code has
`Intl.*` -- but the comment says this was done because npm packages crash
without it, a dependency fix rather than a feature. And `sdks/ui` has real RTL
support (logical properties, `[dir="rtl"]` selectors, `lang="ar"`/`lang="he"`
stories). Direction is handled; translation is not.

One stale claim worth fixing on sight: `crates/zeroship-control/src/notify.rs:169-171`
asserts that "the template KEY carries a locale segment so adding locales later
is a template-pack drop, not a code change." There is no template key;
`render_template` is a hardcoded `match` returning inline English
(`notify.rs:173-260`). The design doc it was copied from does specify keys
(`docs/proposals/2026-06-14-billing-ops-lifecycle-design.md:1153`); the
implementation never built them. This is the "claim that reads as protection"
pattern: it answers the auditor's question before it is asked.

**Recommendation: do not build i18n pre-launch.** There are no users, no locale
signal is collected anywhere, and the audience that most needs good errors --
creators and operators -- is working in English against English-language tooling
and docs. A translation framework now would be infrastructure for users who do
not exist, which this repository's own stated policy rejects.

### 3.2 The cheapest non-foreclosing step

The standard answer is "ship a stable machine-readable code per failure, so
messages can be swapped for catalog lookups later without changing the wire
contract."

**That step is already substantially taken, which changes the recommendation
from "adopt codes" to "consolidate the six code vocabularies that already
exist."** VERIFIED:

| # | Namespace | Convention | Where |
| --- | --- | --- | --- |
| 1 | `ZsErrorCode` / `ErrorCode`, 14-15 gRPC codes | UPPER_SNAKE | `crates/zeroship-runtime/src/rpc/error.rs:63-153`, `sdks/rpc/src/error.ts:20-52` |
| 2 | plugin-db codes | lower_snake, canonicalised to UPPER_SNAKE | `crates/zeroship-plugin-db/src/error.rs:236-291`, `sdks/db/src/errors.ts:27-36` |
| 3 | `AuthErrorCode` | lower_snake | `sdks/auth/src/types.ts:72-100` |
| 4 | `PublicErrorMessage` | lower_snake | `crates/zeroship-auth/src/ui/mod.rs:83-113` |
| 5 | Workflow codes | UPPER_SNAKE, not in the enum | `sdks/bootstrap/src/dispatcher.ts:305,1331` |
| 6 | Billing gates | UPPER_SNAKE, not in the enum | `crates/zeroship-gateway/src/enforce.rs:25-26,43-44` |

The design principle is already written down: `docs/reference/api-design-guidelines.md:131-155`,
principle 9, "Use stable machine-readable error codes."

The gaps that make this fragile:

- **Rust has 15 codes, TS has 14** (Rust-only `UNKNOWN`), hand-mirrored with **no
  parity test**.
- **The control plane has essentially no codes**: 171 inline `"error":` sites in
  `crates/zeroship-control/src/`, exactly one emitting a `code` field
  (`cron/workflow_engine.rs:1638`). The `"error"` field mixes prose
  (`"app not found"`), slugs (`"invalid_scope"`), and leaked Rust type names
  (`"LimitExceededError"`, `"RunConflict"`). Meanwhile
  `sdks/control/src/index.ts:36-51` already reads `body.code` -- the client is
  ready for codes the server does not send.
- **The 5xx allow-lists are hand-mirrored across a language boundary**
  (`dispatch.rs:276-305` and `:336-347`) because canonicalisation is a table, not
  a transform (`fk_violation` becomes `FOREIGN_KEY_VIOLATION`, not
  `FK_VIOLATION`). The code comment records that until 2026-08-10 both lists
  tested a spelling already rewritten one layer down, so every exemption was
  inert.

RFC 9457 problem-details was raised three times in RPC review and deliberately
deferred (`docs/proposals/rpc.md:2277`). No ADR in `docs/decisions/` ratifies the
error-code design at all, despite `docs/feature-map.md:421` marking it shipped.

**So the cheapest non-foreclosing step is not "add codes." It is: (a) finish the
app-dispatch half of the correlation contract, and (b) make the code allow-list
derived rather than hand-mirrored.** The control helper half of (a) is already
closed. Neither step forecloses i18n, and both pay off without translation work.

---

## 4. What is already good

The recommendation should extend these, not replace them.

**The config-refusal family** is genuinely excellent and is the model to copy.

- **Name the exact knob in both tiers, as one constant.**
  `crates/zeroship-worker/src/main.rs:29` -- `const CONTROL_KEY_LABEL: &str = "ZEROSHIP_CONTROL_KEY / --control-key-file"`.
  Siblings at `worker/src/main.rs:33`, `gateway/src/main.rs:40`,
  `auth/src/main.rs:22`, `control/src/main.rs:101,202`.
- **The shared validator names nothing; the caller passes the label.**
  `crates/zeroship-core/src/config/secrets.rs:60`. The rationale at `secrets.rs:48-55`
  records that the module previously interpolated a bare `STASH_SIGNING_KEY`
  which "was right for neither and named nothing a binary reads."
- **Expected vs found, with units.** `"{label} is too short ({} bytes); minimum 32 bytes"`
  (`secrets.rs:67-70`); `"consumers of {canonical} disagree about {property}: {first} declares {first_value}, {second} declares {second_value}"`
  (`crates/zeroship-config-contract/src/contract.rs:53-56`).
- **Say what IS allowed.** `crates/zeroship-core/src/config/names.rs:1026-1029` --
  "names an unsupported secret source; supply the value itself or a
  `urn:zeroship:file:<path>` reference".
- **Print the exact command that fixes it.**
  `crates/zeroship-config-contract/src/docs.rs:54-58` -- "Regenerate with
  `cargo run -p zeroship-config-contract -- env-vars-doc`".
- **Report ALL missing inputs, not the first.** `crates/zeroship-control/src/main.rs:401-417`.
- **Withhold the value, name the identity.** `crates/zeroship-core/src/config/names.rs:1010-1041`.

**The strongest pattern in the tree: leak-proof by type, not by discipline.**
`oidc_callback_public_error` (`crates/zeroship-gateway/src/router/dispatch.rs:2909`)
returns `&'static str`. A `'static` return type makes it **impossible to
interpolate a runtime value** -- the compiler, not a reviewer and not a CI gate,
enforces that nothing internal can reach the user. `PublicErrorMessage`
(`crates/zeroship-auth/src/ui/mod.rs:82-113`) achieves the same via a closed enum with a
`const fn as_str`. These two are the only places where the audience boundary is
guaranteed rather than merely observed, and they are the right model for any new
end-user-facing surface. (Ironically, the doc comment inside
`oidc_callback_public_error` itself contains an em-dash and a section-sign
reference at `dispatch.rs:2913-2914` -- a comment, so harmless, but it shows how
pervasive the habit is.)

**Elsewhere in the tree:**

- `crates/zeroship-cli/src/main.rs:888-894` -- unknown-flag error naming the bad token,
  the *consequence* ("`--control` falling back to its default would deploy to
  http://localhost:9090 instead of the control plane you named"), and corrected
  usage. Regression test at `main.rs:1049-1065`.
- `crates/zeroship-cli/src/main.rs:909-910` -- "no API token found; run `zeroship login`,
  pass `--token=<PAT>`, or set ZEROSHIP_TOKEN" (three exact next actions).
- `sdks/vite-plugin/src/dev-server.ts:431-436` -- names the missing collections,
  the exact next command (`pnpm migrate`), and the db path. (Contains an em-dash.)
- `crates/zeroship-migrate-server/src/api.rs:321-345` -- the correct two-audience split.

**The test that proves a diagnostic names a real variable**, which is the
enforcement model for Section 6:
`crates/zeroship-control/src/main.rs:1645-1689`,
`every_auth_provider_diagnostic_names_a_variable_control_reads()`. Its expected
set is **derived from compiled metadata, never listed**
(`main.rs:1531-1545`: clap `CommandFactory` plus the macro-generated
`ControlSettings::SPECS`), so editing a list cannot satisfy it. The token scanner
(`crates/zeroship-core/src/config/diagnostics.rs:26-31`) is shape-based, not
dictionary-based, "because a scanner that only recognised names it already knew
could not see the stale one". It has a one-variable control at
`main.rs:1691-1708` and states its own non-coverage at `main.rs:1660-1665`. The
inverse property is pinned in `crates/zeroship-core/src/config/secrets.rs:457-505`.

It exists for **2 of 5 servers** (control, migrated). Gateway, worker and auth do
not have one. That is the obvious extend-what-works move.

---

## 5. Options, with costs

### Tier 1 -- mechanical and safe now

**Option 1a. Strip non-ASCII from runtime error strings.**
Scope: 196 literals in error/log positions in non-test files; the em-dash is 83%
of them. The single highest-value edit is `crates/zeroship-plugin-db/src/error.rs:724`
(` <U+2014> caused by: ` becomes ` -- caused by: `), which fixes every
multi-layer DB error at once.
Cost: hours. Risk: low, but **not zero** -- any test asserting on an em-dash
would break, and `sdks/ui` strings (425 of the 952) are user-visible demo copy
where typographic dashes are arguably correct. Recommend scoping to
`crates/` and `sdks/{db,rpc,bootstrap,vite-plugin}`, explicitly excluding
`sdks/ui`.

**Option 1b. Delete doc markers and guard IDs from the four runtime sites.**
`crates/zeroship-data-engine/src/exec.rs:316,326,344` and
`crates/zeroship-data-engine/src/transaction/mod.rs:171`. Replace
`db: autocommit session setup (per-app U+00A7 17.5 + DB-1 guards): ` with
`db: autocommit session setup: `. The guard rationale belongs in the comment
directly above, where it already is (`exec.rs:301`).
Cost: under an hour. Risk: very low.
**Do NOT extend this to test assertions** -- `SEC-1`/`SEC-4` in
`context.rs:1206-1275` are correct for their audience.

**Option 1d. Fix the three concrete leak sites.** Change `{error:?}` to
`{error}` at `crates/zeroship-control/src/cron/workflow_engine.rs:1084` to match its
sibling at `:1080`; interpolate `redact_url(...)` rather than the raw inner error
at `crates/zeroship-plugin-kv/src/backend/redis.rs:170`; and either bound or drop the
4096-byte raw-stderr arm at `crates/zeroship-data-postgres/src/postgres.rs:976-1035`.
Cost: an hour. Risk: very low. All three are currently contained by the 5xx rail,
so this is defence in depth rather than an active exposure -- which is also the
argument for doing it cheaply now rather than scheduling it.

**Option 1c. Close the correlation loop.** This option contains two distinct
paths. The control helper half has shipped: `infrastructure_error_response`
emits its UUID under the wire contract's `trace_id` key, logs the same value,
and `@zeroship/control` lifts it. Other direct control 5xx bodies remain id-less.
Option 1c remains open for app dispatch. Generic sanitized app 5xx bodies expose
only a per-isolate `request_id`, public-code app 5xx bodies expose no id, and
neither response contains the `trace_id` that `@zeroship/rpc` already lifts.
Cost for the remaining path: hours plus an identity and propagation decision.
Risk: low. Payoff: it turns an app's "internal error" into a reportable failure.

### Tier 2 -- needs a design decision

**Option 2a. Classify the anchor failure properly.** The measured SQLSTATE is
part of the contract: `SET LOCAL ROLE` reports `invalid_parameter_value` / `22023`
for a missing role, not `undefined_object` / `42704`. Classify that failure at
the per-app session-setup call site as a `Configuration` error with a public
code (say `schema_not_provisioned`) and a hint naming `zeroship migrate`. Add
that code to the public allow-list.

Two nearby provisioning failures are deliberately different. A missing schema with a fully-qualified query reports `undefined_table` / `42P01`, not
`invalid_schema_name` / `3F000`. A present role without required grants reports `insufficient_privilege` / `42501`. `42P01` and `42501` must not be added to the missing-role classifier. In particular, `42501` is also the ordinary
permission-denied SQLSTATE, so treating it as "run migrate" would prescribe the
wrong remediation for unrelated authorization failures.
Cost: a day, including the allow-list and a regression test.
Payoff: the anchor failure becomes self-diagnosing for the creator, using the
existing rail with no new machinery. **This is the single highest-value change in
this document.**
Decision required: is "the app's DB role does not exist" a creator-facing
condition? I argue yes -- it has a documented one-command fix
(`crates/zeroship-cli/src/migrate.rs:16-25`) and reveals nothing an attacker gains from.

**Option 2b. Emit `hint` on the wire.** The field is populated and dropped
(Section 2.3). Adding it to `build_verbose_error_body` surfaces existing,
already-written remediation text.
Cost: hours plus a decision about whether hints are safe at 5xx (they are
platform-authored, so probably yes, on the same allow-list basis as codes).

**Option 2c. Consolidate the six code vocabularies and derive the allow-list.**
Replace the hand-mirrored `is_public_error_code` lists
(`dispatch.rs:276-305`, `:336-347`) with a single registry that both the Rust
rail and `sdks/db/src/errors.ts` read, plus a cross-language parity test (which
does not exist today).
Cost: several days. Risk: touches the wire contract -- acceptable pre-launch.

**Option 2e. Decide what to do about the 4xx bypass (Section 2.4).** Three
choices: (i) document the `throw new HttpError(400, e.message)` hazard in the
creator-facing docs and leave the rail as is; (ii) strip `message` at 4xx too,
which would break legitimate creator-authored validation errors and is probably
wrong; (iii) sanitise only when the error carries a platform-minted code the
creator did not author. I favour (i) now and (iii) if it ever bites, but this
needs a decision rather than a default.
Cost: (i) is an hour; (iii) is several days.

**Option 2d. Split `PublicErrorMessage::InvalidRequest` into per-cause variants**
so the five auth UI handlers stop sharing one string. The enum and the
`error_code()` rendering already exist (`crates/zeroship-auth/src/ui/mod.rs:82-113`,
`templates/error.html`).
Cost: half a day.

### Tier 3 -- should wait

- **i18n / translation catalogs.** Section 3. No users, no locale signal.
- **RFC 9457 problem-details.** Already deliberately deferred three times
  (`docs/proposals/rpc.md:2277`). Revisit post-launch.
- **Converting all 171 control-plane error bodies to a shared type.** Right
  direction, wrong moment; do it when a code registry exists to convert them to,
  or the work is done twice.
- **A general audience-tagging type system** (e.g. `Public<T>` / `Internal<T>`
  newtypes). Attractive, but it is a large refactor whose benefit is mostly
  delivered by Options 2a and 2c at a fraction of the cost.

---

## 6. How each would be enforced

The repository has a working gate shape, CI-wired at 15+ points in
`.github/workflows/ci.yml`. The question is which parts transfer.

**The ASCII mechanism already exists and is directly reusable.**
`tests/commit_msg_gate.sh:90` uses `LC_ALL=C grep -q '[^ -~\t]'` and reports
"non-ASCII character (em-dash, curly quote, arrow); use ASCII". A runtime-string
gate can use the identical test.

**Proposed: `tests/runtime_string_gate.sh`**, modelled on
`tests/config_name_alignment_gate.sh`, checking (1) no non-ASCII and (2) no
guard-ID or section-marker pattern, in string literals in error positions under
a defined path set.

Properties it must have, all copied from existing gates:

- **A `--self-test` with planted mutations**, per
  `config_name_alignment_gate.sh:309-365`, because "a gate that accepts
  everything and a gate that is broken print the same thing"
  (`commit_msg_gate.sh:9-13`).
- **One-variable controls** on the real inputs, per
  `config_name_alignment_gate.sh:356-359` -- otherwise the mutations prove only
  that the check rejects everything.
- **Assert the REASON, not just rejection**, per `commit_msg_gate.sh:184-194`
  (symbolic codes like `NON_ASCII`, `GUARD_TOKEN`), so it cannot pass for the
  wrong reason.
- **A measured PASS floor**, per `config_name_alignment_gate.sh:425-434`:
  "Nothing FAILED, so this is not a broken check - it is MISSING ones."
- **The inverted-gate half.** The strongest shape in this repo is the raw-env
  scanner (`crates/zeroship-config-contract/src/main.rs:404-415`), where **a clean scan is
  the FAILING case**: two violations are planted under
  `crates/zeroship-config-contract/tests/fixtures/`, and `PLANTED_VIOLATIONS = 2`
  (`main.rs:471`) is documented as "A FLOOR, not a ceiling, is the wrong shape
  here." A runtime-string gate should plant an em-dash and a `DB-1` in a fixture
  and fail if it stops finding them.

### False-positive rate: the honest assessment

This is where the proposal must be careful, because **my own census tags had
severe precision problems, and a gate is just a census that blocks CI.**

Measured on this investigation:

- The `opaque` tag: 792 hits, of which the auth/OIDC bulk were JSON field names
  and RFC 6749 codes -- i.e. **good** patterns flagged as bad. Unusable as a gate.
- The `leak` tag: 833 hits dominated by SQL DDL matching `_role`. Unusable as a
  gate.
- The `guard` tag: 56 hits, of which the majority are correct test assertions.
  **Usable only if scoped to non-test files in an error position** -- which
  reduces it to 5 real hits.
- The `nonascii` tag: **precision is essentially 100%** because it is a character
  class, not a semantic guess. This is the only one of the four I would put in a
  blocking gate.

**Therefore: gate non-ASCII, do not gate the rest.** For guard tokens and doc
markers, fix the 4-8 known sites (Option 1b) and rely on review; a semantic gate
would generate more false positives than findings, and the repository's own
history shows that a gate people learn to work around is worse than none.

Even the non-ASCII gate needs a path allow-list (`sdks/ui` demo copy, WPT,
vendored trees) and an exemption mechanism with a stated reason per entry, per
the `AMBIENT_COMPOSE_KEYS` pattern (`config_name_alignment_gate.sh:63-70`).

**What no gate here can check** (state it, per house style): whether a message is
*true*, whether it names the *right* variable, whether "internal error" was the
correct choice for that failure, and -- critically -- **anything about text
composed at runtime or owned by PostgreSQL**, which is where the anchor
failure's worst content came from.

---

## 7. Arguing against these recommendations

**7.1 The strongest objection: I am proposing to gate the one thing that did not
cause the problem.** The anchor failure was ugly because of an em-dash and a
`DB-1`, but it was *harmful* because a creator-fixable condition was classified
`internal` and blanked. A non-ASCII gate would have caught none of that. If only
one item ships, it should be Option 2a, not the gate. There is a real risk that
the mechanical items get done because they are easy, the CI gate makes the
codebase feel governed, and the classification bug -- the actual defect --
survives.

**7.2 The census may be materially incomplete, and I can prove it is at least
somewhat incomplete.** My first classifier missed the ground-truth string. I
found that only because I had a known answer to check against. For the categories
where I had no ground truth -- leaks, opaque messages -- **I have no equivalent
proof that the instrument sees what it claims to see.** The counts in Section 1
should be read as "at least this many", never as totals. In particular, Finding A
means the highest-severity content in the anchor case was invisible to every
scan I ran.

**7.3 "Extend what works" may be survivorship bias.** The config-refusal family
is excellent, but it governs a *closed, compile-time-enumerable* domain: there is
a finite registry of config names, which is exactly why the derived-expected-set
trick works. Runtime error text has no registry and no closed domain. Copying the
gate shape into a domain that lacks the property that made it work is a real
risk, and it is why I recommend gating only the character-class check, which
needs no registry.

**7.4 The em-dash cleanup may be net-negative in `sdks/ui`.** 425 of the 952
non-test non-ASCII literals are there, and typographic dashes in user-facing demo
copy are a deliberate typographic choice, not a defect. A blanket strip would
degrade them. My scoping recommendation handles this, but it means the headline
"1642 non-ASCII strings" overstates the actionable population by roughly 3x.

**7.5 Closing app-dispatch correlation has a cost I have not measured.** The
generic app body currently exposes a per-isolate `request_id` counter (`"1"`
announces a cold isolate), while the public-code body exposes no id. The new
contract should carry an opaque, joinable `trace_id`; relabeling the counter
would overstate what it can correlate. I have not verified what depends on the
counter's current format. `crates/zeroship-runtime/src/core/dispatch.rs:117-137` uses the
format as evidence for which rail served a request, so changing it would also
invalidate an existing diagnostic technique documented in the tree.

**7.6 My own leak severity arrived before its bounds, and the bounding changed
the answer.** The anchor string contains a database role name, so the natural
first reading is "the platform leaks internals to end users". Tracing
reachability inverted that: the raw text reaches app JS and the operator log, and
is blanked before any anonymous HTTP caller sees it. The scary half of that
finding was one read; the bounding half -- who can actually see it, and does the
protecting thing exist -- was a dozen boring ones. Section 1.5 is written in the
bounded form, but the raw tag counts (833 `leak` hits) would support a far more
alarming document, and that document would be wrong. Anyone extending this work
should re-derive reachability rather than inheriting my classifications.

**7.7 I did not verify runtime behaviour.** Every claim here is from reading
code. I ran no app, reproduced no error, and did not touch the live host. The
decomposition in Section 0 is a reading of the source that matches the observed
string exactly, which is strong evidence but is not the same as having executed
it. Claims about which paths are *reachable* (Section 2.3) are the most likely to
be wrong.

---

## 8. Recommended order, if any

1. **Option 2a** (classify the role-missing failure; public code plus hint). Fixes
   the actual defect.
2. **Option 1c** (finish app-dispatch correlation; the control helper is closed).
   Highest diagnostic payoff per hour.
3. **Option 1b** (delete the four doc-marker strings) and **Option 1d** (the
   three concrete leak sites). Trivial, bounded, an hour each.
4. **Option 1a** (strip non-ASCII in `crates/` and non-UI SDKs) plus the
   non-ASCII-only gate with an inverted self-test.
5. **Option 2e(i)** -- document the 4xx re-throw hazard. Cheap, and it guards the
   one idiom that could turn Section 1.5 from contained into exposed.
6. **Option 2b/2d**, then **2c** when there is appetite for a wire change.

Items 1 and 2 are worth doing even if nothing else on this list ever ships.
