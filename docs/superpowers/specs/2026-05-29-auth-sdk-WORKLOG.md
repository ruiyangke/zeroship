# @zeroship/auth build — loop WORKLOG

`feat/auth-sdk-popup` autonomous pilot loop. **Pilot (user offline 2026-05-29): decide forks myself,
no review gate, commit-only NEVER push. Dispatch next on task-completion; fill idle with DISJOINT work
only.** Specs: `2026-05-29-auth-sdk-design.md` (round-6+) + `2026-05-29-relay-email-design.md` (GO).
Per-slice: implement(TDD faithful) → code-critic(security) → code-fixer → I build the FULL affected
crate set + run all suites + commit.

## State machine
**Subsystem 1 (foundation) ✔ · Subsystem 2 (SDK) ✔ · oidc_rp-breaker ✔ · Slice 3 (scopes) ✔ ·
Slice 4 (pairwise) ✔** → **Slice 5 (relay): 5a RUNNING → 5b → 5c** → backlog → full live e2e (compose).

## Commit log (feat/auth-sdk-popup, commit-only — git log is truth)
1a 5ee245d7 · 1b-mech 83936a83 · 1c 3d96f7ea · 1d d46ac990 · 1b-pool 740f009f · 1b-anchors 4ea94e3b ·
1b-browser aaebed3d · 2a 77e2d18c · 2b c5afa5ae · oidc_rp-breaker c7f905d7 · 3a b2e087cc · 3b 441c0a01 ·
4 b5af568f · relay-subspec d00657ca/f95c7638 (GO 88).

Live: gateway /__zs/auth/{authorize,popup-callback,token,session?mint=1,signout,jwks} + Bearer arm +
per-app PKCE clients + anchors + mint single-flight + wrapper key rotation + per-thread Hydra client +
breaker + declared scopes (manifest→registry→consent two-namespace→token→env.auth) + pws_ on ALL arms
(global UUID never reaches apps) + app_user_identities. SDK @zeroship/auth . /client /react /types (70 tests).

## Now: Slice 5 relay (sub-spec GO 88). Phased per its §12:
- **5a (RUNNING):** Email/Mailer contract (reply_to/envelope_from/RawHeader across stdout/smtp/resend —
  NO SES outbound) + alias mint (token@relay.zeroship.ai → app_user_identities.relay_email, collision
  retry, rotate on re-grant) + email-claim swap per §12.
- **5b:** inbound webhook (managed provider, NEW route+payload+sig) + forwarding (From/Reply-To/envelope-
  from rewrite, header-privacy) + reply→bounce (O7).
- **5c:** bounce/abuse/rate-limit + revocation cascade (control-side admin auto-revoke endpoint, POOLED
  conn — control auth_pg is Arc<Client>, needs pool/owned for the txn).
Sub-spec must-fix still open for 5b/5c: pin the relay-forward SMTP provider; the control admin revoke
endpoint shape. Dev: local MX sink (mailpit/inbucket) for faithful e2e.

## Backlog (before final e2e): 3c gateway route-level required_scopes→403 (apps self-enforce via
env.auth.scopes meanwhile); DPoP-introspection per-app client_id binding (token-confusion, pre-existing,
not a leak); JWKS fetch in core::oidc_verify still per-call client (5-min cached, not brownout surface);
BroadcastChannel name 'zs:auth' vs spec; gateway clippy doc-lints.

## Decisions (in specs): O8 manifest auth.scopes · O7 relay-reply-bounce · S1 client_id binding · S2 F4-B
gateway HMAC pairwise · round-6 mint single-flight+Pool · wrapper key rotation · anchor abs=created_at+30d ·
scope:global signout=this-app-all-devices · app_user_identities key (app_client_id=oac_, pairwise_sub col).

## LESSON: integration-heavy slices SOLO; parallelize only provably-disjoint (sdks vs gateway, doc vs code).
## Live-infra debt for the FINAL e2e: full /token→anchor→/session, 1d Hydra-admin provision, 3a/3b/4/5
DB tests, relay forwarding — all need a live compose stack (Hydra + migrated PG + MX sink). Bring up at END.
