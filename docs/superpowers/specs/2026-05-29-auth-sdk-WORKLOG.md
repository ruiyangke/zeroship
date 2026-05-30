# @zeroship/auth build — loop WORKLOG

`feat/auth-sdk-popup` autonomous pilot loop. **Pilot (user offline 2026-05-29): decide forks myself,
no review gate, commit-only NEVER push.** Specs: `2026-05-29-auth-sdk-design.md` + `2026-05-29-relay-email-design.md`.

## STATUS: feature build COMPLETE (5/5 subsystems). Remaining: live e2e + backlog.

## Commit log (feat/auth-sdk-popup, commit-only)
1a 5ee245d7 · 1b-mech 83936a83 · 1c 3d96f7ea · 1d d46ac990 · 1b-pool 740f009f · 1b-anchors 4ea94e3b ·
1b-browser aaebed3d · 2a 77e2d18c · 2b c5afa5ae · oidc_rp-breaker c7f905d7 · 3a b2e087cc · 3b 441c0a01 ·
4 b5af568f · 5a 2cec959d · 5b 344f0108 · 5c 09f46d83. (+ doc commits; relay sub-spec GO f95c7638.)

Delivered: gateway /__zs/auth/{authorize,popup-callback,token,session?mint=1,signout,jwks} + Bearer arm
+ per-app public PKCE clients + anchors + mint single-flight + wrapper key rotation + per-thread Hydra
client + circuit breaker. env.auth. Declared scopes (manifest→registry→consent two-namespace→token→
env.auth). Pairwise pws_ on ALL arms (global UUID never reaches apps). Relay: alias mint + inbound
webhook + forwarding (real inbox only in RCPT TO) + reply-bounce + revocation cascade + email-claim swap
(apps see pws_ id + relay alias only). SDK @zeroship/auth . /client /react /types.

DB changesets added: 0006 app_session_anchors, 0007 app_scope_defs, 0008 gateway_sessions.granted_scopes,
0009 app_user_identities (+ token_revocations into 0002). includeAll auto-applies.

## REMAINING WORK
### A. Live compose-stack e2e (HIGH — the faithful-e2e validation gate)
Bring up the stack (Hydra + migrated Postgres + an MX sink), run ALL the DB-gated tests live
(auth_token_anchors, app_oauth_client, consent_ui, identities_relay, relay_dedup, sessions, dpop), and a
cross-component headless OAuth flow: provision per-app client → /authorize → Hydra → /token (wrapper) →
app request with Bearer → env.auth sees pws_ + alias email → consent declared scope → /session?mint=1
reload → /signout revokes. Relay: inbound to alias → forward to real inbox (MX sink), reply→bounce,
revoke→stops forwarding. (Browser popup e2e via Playwright is the heavier gold-standard, separate.)
Compose setup is on the merged full-stack-compose work (Caddy + *.zeroship.localhost, Liquibase migrate
service). EXPECT to find never-exercised integration bugs (as the compose work did) and fix them.

### B. Backlog polish (MED)
- 3c: gateway route-level required_scopes → 403 (apps self-enforce via env.auth.scopes meanwhile;
  needs a Manifest rule field + RouteEntry + the gateway check).
- DPoP-introspection per-app client_id binding (token-confusion between apps for opaque DPoP tokens;
  pre-existing, not a UUID leak).
- JWKS fetch in core::oidc_verify still per-call cyper::Client (5-min cached, not the brownout surface).
- control auth_pg Arc<Client> → Pool (relay_revoke opens a per-call dedicated conn; deferred).
- BroadcastChannel name 'zs:auth' vs spec app-ref-scoped; gateway clippy doc-lints.

## Decisions (in specs): O8 manifest auth.scopes · O7 relay-reply-bounce · S1 client_id binding · S2 F4-B
gateway HMAC pairwise · mint single-flight+Pool · wrapper key rotation · anchor abs=created_at+30d ·
scope:global signout=this-app-all-devices · app_user_identities(app_client_id=oac_, pairwise_sub) ·
email_verified describes the real inbox the alias forwards to.

## Process notes
- Per-slice: implement(TDD faithful) → code-critic(security) → code-fixer → I build the FULL affected
  crate set + run all suites + commit. Two workflows hit a StructuredOutput hiccup (5b critic) → recovered
  by re-running review on the on-disk output.
- LESSON: integration-heavy slices SOLO; parallelize only provably-disjoint (sdks vs gateway, doc vs code).
- NOT pushed (pilot directive). When the user returns: summarize, then they decide push/PR.
