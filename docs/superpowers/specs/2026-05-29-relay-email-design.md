# Relay email sub-spec (Subsystem 5)

> **Status:** design. Companion to `2026-05-29-auth-sdk-design.md` §7 (which now carries only the
> one-paragraph summary; this file is the buildable detail it promised). Slice 5 in the main spec's §9
> implements against THIS document.
>
> **Scope of v1:** app → user one-way forwarding only. Inbound *user replies* to a relay alias are
> **bounced** with a clear message (never silently dropped). Two-way reply re-injection (alias → app)
> is **v2**, sketched in §11.
>
> **Pre-launch / no-back-compat (AGENTS.md):** there are no production users. Every struct, route, and
> column below is new or freely-extended; nowhere do we add a shim or a `@deprecated` alias. Where this
> spec extends an existing Rust contract (the `Email`/`Mailer` types, §3) it does so by editing the
> struct and updating **every** driver + caller in the same change — that work is **in scope for
> Slice 5**, not assumed-already-present.

---

## 0. What the round-1 review got right, and what this spec fixes

The first pass of §7 was a managed-provider hand-wave that collided with the real code in four
load-bearing places. This document fixes each, grounded in the actual tree:

| # | Round-1 claim | Reality in the tree | This spec |
|---|---|---|---|
| **B1** | "the sub-spec exists" | the file did not exist | **this file** |
| **B2** | "rewrite `From:`/`Reply-To:` on the *existing* outbound path" | `mailer::types::Email` has no `reply_to`/`envelope_from`; the Resend driver drops `headers` and has no `Reply-To` (`resend.rs`); the SMTP driver explicitly ignores `headers` (`smtp.rs:115`) | **§3 extends the `Email`/`Mailer` contract** (scoped work) so `reply_to`, `envelope_from`, and `headers` are actually emittable on all four drivers |
| **B3** | "reuse the Postmark/SES webhooks; SES inbound POSTs parsed messages" | those webhooks (`bounce.rs`, `sns.rs`) are **delivery-event** webhooks (Bounce/Complaint only, no body/From/To/MIME). SES inbound dumps **raw MIME to S3**, truncates to SNS; only **Postmark inbound** posts parsed JSON | **§4 pins ONE inbound provider (Postmark inbound), specs its parsed-JSON payload + Basic-auth signature**, and keeps it strictly separate from the delivery-event webhooks |
| **B4** | "deleting the grant *cascades* to set `revoked_at`" | no FK (cross-schema), no trigger, no owner, no txn | **§6 names control's `revoke_grant` as the owner and makes the cascade ONE transaction over both schemas via the already-shared `auth_pg` handle** |
| **M1** | "the provider handles DKIM/SPF/DMARC + ARC" against Resend | Resend has no inbound/ARC product at all | **§5 names the concrete identity plan** (relay-domain DKIM selector, SPF, DMARC, SRS envelope rewrite, header strip) and forwards via a provider that *does* sign the relay domain |
| **M2** | header/bounce privacy only covered From/Reply-To | `Return-Path`, `Sender`, `X-Original-To`, `Delivered-To`, `Received` all leak the real inbox | **§5.3 enumerates every header stripped/rewritten and pins the envelope-from to a relay bounce mailbox** |

---

## 1. Architecture at a glance

```
                INBOUND (someone emails the alias)                 OUTBOUND (we forward it)
                ─────────────────────────────────                 ────────────────────────
  app/3rd-party ──MX──► Postmark Inbound Server                   relay handler builds a
   sends to              (parses MIME → JSON)                      provider-neutral `Email`
   {token}@relay.zeroship.ai      │                                        │
                                  │ HTTPS POST (Basic auth)                │ Mailer::send
                                  ▼                                        ▼
                       ┌──────────────────────────┐            ┌────────────────────────┐
                       │  POST /webhooks/relay-in  │            │  outbound mailer driver │
                       │  (crates/auth/src/ui)     │──forward──►│  (Resend / SMTP / SES) │
                       │  verify_basic_auth        │   Email    │  now header-capable §3 │
                       │  → lookup → suppress/     │            └────────────────────────┘
                       │    revoke gate → loop     │                        │
                       │    guard → rate limit     │                        ▼
                       └──────────────────────────┘                  user's real inbox
                                  │ (no active map / suppressed / replies)
                                  ▼
                          provider returns 422 → Postmark bounces to sender
```

Key properties:

- **No bespoke MX.** Postmark's inbound server is the receiving MX; we own only a thin compio HTTPS
  handler. This honours the AGENTS.md "calls fetch / managed concern" classification — the relay is a
  *driver*, not a native kernel surface.
- **One inbound provider, one signature path.** Postmark inbound authenticates with HTTP Basic auth,
  and the tree **already has** `verify_basic_auth` (`crates/auth/src/mailer/bounce.rs:86`). No new
  signature scheme (this is why Postmark, not Mailgun-HMAC or SES-SNS-RSA — see §4.4).
- **Zero tokio.** The handler is a `ntex` route on the auth service (same place the existing webhooks
  live, `server.rs:140-152`); outbound is the existing `Mailer` trait (cyper / `spawn_blocking`
  lettre). No new runtime.

---

## 2. Alias generation (unchanged from §7.1 — referenced, not re-derived)

Alias allocation, the partial-unique constraint, generate-and-retry, the consent-time create, and the
read-through gateway cache are **fully specified in the main spec §7.1 + §8.1 DDL** and are sound
(the review confirmed). This sub-spec depends on, but does not re-derive, them. The one row this spec
reads and writes is:

```sql
auth.app_user_identities (
  id              text PRIMARY KEY,    -- pws_… pairwise sub (deterministic)
  app_id          text NOT NULL,
  global_user_id  uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
  relay_email     text,                -- {token}@{relay_domain}; NULL until email scope granted
  created_at      timestamptz NOT NULL DEFAULT now(),
  revoked_at      timestamptz,
  UNIQUE (app_id, global_user_id)
)
-- partial-unique: active aliases only
CREATE UNIQUE INDEX app_user_identities_relay_active
  ON auth.app_user_identities (relay_email) WHERE relay_email IS NOT NULL AND revoked_at IS NULL;
```

The relay alias is the value the gateway projects into `ZeroShip-User.email` and the id_token `email`
claim, so **no real address ever reaches an app** (main spec §7.1 substitution table). This spec covers
what happens to mail *sent to* that alias.

<!-- Added: B1 — the file now exists; §2 anchors to the already-sound §7.1 alias machinery rather than re-deriving it -->

---

## 3. Extending the outbound `Email`/`Mailer` contract (SCOPED Slice-5 work — NOT a reuse)

> **B2 fix.** The round-1 claim "forward via the existing outbound path, rewriting `From:` and
> `Reply-To:`" is **false against the code as-is**: `mailer::types::Email` (`crates/auth/src/mailer/types.rs:9`)
> is `{ to, from, subject, text, html, headers, tags }` with **no `reply_to` and no `envelope_from`**;
> the Resend driver (`resend.rs:50` `ResendRequest`) serializes only `{ from, to, subject, text, html,
> tags }` and **drops `msg.headers` entirely**; the SMTP driver **explicitly ignores `headers`**
> (`smtp.rs:115` "Arbitrary `msg.headers` are intentionally ignored"). So `Reply-To: alias`, the
> `X-ZS-Relay` loop marker (§7), and the envelope/Return-Path rewrite (§5.3) are **un-emittable
> today.** Making them emittable is the **first task of Slice 5.**

### 3.1 `Email` gains `reply_to` + `envelope_from`

`crates/auth/src/mailer/types.rs`:

```rust
pub struct Email {
    pub to: Address,
    pub from: Address,
    /// NEW — Reply-To mailbox. `None` ⇒ no Reply-To header emitted.
    pub reply_to: Option<Address>,
    /// NEW — SMTP envelope-from (MAIL FROM / Return-Path). `None` ⇒ the driver
    /// uses `from.email` as today. Relay forwards pin this to the relay bounce
    /// mailbox (§5.3) so forwarded-mail bounces NEVER route to the real inbox.
    pub envelope_from: Option<String>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    pub headers: Vec<(String, String)>,   // now actually emitted (§3.2/§3.3)
    pub tags: Vec<String>,
}
```

Pre-launch, this struct is edited in place and **every constructor updated in the same patch** — the
magic-link / verify / reset / suspicious-activity templates (`mailer/templates.rs`) set
`reply_to: None, envelope_from: None` and are otherwise unchanged. No `..Default::default()` shim, no
builder back-compat.

### 3.2 Resend driver actually emits `reply_to` + `headers`

Resend's send API DOES accept these (it's the driver that was incomplete, not the API). Per
<https://resend.com/docs/api-reference/emails/send-email> the body accepts `reply_to` and a
`headers` object. `resend.rs::ResendRequest` is extended:

```rust
#[derive(Debug, Serialize)]
struct ResendRequest<'a> {
    from: String,
    to: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,                       // NEW
    subject: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<serde_json::Map<String, serde_json::Value>>,  // NEW — X-ZS-Relay etc.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<ResendTag<'a>>,
}
```

`reply_to` is `msg.reply_to.map(format_address)`; `headers` is built from `msg.headers` (only the
forward-control headers we set — §7 — plus any caller header). Resend's HTTP API does **not** expose a
per-message envelope-from override, so for the relay forward we use the **SMTP/SES path** when
`envelope_from` is `Some` (Resend stays the default for transactional auth mail where envelope-from is
the verified send domain anyway). This is recorded as the **forwarding driver constraint** in §5.2.

### 3.3 SMTP + SES drivers emit `reply_to`, `envelope_from`, `headers`

`smtp.rs::build_lettre_message` is rewritten to stop ignoring headers:

```rust
let mut builder = Message::builder().from(from_mbox).to(to_mbox).subject(...);
if let Some(rt) = &msg.reply_to { builder = builder.reply_to(format_mailbox(rt)?); }
for (k, v) in &msg.headers {
    // lettre's typed Header trait is awkward for arbitrary strings; we use the
    // raw-header escape hatch `message::header::Header`-via-`HeaderName`/`HeaderValue`.
    builder = builder.header(raw_header(k, v)?);
}
// envelope-from (Return-Path / MAIL FROM): lettre's `Message::envelope()` takes the
// from-address; we override it explicitly so the bounce address is the relay's, not `from`.
let envelope = build_envelope(msg.envelope_from.as_deref(), &msg.from.email, &msg.to.email)?;
transport.send_raw(&envelope, &message.formatted())   // explicit envelope, not send(&message)
```

The SES driver (when added — the tree has the SES *inbound/SNS* verify but no SES *outbound* driver
yet; if outbound stays Resend+SMTP only, §5.2's "use the SMTP driver for relay forwards" stands and SES
outbound is not required for v1).

### 3.4 Regression tests for the contract extension

- `resend.rs` test: a `ResendRequest` built from an `Email` with `reply_to: Some(...)` and a
  `headers: [("X-ZS-Relay","1")]` serializes BOTH fields (today they'd be silently dropped — the test
  fails pre-fix).
- `smtp.rs` test: `build_lettre_message` output (`.formatted()`) contains `Reply-To:` and the
  `X-ZS-Relay:` header line; `build_envelope` returns the relay bounce address as the envelope-from,
  not `msg.from.email`.

<!-- Added: B2 — Email/Mailer contract extension (reply_to, envelope_from, real headers) scoped as Slice-5 work across ALL drivers, with regression tests that fail pre-fix; this is NOT claimed as an existing reuse -->

---

## 4. Inbound — ONE provider (Postmark Inbound), parsed-JSON payload, Basic auth

> **B3 fix.** The delivery-event webhooks already in the tree carry **no message body**:
> `bounce.rs::PostmarkEvent` is `Bounce | SpamComplaint | Other`; `sns.rs::SesEvent` is
> `Bounce | Complaint | Other`. **Inbound mail receive is a different Postmark product with a different
> payload** — there is zero inbound-message type/route/handler in `crates/auth` or `crates/control`
> today. This section specs the NEW route + NEW payload type and keeps them strictly separate from the
> delivery webhooks. It also corrects the round-1 factual error: **SES inbound does NOT POST parsed
> JSON** (it writes raw MIME to S3 / truncates to SNS); **only Postmark Inbound POSTs parsed JSON** —
> which is why Postmark is the pinned provider.

### 4.1 Why Postmark Inbound (decided)

- **Posts parsed JSON**, not raw MIME — so the handler needs no MIME parser
  (<https://postmarkapp.com/developer/webhooks/inbound-webhook>).
- **Authenticated by HTTP Basic auth** — and `verify_basic_auth` **already exists**
  (`bounce.rs:86`, constant-time, fully tested). Exactly one signature path, already built.
- It does inbound parse on a verified **inbound forwarding domain**, complementing the outbound
  relay-domain DKIM we publish (§5).

### 4.2 The NEW inbound route + payload type

New route (registered alongside the existing webhooks in `server.rs`):

```rust
// crates/auth/src/server.rs — next to /webhooks/postmark and /webhooks/ses-sns
.service(web::resource("/webhooks/relay-inbound")
    .route(web::post().to(ui::webhooks::relay_inbound)));
```

New parsed-inbound payload type (a NEW module `crates/auth/src/mailer/inbound.rs`, **not** an
extension of `bounce.rs`'s delivery enum). Only the fields we act on; everything else is ignored by
serde:

```rust
// crates/auth/src/mailer/inbound.rs  (NEW — Postmark Inbound parsed shape)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InboundMessage {
    pub from_full: Mailbox,                 // { Email, Name, MailboxHash }
    #[serde(default)]
    pub to_full: Vec<Mailbox>,              // each { Email, Name, MailboxHash }
    /// The address mail was delivered to — Postmark fills this even with
    /// catch-all/MailboxHash routing. This is the ALIAS we look up.
    pub original_recipient: String,
    pub subject: String,
    pub text_body: String,
    #[serde(default)]
    pub html_body: Option<String>,
    #[serde(default)]
    pub stripped_text_reply: Option<String>,
    #[serde(default)]
    pub headers: Vec<InboundHeader>,        // [{ Name, Value }]
    #[serde(rename = "MessageID")]
    pub message_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Mailbox { pub email: String, #[serde(default)] pub name: Option<String> }

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InboundHeader { pub name: String, pub value: String }
```

`original_recipient` is the alias to resolve. `from_full.email` is the third party that sent it (this
becomes the forwarded `Reply-To` so a user reply in v2 can be routed — in v1 replies bounce, §8).

### 4.3 The handler (`relay_inbound`)

```
1. verify_basic_auth(Authorization, cfg.relay_inbound_user, cfg.relay_inbound_password)
      → 401 on mismatch. (Same primitive as the Postmark bounce webhook.)
      An unverified POST is a forwarding/suppression spoof oracle — so this gate is mandatory
      and the handler 401s when the creds aren't configured (mirrors the postmark webhook's
      "always registered, 401 if unconfigured" stance, server.rs:140 comment).
2. loop guard: if any inbound Header is `X-ZS-Relay` → 200 OK + drop (count++ metric). (§7)
3. resolve alias → real inbox (§4.5): the JOIN + revocation gate. No row / revoked → 422 (bounce, §8).
4. suppression gate: check_suppression(db, real_inbox). Suppressed → 422 (bounce, §8).
5. per-alias + per-app rate limit (§7) via store::ratelimit::consume. Over limit → 429 (Postmark
      retries later) OR 422 drop-with-bounce for sustained abuse (§7 decides).
6. build the forward `Email` (§5) and Mailer::send.
7. 200 OK to Postmark on success; 422 to make Postmark bounce to the original sender on the
      "can't forward" cases (no map / suppressed / reply-not-supported).
```

The handler reuses `db: State<Arc<compio_postgres::Client>>` exactly like `ses_sns`
(`webhooks.rs:198`) — same `ntex` + compio shape, `#[allow(clippy::future_not_send)]`.

### 4.4 Why not Mailgun / SES-SNS for inbound (M-major closed)

The round-1 §7 offered three co-equal providers with three incompatible signature schemes. That left
the anti-spoof story **unbuilt for two of three**: the tree has `verify_basic_auth` (Postmark) and the
SNS-RSA verifier (`sns.rs`) but **no HMAC-SHA256 verifier** for Mailgun's `timestamp+token` inbound
signature. An unverified inbound webhook is a suppression-list / forwarding spoof oracle. **We pin
Postmark Inbound** so there is **exactly one** signature path and it is the one already implemented.
Mailgun and SES-inbound are dropped for v1 (SES-inbound also fails the "parsed JSON" requirement —
it's raw MIME to S3).

### 4.5 alias → real inbox is a JOIN + revocation gate + suppression gate (M-major closed)

> The round-1 line "the handler looks up `alias → real inbox` via `auth.app_user_identities`" hid a
> two-table lookup: `app_user_identities` stores `relay_email` and `global_user_id`, **not the real
> address**. The real inbox is `auth.users.email`, reachable only by JOIN.

The exact query the handler runs (active map only, then suppression-gated):

```sql
-- Step 3: alias → real inbox, ACTIVE map only.
SELECT u.email::text AS real_inbox, i.app_id, i.global_user_id
FROM auth.app_user_identities i
JOIN auth.users u ON u.id = i.global_user_id
WHERE i.relay_email = $1            -- $1 = OriginalRecipient (the alias)
  AND i.revoked_at IS NULL;         -- revoked alias ⇒ no row ⇒ bounce (§8)
-- 0 rows  → 422 bounce ("alias not active")
-- 1 row   → continue
```

Then **step 4** is `check_suppression(db, real_inbox)` (the existing
`mailer::check_suppression`, which keys on the recipient address) — a suppressed/complained user stops
receiving forwards, and the forward 422-bounces instead. Only after both gates pass do we build and
send the forward. The query + both gates are stated as the real path and covered by §10 tests.

`u.email::text` mirrors the existing `users::find_by_id` cast (`users.rs:58` selects `email::text`
because the column is `CITEXT`).

<!-- Added: B3 + two majors — pin ONE inbound provider (Postmark Inbound, parsed JSON, Basic auth = existing verify_basic_auth); NEW route + NEW inbound payload type kept separate from the delivery-event webhooks; spell out the alias→inbox JOIN + revocation gate + suppression gate as the real path; drop Mailgun/SES-inbound (no verifier / not parsed JSON) -->

---

## 5. Deliverability & header privacy (M-major + M-major closed)

> **M1 fix.** "The provider handles DKIM/SPF/DMARC + ARC" was asserted against Resend, which does
> DKIM-sign mail you *send* from a verified domain but does **no inbound receive and no ARC sealing**;
> and forwarding a third party's message re-originated from `relay.zeroship.ai` **breaks the original
> DKIM signature and SPF alignment** — exactly the case ARC/SRS exist to repair. This section names
> the concrete identity plan and the header surgery, instead of waving at a provider.
>
> **M2 fix.** Privacy is not just From/Reply-To. `Return-Path`/envelope-from, `Sender`,
> `X-Original-To`, `Delivered-To`, and the `Received` chain all leak the real inbox; envelope-from is
> *where bounces go*. §5.3 enumerates every one.

### 5.1 Identity plan for `relay.zeroship.ai` (the DNS we publish)

v1 forwards are **re-originated from the relay domain** (the `From:` is rewritten to the relay, §5.3),
so DMARC aligns on the **relay's own DKIM** — we do NOT depend on preserving the third party's broken
signature. The DNS we publish for `relay.zeroship.ai` (prod) / `relay.zeroship.localhost` (dev, no real
DNS — §9):

| Record | Value | Purpose |
|---|---|---|
| **DKIM** | `<selector>._domainkey.relay.zeroship.ai` TXT, key from the outbound provider | the relay signs every forward; DMARC aligns on this |
| **SPF** | `relay.zeroship.ai TXT "v=spf1 include:<provider-spf> -all"` | the forwarding driver's sending IPs are SPF-authorized for the relay envelope domain |
| **DMARC** | `_dmarc.relay.zeroship.ai TXT "v=DMARC1; p=quarantine; rua=mailto:dmarc@zeroship.ai"` | publishes the relay's own policy |
| **MX** | `relay.zeroship.ai MX → Postmark inbound MX` | inbound mail for `*@relay.zeroship.ai` lands at the Postmark inbound server (§4) |

### 5.2 Forwarding driver: re-originate + SRS, not blind relay

Because we **rewrite `From:`** to the relay (privacy — the user's real inbox must not see the third
party's domain as `From`, and the third party must not learn the real inbox), the original DKIM
signature is invalid on the body-as-sent anyway. So the forward is a **fresh send from the relay
identity**, not a transparent relay:

- The **forwarding driver is the header-capable SMTP/SES path** (§3.3), because it can set the SMTP
  **envelope-from** (Return-Path) to the relay bounce mailbox — Resend's HTTP API cannot override
  envelope-from per message (§3.2). The relay forward therefore goes out via the SMTP driver pointed at
  the outbound provider's SMTP endpoint (the relay-domain-verified sending identity). Transactional
  auth mail keeps using whatever `AUTH_MAILER` is configured; the **relay forward path explicitly
  selects the SMTP driver** so envelope-from is controllable.
- **SRS-style envelope rewrite:** the envelope-from (MAIL FROM) is
  `bounce+<opaque>@relay.zeroship.ai`, NOT the third party and NOT the real inbox. SPF then passes for
  the relay envelope domain, and **forwarded-mail bounces route to the relay's own bounce mailbox**
  (handled by the existing Postmark/SES *delivery* webhook → `auth.email_suppressions`), never to the
  original sender and never exposing the real inbox.
- **ARC:** for v1 (From rewritten to relay, DMARC aligned on relay DKIM) ARC is **not required** for
  delivery — the message authenticates as relay-originated mail. We DO strip the inbound
  `Authentication-Results` and any inbound `ARC-*`/`DKIM-Signature` headers (§5.3) so a downstream
  receiver does not see a *broken* upstream signature next to our valid one. (ARC sealing becomes
  relevant only in **v2** if we ever forward *without* rewriting From; tracked in §11.)

### 5.3 Header surgery — exact strip/rewrite list (M2)

On every forward, the relay handler builds the outbound `Email` with **only** these headers, and
**strips everything else from the inbound message** (we do not copy the inbound `headers[]` wholesale):

| Header | Action | Why |
|---|---|---|
| `From:` | **rewrite** → `"{app-name} via relay" <{alias}@relay.zeroship.ai>` | privacy + DMARC alignment on relay DKIM |
| `Reply-To:` | **set** → `{alias}@relay.zeroship.ai` (v1) | replies go back to the relay (which bounces in v1, §8), never to the real inbox or the third party |
| `Return-Path` / envelope-from (MAIL FROM) | **set** → `bounce+<opaque>@relay.zeroship.ai` | bounces of the forward route to OUR bounce handler, never to the real inbox or original sender |
| `Sender:` | **strip** | would otherwise carry an originating address |
| `X-Original-To:` | **strip** | leaks the alias→inbox mapping / real inbox |
| `Delivered-To:` | **strip** | leaks the real inbox (Postmark may include it) |
| `Received:` chain | **strip** (do not copy inbound `Received`) | leaks routing + the real inbox host |
| `Authentication-Results`, `ARC-*`, original `DKIM-Signature` | **strip** | a broken upstream signature next to our valid relay DKIM confuses receivers |
| `X-ZS-Relay:` | **add** → `1` (or a hop count) | loop protection (§7) — re-received copies are recognised and dropped |
| `Subject:`, `Date:`, body (`text`/`html`) | **copy** | the content the user wants |

The real inbox (`u.email`) appears **only** in the SMTP `RCPT TO` (the envelope recipient) — never in
any header. A §10 test asserts the rendered forward (`.formatted()`) contains **no** occurrence of the
real address in any header line, and that the envelope-from is the relay bounce mailbox.

<!-- Added: M1 + M2 — concrete DNS/identity plan (relay DKIM selector, SPF include, DMARC, MX), SRS envelope rewrite via the header-capable SMTP driver, ARC scoping, and the full strip/rewrite header table pinning Return-Path/Sender/X-Original-To/Delivered-To/Received; bounces route to the relay's own handler -->

---

## 6. Lifecycle / revocation cascade — owned, transactional, single-DB (B4 closed)

> **B4 fix.** The round-1 line "deleting the `control.oauth_grants` row cascades: set
> `app_user_identities.revoked_at = now()`" asserted a cross-schema, cross-service UPDATE with **no
> owner, no FK, no transaction, no mechanism**. A grant deleted in control with the auth UPDATE never
> firing leaves a **live relay alias forwarding to the real inbox after the user revoked** — the exact
> privacy failure the design exists to prevent.

**The mechanism already exists in the tree and needs no new infrastructure.** AGENTS.md: *"One
database, separate schemas (control, auth, per-app)."* Control's grant-revoke handler
(`crates/control/src/oauth_grants_handlers.rs::revoke_grant`) **already writes to `control.oauth_grants`
through `state.auth_pg`** — and that same handle already reads `auth.users`
(`admin_handlers.rs:547`) and touches `auth.gateway_sessions` (`backchannel_logout.rs`). So **control's
`auth_pg` reaches both schemas in the one shared Postgres instance.** `compio-postgres` exposes a real
`transaction()` (`generic_client.rs:106`).

**Owner: control's `revoke_grant`. Mechanism: ONE transaction over both schemas via `auth_pg`.** The
handler becomes:

```rust
// crates/control/src/oauth_grants_handlers.rs::revoke_grant
let mut tx = state.auth_pg.transaction().await?;     // one DB, both schemas
tx.execute(
    "DELETE FROM control.oauth_grants WHERE user_id = $1 AND client_id = $2",
    &[&authz.principal_id, &client_id]).await?;
// SAME txn — revoke the relay alias for THIS (app, user). app_id is keyed by client_id via
// control.app_oauth_clients; we resolve client_id → app_id then soft-delete the identity row.
tx.execute(
    "UPDATE auth.app_user_identities i
        SET revoked_at = now()
       FROM control.app_oauth_clients c
      WHERE c.client_id = $2
        AND i.app_id = c.app_id::text
        AND i.global_user_id = $1
        AND i.revoked_at IS NULL",
    &[&authz.principal_id, &client_id]).await?;
tx.commit().await?;                                   // atomic: grant gone ⇔ alias revoked
// only AFTER commit: best-effort Hydra consent-session revoke (already in the handler)
hydra_revoke_consent_sessions(...).await;
```

**Failure semantics (named, per the review's demand):**

- **DELETE + UPDATE are one transaction** → it is impossible to commit one without the other. There is
  no window where the grant is gone but the alias still forwards. This is the property the round-1
  design lacked.
- **Hydra revoke runs *after* commit** (as today) and is best-effort: if it fails the grant + alias
  are already revoked (fail-safe — the privacy-critical state committed first); the handler returns 500
  so the operator retries the Hydra leg, but the alias is **already dead**.
- **The three revoke paths differ and are each pinned:**
  - **Explicit user revoke** (`DELETE /me/oauth-grants/{client_id}`): the path above.
  - **App delete** (`control.apps` row deleted): `ON DELETE CASCADE` from `control.apps` →
    `control.app_oauth_clients` → `control.oauth_clients`, and a companion statement in the
    app-delete handler runs the **same** `UPDATE auth.app_user_identities SET revoked_at = now()
    WHERE app_id = <deleted app>` in the app-delete transaction. (App delete revokes **all** the
    app's aliases, not just one user's.)
  - **Signout** is **not** a revoke — it ends the session, not the grant; the alias stays active so the
    user keeps getting app mail across logins. (Explicitly NOT cascaded — that was an ambiguity in the
    round-1 text.)
- **Re-grant** reuses the deterministic `pws_` row (clears `revoked_at` for the *identity*) but mints a
  **fresh relay alias** by default — see §6.1 for the rotation decision and its UX consequence.

A §10 regression test does the full loop: grant → forward-to-alias succeeds → `revoke_grant` →
inbound to the same alias now 422-bounces (the `revoked_at IS NULL` gate in §4.5 fails) → assert the
forward did NOT reach the real inbox.

<!-- Added: B4 — name the owner (control revoke_grant), the mechanism (single transaction over both schemas via the already-shared auth_pg), and the failure semantics; distinguish explicit-revoke vs app-delete vs signout; add a regression test that asserts inbound to a revoked alias bounces -->

### 6.1 Alias rotation on re-grant — UX consequence flagged (minor closed)

Rotation itself is sound (partial-unique on active aliases, generate-and-retry, deterministic `pws_`
reused). The **deliverability/UX consequence the review flagged:** after re-grant the OLD alias is
revoked and all future inbound to it **bounces** (§4.5 → 422), so a user who re-grants periodically
accumulates dead aliases that newsletters/receipts still hold, producing a stream of bounces and lost
mail with no grace window.

**Decision (matches Apple Hide-My-Email behaviour, which the review cited):** **re-grant does NOT
rotate the alias by default.** The relay alias is **stable across re-grants** — `revoke_grant` sets
`revoked_at`, and a subsequent re-grant **clears `revoked_at` on the same row and reuses the same
`relay_email`** (the partial-unique slot is free precisely because it was revoked, and the
generate-and-retry path is only entered if the *reused* token now collides with a different active
alias, which it won't for the same row). Rotation to a **new** address happens **only on an explicit
user "give me a new address" action** (a future alias-management surface — out of scope for v1 but the
column model already supports it: revoke the row's alias, mint a fresh one). This avoids the
dead-alias bounce stream for the common re-grant case.

> Implementation note for §2/§7.1: the consent-time upsert becomes
> `... DO UPDATE SET relay_email = COALESCE(app_user_identities.relay_email, excluded.relay_email),
> revoked_at = NULL` so a re-grant **un-revokes and keeps** the existing alias rather than minting a
> new one. (The main spec §7.5's "mints a fresh relay alias" on re-grant is **superseded by this
> decision** — recorded here as the relay sub-spec's authoritative call, to be reconciled into §7.5.)

<!-- Added: minor — re-grant keeps the alias stable (Apple Hide-My-Email model) instead of rotating, avoiding the dead-alias bounce stream; supersedes §7.5's "fresh alias on re-grant" -->

---

## 7. Abuse / loop protection — tied to real headers + a named counter store

> **M-major fix.** Round-1 loop protection depended on (a) reading an inbound `X-ZS-Relay` header AND
> (b) **stamping** `X-ZS-Relay` on the outbound forward — but per §3, (b) was un-emittable (headers
> dropped), so the guard was unbuildable: you could never stamp the marker you later check for. And the
> per-alias rate limit named no store. Both are fixed here.

- **Loop protection now works** because §3 makes `X-ZS-Relay` emittable:
  - **Stamp:** every forward adds `X-ZS-Relay: <hop>` (§5.3 header table) via the now-real
    `Email.headers` path (§3.2/§3.3).
  - **Detect:** the inbound handler (§4.3 step 2) inspects `InboundMessage.headers` for `X-ZS-Relay`;
    present ⇒ 200 OK + drop (a forward of our own forward — a loop).
  - **Hop cap:** the stamp carries a hop count; > N (default 3) ⇒ drop. Bounds a relay↔relay loop even
    across two relay aliases.
- **Rate limits use the existing leaky-bucket store** `auth.rate_limits` via
  `crate::store::ratelimit::consume(conn, key, capacity, refill_per_sec)` (`store/ratelimit.rs:32`) —
  the **same** primitive the login/forgot/magic/signup paths use (`ui/login.rs`, etc.). Two buckets per
  inbound message (§4.3 step 5):
  - **per-alias:** `key = "relay:alias:{relay_email}"` — caps a single alias being blasted.
  - **per-app:** `key = "relay:app:{app_id}"` — caps an app's total inbound forward volume.
  - Over limit ⇒ 429 (Postmark retries with backoff) for transient spikes; sustained abuse auto-revokes
    the alias (§6 path, app-scoped) and 422-drops.
- **Bounce/complaint of a forward** feeds `auth.email_suppressions` through the **existing delivery
  webhooks** (`/webhooks/postmark`, `/webhooks/ses-sns`) — these are the *delivery-event* webhooks, the
  correct ones for this (unlike the round-1 conflation). A suppressed real inbox then fails the §4.5
  suppression gate for **all** that user's aliases.

<!-- Added: M-major — loop protection tied to the now-emittable X-ZS-Relay header (stamp + detect + hop cap); per-alias/per-app rate limits named to the existing auth.rate_limits leaky-bucket store (store::ratelimit::consume); untied from the delivery-webhook reuse fiction -->

---

## 8. v1 reply handling — bounce, never silent-drop (O7, decided)

v1 is **app → user one-way**. A user reply *to* a relay alias (inbound where the sender is the real
user and the recipient is the alias, OR any inbound we cannot forward) is **bounced** with a clear
message, never silently dropped:

- The handler returns **422** to Postmark on the unforwardable cases (no active map, suppressed inbox,
  reply-not-supported), which makes **Postmark generate a bounce to the original sender** — a delivery
  failure they can act on, instead of a black hole.
- The bounce copy is "Replies to this address aren't supported yet" for the reply case.

Two-way reply re-injection (alias → the app, so the app sees the user's reply) is **v2** (§11).

---

## 9. Dev / test relay topology — now faithful because the inbound contract is pinned

> **Minor fix.** The round-1 dev injector posted "the same JSON shape the managed provider would POST"
> — but the provider/payload was unpinned (three providers, three shapes), so "the same shape" was
> undefined and the e2e could pass against a fictional payload. Now that §4 pins **Postmark Inbound's
> exact parsed-JSON schema** (`InboundMessage`, §4.2), the dev injector posts **that** schema and the
> e2e is faithful.

- **Relay domain in dev** = `relay.zeroship.localhost` (env `RELAY_DOMAIN`; prod `relay.zeroship.ai`).
- **Local MX sink** = a `mailpit` (or `inbucket`) container in `docker-compose` accepting mail for
  `*.zeroship.localhost`, with an HTTP API to assert delivery (stands in for the user's "real inbox").
- **Inbound provider simulated** by a tiny webhook injector: the e2e POSTs a **literal Postmark
  `InboundMessage` JSON** (the §4.2 schema, with `OriginalRecipient = <alias>`, a `Headers` array, a
  `FromFull`) to `/webhooks/relay-inbound`, **with the Basic-auth header** the handler verifies. This
  exercises the **real handler + real `verify_basic_auth` + real JOIN/suppression/revocation gates +
  real Mailer::send** — only the externally-operated receiving MX is simulated (the one piece compose
  can't stand up).
- **Outbound in dev** = the SMTP driver (§5.2) pointed at the mailpit sink, so the forward (with its
  rewritten `From`/`Reply-To`, stamped `X-ZS-Relay`, pinned envelope-from) actually lands in the sink
  and the e2e asserts the header surgery (§10) against a real rendered message.

The dev config knobs (auth `config.rs`, mirroring the existing `AUTH_MAILER` / `postmark_webhook_*`):

```
RELAY_DOMAIN                  = relay.zeroship.localhost   # prod: relay.zeroship.ai
AUTH_RELAY_INBOUND_USER       = <basic-auth user>          # verify_basic_auth, §4.3 step 1
AUTH_RELAY_INBOUND_PASSWORD   = <basic-auth pass>
AUTH_RELAY_FORWARD_MAILER     = smtp                       # forward path forces SMTP (envelope-from), §5.2
```

<!-- Added: minor — dev injector now posts the PINNED Postmark InboundMessage schema (§4.2) WITH the Basic-auth header, so the faithful e2e exercises the real verifier + lookup + gates + forward against a real payload, not a fictional shape; outbound via SMTP sink so the header surgery is asserted on a rendered message -->

---

## 10. Testing strategy (faithful e2e — no shims)

Mirrors the project's faithful-e2e mandate (real runtime + real handler + real lookup + real forward;
the auto-tx lesson). The relay e2e (item 8 in the main spec §8.6) runs:

1. **Contract-extension regression (§3.4):** Resend `ResendRequest` serializes `reply_to` + `headers`;
   SMTP `build_lettre_message` emits `Reply-To`, `X-ZS-Relay`, and the relay envelope-from. **Each
   fails against today's drivers** (which drop those) — proving they test the fix.
2. **Inbound happy path:** POST a Postmark `InboundMessage` (Basic-auth) for an active alias → assert
   the forward lands in the mailpit sink addressed (RCPT TO) to the real inbox.
3. **Header-privacy assertion:** the rendered forward contains the real address in **no** header line;
   `From`/`Reply-To` are the relay; `Return-Path`/envelope-from is `bounce+…@relay…`; `Sender`,
   `X-Original-To`, `Delivered-To`, inbound `Received`/`Authentication-Results` are absent.
4. **Suppression gate:** suppress the real inbox → the same POST 422-bounces, nothing reaches the sink.
5. **Revocation cascade (the B4 test):** grant → forward succeeds → control `revoke_grant` → re-POST to
   the same alias 422-bounces; assert the forward did NOT reach the sink, and assert the DELETE +
   `revoked_at` UPDATE committed atomically (a partial-commit would leave the alias live).
6. **Loop guard:** POST an `InboundMessage` already carrying `X-ZS-Relay` → 200 + dropped, no forward.
7. **Rate limit:** N+1 rapid POSTs to one alias → the (N+1)th is 429/dropped via
   `auth.rate_limits`.
8. **Re-grant stability (§6.1):** revoke then re-grant → the SAME alias is active again
   (`revoked_at` cleared, same `relay_email`), and inbound to it forwards again — no new alias, no
   dead-alias bounce.

Unauthenticated inbound POST (no/bad Basic auth) → 401, and **no** suppression/forward side-effect
(closes the spoof-oracle).

---

## 11. v2 (out of scope here, tracked)

- **Two-way replies (alias → app):** route a user reply back to the app (the app sees the message),
  with the app's `From` rewritten to the relay so the user still never sees the app's real address.
  Requires storing the third-party `from_full` per thread (the `MailboxHash`/thread key) and, if we
  ever forward *without* rewriting `From`, **ARC sealing** (§5.2) to repair the broken upstream chain.
- **Alias-management surface:** a console UI to list a user's aliases per app and explicitly rotate one
  (the only path that mints a fresh alias, per §6.1) — mirrors Apple Hide-My-Email's management view.

---

## 12. Slice 5 build order (what this spec hands the §9 slice plan)

1. **Extend the `Email`/`Mailer` contract** (§3) across `types.rs` + all four drivers + all template
   callers, with the §3.4 regression tests. (Prerequisite for everything else — without it the forward
   can't set the headers the privacy + loop story needs.)
2. **Inbound payload type + route** (§4): `mailer/inbound.rs`, `/webhooks/relay-inbound`,
   `verify_basic_auth` gate, the §4.5 JOIN + revocation + suppression gates.
3. **Forwarding build + SRS/header surgery** (§5): the relay forward `Email` builder, SMTP-driver
   selection, envelope-from pin, header strip/rewrite table.
4. **Loop guard + rate limits** (§7) on `auth.rate_limits`.
5. **Revocation cascade** (§6): make control's `revoke_grant` transactional over both schemas; the
   app-delete companion UPDATE; the re-grant-stability upsert (§6.1).
6. **Dev topology + faithful e2e** (§9, §10): compose mailpit sink, the Postmark-shaped injector, the
   full §10 matrix.
7. **DNS/identity** (§5.1): publish the relay-domain DKIM/SPF/DMARC/MX (prod ops; dev uses the sink).
