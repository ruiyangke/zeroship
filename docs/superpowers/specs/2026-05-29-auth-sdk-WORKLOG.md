# @zeroship/auth build — loop WORKLOG

`feat/auth-sdk-popup` autonomous pilot loop. **Pilot (user offline 2026-05-29): decide forks
myself, no review gate, commit-only NEVER push. Dispatch the next slice on task-completion (not on
loop ticks); fill idle time with DISJOINT work only.** Spec: `2026-05-29-auth-sdk-design.md` (round-6);
relay in `2026-05-29-relay-email-design.md`. Per-slice: implement(TDD faithful) → code-critic(security)
→ code-fixer → I build the FULL affected crate set + run all suites + commit.

## State machine
**Subsystem 1 (foundation) ✔** · **Subsystem 2 (SDK) ✔** · **oidc_rp-breaker ✔** · relay sub-spec drafted ✔ →
**Slice 3 (scopes): 3a (RUNNING) → 3b** → **Slice 4 (pairwise, REDUCED)** → **Slice 5 (relay)** →
full live e2e (compose stack).

## Commit log (feat/auth-sdk-popup, commit-only — git log is source of truth)
1a 5ee245d7 · 1b-mech 83936a83 · 1c 3d96f7ea · 1d d46ac990 · 1b-pool 740f009f · 1b-anchors 4ea94e3b ·
1b-browser aaebed3d · 2a 77e2d18c · 2b c5afa5ae · relay-subspec(draft) d00657ca · oidc_rp-breaker c7f905d7
(+ doc commits between).

Live surface: gateway /__zs/auth/{authorize,popup-callback,token,session?mint=1,signout,jwks} + Bearer
arm + per-app public PKCE clients + anchors + mint single-flight + wrapper key rotation + per-thread
Hydra client + circuit breaker. SDK @zeroship/auth . /client /react /types (70 faithful tests).

## F4-B pairwise: browser wrapper carries pws_ (done in 1b-anchors). Slice 4 REDUCED to: project pws_
at the ZeroShip-User boundary for the OTHER arms still emitting the global UUID (1c raw-Hydra Bearer,
cookie/sessions, DPoP) + the auth.app_user_identities mapping table (app_id, global_user_id, pws_,
relay_email). derive_pairwise() exists in core::auth.

## Now: Slice 3 declared scopes (split for the consent authz-inversion risk)
- **3a (RUNNING, solo):** manifest auth.scopes (bundle) + control.app_scope_defs + Hydra scope-allowlist
  mirror on deploy (reject platform-vocab collision) + control.oauth_grants ledger + WorkerUser.scopes
  kernel field across all 4 gateway arms + header + env.auth + SDK Session.scopes. Vocabulary + plumbing.
- **3b (next, solo):** consent.rs TWO-NAMESPACE classifier — platform/delegated scopes gated by
  is_authorized_anywhere vs app-declared+identity scopes SELF-GRANTABLE by the end user (fixes the
  authz-inversion that made read:billing un-grantable); applied on BOTH GET render + POST accept;
  Unknown→invalid_scope. Gateway route-level required_scopes → 403. Single oauth_grants delta in the
  consent handler (no parallel ledger / consent loop).

## Slice 5 relay (after 4): sub-spec DRAFT committed (d00657ca, critic 34→revised). RERUN a confirming
critic→reviser pass before BUILDING. Needs Slice 4's app_user_identities. Key real constraints found:
Resend driver drops Email.headers (must extend mailer::types::Email +reply_to/+envelope_from); inbound
≠ delivery-event webhooks (new inbound route+payload); cross-service revocation needs a named owner+outbox.

## Backlog: JWKS fetch in core::oidc_verify still per-call cyper::Client (5-min cached, not the brownout
surface) — bounded follow-up. BroadcastChannel name 'zs:auth' vs spec. gateway clippy doc-lints.

## Decisions (in spec): O8 manifest auth.scopes · O7 relay-reply-bounce · S1 client_id binding(RFC9068,
aud fallback) · S2 F4-B gateway HMAC pairwise · round-6 mint single-flight+Pool · wrapper key rotation
current+prev · anchor abs=created_at+30d · scope:global signout = this-app-all-devices (not IdP nuke).

## LESSON: integration-heavy slices SOLO; parallelize only provably-disjoint file sets (sdks vs gateway,
doc vs code) or use isolation:'worktree'. ## Live-infra debt: full /token→anchor→/session, 1d Hydra-admin
provisioning, 3a control DB tests, relay — all gated on a live compose stack (Hydra+migrated PG). Bring
it up at the END for the real e2e.
