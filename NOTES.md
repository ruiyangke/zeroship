# Restoring principal-grant narrowing after CLI login moved onto the OP

Scratch notes for `fix/op-grant-narrowing`. Diagnosis first, then the
provisioning recommendation the task turns on, then implementation.

Every claim below is labelled:

- **VERIFIED** - I read the cited lines in this worktree.
- **INFERRED** - drawn from verified facts, not itself read.

---

## 1. The regression, re-verified

The brief's account is accurate. Confirming each leg independently:

- **VERIFIED** `crates/zeroship-control/src/device_handlers.rs:665-690`,
  `deploy_scopes_for_principal` selects `grant_name` from
  `zeroship.principal_grants` for the principal and intersects it with the
  requested scopes. `crates/zeroship-control/src/device_handlers.rs:638-663` calls
  `ensure_platform_creator_grants` first, so the intersection has something to
  intersect with.
- **VERIFIED** `crates/zeroship-auth/src/oidc/device_token.rs:680-706`, the OP device
  redemption arm computes `granted_scopes` from the row's requested scope and
  caps it against `platform_cli_scopes()` - the CLIENT REGISTRATION - and
  nothing else. Its own comment at `:686-702` says so explicitly.
- **VERIFIED** the brief's warning about `device_token.rs:908` is correct. The
  `LEFT JOIN zeroship.principal_grants` there is inside `mint_platform_token`,
  a different endpoint. The device-code arm returns at `:718` via
  `mint_grant_access_token` and never reaches it.

So after the login move, no principal-grant read happens anywhere on the
login-to-deploy path.

### 1a. It is worse than "narrowing is gone at login"

**VERIFIED** `crates/zeroship-authn/src/lib.rs:388-421`. Control's bearer verification
has two arms:

- `ProviderAuthz::GoTrueRole` (`:409-419`) resolves the principal and calls
  `load_principal_grants` (`:460-482`), deriving the policy from the LIVE grant
  rows on every request.
- `ProviderAuthz::OAuthScope` (`:389-408`) - the arm an OP-issued CLI token
  takes - derives the policy from the token's `scope` claim alone.

So for a platform-native creator, `principal_grants` is not consulted at login
OR at request time. The table is entirely dead for that population. The
Supabase population still gets live enforcement. That asymmetry is the real
shape of the defect, and it is bigger than the brief states.

**VERIFIED** by the harness itself: `tests/e2e_device_login.sh:427-437` asserts
a fresh platform principal holds 0 grant rows, `:593-601` asserts it still
holds 0 after login, and `:605-620` then creates an app and deploys
successfully with that token. Zero grants, full authority, deploy succeeds.

---

## 2. Why the obvious fix is blocked - confirmed, with the privilege map

**VERIFIED** `db/migrations-ts/20260702000900_grants.ts`:

| table | `zeroship_auth` | `zeroship_control` |
| --- | --- | --- |
| `zeroship.users` | SIUD (`:53`) | SELECT only (`:70`) |
| `zeroship.principal_grants` | SELECT only (`:55`) | SIUD (`:65`) |
| `zeroship.identity_links` | none | SIUD (`:65`) |

Two consequences the brief names one of:

1. Auth cannot provision grants (the brief's point).
2. **Control cannot observe principal creation.** It has SELECT on
   `zeroship.users` and nothing more, and principals are created only by auth
   (**VERIFIED** `crates/zeroship-auth/src/store/users.rs:127`,
   `crates/zeroship-auth/src/identity/linker.rs:436,503,560,613`,
   `crates/zeroship-auth/src/oidc/authorization_code.rs:1526`). This is what kills the
   brief's second candidate outright, not just makes it awkward.

**VERIFIED** there is no auth-to-control call path to hang provisioning on:
`control_url` in auth is used only to render the Supabase approve URL into the
device page (`crates/zeroship-auth/src/ui/device.rs:475`). `ControlEvent` has no auth
producer (`grep -rln ControlEvent crates/` returns only `core` and `control`).

**VERIFIED** there is no operator API for grants at all. `grep principal_grants`
over `crates/control/src/` outside `device_handlers.rs` and
`identity_bridge.rs` is empty. The operator's vehicle is SQL against the table,
which matters for section 4: any design where the operator has no rows to
delete gives the operator no narrowing surface at all.

### 2a. The marker semantics that must survive any move

**VERIFIED** `crates/zeroship-control/src/identity_bridge.rs:63-71,89-107`.
`ensure_platform_creator_grants` keys "already provisioned" off the existence
of an `identity_links` row, NOT off the grant count, precisely so that
"operator revoked everything" is distinguishable from "never provisioned" and a
re-login does not resurrect revoked grants. Pinned by
`crates/zeroship-control/tests/device_handlers_test.rs:2440+`,
`device_token_keeps_reduced_grants_when_identity_link_already_seeded`.

This is load-bearing. Any relocation of provisioning that keys off the grant
count instead re-opens exactly the hole that test was written for.

---

## 3. The candidates, scored

The brief lists four. There is a fifth, and it is already this repo's own
recorded recommendation.

### A - auth provisions on first login, intersects at redemption

Restores the old behaviour in place. Costs:

- Needs INSERT on `principal_grants` for `zeroship_auth`, AND SELECT+INSERT on
  `identity_links` to carry the section-2a marker. `identity_links` is a
  control-owned table with Supabase-specific linkage semantics; handing auth
  write access to it is a much larger step than the grant row itself.
- Collapses a separation that currently exists: auth signs the token, control
  owns the entitlement store. Under A the signer also writes the entitlements,
  so an auth defect yields both a token and the durable rows that justify it.
- Puts a write on the login path.
- Still only narrows at login, so a token already issued keeps its scope for
  its full lifetime.

### B - control provisions out of band, at principal creation

**Not implementable.** Control cannot see principal creation (section 2). The
degenerate form is a sweep over `zeroship.users`, which needs no new privilege
but races first login: sign up and log in inside the sweep interval and the
intersection is empty, so the creator's first `zeroship deploy` 403s. A
confusing failure on a brand new account, on the platform's primary flow.

### C - the OP calls control to provision during login

New inter-service dependency on the login hot path, in a direction that does
not exist today, and login availability becomes coupled to control
availability. The vehicle would be the service-assertion mechanism, which this
brief forbids touching.

### D - registration scope becomes the only cap

This is main today. It deletes the operator's per-principal narrowing outright,
which is the thing the task exists to prevent.

### E - control enforces entitlements at request time (RECOMMENDED)

Not in the brief's list, but it is this repository's own recorded
recommendation, twice:

- **VERIFIED** `docs/proposals/2026-08-16-cli-token-issuance.md:398-404`, phase
  1 item 1.3: "**Recommendation: do not move it.** Issue a coarse token and
  have control enforce entitlements at request time as a resource server."
- **VERIFIED** the same document `:409`, phase 2 item 2.4: "Control becomes a
  resource server. Validates OP-issued tokens; applies entitlement-aware
  authorization at request time."

The document is marked READY and states it executes the accepted ADR
`docs/decisions/2026-06-30-self-contained-auth-replace-hydra.md`. So choosing E
is executing a recorded decision rather than making a new one - which is why I
am proceeding rather than stopping for the operator.

---

## 4. The recommendation, in full

**Split read from write. The intersection is a shared read; the provisioning
stays a control-only write.**

### 4.1 Read rule - `crates/authn`, pure SELECT, no new privilege

For the `ProviderAuthz::OAuthScope` arm, replace "policy = token scope" with:

```
marker    = EXISTS(identity_links WHERE principal_id = $1)
rows      = principal_grants WHERE principal_id = $1
effective = if marker { rows } else { PLATFORM_CLI_ISSUABLE_SCOPES }
policy    = parse(token.scope) INTERSECT effective
```

Properties:

- Needs no new privilege anywhere. `zeroship_control` already has SELECT on
  both tables and `authn` already reads `principal_grants` on the GoTrue arm
  (`crates/zeroship-authn/src/lib.rs:460`), so this is the same query on the other arm.
- Narrowing becomes IMMEDIATE, not next-login. An operator's DELETE narrows the
  token already in the creator's hand, not merely the next one. That is
  strictly stronger than what commit `5ae8c7f7d` removed.
- The unmarked fallback is what stops a first login minting empty authority,
  and it is not a second source of truth: the fallback set is exactly the set
  section 4.2 materializes, so the two never disagree.

The unmarked fallback also makes the rule safe for `authn`'s other consumer.
`crates/zeroship-migrate-server` uses the same `BearerVerifier`
(`crates/zeroship-migrate-server/src/main.rs:167`) against a deployment-configured DSN whose
role I could not confirm, so a token first presented to `migrated` must not
depend on `migrated` being able to write. Under the fallback it does not.

### 4.2 Write - control only, once per principal

SHIPPED, AND NOT UNDER THIS NAME. `ensure_platform_creator_grants` never
existed in the tree under that spelling, so this instruction was already
describing a module that had changed under it. What carries the marker
semantics is `zeroship_authn::platform_cli::materialize_default_grants`, called
from `crates/zeroship-control/src/authz_guard.rs` and
`crates/zeroship-migrate-server/src/auth.rs`. `identity_bridge` itself is
DELETED - it had test callers only. Read the rest of this section as the
reasoning behind that outcome, never as a live instruction.

Keep the marker semantics exactly as they are (section 2a). The CALL SITE moved
from control's `/api/device/token` to control's single bearer convergence point,
`crates/zeroship-control/src/authz_guard.rs`.

Why that site: once login leaves control, a platform-native principal's first
bearer request is the first and only moment control sees it. There is no
earlier hook (section 2), and there is no operator API to hang it off
(section 2).

Why it is not a per-request write: the marker guard means the INSERT is
attempted once per principal and the steady state is pure SELECT.

### 4.3 The objection I am overriding, named

`crates/zeroship-control/src/identity_bridge.rs:3-4` says JIT provisioning "is
deliberately a device-approval write path, not a bearer authz read path". I am
contradicting that sentence and must say why rather than quietly edit it:

it was written when control's device approval existed as a moment control
actually saw the principal. After `5ae8c7f7d` that moment is gone for every
platform-native creator, so the choice is no longer "approval path vs authz
path", it is "authz path vs nowhere". The warning's substance - do not put a
write in a hot read path - survives via the marker guard. The header gets
updated in the same commit; leaving it asserting the old rationale would be
exactly the stale-claim failure this repo keeps hitting.

### 4.4 What this does NOT do

- It does not delete control's device flow, the platform mint, or PATs
  (tasks #3 and #4).
- It adds no privilege to `zeroship_auth`. The auth side is unchanged.
- It adds no back-compat shim; the OAuthScope arm's old behaviour is replaced.

### 4.5 Consequence for the pinned tests - read this before judging the diff

Under E the OP's behaviour is unchanged by design: it still caps to the client
registration. So
`the_cli_device_grant_does_not_consult_stored_principal_grants` keeps its
assertion. What must change is its FRAMING - it is currently documented as a
"KNOWN GAP", and under E it is a deliberate coarse ceiling narrowed downstream.
Renaming and re-documenting it, plus a new control-side test that pins the
narrowing, is the honest inversion. Leaving the words "KNOWN GAP" on a
behaviour that is now intentional would be the same defect in the other
direction.

`tests/e2e_device_login.sh` inverts properly and materially: `:598-601`
currently asserts 0 grants after login, which under E becomes the full
provisioned set, and a new step must delete a row and assert the next control
request is narrowed.

---

## 5. Amendments made during implementation

Three things I got wrong or left open in sections 1-4, corrected here rather
than by editing them silently.

### 5.1 The privilege question, answered by reading rather than assuming

Section 4.1 asserted the read rule needs no new privilege. **VERIFIED** that it
does not, and the specific reason is stronger than "SELECT suffices":
`db/migrations-ts/20260702000900_grants.ts:65` lists BOTH `identity_links` and
`principal_grants` in the same `["select", "insert", "update", "delete"]` grant
to `zeroship_control`. So control already holds more than the read rule needs
AND exactly what the write in 4.2 needs.

**No grant is added by this change, to any role.** `zeroship_auth` is untouched
- it keeps SELECT-only on `principal_grants` (`:55`) and no privilege at all on
`identity_links`, which is what makes the auth side structurally incapable of
provisioning and therefore what forces the design.

### 5.2 Section 4.1's worry about `migrated` was unfounded

I wrote that `crates/zeroship-migrate-server`'s DB role was unverifiable and let that shape the
read/write split. It is verifiable: `deploy/compose/docker-compose.yml:431`
defaults `ZEROSHIP_MIGRATE_SERVER_DATABASE_URL` to
`postgres://zeroship_control:zeroship_control@postgres:5432/zeroship`. So the
reference deployment runs `migrated` as `zeroship_control` and the read is safe
there for the same reason it is safe in control.

The split still stands, but on its real merit rather than that one: the
unseeded fallback means a service that cannot write is never blocked, so the
write can stay in the one service that indisputably owns the table instead of
being duplicated into the shared crate.

### 5.3 The intersection is scoped to the CLI client - a point section 4 missed

Section 4.1 as written would have capped EVERY OAuth token to
`principal_grants`. That is wrong, and it would have broken more than it fixed:
`PLATFORM_CLI_ISSUABLE_SCOPES` is five scopes, so once a principal is seeded,
any token needing `env:write`, `billing:read`, `team:write` or `account:*` -
the console's surface, not the CLI's - would be silently narrowed to nothing.
The constant's own doc comment in `crates/zeroship-core/src/device_grant.rs:55-58` says
what it is: the ceiling registered for the FIRST-PARTY CLI CLIENT.

So the intersection runs only when `client_id == PLATFORM_CLI_CLIENT_ID`. That
is also the faithful restoration rather than a widening: the behaviour
`5ae8c7f7d` removed was `deploy_scopes_for_principal`, which only ever ran on
control's CLI device flow. Restoring exactly that and nothing more is the
smaller and better-supported change.

The archive lifecycle adds `apps:archive` to that CLI ceiling. The
`viewer_role_cannot_use_granted_apps_archive_scope` test still seeds the grant
explicitly so its 403 measures the app-member role check instead of depending
on just-in-time default-grant materialization.

---

## 6. Verification

### 6.1 Results

Every target the brief names, plus the red proof.

| Gate | Target | Result |
| --- | --- | --- |
| `tests/run_auth_suite.sh` | 647, floor 595 | **647 passed**, 0 unexpected skips, 1 allowlisted |
| `cargo test -p zeroship-auth --lib` | 221 | **221 passed** |
| `cargo test -p zeroship-control --lib` | 224 | **224 passed** |
| `cargo test -p zeroship` | 98 across 6 targets | **99 across 6 targets** |
| `authz_guard_oauth_test` (live-db-tests) | 17 | **19 passed** (17 + the 2 added) |
| `tests/commit_msg_gate.sh --range main..HEAD` | 0 rejected | **0 rejected**, 5 checked |

The `zeroship` crate is 99 rather than 98: 71 + 1 + 12 + 8 + 3 + 4 over
`main.rs`, `deploy_test`, `dev_init_test`, `login_test`, `parent_death_test`,
`secrets_test`. Six targets, none missing, and the delta is one MORE test than
the brief expected - main moved to `e510933d2` (secret-file permissions) after
the brief was written.

`tests/e2e_device_login.sh` is edited but NOT run here: it needs the full
compose stack. Its assertions are stated in section 4.5.

### 6.2 The red proof, and why it nearly did not happen

Both new tests were confirmed to FAIL without the fix, by reverting
`crates/zeroship-authn/src/lib.rs`, `crates/zeroship-control/src/authz_guard.rs` and
`crates/zeroship-control/src/device_handlers.rs` and re-running the same binary:

```
17 passed; 2 failed        (fix reverted)
19 passed; 0 failed        (fix applied)
```

- `an_operator_deleting_a_grant_row_narrows_the_next_cli_request`: got 200,
  expected 403. That IS the regression, reproduced.
- `a_first_cli_request_is_authorized_and_materializes_the_default_grants`: got
  `[]`, expected the four defaults.

Worth recording: in that reverted run the FIRST assertion of the second test -
the deploy-check returning 200 - passed anyway. It has to. Old behaviour also
authorized that request, just without narrowing anything ever after. A test
asserting only "the first request works" would have been green on both sides of
the fix and proved nothing.

My first two attempts at a red run never compiled (missing submodule sources,
then missing `sdks/*/dist`), and both exited non-zero with no test output. Had I
run the fix first and skipped the control, I would have had a green with no
evidence the tests could ever fail.

### 6.3 A corrupted incremental cache fakes a broad, plausible failure

The first full auth-suite run reported `only 80 auth tests passed` with
FAILURES in `zeroship-authz` and `zeroship-gateway` - crates this change cannot
reach. The cause was not the code:

```
error: failed to move dependency graph from .../incremental/zeroship_auth-.../dep-graph.part.bin
error: could not compile `zeroship-auth` (lib) due to 2 previous errors
```

`zeroship-auth` failed to BUILD, so its whole test set silently vanished from
the total, and the unrelated crates failed for their own cache reasons.
`rm -rf target/debug/incremental` and re-running gave 647/0. Nothing about the
80 pointed at the cache; it read exactly like a real, broad regression.

The structural check that settled it before the re-run: `zeroship-authn` is
named in only two `Cargo.toml` files (`crates/control`, `crates/zeroship-migrate-server`), and
neither `crates/gateway` nor `crates/authz` depends on `authn` or `control`. So
those failures were not reachable from this diff whatever their cause.
