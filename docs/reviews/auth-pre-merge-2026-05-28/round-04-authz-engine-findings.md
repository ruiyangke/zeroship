# Round 4 — Authz engine: findings

Total: 11 findings (2 critical, 4 high, 3 medium, 2 low).

## CRITICAL

### C1. Cedar policy injection through user-controlled `Policy.name`
**File:** `crates/authz/src/lower.rs:7-12` (lowering) and `crates/control/src/token_handlers.rs:27-32, 205-274` (consumer).
**Severity rationale:** A logged-in user can craft a Personal Access Token whose stored wrapper policy injects arbitrary Cedar `permit (principal, action, resource);` statements into the lowered source. The injected permits are evaluated as part of the token's policy set at every use, defeating the entire "narrow grant" model. The owner of the PAT could intentionally weaponize this to share a "limited" bearer with a CI worker / OAuth client / agent that ends up holding full-account authority. The user-facing label says "deploy only," the audit log records the requested action, and Cedar happily authorizes anything else.
**Reproducer:**
1. Authenticated console user POSTs to `/me/tokens`:
   ```json
   {
     "name": "deploy-prod",
     "policies": {
       "name": "deploy only\npermit (principal, action, resource);\n//",
       "statements": [{
         "effect": "allow",
         "actions": ["apps:read"],
         "resources": [{ "type": "any" }],
         "conditions": []
       }]
     },
     "expires_in_days": 30
   }
   ```
2. `validate_grant_subset` only invokes `enforce` with `token_policy = None`, so the wrapper is never lowered during validation — the bad name passes through unchecked.
3. The wrapper JSON is persisted in `control.permission_tokens.policies` and a `policy_hash` over the full JSON (including the malicious name) is stored. The JWT carries the same hash, so subsequent `policy_hash = $3` checks succeed.
4. When the PAT is used, `load_token_policies` → `policy_set_from_policy` → `lower(...)` emits:
   ```
   // Policy: deploy only
   permit (principal, action, resource);
   //
   permit (
     principal,
     action in [Action::"apps:read"],
     resource
   );
   ```
   Cedar parses two policies; the first is a universal allow. Any action on any resource is now permitted for the bearer.
5. Hand the bearer to a third party. They can call `DELETE /apps/{id}` (any app the principal_id is owner of, plus anything else admin/role grants).
**Suggested fix:** Reject `\n`, `\r`, `*/`, and any non-printable control characters in `Policy.name` at deserialize time (and `name` length cap). Stronger: stop interpolating `policy.name` into Cedar source entirely — emit a constant `// wrapper policy` header and carry the name as Cedar `@annotation("name", "...")` (which Cedar already escapes), or drop the comment line. Either way, add a property test that lowers an arbitrary `Policy { name }` and asserts `PolicySet::from_str` either rejects garbage or yields the same number of policies as `statements.len()`. The same hardening pass needs to cover `Resource::App.id` / `Resource::Org.id` (see H1).

### C2. `audit_locked` forbid policy is dead code — attribute is never set on the App entity
**File:** `policies/platform/audit_locked.cedar:7-8`, `crates/authz/src/entities.rs:207-211` (the producer that never emits the attr), `crates/auth/src/store/migrations.rs:252-279` (no schema column).
**Severity rationale:** The platform ships a `forbid` policy that gates writes on `resource.audit_locked == true`, but `app_entity` only emits `{"suspended": ...}`. Cedar's `resource has audit_locked` evaluates to `false`, so the forbid clause never fires. Ops believes they have a "freeze writes during investigation" lever; they do not. Any incident response that depends on flipping `audit_locked` (e.g., legal hold, security investigation) silently no-ops, and the static-policies test (`platform_policies_test.rs:80`) only passes because the test fixture manually emits the attribute that production code does not.
**Reproducer:**
1. Pretend Ops marks app X as audit-locked (no SQL column exists, but assume it did).
2. Caller hits `POST /apps/X/deploy` as the app owner.
3. `assemble_entities` builds `App::"X"` with only `suspended` attr (line 207-211: `HashMap::from([("suspended", ...)])`).
4. `audit_locked.cedar` evaluates `resource has audit_locked` → `false` → forbid clause does not match.
5. `app_owner.cedar` permits → ALLOW.
**Suggested fix:** Add an `audit_locked BOOLEAN NOT NULL DEFAULT FALSE` column (and `suspended` while we're there) to the `apps` table migration, query it in `load_app_suspended` (rename to `load_app_flags`), and emit `audit_locked` alongside `suspended` in `app_entity`. Add a `forbid_test` that flips the column and asserts a deploy is denied. Also delete the relation-fallback/missing-column swallow in `entities.rs:152-170` — pre-launch, the table either has the columns or the deploy is broken loud and early; "detect-and-warn" arms are explicitly forbidden per AGENTS.md.

## HIGH

### H1. Cedar source injection via `Resource::App.id` / `Resource::Org.id`
**File:** `crates/authz/src/resource.rs:13-19`, `crates/authz/src/lower.rs:59-89`.
**Severity rationale:** `Resource::cedar_uid()` builds `format!("App::\"{id}\"")` with **no escaping**, while the symmetric `entities::uid` helper goes through `cedar_string()` to escape `"`/`\`/`\n`. The unescaped form is the one that lands in the lowered Cedar source via `lower_resources`. A user who controls an app id (e.g., via the future custom-app-name flow, or any code path that lets `String` flow in) can break out of the Cedar string literal and inject Cedar tokens. Whether the injection ends in a *valid* Cedar program is craft-dependent, but even DoS (the wrapper fails to parse, so every subsequent token-policy load 500s) is a significant regression. Combined with C1, this widens the attack surface beyond `Policy.name`.
**Reproducer:**
- Create a PAT with a resource id like `x") || (resource is User) || (App::"`. `validate_grant_subset` will run `enforce` with `Resource::App { id: "x\") || ..." }`; the `build_request` path goes through `entities::uid` which DOES escape, so the *validation* request authorizes fine.
- At use time the SAME wrapper is fetched and `lower_resources` emits `resource == App::"x") || (resource is User) || (App::""` into the Cedar scope, producing either invalid Cedar (DoS for the user) or, if carefully crafted, an extra permit on a different entity type.
**Suggested fix:** Make `Resource::cedar_uid()` use the same `cedar_string()` escaping that `entities::uid` uses (deduplicate the helper). Add a fuzz target / property test that round-trips `Resource::App { id }` for arbitrary strings through `lower()` and asserts (a) `PolicySet::from_str` succeeds and (b) the parsed policy has exactly one statement whose resource scope refers to `App::"<id>"`. Pre-launch, also constrain `App.id` / `Org.id` at deserialize time to the typed-id alphabet (`[A-Za-z0-9_-]`) instead of `String`.

### H2. Consent grantor check denies creators their own scopes (`Resource::Any` mismatch)
**File:** `crates/auth/src/ui/consent.rs:371-409`, `crates/control/src/token_handlers.rs:364-416`, `crates/authz/src/eval.rs:135-141`.
**Severity rationale:** Both `grantor_can_grant_requested_scopes` (consent) and `validate_grant_subset` (PAT mint) ask: "can the principal perform this action against `Resource::Any`?" That lowers to a Cedar request with the entity `Resource::"*"`. The creator policies (`app_owner.cedar`, `app_editor.cedar`, `app_viewer.cedar`) all scope on `resource is App`, which does NOT match `Resource::"*"`. The default platform role from `entities.rs:106` is `'readonly'`, and `readonly.cedar` only grants `*:read` + `account:write`. Net effect: a normal creator (no admin/support/billing role) CANNOT consent to an OAuth client requesting `apps:write`, `apps:deploy`, `apps:delete`, `env:write`, `secrets:write`, `billing:write`, `team:write`, or `deployments:rollback` for their own apps. The consent UI renders the "you cannot grant" decline page (`consent_ui_test.rs:361-376` covers the viewer case but no test covers the common creator+app-owner case). The proposal §11 explicitly says "if user has the action *anywhere*, allow consent"; the implementation enforces "if user has the action against `*`," which is much stricter.
**Reproducer:**
1. Register a normal user (no platform.roles row → COALESCE to 'readonly').
2. Make them owner of app X via `control.app_members(role='owner')`.
3. Drive the consent flow for an OAuth client requesting `apps:write`.
4. `grantor_can_grant_requested_scopes` calls `enforce` with `Resource::Any` and `Action::AppsWrite`. `readonly.cedar` does not grant `apps:write`. `app_owner.cedar` requires `resource is App`, fails to match `Resource::"*"`. → DENY → decline screen.
5. Same user via session can write to App X just fine; same user via PAT-on-specific-app can be granted `apps:write` for `App::"X"` (because the request resource then matches `app_owner.cedar`). Only Resource::Any consent is broken.
**Suggested fix:** Two options, neither shimmy:
- **A (recommended).** Replace `Resource::Any` in the consent/PAT-mint grant check with iteration over `principal.app_owner_of ∪ editor_of ∪ viewer_of` for app-scoped actions, OR a single check against a fresh `Resource::App { id: "*" }` if you extend `app_owner.cedar` with a second permit `permit (... ) when { principal has app_owner_of && !principal.app_owner_of.isEmpty() }` for the `Resource::"*"` case. The literal "any app the user owns" semantics is what the proposal calls for.
- **B.** Loop in the handler: for each scope, iterate the user's owned/editor/viewer app set and stop at first allow. More expensive but matches the design exactly.
Add a regression test: user with `app_owner_of = [X]` and no platform role, consent for `apps:deploy` → allow. (No such test exists today.)

### H3. `Condition::TimeWindow` silently lowers to `true` — users get false sense of security
**File:** `crates/authz/src/lower.rs:135` and `crates/authz/src/lower.rs:141-151`.
**Severity rationale:** `lower_condition(Condition::TimeWindow { .. })` returns the literal Cedar string `"true"`. A `TODO` comment is appended to the source explaining that the wiring is incomplete, but nothing in the runtime, the `validate_grant_subset` path, or the public Policy deserializer rejects time-window conditions. A user who attaches a `TimeWindow { start: "09:00", end: "17:00", tz: "America/Los_Angeles" }` to their PAT — and the docs/proposals advertise this surface — receives a token that the platform claims is restricted to business hours but in reality works 24/7. The condition is the LAST line of defence for a leaked/exfiltrated PAT.
**Reproducer:**
1. Create a PAT with a single statement: `effect: allow, actions: [apps:read], resources: [App::"X"], conditions: [TimeWindow { start: "09:00", end: "17:00", tz: "UTC" }]`.
2. Use the PAT at midnight UTC.
3. `lower` emits `permit (..., resource == App::"X") when { true };` and Cedar allows the request.
**Suggested fix:** Until the time-window evaluator ships (proposal calls it P9-U2), reject `Condition::TimeWindow` at deserialize time with `serde::de::Error::custom("time_window conditions are not yet enforced; do not use")`. Same rejection in `validate_grant_subset` so existing tokens with this condition cannot be created. Once the evaluator lands, swap to a real comparison and remove the rejector + the `append_time_window_todos` helper. This is precisely the "no detect-and-warn paths" AGENTS.md guardrail — the current state is a silent fail-open.

### H4. App suspension never enforced — `suspended` column does not exist; `load_app_suspended` is dead code
**File:** `crates/authz/src/entities.rs:152-170`, `crates/control/src/registry.rs:97-112` (the actual `apps` table), `policies/platform/suspended_apps.cedar`.
**Severity rationale:** The shipping `apps` table schema has no `suspended` column. `load_app_suspended` tries `control.apps` (relation does not exist → 42P01 swallowed), then `apps` (column does not exist → 42703 swallowed), then returns `Ok(false)` unconditionally. The `suspended_apps.cedar` forbid policy therefore never matches, and there is no SQL surface for ops to mark an app suspended. This is the same shape as C2 (audit_locked) but worse because suspended_apps is the abuse-mitigation lever: if a creator app is doing something nasty, ops believes they can flip a bit to halt writes. They cannot.
**Reproducer:**
1. Locate any application; there is no `apps.suspended` column.
2. Even if ops manually `ALTER TABLE apps ADD COLUMN suspended BOOLEAN DEFAULT TRUE` and updates an app to suspended=true, the query in `entities.rs:154` finds the column, returns true, and the forbid policy fires. Without the column, fail-open.
3. Without the column, every `Resource::App` entity has `suspended=false`, and the policy is a no-op.
**Suggested fix:** Same as C2: add `suspended BOOLEAN NOT NULL DEFAULT FALSE` (and `audit_locked`) to the `apps` migration in `crates/control/src/registry.rs:98-108`, replace `load_app_suspended` with a single typed `load_app_flags` query (no relation fallback, no column swallow). Delete the `for table in ["control.apps", "apps"]` loop entirely. Pre-launch, the schema either has the column or the test fails — no detect-and-warn.

## MEDIUM

### M1. Entity cache holds revoked memberships / platform roles up to 30 s
**File:** `crates/authz/src/entities.rs:14, 17-22, 269-290`.
**Severity rationale:** `assemble_entities` caches the user's `platform_role`, `app_owner_of`, `app_editor_of`, `app_viewer_of`, and per-app `suspended` for 30 seconds keyed by `(principal_id, resource_key)`. There is no invalidation hook. When `admin_handlers::revoke_platform_role` or any `control.app_members DELETE` happens, the revoked user can still pass authz for up to 30 s against any resource whose entity tuple is already cached. For a privileged-role revocation triggered by a security event (compromised admin), 30 s of continued root authority is a real exposure window. The audit log will record the calls as `allow` decisions.
**Reproducer:**
1. User A has `platform_role='admin'`, calls `GET /apps`. Cache stores admin entity for key `(A, any:*)`.
2. Ops calls `DELETE /platform/users/A/role` to revoke admin.
3. Within 30 s, A calls `POST /apps/X/delete`. Cache hit → entity says admin → Allow.
**Suggested fix:** Either (a) drop the cache (control plane already hits PG on every request for other reasons), (b) add explicit invalidation: every write to `platform.roles` or `control.app_members` pushes a `(principal_id) → invalidate` event that walks the cache, or (c) shrink TTL to 1-2 seconds *and* add a forced-invalidation hook on revoke. Whichever path, add a regression test: revoke role, replay request within TTL, assert Deny.

### M2. `matched_policies` audit column is always written as `'{}'`
**File:** `crates/authz/src/eval.rs:151-165`, `crates/auth/src/store/migrations.rs:274`.
**Severity rationale:** The schema declares `matched_policies TEXT[] NOT NULL DEFAULT '{}'` so incident response can answer "which policy fired?" Today the INSERT in `audit_decision` never lists `matched_policies` in the column list — every row defaults to `'{}'`. Without per-decision policy attribution, audit replay (e.g., "show me every time the admin forbid fired in the last 24 h") is impossible. Cedar's `Response::diagnostics().reason()` returns the matched policy IDs at no extra cost; today they're discarded.
**Reproducer:** `SELECT matched_policies FROM control.authz_decisions LIMIT 5;` returns `{}` for every row.
**Suggested fix:** Capture `decision.diagnostics().reason()` (an `impl Iterator<Item = &PolicyId>`) in both the owner-allow path and the final eval, propagate it into `audit_decision`, and include it in the INSERT. Add a test asserting that an admin allow records `["admin"]` in `matched_policies`.

### M3. `Effect::Deny` statements in PAT wrappers are dropped by `validate_grant_subset`
**File:** `crates/control/src/token_handlers.rs:370-373`.
**Severity rationale:** `validate_grant_subset` short-circuits `if statement.effect != Effect::Allow { continue; }`. That is correct for the "you can't grant a permission you don't have" check (Deny only narrows). But it also means a user crafting a PAT with `Allow [apps:read]` followed by `Deny [apps:read] when { ip not in 10/8 }` passes validation, gets stored, and is the policy applied at use time. That is the *intended* behavior, but a future maintainer reading the file might add an Allow validation that ignores the Deny narrowing and inadvertently let a user grant a permission they don't have by sandwiching Allow between two Denies. Also: empty `Resource` and empty `Action` lists are not validated; they lower to "false" or "action in []" which is fail-closed but wastes bytes.
**Reproducer:** Submit a PAT with both Allow and Deny statements; only the Allow pairs are subset-checked.
**Suggested fix:** Add a comment on line 370 explaining *why* Deny is skipped (intent), add a `reject_empty_statements` arm that 400s `{actions:[], resources:[]}` so the user gets a useful error instead of a silently-deny token, and add a property test asserting that for any wrapper Policy with mixed effects, the validator only relaxes (never tightens) compared to enforcing each Allow individually.

## LOW

### L1. `Action::PlatformPoliciesWrite` is in the enum and audit vocabulary but has no policy that grants it
**File:** `crates/authz/src/action.rs:21,44,67,108`.
**Severity rationale:** Grep across `policies/` shows zero references to `platform_policies:write`. The enum variant exists, deserializes, lowers, and is accepted by `validate_grant_subset` (which will deny it for everyone except admin via wildcard). Until a policy or admin handler depends on it, it's dead vocabulary that confuses readers. Cedar's default-deny semantics keep it safe.
**Suggested fix:** Either delete the variant (and the matching `from_wire` arm, scope mapping, and audit string list) or write the policy that grants it (likely admin-only) and document the use case. Pre-launch, do not ship enum variants that no code paths use.

### L2. `RequireMfa` and `MfaWithin` always deny because `AuthzGuard` hardcodes `mfa_verified = false`
**File:** `crates/control/src/authz_guard.rs:81-89, 152-159, 190-197`, `crates/authz/src/eval.rs:111`.
**Severity rationale:** `mfa_verified` and `mfa_age_seconds` are hardwired to `false` and `None` in all three guard construction paths. The eval defaults `mfa_age_seconds` to `u32::MAX` when None. Net: any policy with `RequireMfa` or `MfaWithin { .. }` always denies. This is a fail-closed gap (safe), but it means PATs/OAuth scopes that include MFA conditions silently lock out their owners forever. Similar shape to H3 but at least the failure is observable (Deny), not silent allow.
**Suggested fix:** Until MFA is wired into the session model, reject `Condition::RequireMfa` / `Condition::MfaWithin` at PAT deserialize time the same way H3 recommends for TimeWindow. Once MFA verification lands in the session, thread `mfa_verified` and `mfa_age_seconds` from the session row into `AuthzGuard` and lift the rejection.

## Areas reviewed and clean

- **Scope → Action mapping** (`crates/authz/src/scope.rs`) is 1:1, alphabet-checked, and unknown scopes hard-error in `parse_scope_string`. `scopes_to_policy` correctly produces a single Allow statement whose action list maps to the requested scopes and whose resource is `Resource::Any`. Empty scope list correctly produces an empty `action in []` Cedar scope, which Cedar treats as never-matches (fail closed).
- **Two-call enforcement shape** (`crates/authz/src/eval.rs:37-72`) is correct: owner-step uses static policies (including the suspended/audit_locked forbids), and only proceeds to the token-step if the owner step is Allow. The token-step uses only the token policy, so platform forbid policies aren't double-counted but also aren't lost — they were already enforced in step 1. The unit test pattern `owner_unauthorized_returns_deny_even_if_token_grants` validates this directly.
- **policy_hash determinism** (`crates/authz/src/engine.rs:77-127`). `canonical_json` sorts object keys and recurses through arrays/objects. Same JSON content with reordered keys produces the same hash (`engine_test.rs:119-127`). No floating-point ambiguity for the current policy fields (all strings, ints, bools, arrays, objects).
- **Resource scope lowering edge cases**. Empty resource list lowers to `resource` scope plus `when { false }` (never matches). Mixed list with any `Resource::Any` correctly degenerates to plain `resource` scope. Single resource lowers to `resource == App::"id"`. Multi-resource lowers to `resource` scope plus `(eq1 || eq2 || ...)`. All paths confirmed via `engine_test.rs:147-164` and source-read.
- **PAT JWT mint/verify** (`crates/control/src/token_handlers.rs:104-162`). Ed25519, requires `typ=pat+jwt`, kid matched against the issuer, issuer + audience pinned, no `alg=none` window. JWT carries `policy_hash`, DB lookup re-checks `(id, owner_id, policy_hash, kind='pat', not revoked, not expired)`. Solid.
- **OAuth scope subset comparison** (`crates/auth/src/ui/consent.rs:238-249`). Both sides are `sort_dedup`'d before `binary_search`. Subset semantics (`requested ⊆ previously_granted`) is correct; superset requests fall through to the consent UI rather than silently auto-approving. Standard OIDC scopes (`openid`/`email`/`profile`/`offline_access`) are treated as just-another-string in the subset check, which is correct because they're case-sensitive per RFC 6749 and the comparison is exact.
- **Static policy file syntax**. All nine `.cedar` files terminate with `;\n`, and `build.rs:6-19` parses each at compile time so a malformed policy never lands in a binary. `load_platform_policies` joins them with `\n` and re-parses; `tests::all_static_policies_parse_cleanly` asserts the count.
