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

## Active issues (1 + 1 remainder)

| Tier | Issue | Status | Effort |
|---|---|---|---|
| T2 | ISS-11 · 2FA / TOTP | open | M–L |
| T3 | ISS-12b · owned-app blob/bundle cleanup on erase | open (follow-up) | S–M |

All live in `crates/auth` (ISS-12b spans auth → control/blob store) and are independent of
the builder rewrite. **Shipped 2026-06-11:** ISS-12 (GDPR-erase lifecycle, `3a9b2315` +
`a0d23e8c`) and ISS-10 (session visibility/revoke on `/me`, `de38f943`).

---

## T2 — post-launch · power / security

### ISS-11 · No two-factor (TOTP) enrollment
**Status:** open · **Effort:** M–L

Zero MFA code (grep finds only a `token_handlers.rs:512` placeholder and an unrelated
"two factors" comment in `ui/link.rs`). Not a sign-up blocker — password + Google/GitHub
OAuth + magic-link all work. Good GA hardening since creators control money via Stripe Connect.

**Fix:** challenge in the auth-service `/login` flow before `accept_login`, + encrypted-at-rest
secret storage + backup codes + enrollment on `/me` (benefits from ISS-10's `/me` security
card landing first).

## T3 — follow-up

### ISS-12b · Owned-app blob/bundle cleanup on account erase
**Status:** open (follow-up to ISS-12) · **Effort:** S–M

The ISS-12 reaper (`crates/auth/src/cron/account_reaper.rs`) erases the user row + cascades
DB dependents, but does NOT delete the blobs/bundles of apps the user owned — that lives
behind the control plane / blob store (cross-crate from `crates/auth`). Left as a marked TODO
in the reaper docs. **Fix:** on erase, enumerate the user's owned apps and delete their
bundles/assets from the blob store (a control-plane call or a shared cleanup primitive).
Low urgency (orphaned blobs are not a PII leak once the owning user row is gone), but needed
for true storage hygiene.

> **Operator decision still open (ISS-12):** confirm the billing-retention default in
> `account_reaper::user_has_financial_history` — currently *anonymize* creators with a
> `creator_accounts` row (retain financial records per GDPR Art. 17(3)(b)) vs hard-delete.
> Change the predicate there if you want a different policy (e.g. block erase until the
> Connect account is closed).

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
