# ISSUES.md fix loop — worklog (2026-06-11)

Piloted `/loop` to drive the 11 open issues to closure in priority order (T1→T4).
Discipline: TDD (a regression test that fails pre-fix), subagent per fix, pilot
reviews diff+test before accepting, commit per fix, **commit-only NEVER push**.
As each issue closes it is removed from `ISSUES.md` (kept open-only) and logged here.

## Scope change (2026-06-11)
Operator: "ignore the builder's issue, we will rewrite the builder." All builder-coupled
issues (ISS-14, 16, 13, 17, 24, 15, 18, 28) are DEFERRED to the rewrite. Iteration 1
(ISS-14/16 on agents.ts) was killed mid-flight and its working-tree changes discarded.

## Active order (platform/auth only — all in crates/auth)
T1: ISS-12 (GDPR delete) · T2: ISS-10 (session visibility) → ISS-11 (2FA)

## Log

| Iter | Issue(s) | Model | Status | Commit | Notes |
|---|---|---|---|---|---|
| 1 | ISS-14, ISS-16 | opus | ABANDONED | — | builder fix; killed on scope change, tree reverted |
| 2 | ISS-12 (GDPR delete) | opus | DONE | (pending) | request/cancel + reaper + 0034 changeset; 6/6 PG tests (`--test-threads=1`); cargo check clean. Pilot re-verified (caught a 6/6→5/6 concurrency flake; passes single-threaded per repo convention). Billing-retention default flagged for operator. Remainder split to ISS-12b (blob cleanup). |
| 3 | ISS-10 (session visibility) | opus | DONE | de38f943 | list_by_user UNION + revoke_one (IDOR-guarded id+user_id) + /me/sessions routes; 7/7 PG tests; IDOR test proven to have teeth. |
| 3b | ISS-12 gateway_sessions privilege bug | (pilot) | DONE | a0d23e8c | review found UPDATE on gateway_sessions fails under zeroship_auth role (SELECT,DELETE only); switched to DELETE (matches reset.rs). |
| 4 | ISS-11 (2FA / TOTP) | opus | next | — | enroll/verify/backup-codes + encrypted secret + login challenge |

### Iter 2 review notes (ISS-12)
- Verified: changeset 0034 auto-included via `includeAll`, format matches repo, all written
  columns exist on `zeroship.users`, `creator_accounts`/`payouts` exist.
- Reaper: per-user transactions, injection-safe constant FK list, idempotent anonymize, billing
  branch isolated for operator policy change.
- Test is faithful (drives real store/reaper vs live PG, PG-gated skip). DB tests require
  `--test-threads=1` (AGENTS.md convention) — the reaper's `tick()` scans all due users, so
  concurrent tests interfere; documented, not a defect.
- Deferred: owned-app blob/bundle cleanup → ISS-12b (cross-crate).
