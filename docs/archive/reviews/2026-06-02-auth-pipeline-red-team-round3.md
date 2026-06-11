# Auth Pipeline — Red Team Round 3 (Final Verdict)

**Date:** 2026-06-02
**Scope:** Confirm the two round-2 HIGHs are fully closed (sibling sweep + migration
idempotency) and attack the pass-4 changes for new regressions.
**Method:** 5 adversarial lanes + synthesis, run against the live `fsverify` stack
(Postgres 16 + Hydra) with from-zero Liquibase migration.

## Bottom line

**CLEAN.** Both round-2 HIGHs are confirmed closed against the live stack, and pass-4
introduced no new reproduced HIGH or MEDIUM finding. The only reproduced new item is a
benign LOW — a comment-parsing recurrence of the 6.0 bug-class that is a no-op today.
Now also fixed (see below). The auth pipeline ships.

## Per-lane closure

- **0.0 — interactive-mint identity write:** CLOSED. Genuine-path test passes; the
  load-bearing counterfactual (revert → test fails) confirms it; a full mint-path
  sibling sweep found every production cookie/wrapper producer writes (or backstops)
  `app_user_identities`.
- **6.0 — `validCheckSum: ANY` own-line:** CLOSED. Both directives parse and function;
  full from-zero idempotency (76/76, second run 0/76, `validate` clean) confirmed on a
  fresh isolated PG 16.
- **Pass-4 0.0 fix attack:** CLEAN. Reuses the already-deployed `identities::upsert`;
  no clobber, no cross-tenant/IDOR, no login-breaking failure (best-effort
  log-and-continue); RLS GUC structurally prevents cross-write.
- **Pass-4 7.0 fix attack:** CLEAN. Adds `billing` only to `fleet_wide_reader` (single
  caller `list_apps`); no env/secrets/deploy/team bleed (Cedar probe DENIED all);
  `api_key` is `serde(skip)`; C1 membership-scoping preserved.
- **Pass-4 5.1 fix attack:** CLEAN. Locked/absent/wrong-pw all → 401 with identical
  body/cookies/headers; no new oracle; lock still holds and recovers; the 403 arm is
  admin-only (`disabled_at` never written by prod). Residual timing delta is a
  pre-existing F7 artifact masked by Argon2, not introduced here.

## New finding

### LOW — `validCheckSum` prose comments mis-parsed as directives (6.0 bug-class recurrence)

In `0025_roles_rls.sql` and `0031_app_members_owner_backfill.sql`, an explanatory
comment line began with the word `validCheckSum`, which the Liquibase formatted-SQL
parser matched as a second `validCheckSum` directive. Harmless today because the parsed
value also began with `ANY` (a no-op in the OR-set): from-zero migrate is clean, second
update idempotent, `validate` passes. **Latent risk:** a future reword starting with a
hex token would register a bogus specific checksum.

**Fixed (this round):** reworded both comment lines to begin "We mark this ANY
because …" so the comment no longer starts with the directive keyword. Re-verified
against a fresh DB: 76/76 from zero, 0/76 second run, idempotent.

## Convergence

| Stage | Open HIGH |
| --- | --- |
| Initial security review (71 agents) | 1 crit + 2 high |
| Red-team round 1 | F1–F7 regressions surfaced |
| Red-team round 2 | 2 HIGH (0.0, 6.0) |
| Red-team round 3 | **0** |

Four fix passes + three red-team rounds. No open HIGH or MEDIUM remaining.
