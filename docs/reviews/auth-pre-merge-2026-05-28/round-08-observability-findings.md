# Round 8 — Observability and audit trail: findings

Total: 16 findings (2 critical, 6 high, 5 medium, 3 low).

Scope walked: every `audit::emit` / `audit::emit_strict` / `audit::log` /
`emit_revocation_audit` / `audit_event` call site in `crates/auth`, `crates/control`,
`crates/gateway`, `crates/sandbox-agent`; the `auth.audit_events` and `app_audit`
table definitions and indexes (`crates/auth/src/store/migrations.rs:122-136`,
`crates/control/src/registry.rs:317-344`); the `control.authz_decisions` table
writes (`crates/authz/src/eval.rs:222-255`) plus all `AuthzGuard::require`
call sites; every `tracing::*` macro call in the auth + control + gateway
crates for PII / log-injection vectors; subscriber init in
`crates/core/src/observability.rs`; rate-limit denial paths in
`crates/auth/src/ui/*` and the gateway/control rate-limiters; health endpoints
on control (`/health`) and gateway (`/health`); Stripe webhook + Postmark
webhook handlers as inbound-state-change audit surfaces.

Inventory (69 audit::emit call sites, 0 of which populate ip/user_agent/request_id):

| Audit pipeline | Where | Sink | Stdout fan-out | Strict variant |
| --- | --- | --- | --- | --- |
| `auth::audit::emit` / `emit_strict` | `crates/auth/src/audit.rs` | `auth.audit_events` (PG) | yes (`tracing target=auth.audit`) | yes |
| `control::audit::log` (Action enum) | `crates/control/src/audit.rs` | `app_audit` (PG) | no | no |
| `control::admin_handlers::audit_event` | `crates/control/src/admin_handlers.rs:595-628` | `auth.audit_events` (PG, raw INSERT) | no | no |
| `control::backchannel_logout::emit_revocation_audit` | `crates/control/src/backchannel_logout.rs:185-217` | `auth.audit_events` (PG, raw INSERT) | no | no |
| `authz::audit_decision` | `crates/authz/src/eval.rs:222-255` | `control.authz_decisions` (PG) | no | no |
| `sandbox-agent::audit::record` | `crates/sandbox-agent/src/audit.rs:67-69` | `tracing target=audit` only | yes | n/a |

**Headline gap:** four parallel audit-insertion paths, no unified contract.
Three of the four bypass the `auth.audit` stdout-fan-out side channel that
SIEM ingestion depends on (per `audit.rs:4` "stdout JSON for SIEM ingestion").
Zero out of 69 `audit::emit` call sites populate `ip`, `user_agent`, or
`request_id` — the schema columns are there but always NULL on insert.

---

## CRITICAL

### C1. `auth.audit_events` is plain BIGSERIAL with no tamper-evidence and no GRANT/REVOKE — any actor with the auth-PG role can erase their tracks

**File:** `crates/auth/src/store/migrations.rs:122-136` (table + indexes);
no `REVOKE` / `TRIGGER` / `RULE` anywhere in the file (grep confirms zero
hits for `REVOKE`, `GRANT`, `TRIGGER`, `RULE` across all auth migrations).

**Severity rationale:** Every component of the auth stack — including the
ntex HTTP handlers, the cron sweeper, the test fixtures, and the hydra
sidecar — connects with one PG superuser/role. The audit_events table is a
plain heap with `BIGSERIAL PRIMARY KEY` and two B-tree indexes; nothing
forbids `DELETE FROM auth.audit_events`, `UPDATE auth.audit_events SET
detail = ...`, or `TRUNCATE auth.audit_events`. There is:

- no `BEFORE UPDATE OR DELETE` trigger that raises an exception
- no `REVOKE DELETE, UPDATE, TRUNCATE ON auth.audit_events FROM PUBLIC`
- no row-level `prev_hash`/`row_hash`/Merkle/signed-row chain (the columns
  don't exist)
- no `WORM` / partition-attach pattern (the proposal §15 line in
  `audit.rs:4` says "for query/retention" only)
- no out-of-band copy (the stdout fan-out goes to the same process's
  stderr/stdout — a compromise that owns the process owns both sinks).

An attacker who escalates to the runtime user (a real risk given that the
ntex handlers, the Postmark webhook, and Stripe webhook all run as the
same Unix user with the same PG creds) can simply
`DELETE FROM auth.audit_events WHERE user_id = $compromised_uid` and
the audit trail of their own account takeover disappears with no
detection signal. Cf. NIST 800-53 AU-9, ISO 27001 A.12.4.2.

**Reproducer:**
```sql
-- runs successfully under the auth role today:
DELETE FROM auth.audit_events WHERE user_id = '00000000-0000-0000-0000-000000000001';
UPDATE auth.audit_events SET outcome = 'success' WHERE event_type = 'login_failure';
TRUNCATE auth.audit_events;
```

**Fix:** at minimum (still pre-launch, cheap):

1. Add to the migration immediately after `CREATE TABLE auth.audit_events`:
   ```sql
   ALTER TABLE auth.audit_events ADD COLUMN prev_hash BYTEA;
   ALTER TABLE auth.audit_events ADD COLUMN row_hash BYTEA NOT NULL DEFAULT '\x00';
   CREATE OR REPLACE FUNCTION auth.audit_chain() RETURNS trigger AS $$
   DECLARE
       last_hash BYTEA;
   BEGIN
       SELECT row_hash INTO last_hash FROM auth.audit_events
         ORDER BY id DESC LIMIT 1;
       NEW.prev_hash := COALESCE(last_hash, '\x00');
       NEW.row_hash := digest(
           NEW.prev_hash::text || NEW.event_type || NEW.outcome ||
           COALESCE(NEW.user_id::text, '') || COALESCE(NEW.detail::text, ''),
           'sha256');
       RETURN NEW;
   END $$ LANGUAGE plpgsql;
   CREATE TRIGGER audit_chain_before_insert BEFORE INSERT ON auth.audit_events
     FOR EACH ROW EXECUTE FUNCTION auth.audit_chain();
   CREATE OR REPLACE FUNCTION auth.audit_block_tamper() RETURNS trigger AS $$
   BEGIN
       RAISE EXCEPTION 'audit_events is append-only';
   END $$ LANGUAGE plpgsql;
   CREATE TRIGGER audit_block_update BEFORE UPDATE ON auth.audit_events
     FOR EACH ROW EXECUTE FUNCTION auth.audit_block_tamper();
   CREATE TRIGGER audit_block_delete BEFORE DELETE ON auth.audit_events
     FOR EACH ROW EXECUTE FUNCTION auth.audit_block_tamper();
   ```
2. Add a daily chain-verification cron in `crates/auth/src/cron/` that
   reads the last N rows and asserts the hash chain.
3. Long-term: dual-write to an external append-only store (S3 with
   object-lock, or a separate PG with a non-superuser writer role).

Same finding applies to `app_audit` (`crates/control/src/registry.rs:317-344`)
and `control.authz_decisions` (`crates/authz` migrations).

---

### C2. `control::audit::log` records `actor: "admin"` as a literal string instead of the actual `AuthzGuard.principal_id`

**File:** `crates/control/src/env_handlers.rs:108-117, 140-149, 197-206,
279-288, 360-369`; `crates/control/src/stripe_handlers.rs:118-125, 189-196,
207-216`; supporting type in `crates/control/src/audit.rs:46-83` (`actor:
&'a str`); the docstring at `audit.rs:9-10` acknowledges this gap
("`actor` today is always `"admin"` (we don't have multi-actor auth on
the master key yet). When that lands, plumb the user identity in.").

**Severity rationale:** Every var/secret mutation, every Stripe account
link/unlink, and every payout-record action writes a row to `app_audit`
with `actor = "admin"` regardless of which platform admin / PAT / OAuth
token initiated the action. The `AuthzGuard` already carries
`principal_id: Uuid` and `token_id: Option<Uuid>` (verified in
`crates/control/src/authz_guard.rs:14-21`) — both are dropped on the
floor before `audit::log` is called. Concrete failure scenarios this
hides:

- Admin A leaks `STRIPE_LIVE_KEY` and blames admin B. The audit table
  literally cannot answer "which admin did it" because every row says
  "admin".
- A PAT with `BillingWrite` is stolen; the attacker links a Stripe
  account they control via `POST /creators/{id}/stripe/callback`. The
  audit row says `actor=admin`, hiding which PAT was abused.
- The OAuth `BillingWrite` flow (introspected via `oauth_guard_from_bearer`
  in `authz_guard.rs:190-233`) attributes the action to "admin" with no
  `token_id` or scope trace.

Pairs catastrophically with C1: an attacker can both (a) be invisible
in the actor column and (b) delete the row anyway.

**Reproducer:**
```bash
# Two distinct admins both end up indistinguishable in app_audit:
curl -X POST -u admin_A:pat_AAA control/v1/apps/$APP/env/vars \
  -d '{"key":"FOO","value":"1"}'
curl -X POST -u admin_B:pat_BBB control/v1/apps/$APP/env/vars \
  -d '{"key":"FOO","value":"2"}'

psql -c "SELECT actor, action, resource, at FROM app_audit WHERE app_id='$APP' ORDER BY at DESC LIMIT 2"
# actor | action  | resource | at
# admin | set_var | FOO      | 2026-05-28 ...
# admin | set_var | FOO      | 2026-05-28 ...
```

**Fix:**

1. Promote `actor` in `AuditEntry` to a structured shape:
   ```rust
   pub struct AuditEntry<'a> {
       pub principal_id: Option<Uuid>,
       pub token_id: Option<Uuid>,
       pub auth_method: &'a str,        // "console_session" | "pat" | "oauth"
       pub app_id: Option<Uuid>,
       pub creator_id: Option<Uuid>,
       pub action: Action,
       pub resource: Option<&'a str>,
       pub source_ip: Option<&'a str>,
       pub user_agent: Option<&'a str>,
       pub request_id: Option<&'a str>,
   }
   ```
2. Migrate `app_audit` to add `principal_id UUID`, `token_id UUID`,
   `auth_method TEXT`, `user_agent TEXT`, `request_id TEXT`, then
   `DROP COLUMN actor` (pre-launch — no back-compat shim, per
   `AGENTS.md`).
3. Update every call site to forward `guard.principal_id`,
   `guard.token_id`, and the request headers. The `request_ip` already
   exists on `AuthzGuard`; thread it through instead of re-extracting
   via `source_ip(&req, &state)`.

---

## HIGH

### H1. Zero of 69 `audit::emit` call sites populate `ip`, `user_agent`, or `request_id` — the columns exist but are always NULL

**File:** `crates/auth/src/audit.rs:13-23` (struct), schema at
`crates/auth/src/store/migrations.rs:129-132` (`request_id TEXT`,
`ip INET`, `user_agent TEXT`); confirmed by:
```
$ grep -rn "ip: Some\|user_agent: Some\|request_id: Some" crates/auth/ --include="*.rs"
# (no output — zero occurrences)
```

Concrete call sites that bind an IP for rate-limit purposes but never
forward it to the audit row immediately after:

- `crates/auth/src/ui/login.rs:236-246` extracts `ip` for buckets, then
  emits `event_type: "login_failure"` at line 251 with `..Default::default()`
- `crates/auth/src/ui/signup.rs:129-153` (same shape, signup_throttled)
- `crates/auth/src/ui/forgot.rs:86-89, 102-111, 166-175` (forgot)
- `crates/auth/src/ui/magic.rs:179-180, 199, 772-773, 786` (magic)
- `crates/auth/src/ui/link.rs:186-187, 201` (account link)

**Severity rationale:** Forensics on a credential-stuffing or
account-takeover incident becomes guesswork. We can see "100 login
failures for `alice@…`" but cannot tell whether they came from one IP
(brute-force from a single host — block the IP) or 100 IPs
(distributed credential-stuffing — different mitigation). The
`user_agent` and `request_id` columns are equally always-NULL, which
makes correlation with downstream worker logs (which DO receive
`X-Request-Id`, per `crates/gateway/src/proxy.rs:393`) impossible.
Per proposal §15 and the schema definition itself, those columns are
load-bearing for SIEM correlation — they shipped empty.

The auth service does not install any request-id middleware. The
only place a request_id exists in auth is `AuditEvent.request_id`,
which no handler populates. Gateway emits `X-Request-Id` per
`router/dispatch.rs:1142`, but auth-service handlers never read it
from request headers either.

**Fix:**

1. Add `crates/auth/src/headers.rs::RequestContext` middleware that
   extracts `(ip, user_agent, x_request_id_or_generate)` once per
   request and stows them in `HttpRequest::extensions_mut()`.
2. Add `audit::AuditEvent::from_req(req: &HttpRequest)` builder so
   every call site reads
   `AuditEvent::from_req(&req).event_type(...).user_id(...).detail(...)`.
3. Mass-update the 67 in-tree `audit::emit` call sites to use the
   builder. Failing to populate IP should be impossible by
   construction, not by convention.
4. For the strict path (password reset, role grant), require IP via
   a typestate or compile-time check — `emit_strict` with
   `ip: None` should not compile.

---

### H2. PAT mint, PAT revoke, OAuth client registration, OAuth client deletion, OAuth grant revocation, and console session create emit no audit row at all

**Files:**

- `crates/control/src/token_handlers.rs:208-285` (`create_token` — PAT
  mint) and `:325-365` (`delete_token` — PAT revoke). `grep -c "audit"`
  in this file: 0.
- `crates/control/src/oauth_handlers.rs` (entire file — OAuth client
  CRUD with platform-admin scope). `grep -c "audit\|emit"`: 0.
- `crates/control/src/oauth_grants_handlers.rs:83-143`
  (`revoke_grant` — deletes `control.oauth_grants` and revokes hydra
  tokens). No audit emit.
- `crates/control/src/console_sessions.rs:64` (`INSERT INTO
  auth.console_sessions`). No audit emit at create time. `grep -c
  "audit\|emit"`: 1 occurrence which is unrelated (a comment).
- `crates/auth/src/ui/consent.rs:130-198` (`post_consent_accept` —
  records the OAuth scope grant in `control.oauth_grants` and tells
  hydra to mint tokens). `grep -c "audit::emit"`: 0.
- `crates/auth/src/ui/consent.rs:203-231` (`post_consent_deny`). Same.
- `crates/auth/src/ui/device.rs` (entire device-flow handler).
  `grep -c "audit\|emit"`: 0.

**Severity rationale:** These are exactly the events proposal §15
labels as CRITICAL ("issuance of any credential, revocation, scope
change, consent decision"). Today, an attacker who steals a console
session and mints a PAT with `BillingRead` + `BillingWrite` →
`AccountWrite` policies, lists / refreshes / revokes existing PATs,
and registers a new OAuth client they control as a phishing landing,
leaves zero audit rows in `auth.audit_events` or `app_audit` (the
`control.permission_tokens` and `control.oauth_clients` rows are the
only trace, and those tables are mutable). The `control.authz_decisions`
table will have a `decision=allow` row for the PATs/OAuth-client
action — but it carries the cedar action identifier (e.g.
`Action::AccountWrite`), not the application semantics
("a PAT was minted with policy X"). Forensics cannot reconstruct
"what PAT names were minted and revoked" from the audit table.

**Fix:**

1. Add audit emits at the obvious points (success and failure
   branches both). PAT mint:
   ```rust
   audit::emit(&state.auth_pg, &AuditEvent {
       event_type: "pat_minted",
       outcome: "success",
       user_id: Some(&guard.principal_id),
       request_id: Some(&request_id),
       ip: guard.request_ip,
       user_agent: ua_header,
       auth_method: Some("console_session"),
       detail: json!({
           "token_id": token_id,
           "name": name,
           "expires_at": expires_at,
           "policy_hash": policy_hash,
       }),
   }).await;
   ```
2. Add `pat_revoked`, `oauth_client_registered`, `oauth_client_deleted`,
   `oauth_grant_revoked`, `console_session_created`,
   `consent_accepted`, `consent_denied`, `device_code_authorized`.
3. Use `emit_strict` for the mint paths so a PG failure surfaces as a
   request error — credential issuance without an audit row is the
   exact case the strict variant was designed for (per `audit.rs:62-68`).

---

### H3. Stripe webhook `record_payout` writes money-movement to `stripe_payouts` but emits no audit row

**File:** `crates/control/src/stripe_handlers.rs:371-484` (the `webhook`
handler). The `Ok(rec)` arm at line 474 returns HTTP 200 with the new
payout id; no `audit::log` call. The `Duplicate` arm and the error
arm also emit nothing.

**Severity rationale:** Inbound state changes that move money on the
platform have weaker audit coverage than outbound admin actions
(`link_account` / `unlink_account` both audit). A compromised Stripe
account or a Stripe-side webhook replay (despite the HMAC) is the
exact scenario where ops needs an "I see a 200 OK was returned at
T+0 with creator_id=X, payload_hash=Y, gross=Z" entry. Today there is
only the `stripe_payouts` row (mutable, no chain) and the tracing
log line (line 440 only emits on the missing-creator-id branch).

**Severity rationale (additive):** `app_audit` has a `RecordPayout`
variant (`crates/control/src/audit.rs:28`) — the enum was authored
for this case. The call site was never added.

**Fix:** add an `audit::log(state.registry, AuditEntry { action:
Action::RecordPayout, app_id: None, creator_id: Some(creator_id),
resource: Some(&event.id), source_ip: None, ... })` in both the `Ok`
and the `Duplicate` arms (the latter is a useful signal that someone
is replaying). Include the `payload_hash` in a `detail: JSONB` column
(needs `app_audit` migration to add that column).

---

### H4. `AuthzGuard::require` drops `request_id` on the floor — the `control.authz_decisions` row is therefore decorrelated from the rest of the request trace

**File:** `crates/control/src/authz_guard.rs:64-75`. The `AuthzContext`
literal sets `request_id: None` (line 74). The authz crate at
`crates/authz/src/eval.rs:30, 222-255` plumbs the field all the way
to `INSERT INTO control.authz_decisions ... request_id` (line 237),
so the column exists, but the call site never populates it. Same
shape at `crates/control/src/token_handlers.rs:416`
(`is_authorized_anywhere` call) — `request_id: None`.

**Severity rationale:** A `decision=deny` row in `control.authz_decisions`
cannot be joined to the originating HTTP request, the upstream tracing
span, or any future `auth.audit_events` row that ends up carrying a
request_id (per H1 fix). The proposal §15 ADR for `authz_decisions`
explicitly calls out request-id correlation as the primary forensics
join key. The column is shipped but always NULL — same failure mode
as H1.

**Fix:** read `X-Request-Id` (or generate a UUIDv4 if absent) in the
control AuthzGuard `from_request` impl, stash it on the guard, and
forward it into `AuthzContext.request_id`. Same change at the
`is_authorized_anywhere` call site in `token_handlers.rs:416`. Same
change in any other `AuthzContext { request_id: None, ... }` literal
across the workspace.

---

### H5. Three parallel raw `INSERT INTO auth.audit_events` paths bypass `audit::emit`/`emit_strict`'s stdout SIEM fan-out

**Files:**

- `crates/control/src/admin_handlers.rs:595-628`
  (`fn audit_event` — used by `grant_platform_role`,
  `revoke_platform_role`, `set_app_audit_lock`, and at least one more
  admin handler).
- `crates/control/src/backchannel_logout.rs:185-217`
  (`fn emit_revocation_audit`).
- `crates/control/src/audit.rs:58-83` (`fn log` — writes to
  `app_audit`, not `auth.audit_events`, but also no stdout fan-out).

**Severity rationale:** `crates/auth/src/audit.rs:27-58` documents the
contract: "PG `auth.audit_events` (for query/retention) AND stdout JSON
(for SIEM ingestion, per proposal §15)." The three raw INSERT paths
write to PG but emit nothing on stdout. If a SIEM ingest pipeline
relies on the stderr/stdout JSON stream (the documented surface), the
following CRITICAL events are silently dropped from SIEM:

- `platform_role_granted` (admin role bestowal)
- `platform_role_revoked` (admin role removal)
- `app_audit_lock_updated`
- `backchannel_logout_revoke` (mass session revocation)
- every var/secret mutation, every Stripe link/unlink/payout

Per `AGENTS.md`'s "Native primitives are the kernel … small and stable
on purpose" rule, having four audit-emission paths is anti-pattern by
the project's own standards.

**Fix:** delete `audit_event` in `admin_handlers.rs` and
`emit_revocation_audit` in `backchannel_logout.rs`. Replace with
`audit::emit_strict` for these CRITICAL events. Migrate
`control/src/audit.rs` to share the `auth::audit::AuditEvent` shape and
go through the same `emit`/`emit_strict` path — both audit tables can
coexist behind one function (it's just the target table that changes).
Per `AGENTS.md` "no back-compat shims", do the table consolidation in
one PR.

---

### H6. `webhooks.rs:131` logs the bouncing email address verbatim to tracing — PII leak into pretty-mode logs / log aggregators

**File:** `crates/auth/src/ui/webhooks.rs:104, 131, 142, 283, 315`.
Examples:
```rust
tracing::info!(email = %b.email, kind = %b.r#type, "postmark soft bounce");
tracing::error!(error = %e, email = %b.email, "suppression add failed");
tracing::error!(error = %e, email = %rec.email_address, "...");
```

Compare with the same file's audit-emit path at line 107-126: the
*audit detail* explicitly drops to `email_domain` ("Audit detail uses
email_domain only … to keep PII out of the audit stream"). The
parallel `tracing::info!` at line 131 then emits the full address
into the same process's stdout — defeating the same-process PII
goal. Same pattern at `crates/auth/src/ui/magic.rs:276`:
```rust
tracing::warn!(error = %e, email = %email_norm, "magic_link email send failed");
```

**Severity rationale:** Pretty-mode logs (default on TTY per
`crates/core/src/observability.rs:36-42`) and any non-JSON log
aggregator that mirrors stdout (e.g. journald → syslog) will retain
the email indefinitely. The audit stream's PII discipline becomes
moot because the log stream alongside it leaks the same field. This
also breaks the "right to be forgotten" story — a GDPR deletion
request cannot scrub aggregated log files holding the email in a
`postmark soft bounce` line.

**Fix:** replace `email = %x.email` with `email_hash =
%hex::encode(Sha256::digest(x.email.as_bytes()))` and `email_domain =
%x.email.split('@').nth(1).unwrap_or("")` in all five sites. Reuse
the `email_hash` convention already in `forgot.rs:85`. For magic
link send failures, replace `email = %email_norm` with `user_id =
%user.id` if a user lookup happened, else `email_hash`.

---

## MEDIUM

### M1. `auth.audit_events` schema lacks `outcome` enum constraint and `event_type` enum constraint — typos pass silently

**File:** `crates/auth/src/store/migrations.rs:122-134`. Both
`event_type TEXT` and `outcome TEXT` accept any string. Compare to
`auth.identity_link_status` which (per proposal) is a strict enum
elsewhere.

**Severity rationale:** A typo at a call site (`outcome: "succes"`,
`event_type: "login_falure"`) silently inserts the malformed string.
Downstream queries (`WHERE event_type = 'login_failure'`) miss the
row, and the SIEM rule "alert on 5 login_failure in 60s" misses the
attack. Today there are 67 call sites — manual auditing for typos is
not scalable. The Rust call sites use `&'static str` literals which
helps in-tree, but only `cargo grep` would catch a typo introduced by
another contributor.

**Reproducer:** `audit::emit(..., AuditEvent { event_type:
"login_failuer", outcome: "failure", ...})` inserts fine; the SIEM
alert never fires.

**Fix:** add `CHECK (outcome IN ('success', 'failure'))` and a Rust
`enum EventType` with `as_str()` (mirror the
`crates/control/src/audit.rs::Action` enum pattern). The DB-level
CHECK catches contributors who write raw INSERT (per H5). Long-term:
move event types to a small `event_type` lookup table referenced by
FK.

---

### M2. Health endpoints leak no info today, but no `/readyz` distinguishes "not ready" from "broken"

**Files:** `crates/control/src/internal.rs:50-52` (control `/health`);
`crates/gateway/src/main.rs:250-252` (gateway `/health`); both return
hard-coded `{"status":"ok"}` with no DB roundtrip, no upstream
hydra reachability check, no clock-skew check.

**Severity rationale:** Liveness vs readiness collapse. K8s / Nomad
will treat the process as healthy even if (a) PG is unreachable,
(b) hydra-admin is down (the auth service cannot accept_login), or
(c) the wrapper-token signing key is wrong. The compromise direction
is "false ok" not "info leak" — but a `/readyz` that PINGs PG plus
hydra would let the orchestrator drain the pod proactively. Compare
sandbox-agent which does split `/livez` (`crates/sandbox-agent/src/main.rs:178`)
from `/readyz` (`:172`) and has documented semantics in
`crates/sandbox-agent/src/metrics.rs:23`.

Inverse risk (info leak) is currently zero on control + gateway —
both return a static body. Do not regress that when adding `/readyz`.

**Fix:** add a `/readyz` to control + gateway that runs:
- `SELECT 1` on `auth_pg` with a 200ms timeout
- HEAD on `hydra_admin_url/health/ready` (control only)
- check `state.master_key.is_some()` (control)
Return 503 with `{"ready": false, "reason": "<short>"}` — keep
the reason field a fixed enum so it cannot leak query plans / row
counts.

---

### M3. OAuth-callback failure audit details echo attacker-controlled query params verbatim into JSONB

**File:** `crates/auth/src/ui/oauth_google.rs:160-176` (and the
`oauth_github.rs` mirror).
```rust
detail: json!({
    "reason": "upstream_error",
    "error": err,
    "description": query.error_description,
}),
```
`err` and `query.error_description` come from
`?error=...&error_description=...` on the callback URL — fully
attacker-controlled. Similar at line 234-238: `"error": e.to_string()`
where `e` may carry a JWT-claim error message that includes
upstream-decoded fields.

**Severity rationale:** Stored XSS-into-audit-viewer is the canonical
risk — any future audit-events dashboard that does not HTML-escape
this JSONB blob renders attacker-controlled markup. Also a log
storage poisoning vector if a SIEM regex-parses the `description`
field. JSON-level escaping is correct here (Postgres JSONB and
serde_json both escape control bytes), but downstream consumers
typically read `detail->>'description'` as raw text.

**Fix:** cap `error` to `^[a-z_]{1,64}$` (OAuth 2.0 RFC 6749 §4.1.2.1
defines a closed enum: `access_denied`, `invalid_request`,
`server_error`, `temporarily_unavailable`, `unauthorized_client`,
`unsupported_response_type`, `invalid_scope`). Drop
`error_description` entirely from the audit detail (it's free-form
attacker prose); replace with a hash if forensics need it. Same
treatment for `e.to_string()` at line 237 — replace with a stable
error code from a fixed enum.

---

### M4. `logout` audit row carries `user_id: None` and stuffs the subject into `detail.subject` instead of the dedicated column

**File:** `crates/auth/src/ui/logout.rs:165-180`.
```rust
audit::emit(
    db.as_ref(),
    &AuditEvent {
        event_type: "logout",
        outcome: "success",
        user_id: None,                              // <-- field empty
        client_id: info.client.as_ref().map(|c| c.client_id.as_str()),
        auth_method: None,
        detail: serde_json::json!({
            "subject": subject,                    // <-- duplicated here
            "rp_initiated": info.rp_initiated,
        }),
        ..Default::default()
    },
)
```

**Severity rationale:** Queries like `SELECT * FROM auth.audit_events
WHERE user_id = $1 ORDER BY occurred_at DESC` miss the logout rows
for that user. The `auth_audit_user_idx` index
(`migrations.rs:135`) is rendered useless for the logout event_type;
a full scan + JSONB extract is needed. Forensics for a
"compromised-account timeline" miss the legitimate-user logout
entirely.

**Fix:** `Uuid::parse_str(&subject).ok().as_ref()` into `user_id`.
Drop the `subject` key from `detail` (it duplicates `user_id`).
Add a regression test that asserts `event_type = 'logout'` rows
have non-null `user_id` whenever `subject` is a valid UUID.

---

### M5. `signup` success path emits `verification_issued` but no `signup_success` / `account_created` event

**File:** `crates/auth/src/ui/signup.rs:173-269`. After
`users::create` succeeds, the only audit row emitted is
`verification_issued` at line 254 (and only if the verification token
issue itself succeeded — failures at line 266 emit nothing). The
`Err(e) if e.db_code() == Some("23505")` branch (duplicate email at
line 175) emits no audit either.

**Severity rationale:** "How many new accounts were created in the
last hour" is unanswerable from `auth.audit_events`. The
`verification_issued` row is a proxy but only fires if the verifier
issue + email send paths succeed. Detect-attacker-creating-100-accounts
fails. The signup_failed event (line 181) catches DB errors but not
the duplicate-email branch, which is the exact signal for
enumeration-via-signup.

**Fix:** add `event_type: "account_created"` immediately after the
successful `users::create` at line 174, with `user_id: Some(&user.id)`,
`auth_method: Some("password")`. Add `event_type:
"signup_duplicate_email"` (outcome: failure) on the 23505 branch
without including the email — `email_hash` only, per H6.

---

## LOW

### L1. `audit::emit_strict` swallows the stdout fan-out path's failure mode silently — its docstring promises propagation but only the PG arm propagates

**File:** `crates/auth/src/audit.rs:62-89`. The "strict" docstring
says "propagates PG insert errors instead of swallowing them. Use for
security-critical state transitions … where a missing audit row IS a
real correctness failure." But the implementation calls
`tracing::info!(target: "auth.audit", ...)` first (which is
non-fatal), then `store::insert`. The stdout JSON line goes out
regardless of subsequent failure, so a partial-failure where stdout
landed but PG INSERT errored leaves a SIEM record without a PG
counterpart. Two views of the same event diverge.

**Severity rationale:** Edge case (PG outages are rare and short),
but the strict-variant contract is exactly the case where divergence
matters. Probable noise more than risk.

**Fix:** call `store::insert` FIRST, then `tracing::info!` on success
only. Mirror in `emit` too. Add a comment explaining the ordering.

---

### L2. `payload = %stdout_payload` Display-formats a JSON Value into pretty-mode logs — newlines inside the detail are escaped by serde_json but not visually obvious

**File:** `crates/auth/src/audit.rs:40, 74`.
`tracing::info!(target: "auth.audit", payload = %stdout_payload, ...)`
uses `Display` (which is `serde_json::Value::to_string()`). serde_json
correctly escapes `\n`/`\r`/`\"`/control bytes, so log-injection via
attacker-controlled `detail` JSON is bounded. BUT in pretty mode
(default on TTY), the resulting one-line JSON blob is dropped into a
multi-line "pretty" frame, and an attacker who controls a string
field can include `[2J` (ANSI clear-screen) — serde_json does
NOT escape ESC by default. Tested in OAuth callback failure (M3):
`?error_description=%1B%5B2J` ends up as a literal ESC byte in the
pretty log frame.

**Severity rationale:** Affects only operators tailing pretty logs;
JSON / logfmt / bunyan modes (production default) are unaffected.
Low operational risk; mostly a "developer paper cut" / shoulder-surf
risk.

**Fix:** before forwarding `description` (or any attacker-controlled
string) into audit detail or tracing fields, strip control bytes
(`'\x00'..='\x1F'` and `'\x7F'`). Run a pass over all
`detail: json!({...})` literals.

---

### L3. `audit_events` `BIGSERIAL PRIMARY KEY` exposes monotonic insert volume to anyone with read access

**File:** `crates/auth/src/store/migrations.rs:122-134`. `id BIGSERIAL`
leaks the absolute volume of audit events from the side channel of
`MAX(id)`. Combined with `ORDER BY id DESC LIMIT 1` queries, a
non-super-user with `SELECT` access can poll the table and observe
issue/insert rate without seeing rows.

**Severity rationale:** Tiny information leak — only matters if the
audit-read role is split from audit-write (recommended in C1 fix but
not yet implemented). Pre-launch, every role has the same access.

**Fix:** when the C1 fix lands (chain + WORM), also switch
`BIGSERIAL` to `bigint GENERATED ALWAYS AS IDENTITY` and confirm the
chain forms the integrity proof, not the id.

---

## Subsystems audited but no findings

- `crates/sandbox-agent/src/audit.rs` — well-designed, ships only to
  `tracing target=audit`, deliberately not PG-backed; explicitly
  documents the log-injection caveat at line 64-66.
- `control.authz_decisions` schema (`crates/auth/tests/migrations_smoke.rs:33-105`)
  — verified the table is queryable, every required column is present,
  request_id column does exist. The only gaps are the call-site
  population gaps in H4.
- `crates/gateway/src/router/dispatch.rs:496` — request_id is correctly
  generated as `Uuid::new_v4()` per request and propagated as
  `X-Request-Id` to the worker upstream
  (`crates/gateway/src/proxy.rs:393`) and back to the client
  (`router/dispatch.rs:1143`). Within the gateway request path the
  request_id flows through, so the only missing leg is the auth
  service surface (H1, H4).

## Subsystems NOT fully audited (out of scope or insufficient depth)

- The plugin-db and plugin-kv layers — out of scope for an
  auth/observability review. Per-app audit-table provisioning
  (`__zeroship_audit_*`) was noted via grep (`crates/plugin-db/tests/
  sqlite_integration.rs:1536`) but not inspected.
- The metering and billing-metering pipeline
  (`crates/control/src/metering.rs`, `crates/platform/src/enforcement/
  quota.rs`) — only the rate-limit denial side was checked. Spend
  caps, the meter trait, and `metering` audit-coverage need a separate
  pass.
- The runtime-side `tracing` of `env.*` namespace calls
  (`plugin-db`, `plugin-storage`) — whether secrets/PII leak into
  worker logs is the right question for a per-namespace observability
  audit; out of scope here.
- The sandbox-agent's audit fan-out beyond
  `crates/sandbox-agent/src/audit.rs` (e.g. nomad/k3s backend audit
  trails) — only the central audit module was inspected.
- Wire-format JSON of audit-emit on stdout for non-JSON subscribers
  (`logfmt`, `bunyan`, `compact`): only `json` and `pretty` were
  reasoned about. The other formats may treat `payload = %value`
  differently.
- Cron/sweep handlers (`crates/auth/src/cron/`) for audit coverage
  on the GC actions they perform (e.g., wrapper-revoked-subjects
  sweep, jwk_rotation). `jwk_rotation.rs:234` logs to tracing only;
  no audit row for "key X was retired".
