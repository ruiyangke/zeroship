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
| 2 | ISS-12 (GDPR delete) | opus | in progress | — | crates/auth delete path; TDD |
