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
| **B2** | "rewrite `From:`/`Reply-To:` on the *existing* outbound path" | `mailer::types::Email` has no `reply_to`/`envelope_from`; the Resend driver drops `headers` and has no `Reply-To` (`resend.rs`); the SMTP driver explicitly ignores `headers` (`smtp.rs:117`) | **§3 extends the `Email`/`Mailer` contract** (scoped work) so `reply_to`, `envelope_from`, and `headers` are actually emittable on **all three** drivers (stdout, smtp, resend — the only `impl Mailer` in the tree) |
| **B3** | "reuse the Postmark/SES webhooks; SES inbound POSTs parsed messages" | those webhooks (`bounce.rs`, `sns.rs`) are **delivery-event** webhooks (Bounce/Complaint only, no body/From/To/MIME). SES inbound dumps **raw MIME to S3**, truncates to SNS; only **Postmark inbound** posts parsed JSON | **§4 pins ONE inbound provider (Postmark inbound), specs its parsed-JSON payload + Basic-auth signature**, and keeps it strictly separate from the delivery-event webhooks |
| **B4** | "deleting the grant *cascades* to set `revoked_at`" | no FK (cross-schema), no trigger, no owner, no txn — **and `state.auth_pg` is `Arc<Client>`, so `.transaction()` (which needs `&mut self`) cannot be called on it** | **§6 names control's `revoke_grant` as the owner and opens the cross-schema transaction on a *dedicated owned* `Client` built from the existing `auth_db_url` (the field AppState already carries "for short-lived dedicated sessions"), NOT on the shared `Arc<Client>`** |
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
                       ┌──────────────────────────┐            ┌────────────────────────────┐
                       │  POST /webhooks/relay-in  │            │  relay_forward_mailer       │
                       │  (crates/auth/src/ui)     │──forward──►│  (its OWN Arc<dyn Mailer>,  │
                       │  verify_basic_auth        │   Email    │  §5.2 — SMTP driver, header-│
                       │  → spam gate → idempotency│            │  + envelope-capable §3)     │
                       │  → lookup → suppress/     │            └────────────────────────────┘
                       │    revoke gate → loop     │                        │
                       │    guard → rate limit     │                        ▼
                       └──────────────────────────┘                  user's real inbox
                                  │ (no active map / suppressed / replies)
                                  ▼
                  handler EXPLICITLY emits a bounce email to the original
                  sender (via the outbound mailer), THEN returns 200 to
                  Postmark (a non-2xx makes Postmark RETRY, not bounce — §8)
```

Key properties:

- **No bespoke MX.** Postmark's inbound server is the receiving MX; we own only a thin compio HTTPS
  handler. This honours the AGENTS.md "calls fetch / managed concern" classification — the relay is a
  *driver*, not a native kernel surface.
- **One inbound provider, one signature path.** Postmark inbound authenticates with HTTP Basic auth,
  and the tree **already has** `verify_basic_auth` (`crates/auth/src/mailer/bounce.rs:86`). No new
  signature scheme (this is why Postmark, not Mailgun-HMAC or SES-SNS-RSA — see §4.4).
- **Zero tokio.** The handler is a `ntex` route on the auth service (same place the existing webhooks
  live, `server.rs:140-152`); outbound is the same `Mailer` trait (cyper / `spawn_blocking` lettre),
  but a **second, dedicated `relay_forward_mailer` instance** (SMTP, envelope-from-capable, §5.2a) —
  not the transactional `AUTH_MAILER` one. No new runtime.

---

## 2. Alias generation (unchanged from §7.1 — referenced, not re-derived)

Alias allocation, the partial-unique constraint, generate-and-retry, the consent-time create, and the
read-through gateway cache are **fully specified in the main spec §7.1 + §8.1 DDL** and are sound
(the review confirmed). This sub-spec depends on, but does not re-derive, them. The one row this spec
reads and writes is:

```sql
auth.app_user_identities (
  app_client_id   text NOT NULL,        -- per-app OAuth client_id (oac_<base62>); §6.2
  global_user_id  uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
  pairwise_sub    text NOT NULL,        -- pws_… pairwise sub (deterministic); indexed for reverse-lookup
  relay_email     text,                -- {token}@{relay_domain}; NULL until email scope granted
  created_at      timestamptz NOT NULL DEFAULT now(),
  revoked_at      timestamptz,
  PRIMARY KEY (app_client_id, global_user_id)
)
CREATE INDEX app_user_identities_pairwise_sub_idx
  ON auth.app_user_identities (pairwise_sub);
-- partial-unique: active aliases only
CREATE UNIQUE INDEX app_user_identities_relay_active_idx
  ON auth.app_user_identities (relay_email) WHERE relay_email IS NOT NULL AND revoked_at IS NULL;
```

> Slice-4 reconciliation: `app_client_id` (the `oac_` client_id, §6.2) replaces the round-2 `app_id`
> column name, and `pairwise_sub` is its own column (the natural key is `(app_client_id,
> global_user_id)`, NOT the `pws_`). The §6.2 pin and the three keying sites below are unchanged —
> all key on `app_client_id` = the `oac_` client_id.

The relay alias is the value the gateway projects into `ZeroShip-User.email` and the id_token `email`
claim, so **no real address ever reaches an app** (main spec §7.1 substitution table). This spec covers
what happens to mail *sent to* that alias.

<!-- Added: B1 — the file now exists; §2 anchors to the already-sound §7.1 alias machinery rather than re-deriving it -->

---

## 3. Extending the outbound `Email`/`Mailer` contract (SCOPED Slice-5 work — NOT a reuse)

> **B2 fix.** The round-1 claim "forward via the existing outbound path, rewriting `From:` and
> `Reply-To:`" is **false against the code as-is**: `mailer::types::Email` (`crates/auth/src/mailer/types.rs:9`)
> is `{ to, from, subject, text, html, headers, tags }` with **no `reply_to` and no `envelope_from`**;
> the Resend driver (`resend.rs:51` `ResendRequest`) serializes only `{ from, to, subject, text, html,
> tags }` and **drops `msg.headers` entirely**; the SMTP driver **explicitly ignores `headers`**
> (`smtp.rs:117` "Arbitrary `msg.headers` are intentionally ignored"). So `Reply-To: alias`, the
> `X-ZS-Relay` loop marker (§7), and the envelope/Return-Path rewrite (§5.3) are **un-emittable
> today.** Making them emittable is the **first task of Slice 5.**
>
> **There are exactly THREE `impl Mailer` in the tree** (verified `grep 'impl Mailer for'`):
> `StdoutMailer` (`stdout.rs`), `SmtpMailer` (`smtp.rs`), `ResendMailer` (`resend.rs`); `build_mailer`
> (`main.rs:293`) dispatches `stdout|smtp|resend` and the error string is literally
> `"unknown mailer …; use stdout|smtp|resend"`. **There is NO SES *outbound* driver** — `sns.rs` is an
> inbound SNS *signature verifier* (RSA-SHA1 against the SigningCertURL cert), not a `Mailer`. Every
> "four drivers" / "SES outbound" mention from round 2 is a factual error and is corrected to three
> below. SES-outbound is **explicitly out of scope** for v1 (tracked in §11); it is not needed because
> the forward path uses the SMTP driver (§5.2).

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

**All three drivers must keep compiling against the widened struct (verified caller set):**

- **`StdoutMailer::send`** (`stdout.rs`) — the dev default — gains nothing functional, but its `send`
  must still construct/consume the struct. We extend its `eprintln!`/`tracing::info!` block to print
  `reply_to` and `envelope_from` (so a dev can eyeball the relay header surgery in the terminal); it
  deliberately ignores `headers` beyond a debug-print. The four `Email { … }` literals **inside its
  own test/other modules** must add the two fields. (The struct widening is a compile error until
  every literal in the crate — including the `smtp.rs`/`resend.rs` test fixtures — sets the two new
  fields; §3.4 lists them.)
- **`SmtpMailer`** and **`ResendMailer`** — the functional drivers — are extended in §3.2/§3.3.

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
per-message envelope-from override, so the **Resend driver cannot serve the relay forward path** when
`envelope_from` is `Some`. The relay forward therefore goes out via the **SMTP driver** (§5.2); Resend
stays a valid choice only for transactional auth mail, where envelope-from is the verified send domain
anyway. This is recorded as the **forwarding driver constraint** in §5.2, and is *why §5.2 pins the
relay-forward mailer to SMTP regardless of what `AUTH_MAILER` selects for transactional mail.*

### 3.3 SMTP driver emits `reply_to`, `envelope_from`, `headers` (verified against lettre 0.11.22)

This is the driver the relay forward path runs on (§5.2). Two lettre APIs are load-bearing and both
were **verified against the pinned `lettre = "0.11"` (lock: 0.11.22)**, not assumed:

**(1) Arbitrary `(name, value)` headers — lettre needs a custom `Header` newtype.** lettre 0.11 has
**no** one-call `(name: &str, value: &str)` raw-header API — `Message::builder().header(h)` takes a
typed value implementing the `Header` trait. The `smtp.rs:117` comment already records that this is
"awkward" and was deliberately skipped. So Slice 5 adds a tiny `RawHeader` newtype that implements
`lettre::message::header::Header` for an opaque `(HeaderName, String)`:

```rust
// crates/auth/src/mailer/smtp.rs  (NEW — the raw-header escape hatch the comment said it lacked)
use lettre::message::header::{Header, HeaderName, HeaderValue};

#[derive(Clone)]
struct RawHeader { name: HeaderName, value: String }

impl Header for RawHeader {
    fn name() -> HeaderName { HeaderName::new_from_ascii_str("X-ZS-Placeholder") } // unused; we set per-instance below
    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self { name: HeaderName::new_from_ascii_str("X-ZS-Relay"), value: s.to_owned() })
    }
    fn display(&self) -> HeaderValue {
        HeaderValue::dangerous_new_pre_encoded(self.name.clone(), self.value.clone(), self.value.clone())
    }
}
```

The set of headers the relay actually injects is **closed and ASCII** (`X-ZS-Relay`, and nothing
caller-supplied is forwarded — §5.3 builds the outbound header set from scratch, it does **not** copy
inbound `headers[]`), so the newtype only ever carries platform-controlled `X-` names. `build_lettre_message`
becomes:

```rust
let mut builder = Message::builder().from(from_mbox).to(to_mbox).subject(msg.subject.clone());
if let Some(rt) = &msg.reply_to {
    builder = builder.reply_to(format_mailbox(rt)?);   // lettre 0.11 has .reply_to() — verified
}
for (name, value) in &msg.headers {                    // only X-ZS-Relay in practice
    builder = builder.header(RawHeader::for_pair(name, value)?);
}
let message = /* …existing single-part / multipart body branch, unchanged… */;
```

**(2) Explicit envelope (Return-Path / MAIL FROM) — `SmtpTransport::send_raw` + `Envelope::new`.**
The blocking `SmtpTransport` used in `smtp.rs` implements `Transport`, whose `send_raw(&self,
envelope: &Envelope, email: &[u8])` is **verified present in lettre 0.11.22** (it is the low-level
companion to `send`). The envelope-from is built independently of the message's `From:` header via
`lettre::address::Envelope::new(Some(mail_from), vec![rcpt_to])` — also verified — so we can pin
MAIL FROM to the relay bounce mailbox while `From:` is the relay alias display address:

```rust
use lettre::address::Envelope;

fn build_envelope(envelope_from: Option<&str>, header_from: &str, rcpt: &str)
    -> Result<Envelope, MailerError>
{
    let mail_from: lettre::Address =
        envelope_from.unwrap_or(header_from).parse().map_err(cfg_err)?;   // pin bounce mailbox
    let to: lettre::Address = rcpt.parse().map_err(cfg_err)?;
    Envelope::new(Some(mail_from), vec![to]).map_err(cfg_err)
}
```

The send call changes from `transport.send(&message)` to the explicit-envelope variant, still wrapped
in `spawn_blocking` exactly as today (`smtp.rs:99`):

```rust
let raw = message.formatted();
let envelope = build_envelope(msg.envelope_from.as_deref(), &msg.from.email, &msg.to.email)?;
let send_result = compio::runtime::spawn_blocking(move || transport.send_raw(&envelope, &raw)).await;
```

Transactional auth mail (where `envelope_from` is `None`) keeps using `send(&message)` so its behaviour
is byte-for-byte unchanged; only the relay forward (which always sets `envelope_from`) takes the
`send_raw` arm. **`send_raw` is what makes the entire §5.3 Return-Path story implementable** — without
it the envelope-from pin would have no mechanism, which is the gap the round-2 critique flagged.

**No SES outbound driver is added.** The relay forward runs on SMTP (§5.2); SES-outbound is v1
out-of-scope (§11).

### 3.4 Regression tests for the contract extension

- **All existing `Email { … }` literals updated.** The struct widening (§3.1) is a compile error
  until every `Email` constructor in the crate sets `reply_to`/`envelope_from`. The known set
  (verified): the four template constructors in `mailer/templates.rs`, and the `Email { … }` test
  fixtures in `smtp.rs` (`build_lettre_message_text_only`, `build_lettre_message_with_html`). Each
  adds `reply_to: None, envelope_from: None`. (No `resend.rs` fixture builds an `Email` today — its
  tests build `format_address`/`ResendRequest` directly — but the new serialization test below adds
  one.)
- `resend.rs` test: a `ResendRequest` built from an `Email` with `reply_to: Some(...)` and a
  `headers: [("X-ZS-Relay","1")]` serializes BOTH fields (today they'd be silently dropped — the test
  fails pre-fix).
- `smtp.rs` test 1 (`build_lettre_message` headers): `build_lettre_message` output (`.formatted()`)
  contains a `Reply-To:` line and the `X-ZS-Relay:` header line (today neither appears — the test
  fails pre-fix because headers are dropped and there is no `reply_to` field).
- `smtp.rs` test 2 (`build_envelope` Return-Path): `build_envelope(Some("bounce+x@relay.zeroship.ai"),
  "alias@relay.zeroship.ai", "real@inbox.test")` returns an `Envelope` whose `from()` is the bounce
  mailbox, **not** `alias@…` and **not** `real@inbox.test`. This is the test that proves `send_raw`'s
  envelope is what pins MAIL FROM (the §5.3 Return-Path guarantee), independent of the `From:` header.

<!-- Added: B2 — Email/Mailer contract extension (reply_to, envelope_from, real headers) scoped as Slice-5 work across the THREE drivers (stdout, smtp, resend — there is no SES outbound), with regression tests that fail pre-fix; this is NOT claimed as an existing reuse. Round 3: corrected "four drivers"→three; verified lettre 0.11.22 RawHeader + send_raw + Envelope::new APIs; stdout literals updated to keep the crate compiling -->

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
    /// The literal RCPT-TO mail was delivered to — Postmark fills this even with
    /// catch-all/MailboxHash routing, and it MAY carry a "+hash" suffix and mixed
    /// case (Postmark's own sample is `yourhash+SampleHash@inbound.postmarkapp.com`).
    /// We `normalize_alias()` it (§4.4a) before the exact-match lookup.
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

> **Two status-code corrections from round 2, both load-bearing (verified against Postmark's docs):**
>
> 1. **A non-2xx does NOT bounce to the sender — it makes Postmark RETRY.** Postmark Inbound retries
>    a non-200 webhook on a schedule (1m, 5m, 10m, 15m, 30m, 1h, 2h, 6h — 8 retries), and *"if all of
>    the retries have failed, your Inbound page will show the message as Inbound Error. **Failed
>    messages don't bounce to the original sender;** they remain in Postmark for manual retry via
>    API."* (<https://postmarkapp.com/developer/webhooks/inbound-webhook> + the retries article). So
>    the round-2 story "return 422 → Postmark bounces to the sender" is **factually wrong** and would
>    instead create a 10.5-hour retry storm ending in a silent black hole — the *exact* opposite of
>    "never silent-drop." **Corrected design:** the handler itself **emits the bounce** (an explicit
>    bounce email via the outbound mailer to the original sender) and then returns **200** so Postmark
>    is done. Non-2xx is reserved ONLY for genuinely retryable transient faults (our DB/mailer is
>    momentarily down), where a Postmark retry is exactly what we want.
> 2. `ConsumeResult` is `{ state, consumed: bool }` (`store/ratelimit.rs:22`) — `consume` returns a
>    **boolean**, it does not itself produce a 429. The handler inspects `consumed` and chooses the
>    status. A single bucket's `consumed=false` is a *transient* spike, not "sustained abuse"; §7
>    defines the separate second-tier signal that distinguishes them.

```
0. parse body as InboundMessage (§4.2). Body too large / unparseable → 200 + log (a malformed
      inbound is not retryable; do NOT make Postmark retry a body it will re-send identically).
1. verify_basic_auth(Authorization, cfg.relay_inbound_user, cfg.relay_inbound_password)
      → 401 on mismatch. (Same primitive as the Postmark bounce webhook.)
      An unverified POST is a forwarding/suppression spoof oracle — so this gate is mandatory
      and the handler 401s when the creds aren't configured (mirrors the postmark webhook's
      "always registered, 401 if unconfigured" stance, server.rs:140 comment).
2. idempotency / replay guard (§7.1): dedup on InboundMessage.MessageID. Already seen → 200 + drop,
      NO second forward. (Basic-auth proves the secret, NOT per-message freshness — a captured POST
      replays N forwards without this. New first-class step, was missing in round 2.)
3. loop guard: if any inbound Header is `X-ZS-Relay` → 200 OK + drop (count++ metric). (§7)
4. normalize OriginalRecipient (§4.4a) → the alias key: lowercase the whole address, strip any
      "+tag"/MailboxHash from the local part, before the exact-match lookup.
5. resolve alias → real inbox (§4.5): the JOIN + revocation gate.
      No row / revoked → emit bounce("address no longer active") + 200. (§8)
6. suppression gate: check_suppression(db, real_inbox).
      Suppressed → 200 + drop, NO bounce (a suppressed inbox already bounced/complained once;
      emitting another bounce to the *original sender* leaks nothing useful and risks a loop). (§8)
7. spam/abuse gate (§5.4): if the inbound failed sender authentication or scored as spam, drop + 200
      (do NOT forward, do NOT bounce — bouncing spam back to a forged sender is backscatter).
8. per-alias + per-app rate limit (§7) via store::ratelimit::consume → ConsumeResult.consumed:
      consumed == true  → continue.
      consumed == false → return 503 with Retry-After (Postmark RETRIES the spike later, smoothing
                          a burst) UNLESS the second-tier abuse counter (§7) has tripped, in which
                          case auto-revoke the alias (§7 → control revoke path) and 200 + drop.
9. build the forward `Email` (§5) on the relay_forward_mailer (§5.2) and send.
      mailer Suppressed/Transport error that is transient → 503 (Postmark retries);
      permanent build error → emit bounce + 200.
10. 200 OK to Postmark on success.
```

**Bounces are emitted by us, via the outbound mailer**, to `from_full.email` (the original sender),
from `bounce+<opaque>@relay.zeroship.ai`, with a clear body (e.g. "This address no longer forwards" /
"Replies aren't supported yet"). That outbound bounce is itself subject to `check_suppression`, so we
never bounce-loop against a suppressed sender. The handler returns 200 to Postmark in every
non-transient case so Postmark's retry queue stays empty.

The handler reuses `db: State<Arc<compio_postgres::Client>>` exactly like `ses_sns`
(`webhooks.rs:198`) — same `ntex` + compio shape, `#[allow(clippy::future_not_send)]` — and also
extracts `State<RelayForwardMailer>` (the dedicated relay mailer, §5.2).

### 4.4 Why not Mailgun / SES-SNS for inbound (M-major closed)

The round-1 §7 offered three co-equal providers with three incompatible signature schemes. That left
the anti-spoof story **unbuilt for two of three**: the tree has `verify_basic_auth` (Postmark) and the
SNS-RSA verifier (`sns.rs`) but **no HMAC-SHA256 verifier** for Mailgun's `timestamp+token` inbound
signature. An unverified inbound webhook is a suppression-list / forwarding spoof oracle. **We pin
Postmark Inbound** so there is **exactly one** signature path and it is the one already implemented.
Mailgun and SES-inbound are dropped for v1 (SES-inbound also fails the "parsed JSON" requirement —
it's raw MIME to S3).

### 4.4a Normalizing `OriginalRecipient` before the lookup (minor closed)

The §4.5 JOIN is an **exact** match `WHERE i.relay_email = $1`, and `relay_email` is stored as a
lowercase `{token}@{relay_domain}`. But Postmark's `OriginalRecipient` is the literal RCPT-TO and can
arrive in a shape that won't exact-match a freshly-minted alias:

- **Mixed case.** The domain is case-insensitive per RFC 5321; the local part is technically
  case-sensitive but real-world senders fold it. An inbound `Token@Relay.Zeroship.AI` would miss a
  stored `token@relay.zeroship.ai`.
- **`+tag` / MailboxHash.** Postmark's own example `OriginalRecipient` is
  `"yourhash+SampleHash@inbound.postmarkapp.com"` — i.e. it *does* carry the `+hash`. A subaddress
  suffix on our alias (`token+anything@relay…`) must resolve to the base alias.

**Normalization (applied in step 4, before the lookup):**

```rust
fn normalize_alias(original_recipient: &str) -> Option<String> {
    let (local, domain) = original_recipient.rsplit_once('@')?;
    let local = local.split('+').next().unwrap_or(local);          // strip +tag / MailboxHash
    Some(format!("{}@{}", local.to_ascii_lowercase(), domain.to_ascii_lowercase()))
}
```

Because the alias `token` is generated from a base62 charset (`[0-9A-Za-z]`), lowercasing the local
part is **only** lossy if two minted tokens differ solely by case. To make lowercasing safe we **pin
the alias token to lowercase base36 (`[0-9a-z]`) at generation** (a one-line change to the §7.1
generator's alphabet, recorded here as the relay sub-spec's authoritative call) so the
normalize-on-read can lowercase without ambiguity. `relay_email` is therefore always stored and
compared lowercased, and the §10 matrix adds a test that `Token+Hash@Relay.Zeroship.AI` resolves to
the same alias row as `token@relay.zeroship.ai`.

### 4.5 alias → real inbox is a JOIN + revocation gate + suppression gate (M-major closed)

> The round-1 line "the handler looks up `alias → real inbox` via `auth.app_user_identities`" hid a
> two-table lookup: `app_user_identities` stores `relay_email` and `global_user_id`, **not the real
> address**. The real inbox is `auth.users.email`, reachable only by JOIN.

The exact query the handler runs (active map only, then suppression-gated):

```sql
-- Step 5: alias → real inbox, ACTIVE map only.
SELECT u.email::text AS real_inbox, i.app_client_id, i.global_user_id
FROM auth.app_user_identities i
JOIN auth.users u ON u.id = i.global_user_id
WHERE i.relay_email = $1            -- $1 = normalize_alias(OriginalRecipient) (§4.4a), lowercased, +tag stripped
  AND i.revoked_at IS NULL;         -- revoked alias ⇒ no row ⇒ explicit bounce (§8)
-- 0 rows  → emit bounce ("address no longer active") + 200 to Postmark (§8)
-- 1 row   → continue
```

Then **step 6** is `check_suppression(db, real_inbox)` (the existing
`mailer::check_suppression`, which keys on the recipient address) — a suppressed/complained user stops
receiving forwards, and the inbound is **dropped with 200, NO bounce** (a suppressed inbox already
bounced/complained; bouncing to the *original sender* leaks nothing useful, §4.3 step 6 / §8). A
**revoked/unknown alias** (the 0-row case above) is different — that one **does** get an explicit
bounce to the sender (§8). Only after the lookup + suppression gates pass do we build and send the
forward. The query + both gates are the real path and covered by §10 tests.

`u.email::text` mirrors the existing `users::find_by_id` cast (`users.rs:58` selects `email::text`
because the column is `CITEXT`).

<!-- Added: B3 + two majors — pin ONE inbound provider (Postmark Inbound, parsed JSON, Basic auth = existing verify_basic_auth); NEW route + NEW inbound payload type kept separate from the delivery-event webhooks; spell out the alias→inbox JOIN + revocation gate + suppression gate as the real path; drop Mailgun/SES-inbound (no verifier / not parsed JSON). Round 3: corrected the bounce semantics (Postmark does NOT bounce on non-2xx — it retries then black-holes; the handler emits the bounce itself + returns 200); ConsumeResult.consumed branch made explicit; OriginalRecipient normalization (§4.4a, lowercase + strip +tag); idempotency/replay guard + spam gate added as first-class steps -->

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
| **DKIM** | `<selector>._domainkey.relay.zeroship.ai` TXT, key from the **relay-forward SMTP provider** (§5.2) | the relay signs every forward; DMARC aligns on this |
| **SPF** | `relay.zeroship.ai TXT "v=spf1 include:<relay-forward-smtp-spf> -all"` | authorizes the **relay-forward SMTP** sending IPs (§5.2) for the relay envelope domain — see the SPF-coverage note below |
| **DMARC (warm-up)** | `_dmarc.relay.zeroship.ai TXT "v=DMARC1; p=none; rua=mailto:dmarc@zeroship.ai; ruf=mailto:dmarc@zeroship.ai"` | **starts at `p=none`** so aggregate reports surface SPF/DKIM-alignment problems *before* any real forward is quarantined |
| **DMARC (steady)** | escalate to `p=quarantine` after the warm-up soak (alignment ≈100% in `rua`) | the relay's enforced policy once warmed |
| **MX** | `relay.zeroship.ai MX → Postmark inbound MX` | inbound mail for `*@relay.zeroship.ai` lands at the Postmark inbound server (§4) |

> **SPF coverage (minor closed).** §5.2 sends forwards via the **relay-forward SMTP driver**, whose
> sending IPs may differ from the transactional `AUTH_MAILER` provider's. The relay-domain SPF
> `include:` MUST cover **exactly those relay-forward IPs**; if the relay-forward endpoint and the
> transactional provider differ, the record must `include:` **both** (or SPF `-all` will hard-fail
> legitimate forwards). The relay-forward provider's published SPF include (e.g. its
> `spf.<provider>.com`) is the authoritative value to embed; it is **not** assumed to equal whatever
> the transactional provider uses.
>
> **DMARC ramp + independent warm-up (minor closed).** Publishing `p=quarantine` from day one with no
> aggregate-report soak risks silently quarantining real forwards during warm-up. We publish `p=none`
> with `rua`/`ruf` first (industry-standard 30-day-class soak: `p=none` → verify alignment ≈100% →
> `p=quarantine`; `p=reject` is a later, optional step). `relay.zeroship.ai` gets its **own** warm-up
> and does **not** ride the transactional auth domain's reputation — see §5.5 reputation isolation.

### 5.2 Forwarding driver: re-originate + SRS, not blind relay

Because we **rewrite `From:`** to the relay (privacy — the user's real inbox must not see the third
party's domain as `From`, and the third party must not learn the real inbox), the original DKIM
signature is invalid on the body-as-sent anyway. So the forward is a **fresh send from the relay
identity**, not a transparent relay:

- The **forwarding driver is the header-capable SMTP path** (§3.3), because it can set the SMTP
  **envelope-from** (Return-Path) to the relay bounce mailbox via `send_raw` — Resend's HTTP API cannot
  override envelope-from per message (§3.2). **This requires a SECOND mailer, fully specced in §5.2a**
  — the existing single `AUTH_MAILER` mailer is NOT reused, because in prod it is very likely
  `AUTH_MAILER=resend` (the obvious transactional choice), and a Resend mailer in hand cannot pin
  envelope-from at all. Transactional auth mail keeps using whatever `AUTH_MAILER` selects; the relay
  forward path uses its **own** dedicated SMTP mailer with the **relay sending identity**.
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

### 5.2a The second mailer — construction + injection (BLOCKER closed)

> The round-2 spec asserted "the relay forward path selects the SMTP driver" but never specced the
> *construction* or *injection* of a second mailer. Against the actual wiring this is unbuildable:
> `build_mailer` (`main.rs:293`) constructs **exactly one** `Arc<dyn Mailer>` from `AUTH_MAILER`,
> `server.rs:227/238` injects that one instance with `.state(mailer.clone())`, and handlers extract
> `State<Arc<dyn Mailer>>`. There is **no** second mailer, no second `build_mailer` call, and no DI
> slot. This section adds all three. Two distinct sending identities/credential sets coexist:
> the **transactional** mailer (magic-link/reset/verify) and the **relay-forward** mailer.

**(1) A newtype around the second `Arc<dyn Mailer>` so ntex `State` can distinguish them.** ntex's
`State<T>` keys on the concrete type, so two `Arc<dyn Mailer>` states would collide. Wrap the relay
one:

```rust
// crates/auth/src/mailer/mod.rs
#[derive(Clone)]
pub struct RelayForwardMailer(pub Arc<dyn Mailer>);
```

Transactional handlers keep extracting `State<Arc<dyn Mailer>>`; the relay handler extracts
`State<RelayForwardMailer>`. No collision, no ambiguity.

**(2) A second constructor + its own config block.** `build_relay_forward_mailer(cfg)` mirrors
`build_mailer` but is keyed on `AUTH_RELAY_FORWARD_MAILER` (default `smtp`) and reads a **separate**
`AUTH_RELAY_SMTP_*` config block (so the relay sending identity/credentials are independent of the
transactional `AUTH_SMTP_*`). It rejects `resend` for the relay role (Resend can't pin envelope-from,
§3.2):

```rust
// crates/auth/src/main.rs (next to build_mailer)
fn build_relay_forward_mailer(cfg: &AuthConfig) -> Result<RelayForwardMailer, AuthError> {
    match cfg.relay_forward_mailer.as_str() {     // AUTH_RELAY_FORWARD_MAILER, default "smtp"
        "smtp" => {
            let host = cfg.relay_smtp_host.clone().ok_or_else(|| AuthError::Config(
                "AUTH_RELAY_SMTP_HOST is required when --relay-forward-mailer=smtp".into()))?;
            let driver = SmtpMailer::new(&SmtpConfig {
                host,
                port: cfg.relay_smtp_port,            // AUTH_RELAY_SMTP_PORT
                username: cfg.relay_smtp_username.clone(),
                password: cfg.relay_smtp_password.clone(),
                use_starttls: cfg.relay_smtp_starttls,
            }).map_err(|e| AuthError::Config(format!("relay smtp mailer: {e}")))?;
            Ok(RelayForwardMailer(Arc::new(driver)))
        }
        "stdout" => Ok(RelayForwardMailer(Arc::new(StdoutMailer))),   // dev: forward → terminal
        other => Err(AuthError::Config(format!(
            "AUTH_RELAY_FORWARD_MAILER={other:?} unsupported; relay forward needs envelope-from \
             control — use smtp (or stdout in dev). resend cannot pin envelope-from (§3.2)."))),
    }
}
```

The relay SMTP config also carries the **relay sending identity** — the `From:`/envelope domain is
`relay.zeroship.ai` (`RELAY_DOMAIN`, §9), and the bounce mailbox `bounce+<opaque>@relay.zeroship.ai`
(§5.3) — so this credential set is bound to the relay-domain-verified DKIM/SPF (§5.1), distinct from
the transactional domain.

**(3) Injection.** `main.rs` builds both at boot and `server::run` threads both into ntex:

```rust
// main.rs
let mailer: Arc<dyn Mailer> = build_mailer(&cfg)?;                         // transactional (existing)
let relay_forward_mailer: RelayForwardMailer = build_relay_forward_mailer(&cfg)?;   // NEW
// …
server::run(cfg, admin, db, google_jwks, mailer, relay_forward_mailer).await
```

```rust
// server.rs run(): one more param + one more .state()
.state(mailer.clone())
.state(relay_forward_mailer.clone())   // NEW — State<RelayForwardMailer> for relay_inbound
```

Both are constructed during the cheap pre-boot validation pass (alongside the existing
`build_mailer` call at `main.rs:84`), so a misconfigured relay SMTP block fails fast with a named env
var, exactly like the transactional mailer does today.

> **Why a separate credential set, not just `AUTH_MAILER=smtp` reused:** even when transactional mail
> *is* SMTP, the relay must sign with the **relay** domain's DKIM and send from the **relay**
> envelope identity for §5.1 alignment and §5.5 reputation isolation — a different host/identity than
> the transactional auth domain. Two identities is the design, not an accident.

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

### 5.4 Inbound content / spam gate BEFORE forward (major closed — content-risk + reputation)

> Re-originating a third party's content under the relay's own `From`/DKIM means **vouching, with the
> relay's reputation, for whatever reached the alias** — receipts and newsletters, but also spam and
> phishing aimed at the alias. With no gate, forwarding spam under relay DKIM torches
> `relay.zeroship.ai`'s reputation — the exact failure Apple/Fastmail/SimpleLogin mitigate by
> filtering inbound **before** forward (SimpleLogin runs DMARC/SPF/DKIM checks on the original sender
> pre-forward; its anti-phishing docs describe exactly this). Round 2 had **no** spam/content step in
> §4.3's gate list. Added here as step 7 of §4.3:

- **Rely on Postmark Inbound's own spam scoring + sender authentication, and read it from the inbound
  payload.** Postmark Inbound runs the message through SpamAssassin and surfaces the verdict in the
  message `Headers` (`X-Spam-Status`, `X-Spam-Score`) and includes the original
  `Authentication-Results` / SPF / DKIM result headers. The handler inspects those:
  - `X-Spam-Status: Yes` (or score ≥ a configured threshold, default 5.0) ⇒ **drop + 200, no forward,
    no bounce** (bouncing spam to a forged `from_full` is backscatter — which would itself harm relay
    reputation).
  - Original-sender DMARC **fail** (the inbound `Authentication-Results` shows SPF+DKIM both fail /
    `dmarc=fail`) ⇒ same drop. We are not willing to re-sign a message its own domain disowns.
- **This gate runs before the forward build (§4.3 step 7), after the cheap auth/loop/lookup gates** so
  we never spend a forward on spam, and never let spam ride relay DKIM.
- A §10 test posts an `InboundMessage` whose `Headers` carry `X-Spam-Status: Yes` and asserts no
  forward reaches the sink (drop + 200), and a second posts a `dmarc=fail` `Authentication-Results`
  and asserts the same.

### 5.5 Reputation isolation for `relay.zeroship.ai` (major closed)

> A relay-reputation hit must **not** poison magic-link / password-reset deliverability. Transactional
> auth mail and relay-forwarded mail are different risk classes (one is fully platform-authored, the
> other re-originates arbitrary inbound), so they must not share a reputation surface — the
> standard isolation pattern (separate subdomain + separate sending IP pool; a spam-complaint spike on
> one subdomain on a shared IP drags down every domain on that IP).

- **Separate sending domain:** relay forwards send from `relay.zeroship.ai`; transactional auth mail
  sends from the auth domain (`auth.zeroship.ai` / the `AUTH_MAILER` verified domain). Different DKIM
  `d=`, different SPF, different DMARC org (§5.1).
- **Separate IP pool:** the relay-forward SMTP identity (§5.2a) is provisioned on a **dedicated
  sending IP pool**, not the transactional pool. A reputation hit from forwarded content stays on the
  relay pool and cannot degrade magic-link/reset deliverability. (This is an ops/provisioning
  requirement on the relay-forward provider, recorded in the §12 build order under DNS/identity.)
- **Independent warm-up:** the relay domain + IP pool warm up on their own ramp (§5.1 DMARC `p=none`
  soak); they do not inherit the transactional domain's standing.

<!-- Added: M1 + M2 — concrete DNS/identity plan (relay DKIM selector, SPF include, DMARC, MX), SRS envelope rewrite via the header-capable SMTP driver, ARC scoping, and the full strip/rewrite header table pinning Return-Path/Sender/X-Original-To/Delivered-To/Received; bounces route to the relay's own handler. Round 3: §5.2a fully specs the SECOND mailer (newtype State, build_relay_forward_mailer, AUTH_RELAY_SMTP_* config, injection); §5.4 adds the inbound spam/DMARC gate before forward; §5.5 adds reputation isolation (separate domain + IP pool + warm-up); §5.1 adds the DMARC p=none ramp + SPF coverage of the relay-forward IPs -->

---

## 6. Lifecycle / revocation cascade — owned dedicated-client transaction, cross-schema, single-DB (B4 closed)

> **B4 fix.** The round-1 line "deleting the `control.oauth_grants` row cascades: set
> `app_user_identities.revoked_at = now()`" asserted a cross-schema, cross-service UPDATE with **no
> owner, no FK, no transaction, no mechanism**. A grant deleted in control with the auth UPDATE never
> firing leaves a **live relay alias forwarding to the real inbox after the user revoked** — the exact
> privacy failure the design exists to prevent.

> **Round-3 BLOCKER correction — the cascade does NOT compile as round-2 wrote it.** Round 2's code was
> `let mut tx = state.auth_pg.transaction().await?;`. But `compio_postgres::Client::transaction`
> requires **`&mut self`** (verified `client.rs:613` and `generic_client.rs:231`), and
> `AppState.auth_pg` is **`Arc<compio_postgres::Client>`** (verified `lib.rs:162`) — a single shared
> connection behind an `Arc`, from which you can only get `&Client`, never `&mut`. So
> `state.auth_pg.transaction()` is a type error. This is the **same `Arc<Client>`-vs-pool problem the
> MAIN spec already diagnosed and fixed for the gateway** (main spec lines 519–522 / 1070–1076:
> *"`AppState.db` is a single `Arc<Client>`, not a pool, and a transaction needs `&mut self`"* →
> migrate to a `Pool`). The sub-spec re-introduced it for control and asserted the opposite ("the
> mechanism already exists … needs no new infrastructure"). It needed new infrastructure after all.

**Owner: control's `revoke_grant`.** AGENTS.md guarantees one Postgres instance with `control` and
`auth` as separate schemas, so a single cross-schema transaction is *physically* possible — the only
question is **which client object opens it**, and `Arc<Client>` cannot. AppState already carries the
answer: **`auth_db_url`** — the connection URL the field doc (`lib.rs:163-165`) says exists *"for
short-lived dedicated sessions"*. The cascade opens a **fresh, owned `Client`** on that URL (the exact
pattern `http_util.rs:109` already uses: `compio_postgres::connect(&url, NoTls)` + spawn the
connection task), giving an **owned `mut` client** that CAN open a transaction:

```rust
// crates/control/src/oauth_grants_handlers.rs::revoke_grant
// Open a dedicated owned client for the cross-schema transaction. auth_db_url is the same
// physical DB as auth_pg, but owned (mut) — auth_pg (Arc<Client>) can't open a txn.
let (mut conn, connection) = compio_postgres::connect(&state.auth_db_url, NoTls).await?;
compio::runtime::spawn(async move { let _ = connection.run().await; }).detach();

let mut tx = conn.transaction().await?;               // &mut conn — compiles; one DB, both schemas
tx.execute(
    "DELETE FROM control.oauth_grants WHERE user_id = $1 AND client_id = $2",
    &[&authz.principal_id, &client_id]).await?;
// SAME txn — revoke the relay alias for THIS (app, user). app_user_identities.app_client_id holds
// the per-app OAuth client_id (oac_<base62>, §6.2), so the alias row is keyed DIRECTLY by client_id —
// no join, exact-match on the SAME value the explicit-revoke path already has in hand.
tx.execute(
    "UPDATE auth.app_user_identities
        SET revoked_at = now()
      WHERE app_client_id = $2                          -- $2 = client_id (oac_…), §6.2
        AND global_user_id = $1
        AND revoked_at IS NULL",
    &[&authz.principal_id, &client_id]).await?;
tx.commit().await?;                                   // atomic: grant gone ⇔ alias revoked
// only AFTER commit: best-effort Hydra consent-session revoke (already in the handler)
hydra_revoke_consent_sessions(...).await;
```

> **Scoped as Slice-5 infra work, not assumed-away.** Opening a dedicated `Client` per revoke is
> acceptable (revoke is a rare, user-initiated action — not a hot path). If profiling later shows it
> matters, the *better* long-term fix is to **migrate `AppState.auth_pg` from `Arc<Client>` to a
> compio-postgres `Pool`** (the same migration the main spec applied to the gateway,
> `compio-postgres/src/pool.rs` `PooledClient`) and check out a `mut` pooled conn in `revoke_grant`.
> Either option is real infrastructure; §12 lists it as explicit Slice-5 work. The round-2 "needs no
> new infrastructure" claim is retracted.

**`app_user_identities.app_client_id` holds the per-app OAuth `client_id` (`oac_<base62-app-id>`) —
pinned (major closed); the column is NAMED `app_client_id` (Slice 4) so the pin is explicit.** The
main-spec DDL types it as `TEXT`; this sub-spec pins its content to the **per-app client_id** for
three reasons: (1) the explicit-revoke path has `client_id` directly as its path param, so the
UPDATE is a plain equality with **no cross-schema join** (round 2's `JOIN control.app_oauth_clients
… c.app_id::text` was both an extra dependency and a *different key* than the app-delete companion
used — the bug the critique flagged); (2) control can map a deleted app's `uuid → client_id`
deterministically via `client_id_for_app(uuid)` (`app_oauth_client.rs:133`, `oac_<base62(uuid)>`) so
app-delete keys the **same** column the same way; (3) the gateway already has `route.oauth_client_id`
in hand when it upserts the identity row (Slice 4, `router::identities::upsert`), so the WRITE and
both revoke paths agree on one value. **All three sites (gateway upsert, explicit-revoke UPDATE,
app-delete UPDATE) key on `app_client_id` = the `oac_` client_id.** The §10 matrix asserts the
gateway writes `client_id` into `app_client_id` and that both UPDATEs match it.

**Failure semantics (named, per the review's demand):**

- **DELETE + UPDATE are one transaction** → it is impossible to commit one without the other. There is
  no window where the grant is gone but the alias still forwards.
- **Hydra revoke runs *after* commit** (as today) and is best-effort: if it fails the grant + alias
  are already revoked (fail-safe — the privacy-critical state committed first); the handler returns 500
  so the operator retries the Hydra leg, but the alias is **already dead**.
- **Cross-service concurrency contract for the shared `revoked_at` column (major closed).** Two
  services write `revoked_at` on the same `(app_id, global_user_id)` row: **control's `revoke_grant`**
  sets it (above), and **auth's `accept_consent`** clears it on re-grant (§6.1). A naive interleave
  (revoke commits `revoked_at=now()`, a concurrent re-consent's `COALESCE` re-clears it) could leave a
  **live alias for a just-revoked grant**. The contract that prevents it:
  - **The invariant is `grant absent ⇒ alias revoked`.** The `control.oauth_grants` row is the single
    source of truth for "is this grant live"; the alias's `revoked_at` must agree with it.
  - **Both writers take the row lock on the *grant* row first.** Control's transaction `DELETE`s the
    `control.oauth_grants` row (acquiring its row lock) **before** the alias UPDATE, in the same txn.
    Auth's `accept_consent` `INSERT … ON CONFLICT` on `control.oauth_grants` (the grant write, main
    spec §5.2) **also** locks that row, in the same txn that clears the alias `revoked_at`. So the two
    transactions **serialize on the `control.oauth_grants` row** — they cannot interleave the way the
    critique described. The auth-local consent **advisory** lock (main spec §7.1) still guards
    concurrent *first-consents*, but the cross-service ordering is enforced by the shared grant-row
    lock, which both txns must hold.
  - **Canonical ordering:** whichever transaction commits **last** wins, and because both touch the
    grant row first, "last to commit on the grant row" is also "last to set the alias state" — they
    are consistent. If revoke commits last: grant absent + alias revoked (correct). If re-consent
    commits last: grant present + alias active (correct, the user just re-granted). There is **no**
    state where grant is absent but alias is active. A §10 concurrency test races a `revoke_grant`
    against an `accept_consent` re-grant on the same `(app, user)` and asserts the terminal state
    satisfies the invariant in both commit orders.
- **The three revoke paths differ and are each pinned:**
  - **Explicit user revoke** (`DELETE /me/oauth-grants/{client_id}`): the path above; keys on
    `client_id` directly.
  - **App delete** (`control.apps` row deleted, `api.rs:delete_app`): control DB rows
    `ON DELETE CASCADE` from `control.apps` → `control.app_oauth_clients` → `control.oauth_clients`,
    but **there is NO cross-schema FK** from `auth.app_user_identities` to anything in control
    (intentional — the auth schema may live in a separate cluster, `lib.rs:158-161`). **So the
    app-delete companion UPDATE is the ONLY thing preventing orphaned live aliases.** `delete_app`
    resolves the deleted app's `client_id = client_id_for_app(uuid)` and runs, **in the app-delete
    path**, the same-keyed UPDATE on a dedicated `auth_db_url` client:
    `UPDATE auth.app_user_identities SET revoked_at = now() WHERE app_client_id = <oac_client_id> AND
    revoked_at IS NULL` — revoking **all** of that app's aliases (every user), keyed on the **same
    `client_id`** the explicit path uses. (It cannot be in the `delete_app` control-schema transaction
    because that txn runs on the control registry client, not `auth_db_url`; it runs as an immediately-
    following statement, and a §10 test asserts app-delete revokes all that app's aliases so a dropped
    companion statement is caught.)
  - **Signout** is **not** a revoke — it ends the session, not the grant; the alias stays active so the
    user keeps getting app mail across logins. (Explicitly NOT cascaded.)
- **Re-grant keeps the SAME alias** (clears `revoked_at` on the deterministic `pws_` row) — see §6.1.

A §10 regression test does the full loop: grant → forward-to-alias succeeds → `revoke_grant` →
inbound to the same alias now **emits a bounce + 200** (the `revoked_at IS NULL` gate in §4.5 fails →
§8) → assert the forward did NOT reach the real inbox, and assert the `DELETE` + `revoked_at` UPDATE
committed **atomically** (a partial commit would leave the alias live).

<!-- Added: B4 — name the owner (control revoke_grant). Round 3: FIX the non-compiling cascade — auth_pg is Arc<Client> and transaction() needs &mut, so open a dedicated owned Client from auth_db_url (the field that exists for exactly this); pin app_user_identities.app_id to the oac_ client_id and key gateway-write + both revoke UPDATEs on it consistently (no cross-schema join); define the cross-service concurrency contract on the shared revoked_at (serialize on the control.oauth_grants row, invariant grant-absent⇒alias-revoked); state the no-cross-schema-FK consequence makes the app-delete companion UPDATE load-bearing; correct the revoked-alias outcome to explicit-bounce+200 (§8) -->

### 6.1 Alias rotation on re-grant — UX consequence flagged (minor closed)

Rotation itself is sound (partial-unique on active aliases, generate-and-retry, deterministic `pws_`
reused). The **deliverability/UX consequence the review flagged:** after re-grant the OLD alias is
revoked and all future inbound to it **bounces** (§4.5 miss → explicit bounce + 200, §8), so a user
who re-grants periodically accumulates dead aliases that newsletters/receipts still hold, producing a
stream of bounces and lost mail with no grace window.

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
>
> **This upsert is the auth-side writer of `revoked_at` in the §6 cross-service contract.** It runs in
> `accept_consent`'s grant transaction — the **same** transaction that writes the
> `control.oauth_grants` row — so it holds that grant row's lock before clearing `revoked_at`,
> serializing against control's `revoke_grant` (which holds the same row's lock). That is what makes
> the `COALESCE … revoked_at = NULL` safe against a concurrent revoke: the two never interleave on the
> alias because they queue on the grant row. See §6's concurrency contract for the invariant proof.

<!-- Added: minor — re-grant keeps the alias stable (Apple Hide-My-Email model) instead of rotating, avoiding the dead-alias bounce stream; supersedes §7.5's "fresh alias on re-grant" -->

### 6.2 What `app_user_identities.app_client_id` holds — the per-app `client_id` (authoritative)

The main-spec DDL (`§8.1`) types `app_user_identities.app_client_id` as **`TEXT`**. **This sub-spec
pins its content to the per-app OAuth `client_id`, `oac_<base62-app-id>`** (`client_id_for_app(uuid)`,
`app_oauth_client.rs:133`) — and Slice 4 NAMES the column `app_client_id` so the pin is explicit at
the schema level (no uuid-as-text ambiguity). Rationale and the three consistent sites:

| Site | Has in hand | Keys `app_client_id` as |
|---|---|---|
| **Gateway upsert** of the identity row (when projecting `ZeroShip-User`, `router::identities::upsert`) | `route.oauth_client_id` | writes `client_id` |
| **Explicit revoke** (`revoke_grant`, §6) | the `{client_id}` path param | `WHERE app_client_id = client_id` (no join) |
| **App delete** (`delete_app`, §6) | the app `uuid` → `client_id_for_app(uuid)` | `WHERE app_client_id = client_id` (same value) |

Because `client_id` is **deterministic from the app uuid** (`oac_<base62(uuid)>`), control can always
reconstruct it from a deleted app's uuid without a lookup, so the app-delete companion UPDATE keys on
**exactly the same value** as the gateway wrote and the explicit-revoke path uses. This closes the
critique's "the 'same UPDATE' is not actually the same key" hazard: there is now **one** key,
`app_client_id` = the `oac_` client_id, at all three sites. (Slice 4 implements the gateway-upsert
leg in `crates/gateway/src/identities.rs`, keyed on `(app_client_id, global_user_id)`.)

<!-- Added: major — pin app_user_identities.app_id to the per-app oac_ client_id; make the gateway write, explicit-revoke UPDATE, and app-delete UPDATE all key on that single deterministic value (no cross-schema join, no key mismatch); note the absence of a cross-schema FK makes the app-delete companion UPDATE the sole guard against orphaned live aliases -->

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
  `crate::store::ratelimit::consume(conn, key, capacity, refill_per_sec) -> ConsumeResult` — the
  **same** primitive the login/forgot/magic/signup paths use. `consume` returns
  `ConsumeResult { state, consumed: bool }` (`store/ratelimit.rs:22`); it does **not** itself produce
  a status — the handler branches on `consumed` (round-2 wrote "Over limit ⇒ 429" as if `consume`
  signalled it; it does not). Two buckets per inbound message, with **pinned** capacity/refill:
  - **per-alias:** `key = "relay:alias:{relay_email}"`, `capacity = 20.0`, `refill_per_sec = 0.0167`
    (≈ 60/hour steady, burst 20). Caps one alias being blasted; sized so a busy newsletter/receipt
    alias is never throttled but a flood is.
  - **per-app:** `key = "relay:app:{client_id}"`, `capacity = 200.0`, `refill_per_sec = 0.333`
    (≈ 20/min steady, burst 200). Caps an app's total inbound forward volume.
  - **The `consumed` branch (explicit):**
    ```
    let alias_ok = consume(conn, alias_key, 20.0, 0.0167).await?.consumed;
    let app_ok   = consume(conn, app_key,   200.0, 0.333 ).await?.consumed;
    if !(alias_ok && app_ok) {                       // over a bucket
        if over_limit_streak(conn, alias_key) >= ABUSE_STREAK {   // §below — second-tier signal
            auto_revoke_alias(...);                   // crosses into control's revoke path
            return Ok(http_200_drop());               // sustained abuse: drop, no retry
        }
        return Ok(http_503_retry_after(retry_secs));  // transient spike: 503 → Postmark RETRIES
    }
    ```
- **"Sustained abuse" is a SEPARATE, measurable second-tier signal — one bucket's `consumed=false`
  cannot distinguish a transient spike from sustained abuse (major closed).** A leaky bucket gives a
  single boolean per call; "sustained" needs a *count of consecutive over-limit windows*. We add a
  second `auth.rate_limits` counter bucket keyed `"relay:abuse:{relay_email}"` that is **incremented
  only when the per-alias bucket reports `consumed=false`** and **reset on any successful forward**;
  when its running count crosses `ABUSE_STREAK` (default 5 consecutive over-limit windows) the alias
  is auto-revoked. This is a deliberate, observable threshold, not an inference from one boolean.
- **The auto-revoke crosses services, and the auth-side handler CANNOT do it transactionally (major
  closed).** Per §6 the revoked_at write that respects the cross-service contract is owned by
  **control's revoke path** (it must serialize on the `control.oauth_grants` row). The auth-side relay
  handler therefore does **not** UPDATE `app_user_identities` directly; it calls control's internal
  revoke endpoint (an admin-authenticated `POST` to control, analogous to the existing control↔auth
  admin calls) which runs the §6 transactional cascade. If that call fails, the handler still returns
  200 + drop for *this* message (the rate limiter already blocked the flood); the revoke is retried
  out-of-band. (Naming who performs the revoke was an explicit ask in the critique.)
- **Bounce/complaint of a forward** feeds `auth.email_suppressions` through the **existing delivery
  webhooks** (`/webhooks/postmark`, `/webhooks/ses-sns`) — these are the *delivery-event* webhooks, the
  correct ones for this (unlike the round-1 conflation). A suppressed real inbox then fails the §4.5
  suppression gate for **all** that user's aliases.

### 7.1 Idempotency / replay protection on `MessageID` (minor closed)

`verify_basic_auth` proves the request carries the shared Basic-auth secret — it is **not** a
per-message signature, so a captured valid inbound POST can be **replayed** to re-trigger a forward
(amplification: one captured legit inbound → N forwards to the real inbox). Basic auth over TLS gives
confidentiality of the creds at rest, not message freshness, so replay is in-scope. Postmark Inbound
carries a stable `MessageID`; the handler dedups on it (§4.3 step 2):

- **Dedup store:** a short-TTL key in the existing KV/`auth.rate_limits`-style table — concretely a
  row `relay_seen:{MessageID}` with a 24h TTL (longer than Postmark's ≤6h retry window, so a genuine
  Postmark *retry* of a message we already forwarded is also deduped to one forward). `setIfAbsent`
  semantics: first POST inserts and forwards; a replay/retry finds the key present ⇒ **200 + drop, no
  second forward**.
- **Interaction with Postmark retries:** because we return 200 on the happy path, Postmark does not
  retry a forwarded message anyway; the dedup is belt-and-suspenders for (a) a malicious replay and
  (b) the edge where our 200 is lost in transit and Postmark retries a message we *did* forward.
- A §10 replay test posts the **same** `MessageID` twice and asserts **exactly one** forward reaches
  the sink.

<!-- Added: M-major — loop protection tied to the now-emittable X-ZS-Relay header (stamp + detect + hop cap); rate limits named to auth.rate_limits with PINNED capacity/refill for both buckets; the ConsumeResult.consumed branch made explicit (consume returns a bool, not a 429); "sustained abuse" defined as a separate consecutive-over-limit counter (ABUSE_STREAK) since one bucket's consumed=false can't distinguish spike from abuse; the auto-revoker NAMED (control's revoke path, not the auth handler, per §6's cross-service contract); §7.1 adds MessageID idempotency/replay protection -->
<!-- Round-3 status correction: transient over-limit returns 503 (Postmark RETRIES) not 429-as-bounce; there is no "422-bounce" from the rate limiter -->

---

## 8. v1 reply handling — WE emit the bounce, never silent-drop (O7, decided; round-3 mechanism fix)

> **Round-3 correction — Postmark does NOT bounce to the sender on a non-2xx webhook response.** The
> round-2 story rested on "return 422 → Postmark generates a bounce to the original sender." Postmark's
> documented inbound behaviour is the opposite: a non-200 is a **retry** signal (8 retries over
> ~10.5h), and *"if all of the retries have failed, your Inbound page will show the message as Inbound
> Error. **Failed messages don't bounce to the original sender;** they remain in Postmark for manual
> retry via API"* (<https://postmarkapp.com/developer/webhooks/inbound-webhook>). So a 422 would cause a
> 10.5-hour retry storm ending in a **silent black hole** — exactly what "never silent-drop" forbids.

v1 is **app → user one-way**. A user reply *to* a relay alias (sender is the real user, recipient is
the alias) OR any inbound we cannot forward (no active map / revoked alias / reply-not-supported) is
**bounced — by us — never silently dropped**:

- **The handler itself emits the bounce email** via the outbound mailer (`relay_forward_mailer`,
  §5.2a) to `from_full.email` (the original sender), from `bounce+<opaque>@relay.zeroship.ai`, with a
  clear body. Then it returns **200** to Postmark so Postmark's retry queue stays empty.
- **Status-code policy (pinned):**
  - **200 + we-emit-a-bounce** — unforwardable-but-known cases: no active map, revoked alias,
    reply-not-supported. The sender gets a real delivery-failure (our bounce), not a black hole.
  - **200 + silent drop (NO bounce)** — spam/DMARC-fail (§5.4) and suppressed-inbox (§4.3 step 6):
    bouncing to a forged/suppressed sender is backscatter and would harm relay reputation.
  - **503 (retryable)** — only genuine transient faults (our DB/mailer momentarily down) and transient
    rate-limit spikes (§7), where a Postmark retry is the desired behaviour.
- The bounce copy is "Replies to this address aren't supported yet" for the reply case, and "This
  address no longer forwards" for the revoked/unknown-alias case.
- **The emitted bounce is itself `check_suppression`-gated**, so we never bounce-loop into a suppressed
  sender.

A §10 test asserts that an inbound to a revoked alias produces an **actual bounce email in the sink**
addressed to the original sender (not merely a non-2xx), and a 200 to Postmark — proving the
"never-silent-drop" guarantee is grounded on a bounce *we* send, not on unverified provider behaviour.

Two-way reply re-injection (alias → the app, so the app sees the user's reply) is **v2** (§11).

<!-- Added: minor — Postmark inbound does NOT bounce on non-2xx (verified against its docs: non-200 = retry, exhaustion = Inbound Error, no upstream bounce); rewrite so the HANDLER emits the bounce via the outbound mailer + returns 200; pin the 200-bounce / 200-silent-drop / 503-retry status policy; re-ground the never-silent-drop guarantee on a bounce we send -->

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
- **Outbound in dev** = the dedicated `relay_forward_mailer` (§5.2a), set to `smtp` and pointed at the
  mailpit sink, so the forward (with its rewritten `From`/`Reply-To`, stamped `X-ZS-Relay`, pinned
  envelope-from via `send_raw`) actually lands in the sink and the e2e asserts the header surgery (§10)
  against a real rendered message. (In dev, `AUTH_RELAY_FORWARD_MAILER=stdout` is also valid — the
  forward prints to the terminal — but the e2e uses `smtp` so it can assert the rendered envelope.)

The dev config knobs (auth `config.rs`, mirroring the existing `AUTH_MAILER` / `AUTH_SMTP_*` /
`postmark_webhook_*`). The relay-forward SMTP block is **separate** from the transactional `AUTH_SMTP_*`
so the relay sending identity/credentials are independent (§5.2a):

```
RELAY_DOMAIN                  = relay.zeroship.localhost   # prod: relay.zeroship.ai
AUTH_RELAY_INBOUND_USER       = <basic-auth user>          # verify_basic_auth, §4.3 step 1
AUTH_RELAY_INBOUND_PASSWORD   = <basic-auth pass>
AUTH_RELAY_FORWARD_MAILER     = smtp                       # forward path forces SMTP (envelope-from), §5.2a
                                                           #   resend is REJECTED here (can't pin envelope-from)
AUTH_RELAY_SMTP_HOST          = mailpit                    # relay-forward SMTP host (prod: relay provider SMTP)
AUTH_RELAY_SMTP_PORT          = 1025                       # mailpit's SMTP port in dev
AUTH_RELAY_SMTP_USERNAME      = <relay smtp user>          # prod only; mailpit needs none
AUTH_RELAY_SMTP_PASSWORD      = <relay smtp pass>          # prod only
AUTH_RELAY_SMTP_STARTTLS      = false                      # dev sink is plaintext; prod true
```

Config fields added to `AuthConfig` (`config.rs`, alongside `smtp_*`): `relay_forward_mailer: String`
(default `smtp`), `relay_smtp_host: Option<String>`, `relay_smtp_port: u16` (default 587),
`relay_smtp_username/password: Option<String>`, `relay_smtp_starttls: bool`, `relay_domain: String`,
`relay_inbound_user/password: Option<String>`. The `Debug` impl redacts `relay_smtp_password` and
`relay_inbound_password` (mirroring the existing `smtp_password`/`resend_api_key` redaction,
`config.rs:402-403`).

<!-- Added: minor — dev injector now posts the PINNED Postmark InboundMessage schema (§4.2) WITH the Basic-auth header, so the faithful e2e exercises the real verifier + lookup + gates + forward against a real payload, not a fictional shape; outbound via SMTP sink so the header surgery is asserted on a rendered message -->

---

## 10. Testing strategy (faithful e2e — no shims)

Mirrors the project's faithful-e2e mandate (real runtime + real handler + real lookup + real forward;
the auto-tx lesson). The relay e2e (item 8 in the main spec §8.6) runs:

1. **Contract-extension regression (§3.4):** Resend `ResendRequest` serializes `reply_to` + `headers`;
   SMTP `build_lettre_message` emits `Reply-To` + `X-ZS-Relay`; `build_envelope` returns the relay
   bounce mailbox as the envelope-from (the `send_raw` envelope), not `from`/`real-inbox`. **Each
   fails against today's drivers** (which drop those) — proving they test the fix.
2. **Inbound happy path:** POST a Postmark `InboundMessage` (Basic-auth) for an active alias → assert
   the forward lands in the mailpit sink addressed (RCPT TO) to the real inbox, sent via the dedicated
   `relay_forward_mailer` (§5.2a).
3. **Header-privacy assertion:** the rendered forward contains the real address in **no** header line;
   `From`/`Reply-To` are the relay; `Return-Path`/envelope-from is `bounce+…@relay…`; `Sender`,
   `X-Original-To`, `Delivered-To`, inbound `Received`/`Authentication-Results` are absent.
4. **Suppression gate:** suppress the real inbox → the same POST returns **200 + silent drop** (NO
   bounce, NO forward); nothing reaches the sink (§4.3 step 6 / §8).
5. **Revocation cascade (the B4 test):** grant → forward succeeds → control `revoke_grant` → re-POST to
   the same alias → the handler **emits a bounce email to the original sender in the sink** + returns
   200; assert the forward did NOT reach the real inbox, and assert the `DELETE` + `revoked_at` UPDATE
   committed **atomically** (a partial commit would leave the alias live). Also asserts the cascade
   runs on a dedicated `auth_db_url` client (not the `Arc<Client>` `auth_pg`).
6. **Loop guard:** POST an `InboundMessage` already carrying `X-ZS-Relay` → 200 + dropped, no forward.
7. **Rate limit (transient):** burst past the per-alias capacity → the over-limit POST returns
   **503 (Retry-After)** via `auth.rate_limits` `ConsumeResult.consumed=false`, and Postmark would
   retry it; assert no forward for the over-limit message.
8. **Sustained-abuse auto-revoke (§7):** `ABUSE_STREAK` consecutive over-limit windows on one alias →
   assert the alias is auto-revoked (the control revoke path is invoked) and subsequent inbound bounces.
9. **Re-grant stability (§6.1):** revoke then re-grant → the SAME alias is active again
   (`revoked_at` cleared, same `relay_email`), and inbound to it forwards again — no new alias, no
   dead-alias bounce.
10. **Cross-service concurrency (§6):** race a `revoke_grant` against an `accept_consent` re-grant on
    the same `(app, user)` → assert the terminal state satisfies `grant absent ⇒ alias revoked` in
    **both** commit orders (no live-alias-for-revoked-grant window).
11. **App-delete revokes all aliases (§6):** create two users' aliases on app A → `delete_app(A)` →
    assert **both** `app_user_identities` rows for A's `client_id` are revoked (the companion UPDATE is
    load-bearing because there is no cross-schema FK; a dropped companion statement fails this test).
12. **`OriginalRecipient` normalization (§4.4a):** POST with `OriginalRecipient =
    Token+Hash@Relay.Zeroship.AI` → resolves to the same alias row as `token@relay.zeroship.ai`,
    forwards normally.
13. **Replay/idempotency (§7.1):** POST the **same** `MessageID` twice → **exactly one** forward in
    the sink.
14. **Spam/DMARC gate (§5.4):** POST an `InboundMessage` with `Headers` carrying `X-Spam-Status: Yes`
    (and a second with `Authentication-Results: …dmarc=fail`) → **200 + silent drop**, no forward, no
    bounce.

Unauthenticated inbound POST (no/bad Basic auth) → 401, and **no** suppression/forward/bounce
side-effect (closes the spoof-oracle).

---

## 11. v2 (out of scope here, tracked)

- **Two-way replies (alias → app):** route a user reply back to the app (the app sees the message),
  with the app's `From` rewritten to the relay so the user still never sees the app's real address.
  Requires storing the third-party `from_full` per thread (the `MailboxHash`/thread key) and, if we
  ever forward *without* rewriting `From`, **ARC sealing** (§5.2) to repair the broken upstream chain.
- **Alias-management surface:** a console UI to list a user's aliases per app and explicitly rotate one
  (the only path that mints a fresh alias, per §6.1) — mirrors Apple Hide-My-Email's management view.
- **SES outbound driver:** the tree has no SES *outbound* `Mailer` today (only the SES *inbound* SNS
  verifier, `sns.rs`). If a future relay-forward provider is SES, add a fourth `Mailer` impl with
  envelope-from control; until then the relay forward runs on SMTP (§5.2) and SES-outbound is **not**
  built. (Explicitly out of scope for v1 — this is the corrected count: three drivers today.)

---

## 12. Slice 5 build order (what this spec hands the §9 slice plan)

1. **Extend the `Email`/`Mailer` contract** (§3) across `types.rs` + the **three** drivers
   (stdout, smtp, resend — there is no SES outbound) + all template callers, with the §3.4 regression
   tests, including the verified lettre `RawHeader` newtype + `send_raw`/`Envelope::new` path on
   `smtp.rs`. (Prerequisite for everything else.)
2. **Second mailer wiring** (§5.2a): `RelayForwardMailer` newtype, `build_relay_forward_mailer`, the
   `AUTH_RELAY_SMTP_*` config block, and the `server::run` injection — so the relay handler has an
   envelope-capable SMTP mailer in hand, distinct from the transactional `AUTH_MAILER` one.
3. **Inbound payload type + route** (§4): `mailer/inbound.rs`, `/webhooks/relay-inbound`,
   `verify_basic_auth` gate, `normalize_alias` (§4.4a), `MessageID` idempotency (§7.1), the §4.5 JOIN +
   revocation + suppression gates, and the §5.4 spam/DMARC gate.
4. **Forwarding build + SRS/header surgery** (§5): the relay forward `Email` builder, envelope-from pin
   via `send_raw`, header strip/rewrite table; the handler-emits-the-bounce + 200 status policy (§8).
5. **Loop guard + rate limits** (§7) on `auth.rate_limits`: pinned per-alias/per-app capacities, the
   `ConsumeResult.consumed` branch, the `ABUSE_STREAK` second-tier counter, and the control-side
   auto-revoke call.
6. **Revocation cascade** (§6): `revoke_grant` opens the cross-schema transaction on a **dedicated
   owned `Client` from `auth_db_url`** (NOT the `Arc<Client>` `auth_pg`) — or, as the better long-term
   alternative, migrate `auth_pg` to a `Pool` and check out a `mut` conn; the app-delete companion
   UPDATE keyed on `client_id`; the cross-service grant-row-lock concurrency contract; the
   re-grant-stability upsert (§6.1). **This is real infra work, not assumed-away.**
7. **`app_user_identities.app_client_id = client_id` pin** (§6.2): the column is NAMED `app_client_id`
   (Slice 4) and the gateway upsert (`router::identities::upsert`) writes the `oac_` client_id into it;
   the main-spec DDL is reconciled to the same shape (`app_client_id`, `pairwise_sub` column, PK
   `(app_client_id, global_user_id)`).
8. **Dev topology + faithful e2e** (§9, §10): compose mailpit sink, the Postmark-shaped injector, the
   full §10 matrix (14 cases).
9. **DNS/identity + reputation isolation** (§5.1/§5.5): publish the relay-domain DKIM/SPF (covering the
   relay-forward IPs)/DMARC (`p=none` warm-up → `p=quarantine`)/MX; provision the relay-forward SMTP on
   a **dedicated sending IP pool** separate from transactional mail (prod ops; dev uses the sink).
