# Round 6 — Input validation: findings

Total: 11 findings (0 critical, 2 high, 5 medium, 4 low).

Scope walked: every `ntex::web::types::{Json, Query, Form, Path, Payload}` extractor
in `crates/control`, `crates/auth/src/ui/*`, and `crates/gateway/src`; the
`.zship` ingest pipeline in `crates/bundle`; Stripe webhook signature/body path;
the OIDC stash cookies + post-callback redirect; `scope::parse_scope_string`
fan-out; OAuth client registration; PAT body bounds; the gateway's DPoP /
wrapper-token exchange headers; and the email/UUID validation paths shared
across `auth.users` writers.

ntex defaults that bound the surface (verified in
`~/.cargo/registry/.../ntex-3.7.2/src/web/types/{json.rs,form.rs,payload.rs}`):

- `Json<T>` extractor: **256 KiB** body cap.
- `Form<T>` extractor: **32 KiB** body cap.
- `String` / `Bytes` extractors (`PayloadConfig::default`): **256 KiB**.
- `/api/apps/{id}/deploy` is the only route with an explicit override
  (`PayloadConfig::new(MAX_COMPRESSED_BYTES)` = 256 MiB; runtime tracks the
  decompressed flow inside `bundle::unpack`).

Headers/URL bounds enter through ntex's HTTP/1.1 decoder (`MAX_HEADERS = 96`),
which is enforced before any handler runs.

## CRITICAL

(none)

## HIGH

### H1. Console + gateway post-callback Location header trusts the user-supplied URI path verbatim (open redirect / phishing primitive)

**File:** `crates/control/src/api.rs:556-569` (`auth_callback`),
`crates/control/src/api.rs:674-700` (`start_oidc_redirect`),
`crates/gateway/src/router/dispatch.rs:1282-1296` (gateway callback),
`crates/gateway/src/router/dispatch.rs:1325-1358` (gateway start_oidc_redirect).

**Severity rationale:** Both the control plane and the gateway stash
`req.uri().path_and_query()` in the HMAC-signed OIDC stash cookie at the
"reject unauthenticated request" point, and then 302 to that string verbatim
after the OIDC dance succeeds (`builder.header("location", original_path)`).
The HMAC protects against an attacker swapping the value once the dance
starts, but it does **not** protect against an attacker initiating the dance
with a hostile path: a protocol-relative request line such as
`GET //evil.com/path HTTP/1.1` parses to `path_and_query == "//evil.com/path"`,
which a 302 emits as `Location: //evil.com/path`, and the browser dereferences
that as `https://evil.com/path`. Result: any link of the form
`https://console.zeroship.ai//evil.com/...` is a login-laundered phishing
redirect — the user's browser ends up on `evil.com` after a fully legitimate
sign-in flow at `auth.zeroship.ai`, with the zeroship origin in the address
bar at the moment they followed the link. The same gadget works at every
end-user app served by the gateway.

**Reproducer:**
```
$ curl -i 'https://console.zeroship.ai//attacker.example/path'
# control returns 302 to hydra with stash cookie containing
# original_path="//attacker.example/path"
# User completes the OIDC dance.
# /auth/callback responds:
#   HTTP/1.1 302 Found
#   location: //attacker.example/path
# Browser follows to https://attacker.example/path
```

**Suggested fix:** Before stashing, normalize `original_path`: reject any
value whose `path_and_query` does not start with `/` followed by a non-`/`
character (or is just `/`). The conservative check is "must match
`^/[^/\\\\]`"; the safer one is to reparse with `url::Url::parse` against
the request's own origin and refuse anything that resolves to a different
authority. Apply the same gate in both `start_oidc_redirect` paths AND
defensively in the callback (don't trust the post-decode stash either, since
old cookies could be in flight if the rule changes later).

### H2. OAuth dynamic-client registration has no length, scheme, or count caps on `client_uri` / `logo_uri` / `redirect_uris`

**File:** `crates/control/src/oauth_handlers.rs:252-275` (`validate_create_body`),
called from `create_oauth_client` at `:83-147`.

**Severity rationale:** `CreateOauthClientBody` (the
`POST /admin/oauth-clients` payload) enforces only "client_id non-empty,
client_name non-empty, redirect_uris non-empty, token_endpoint_auth_method ∈
{client_secret_basic, none}". There is no upper bound on the **length** of
any string field, no cap on the **number** of `redirect_uris`, no check that
`client_uri` / `logo_uri` / each `redirect_uri` parses as a URL, and no
`https://` scheme enforcement. Because the route is gated by
`PlatformPoliciesWrite` it is admin-authenticated, so this is not a remote
unauthenticated DoS — but it (a) lets a compromised or low-trust admin row
register `javascript:alert(1)` as `logo_uri`, which is then rendered into
the consent template at `crates/auth/src/ui/templates/consent.html:5` inside
`<img src="{{ logo }}">` (modern browsers ignore `javascript:` in `img/src`
today, but the value also surfaces via `oauth_grants_handlers::list_grants`
and the dashboard "Connected Apps" UI where it may land in `href`); and
(b) lets the same admin row stuff a 1 MB `client_name` into hydra which is
then echoed back to the consent screen on every login attempt against that
client. The native cap is the ntex `Json` 256 KiB body limit minus everything
else — i.e. effectively no cap on individual fields.

**Reproducer:**
```
$ curl -X POST https://console.zeroship.ai/admin/oauth-clients \
    -H 'authorization: Bearer <admin pat>' \
    -H 'content-type: application/json' \
    -d '{
      "client_id": "attacker",
      "client_name": "...100 KiB of HTML...",
      "client_uri": "javascript:alert(1)",
      "logo_uri": "javascript:alert(1)",
      "redirect_uris": ["http://evil/path"],
      "grant_types": ["authorization_code"],
      "response_types": ["code"],
      "scope": "openid",
      "token_endpoint_auth_method": "client_secret_basic"
    }'
# 201 Created. Subsequent logins against client_id=attacker render
# client_name verbatim into LoginPage.client_name (auth/src/ui/login.rs:82)
# and the logo into the consent template.
```

**Suggested fix:** Add bounds and scheme enforcement before the hydra call:
- `client_id`: 1..=128 chars, ASCII printable, no whitespace.
- `client_name`: 1..=200 chars (display name, not arbitrary HTML).
- `client_uri` / `logo_uri`: parse with `url::Url::parse`, require
  `scheme == "https"` (production) or `https`/`http` in `insecure_dev`.
  Reject any URL whose scheme is `javascript:`, `data:`, `vbscript:`, etc.
- `redirect_uris`: max 32 entries, each ≤ 1024 chars, parse as URL, require
  `https://` in production. (Hydra additionally enforces exact-match at
  authorize time, but defence-in-depth here keeps the DB clean.)
- `scope`: limit to 128 KiB worth of scope-string before calling
  `validate_scope_list`.

## MEDIUM

### M1. `signup` interpolates user-supplied `login_challenge` into the post-signup `Location` header without validation

**File:** `crates/auth/src/ui/signup.rs:81, 247-259` (`redirect_to_login`).

**Severity rationale:** `redirect_to_login` builds
`format!("/login?login_challenge={challenge}")` and stuffs it into a
`Location: ...` header via `HeaderValue::from_str`. `HeaderValue::from_str`
rejects CTL bytes (CR/LF/NUL), so this is **not** an HTTP-response-splitting
vector. But:
1. `challenge` is not URL-encoded — a payload like
   `login_challenge=foo&malicious=value` smuggles a second query parameter
   into `/login`.
2. `challenge` has no length cap. A request body inside the 32 KiB ntex
   Form limit can include ~16 KiB of `login_challenge`, all of which is
   echoed into the response header (which then needs to fit under ntex's
   response header buffer; some clients may choke).
3. Although `HeaderValue::from_str` strips on failure and falls back to
   `/login`, characters like `%0A` or `\r` are rejected as bytes but `&`,
   `?`, `#` parse through and change the meaning of the URL on the next
   GET.

**Reproducer:** POST `/signup?login_challenge=foo%26malicious%3Dvalue` with
a valid CSRF + form body. The 302 response Location is
`/login?login_challenge=foo&malicious=value`, which the user's browser
follows to a `/login` GET with an extra parameter the handler doesn't
expect. Combined with H1 above, an attacker can use this to inject a
`?next=//evil.com` parameter and trigger any future redirect on `/login`
that consults it.

**Suggested fix:** URL-encode the value:
```rust
let challenge_enc: String =
    url::form_urlencoded::byte_serialize(challenge.as_bytes()).collect();
let to = format!("/login?login_challenge={challenge_enc}");
```
and add a 256-byte (or so) cap on the raw `login_challenge` value at parse
time (`SignupQuery` deserialiser, plus `LoginForm`).

### M2. Magic-link email embeds the raw `User-Agent` header without length or charset filtering

**File:** `crates/auth/src/ui/magic.rs:1179-1182` (`user_agent_str`),
called from `magic::start` at `:233`.

**Severity rationale:** The UA header is reflected verbatim into the
`requesting_device` field of `MagicLinkHtml` and `MagicLinkText`. The
HTML template auto-escapes for HTML context, so this is not an XSS sink in
the rendered email body. The risk is:
- **Email DoS / SMTP size limits.** A multi-kilobyte UA gets baked into
  every magic-link email that one rate-limited-but-still-issued device
  triggers — at 8 KiB headers × N retries this lands as a >32 KiB email
  body. Some SMTP providers reject >100 KiB.
- **PII / log volume.** The header is stored only in the email, not in
  audit, so this is bounded; but `tracing::warn!` on send failure attaches
  the email context.
- **Header smuggling into deliverability templates.** The template uses
  the value to brand the email with the requesting device — an attacker
  who controls the UA on the requesting device can paste arbitrary text
  ("zeroship has been hacked, click here…") into a legitimately-delivered
  zeroship email.

**Reproducer:**
```
$ curl -X POST https://auth.zeroship.ai/magic/start \
    -H 'user-agent: ZEROSHIP SECURITY ALERT — visit https://evil...' \
    --data 'csrf=...&email=victim@example.com&login_challenge=...'
# victim@example.com receives a legitimately-signed zeroship email whose
# body contains the attacker's text inside the "requesting device" field.
```

**Suggested fix:** Cap and sanitise:
```rust
fn user_agent_str(h: Option<&HeaderValue>) -> String {
    h.and_then(|v| v.to_str().ok())
        .map(|s| {
            let trimmed: String = s.chars().take(120)
                .filter(|c| !c.is_control()).collect();
            if trimmed.is_empty() { "Unknown device".into() } else { trimmed }
        })
        .unwrap_or_else(|| "Unknown device".into())
}
```
A 120-character cap matches what real UAs look like; anything longer is
junk worth dropping.

### M3. Email format is barely validated; the only check is "contains `@` and len ≥ 3"

**File:** `crates/auth/src/ui/signup.rs:108-112`,
`crates/auth/src/ui/forgot.rs:75` (via `email_norm`),
`crates/auth/src/ui/magic.rs:166` (via `email_norm`),
`crates/auth/src/store/users.rs:69-95` (no store-layer cap).

**Severity rationale:** `signup` accepts anything that satisfies
`email.contains('@') && email.len() >= 3`. There is no upper bound on the
local-part, no length cap (so a 30 KiB "email" inside a 32 KiB form body
gets persisted into `auth.users.email` as `citext`), no whitespace
rejection, no Unicode-normal-form check (mixed-script confusables can
collide with existing accounts via `citext`'s case-fold comparison). The
`magic_link::issue` SQL path inserts the same string into
`auth.magic_links.email`, which is later used to find-or-create users —
so a multi-KiB email becomes a multi-KiB row in `auth.users` plus a
multi-KiB token row. Storage cost per attacker request: O(KiB), with no
per-IP cap on signup beyond the rate limiter (which gates rate, not row
size).

**Reproducer:**
```
$ curl -X POST https://auth.zeroship.ai/signup -d \
    'csrf=...&name=x&password=verylongpassphrasehere&email=a@'\
    'b'<random 30000 chars>'.com'
# Persists a 30 KB row into auth.users; subsequent magic-link issues
# pin 30 KB into auth.magic_links.email.
```

**Suggested fix:** Add explicit caps at the form-deserialisation layer (or
the first handler instruction):
- Trim, lowercase, normalize to NFKC.
- Reject if length > 254 (RFC 5321 max email length).
- Reject if local-part > 64 (RFC 5321 §4.5.3.1.1).
- Reject if domain doesn't match
  `/^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$/`.
- Apply identical rules in `signup`, `forgot`, `magic`, and the user creation
  path inside `magic::find_or_create_magic_user`.

### M4. `name` field on `signup` has no length cap; lands in `auth.users.name` and is reflected into every magic-link / reset email

**File:** `crates/auth/src/ui/signup.rs:159, 165`,
`crates/auth/src/store/users.rs:69-95`.

**Severity rationale:** Same shape as M3 but for `name`. The ntex Form
extractor caps the entire form at 32 KiB, so an attacker can persist ~30 KiB
of name. This then flows into the password-reset email's
`name.split_whitespace().next()` and into the magic-link email's name hint
— so the "Hi <name>" greeting on every future email contains 30 KiB of
attacker-controlled text up to the first whitespace. Lower than M3 because
the value is split on whitespace before use, so practical exploitation
requires a name with no whitespace.

**Suggested fix:** Cap `name` at 200 chars at the handler:
```rust
let name = form.name.trim();
if name.is_empty() || name.chars().count() > 200 {
    return render_signup_error(&challenge, &cfg, "name must be 1-200 characters");
}
```

### M5. `user_id` text columns receive untrimmed UUID strings; downstream parsers eat the cost

**File:** `crates/auth/src/ui/me.rs:222-224` (`users::find_by_id`),
`crates/auth/src/store/users.rs` (look for the user_id-as-string path).

**Severity rationale:** Where the code uses `Uuid::parse_str` and rejects
non-UUIDs (e.g. `crates/control/src/api.rs:100-106`), failures cleanly land
as 400. But `me::resolve_user` calls
`users::find_by_id(db, &session.user_id.to_string())` — the input there is
already a typed `Uuid`, so this is fine. However, `unlink` uses
`path: ntex::web::types::Path<(String,)>` and passes `provider` (untrimmed,
uncapped) into `unlink_preserving_credential`'s SQL parameter. That's
parameterised SQL, so no injection — but a 1 MiB provider string round-trips
to PG and back. The same pattern exists in
`crates/control/src/oauth_grants_handlers.rs:84-93` (`client_id` from
`Path<String>` into SQL with no length cap).

**Suggested fix:** Add a cheap length gate at every `Path<String>` entry
point that flows into SQL: e.g. reject `provider.len() > 64` and
`client_id.len() > 128` before issuing the query.

## LOW

### L1. `set_secret` value cap of 64 KiB applies to plaintext but the JSON body cap is 256 KiB — mismatch

**File:** `crates/control/src/env_store.rs:62` (`MAX_VALUE_BYTES = 64 KiB`),
called from `crates/control/src/env_handlers.rs:88-118` (`set_var`/`set_secret`).

**Severity rationale:** Per-value cap is enforced in `env_store`, so the
"too big" arm is properly rejected (`EnvError::TooLarge`). But the ntex
Json extractor allocates the whole 256 KiB body before the handler sees
it — an attacker burning CPU on 4× over-cap rejected requests is a small
multiplier on the rate limiter. Cosmetic; not exploitable.

**Suggested fix:** Wire `PayloadConfig::new(80 * 1024)` onto
`/api/apps/{id}/vars` and `/api/apps/{id}/secrets` so the parser doesn't
buffer payloads beyond the per-value cap.

### L2. Stripe webhook `event.id`, `event_type`, `currency` echoed back into logs without length sanitisation

**File:** `crates/control/src/stripe_handlers.rs:436-444` (uses
`sanitize_event_id` already, good), but
`crates/control/src/stripe_handlers.rs:419` reflects the serde error message
(`format!("invalid json: {e}")`) directly into the response body.

**Severity rationale:** The serde error string includes a fragment of the
malformed JSON. Stripe webhook bodies are signature-verified by the time
we get here in production (so the body content is from Stripe), but the
verification can be skipped via `insecure_dev`. In dev mode the body is
fully attacker-controlled. The reflection here is response body, not log,
so no log injection — but error-detail-disclosure semantics deserve a
constant string.

**Suggested fix:** `return err_json(400, "invalid json");` — drop the inner
serde error; log it at debug.

### L3. `device::DeviceForm.user_code` echoed back into the rendered form on error without length cap

**File:** `crates/auth/src/ui/device.rs:42-94`, particularly the
`render_form(user_code, ...)` calls on error paths.

**Severity rationale:** Askama auto-escapes for HTML attribute context, so
no XSS. But a 32 KiB `user_code` (under the ntex Form limit) gets re-baked
into every error response on a tight loop while the attacker probes hydra
— amplifies bandwidth on the failure path.

**Suggested fix:** After `let user_code = form.user_code.trim();`, reject
`user_code.len() > 32` (real device codes are 8 chars).

### L4. PAT `name` has no upper bound

**File:** `crates/control/src/token_handlers.rs:27-32` (`CreateTokenBody`).

**Severity rationale:** `name: String` is bounded only by the JSON 256 KiB
body cap. The DB column probably accepts it (PG `text` is unbounded). The
PAT list endpoint surfaces it back. No exploit beyond cosmetic — but a 100
KiB PAT name in the list response is awkward.

**Suggested fix:** Add `if body.name.chars().count() > 200 { return
bad_request(...); }` next to the `expires_in_days` validation.

## Areas reviewed and clean

- `crates/authz/src/scope.rs::parse_scope_string` — closed-vocabulary, rejects
  unknown scopes (line 184); `crates/control/src/oauth_handlers.rs:230-250`
  validates the scope list at registration time the same way.
- `crates/bundle/src/unpack.rs` — manifest read capped at 1 MiB
  (`MAX_MANIFEST_BYTES`), decompressed flow capped at 256 MiB, single-blob
  capped at 16 MiB, blob count capped at 10 000, hash format validated via
  `blob::validate_hash_format`, tar paths rejected if outside `blobs/`. The
  zstd decoder is hard-limited via `Read::take(MAX_DECOMPRESSED_BYTES + 1)`
  so zip-bomb-style attacks die before exhausting memory.
- `crates/control/src/api.rs::deploy` — explicit `PayloadConfig::new(256 MiB)`,
  rate-limited content-type check happens before any byte is read into a
  tmp file, the tmp path is `uuid::new_v4()`-randomised so concurrent
  uploads can't collide, mmap is dropped before unlink. Authz happens
  before the body is consumed.
- `crates/control/src/stripe_handlers.rs::webhook` — 256 KiB body cap
  *before* sig verification; 4 KiB cap on the signature header; HMAC-SHA256
  compared via `subtle::ConstantTimeEq`; 300 s clock skew window;
  `sanitize_event_id` on the log path; SHA-256 of the raw body recorded
  for tamper detection. The constant-time loop OR-fold over `v1s` correctly
  doesn't early-exit.
- `crates/core/src/dpop.rs::verify` — strict header allowlist (`typ ==
  "dpop+jwt"`, alg in fixed set), 3-part JWT split, body decoded
  separately so a malformed body fails fast before crypto, `htm`/`htu`
  compared exactly, `iat` skew bounded, `ath` checked against
  `SHA-256(access_token)` when present. The ntex header decoder caps
  individual header bytes long before the JWT base64 decode runs.
- `crates/control/src/token_handlers.rs::validate_grant_subset` — caps the
  expanded action×resource pair count at `MAX_GRANT_PAIRS = 10_000`
  (line 23), so a deeply-nested policy can't blow up the authz call
  fan-out.
- `crates/control/src/env_handlers.rs::list_audit` — `query.limit: i64` is
  clamped `1..=500` inside `audit::recent_for_app` (line 91); negative or
  huge values are bounded before the SQL.
- `crates/authz/src/entities.rs::cedar_string` — properly escapes `"`,
  `\\`, `\n`, `\r`, `\t` for Cedar string literals; the entity
  composition uses `EntityUid::from_str(&format!("{type}::{}",
  cedar_string(id)))`, so user-controlled `id` strings cannot break out
  of the Cedar string context (this is what closes the R4 injection
  vector via condition operands).
- `crates/auth/src/ui/login.rs` — uses `HeaderValue::from_str(&redirect_to)`
  with a static `/` fallback on parse failure; the only `redirect_to` value
  here comes from hydra's `accept_login` response, which is trusted.
- `crates/control/src/oidc_rp.rs::Stash::decode` — constant-time MAC
  compare via manual XOR-fold over equal-length byte slices, signature
  verification happens before JSON parse, returns `None` on any decode
  failure (no error-detail leakage). The `OAuthStash` (auth crate) is
  identical in shape.
- `crates/auth/src/ui/{oauth_google,oauth_github}.rs` — both verify the
  signed stash cookie *before* consuming `code`/`state` query params, emit
  audit events on every failure arm, and clear the stash cookie on every
  exit path (success, upstream error, verifier mismatch).
- `crates/control/src/internal.rs` — `app_id` parsed with explicit
  `Uuid::parse_str`/`app_id.parse::<Uuid>()` and 400 on failure; the
  control-key auth check happens first; `merged_env_for_worker`
  distinguishes "not found" from "internal error" cleanly.
- `crates/gateway/src/dpop_exchange.rs::handle` — strict step-by-step
  parsing (`Authorization: Bearer …`, exactly-one `DPoP` header, host
  used as-is into `expected_uri`, jti replay check with a typed cache,
  hydra introspection gate); no path that 5xxs on attacker input without
  logging.
- Path-traversal review: every `Path<String>` that flows into a file
  system operation is either (a) parsed as a UUID via
  `Uuid::parse_str` before use (control plane app handlers, env handlers,
  PAT, oauth grants); (b) validated against a fixed regex (env-store key
  `valid_key`, line 66); or (c) passed only as a parameterised SQL bind.
  No `Path<String>` reached `std::fs` without normalisation.
