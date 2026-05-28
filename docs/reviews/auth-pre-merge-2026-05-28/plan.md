# Pre-merge audit: auth-phase-1 through auth-phase-10

**Branch:** `proposal/auth-server`
**Goal:** Find and fix bugs before merging 229 commits to `main`.
**Pattern:** 10 rounds. Each round = critic agent (finds) + fixer agent (commits). All fixes go through tests against live PG (5441) + Hydra (4444/4445).

## Round assignments

| Round | Theme | Critic | Fixer |
|---|---|---|---|
| R1 | Auth flow state machines (login/signup/magic/oauth/reset) — race, error paths, timing leaks | codex | codex |
| R2 | Token security (PAT, OAuth introspect, DPoP, wrapper tokens — replay, signature, jti) | opus | codex |
| R3 | Schema & migrations (race, constraints, indices, NULL handling, idempotency) | codex | codex |
| R4 | Authz engine (Cedar policies, two-call, scope mapping, entity assembly, edge cases) | opus | codex |
| R5 | Race conditions (concurrent grants, token sweep, multi-instance startup, advisory locks) | codex | codex |
| R6 | Input validation (bounds, URL parsing, redirect_uri, scope strings, body sizes) | opus | codex |
| R7 | Crypto hygiene (constant-time eq, KDF, RNG, key rotation, encrypted storage) | codex | codex |
| R8 | Observability & audit (missing events, log injection, PII in logs, audit chain) | opus | codex |
| R9 | Error handling (panics in async, error message disclosure, unhandled Results) | codex | codex |
| R10 | Cross-binary integration (control↔auth↔gateway, request-id, header validation) | opus | codex |

## Severity tiers

- **CRITICAL**: shipping-blocker; remote code exec, auth bypass, data leak
- **HIGH**: must fix before merge; logic bugs, missing validation, race conditions with material impact
- **MEDIUM**: should fix before merge; smells, missing tests, brittle code
- **LOW**: backlog post-merge; style, future-proofing, comments

## Process

1. Critic runs against the latest `proposal/auth-server` HEAD; produces `round-NN-{theme}-findings.md`
2. Findings sorted CRITICAL → HIGH → MEDIUM → LOW
3. Fixer drains in severity order; each fix is its own commit with a regression test
4. Round complete when:
   - All CRITICAL+HIGH closed
   - MEDIUM partially closed (depending on time)
   - Test suite passes against live PG + Hydra
   - Diff merged into `proposal/auth-server`

## Tracking

Each round's findings + fix commits logged here. Final backlog of MEDIUM+LOW carried into the post-merge stack.
