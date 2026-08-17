# Restoring principal-grant narrowing after CLI login moved onto the OP

Scratch notes for `fix/op-grant-narrowing`. Diagnosis first, then the
provisioning recommendation the task turns on, then implementation.

Every claim below is labelled:

- **VERIFIED** - I read the cited lines in this worktree.
- **INFERRED** - drawn from verified facts, not itself read.

---

## 1. The regression, re-verified

The brief's account is accurate. Confirming each leg independently:

- **VERIFIED** `crates/control/src/device_handlers.rs:665-690`,
  `deploy_scopes_for_principal` selects `grant_name` from
  `zeroship.principal_grants` for the principal and intersects it with the
  requested scopes. `crates/control/src/device_handlers.rs:638-663` calls
  `ensure_platform_creator_grants` first, so the intersection has something to
  intersect with.
- **VERIFIED** `crates/auth/src/oidc/device_token.rs:680-706`, the OP device
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

**VERIFIED** `crates/authn/src/lib.rs:388-421`. Control's bearer verification
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
   (**VERIFIED** `crates/auth/src/store/users.rs:127`,
   `crates/auth/src/identity/linker.rs:436,503,560,613`,
   `crates/auth/src/oidc/authorization_code.rs:1526`). This is what kills the
   brief's second candidate outright, not just makes it awkward.

**VERIFIED** there is no auth-to-control call path to hang provisioning on:
`control_url` in auth is used only to render the Supabase approve URL into the
device page (`crates/auth/src/ui/device.rs:475`). `ControlEvent` has no auth
producer (`grep -rln ControlEvent crates/` returns only `core` and `control`).

**VERIFIED** there is no operator API for grants at all. `grep principal_grants`
over `crates/control/src/` outside `device_handlers.rs` and
`identity_bridge.rs` is empty. The operator's vehicle is SQL against the table,
which matters for section 4: any design where the operator has no rows to
delete gives the operator no narrowing surface at all.

### 2a. The marker semantics that must survive any move

**VERIFIED** `crates/control/src/identity_bridge.rs:63-71,89-107`.
`ensure_platform_creator_grants` keys "already provisioned" off the existence
of an `identity_links` row, NOT off the grant count, precisely so that
"operator revoked everything" is distinguishable from "never provisioned" and a
re-login does not resurrect revoked grants. Pinned by
`crates/control/tests/device_handlers_test.rs:2440+`,
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
  (`crates/authn/src/lib.rs:460`), so this is the same query on the other arm.
- Narrowing becomes IMMEDIATE, not next-login. An operator's DELETE narrows the
  token already in the creator's hand, not merely the next one. That is
  strictly stronger than what commit `5ae8c7f7d` removed.
- The unmarked fallback is what stops a first login minting empty authority,
  and it is not a second source of truth: the fallback set is exactly the set
  section 4.2 materializes, so the two never disagree.

The unmarked fallback also makes the rule safe for `authn`'s other consumer.
`crates/migrated` uses the same `BearerVerifier`
(`crates/migrated/src/main.rs:167`) against a deployment-configured DSN whose
role I could not confirm, so a token first presented to `migrated` must not
depend on `migrated` being able to write. Under the fallback it does not.

### 4.2 Write - control only, once per principal

Keep `identity_bridge::ensure_platform_creator_grants` exactly as it is, marker
semantics and all (section 2a). Move only its CALL SITE, from control's
`/api/device/token` to control's single bearer convergence point,
`crates/control/src/authz_guard.rs:173`.

Why that site: once login leaves control, a platform-native principal's first
bearer request is the first and only moment control sees it. There is no
earlier hook (section 2), and there is no operator API to hang it off
(section 2).

Why it is not a per-request write: the marker guard means the INSERT is
attempted once per principal and the steady state is pure SELECT.

### 4.3 The objection I am overriding, named

`crates/control/src/identity_bridge.rs:3-4` says JIT provisioning "is
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
