# Known Issues

Open platform-level gaps with no full fix landed. Each entry is a self-contained note:
what's actually wrong today, the fix + rough effort, and dependencies.

> **2026-06-11 — builder issues deferred.** The zeroship-builder is slated for a rewrite,
> so all builder-coupled issues are parked under [Deferred](#deferred--superseded-by-the-builder-rewrite)
> rather than fixed against the current builder. The **active** set below is the
> platform/auth work that stands independent of the builder. Closed and removed-surface
> entries live in git history (this file before commit `7ad090ff`).

## Legend

**Status** — `open` · `re-scoped` (open, but title/severity/fix-path corrected 2026-06-11).
**Tiers** — `T1` GA-blocking (compliance) · `T2` post-launch power/security.

## Active issues (3)

| Tier | Issue | Status | Effort |
|---|---|---|---|
| **T1** | ISS-12 · account deletion / GDPR | open (unblocked) | M |
| T2 | ISS-10 · session visibility on `/me` | re-scoped | S–M |
| T2 | ISS-11 · 2FA / TOTP | open | M–L |

All three live in `crates/auth` and are independent of the builder rewrite.

---

## T1 — GA-blocking (compliance)

### ISS-12 · No account-deletion / GDPR-erase path
**Status:** open · **Effort:** M · **Unblocked** (mailer now exists)

No delete/erase path in `crates/auth` or `crates/control` (only a `disabled_at` soft-disable
column with no user-space setter). Schema readiness is partial: 10/15 FKs to `zeroship.users`
are `ON DELETE CASCADE`, but 5 are not (`0004_control.sql:93,224,256,270` creator/billing +
attribution rows; `0003_platform.sql:12`), so a hard `DELETE` is blocked for any creator with
a Stripe/billing row.

GA-blocking for EU sign-ups (Art. 17), not launch-blocking — early requests can be fulfilled
manually within the ~30-day window. The ToS/privacy pages already promise it.

**Fix:** request → grace-period → hard-delete job (host in the `crates/auth` `cron/` module);
a SET-NULL/anonymize decision for the 5 non-cascade FKs; Stripe-Connect retention handling;
blob/bundle cleanup for owned apps. The confirm/undo email is free now that the mailer exists.

## T2 — post-launch · power / security

### ISS-10 · No active-session list / single-session revoke
**Status:** re-scoped (was "/auth/sessions endpoints") · **Effort:** S–M

The `auth_sessions` table the old title assumed is gone; the model is `idp_sessions` +
per-app `gateway_sessions`. Security-critical revocation **already exists**: password-reset
cascade (`ui/reset.rs`), RP-/backchannel-logout (`crates/gateway/src/backchannel_logout.rs`),
`credential_version` bumps. Only the **visibility** layer is missing — list a user's active
sessions (device/IP/UA) and revoke one without a password change.

**Fix:** a `list_by_user` union over the two session tables + two handlers on the existing
auth-service `/me` page. Target `crates/auth`, not control.

### ISS-11 · No two-factor (TOTP) enrollment
**Status:** open · **Effort:** M–L

Zero MFA code (grep finds only a `token_handlers.rs:512` placeholder and an unrelated
"two factors" comment in `ui/link.rs`). Not a sign-up blocker — password + Google/GitHub
OAuth + magic-link all work. Good GA hardening since creators control money via Stripe Connect.

**Fix:** challenge in the auth-service `/login` flow before `accept_login`, + encrypted-at-rest
secret storage + backup codes + enrollment on `/me` (benefits from ISS-10's `/me` security
card landing first).

---

## Deferred — superseded by the builder rewrite

These were all coupled to the current `apps/zeroship-builder` (or to dormant platform
primitives whose only consumer is the builder). They are **not** being fixed against the
present builder; re-triage them after the rewrite defines what it actually needs. Full
detail is in git history (pre-`7ad090ff`).

- **ISS-14** · builder seeds 3 fabricated issues into the PM agent (`agents.ts seedIssues()`).
- **ISS-16** · builder reports an invented "B+" scorecard to the PM agent (`agents.ts defaultScores()`).
- **ISS-13** · skill registry catalogue-only; `/skills` ships a disabled "Add to project" CTA.
- **ISS-17** · no incidents timeline (HealthCanvas + SRE monitor).
- **ISS-24** · per-app migration journal exists (`__zeroship_migrations`) but no UI exposure.
- **ISS-15** · control plane keeps only a single overwrite-only `deploy_hash` (no deploy history) — was consumed by the deleted PlanCanvas.
- **ISS-18** · no performance-metering pipeline / `env.meter` primitive — fed the deleted HealthCanvas.
- **ISS-28** · no cron / scheduled-worker harness — PM digest + SRE monitor never fire.

> Note on ISS-15/18/28: these are platform-primitive-shaped (deploy history, metering,
> scheduler) but currently have **no live non-builder consumer**, so building them
> speculatively before the rewrite defines the need would be guesswork. Revisit as real
> platform primitives if the rewrite (or another surface) calls for them.
