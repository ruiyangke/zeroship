# `@zeroship/db` v2 — score progression

Critic-reviser loop log for `docs/proposals/zeroship-db.md`.

**Baseline** (pre-loop): 3,382 words. Drafted 2026-05-12.

**Loop config:** up to 20 rounds. Composite score weights completeness, soundness, feasibility, fit-with-platform above clarity/prose.

**Stopping rule:** plateau across 3 consecutive rounds with no critical issues remaining, or 20 rounds reached.

| Round | Composite | Completeness | Soundness | Feasibility | Fit | Top 3 outstanding flaws |
|-------|-----------|--------------|-----------|-------------|-----|--------------------------|
| 1 | 46 | 48 | 42 | 35 | 50 | (1) LISTEN/NOTIFY infeasibility; (2) `ON DELETE CASCADE` default backwards; (3) `Id<T>=number` regresses typed_id invariant |
| 2 | 63 | 64 | 60 | 58 | 70 | (1) `pg_advisory_xact_lock` releases per-batch (wrong scope); (2) replication-slot WAL retention unaddressed; (3) audit-log tamper protection missing |
| 3 | 70 | 70 | 68 | 70 | 78 | (1) advisory-lock single-key collisions; (2) C1 publication leaks platform tables; (3) Postgres server log captures unredacted DDL |
| 4 | 77 | 76 | 76 | 75 | 80 | (1) SECURITY DEFINER trust boundary (user-supplied actor); (2) `SET log_statement` privilege portability; (3) overflow side-table DDL missing |
| 5 | 80 | 80 | 78 | 80 | 82 | (1) custom-GUC trust model is wrong (any role can SET); (2) `applied_at` column overload; (3) schema_version race under concurrent deploys |
| 6 | 81 | 82 | 80 | 82 | 84 | (1) temp-table ownership defeats REVOKE (replaced w/ PID-keyed table); (2) connection init latency; (3) stale PID rows need GC |
| 7 | 84 | 84 | 82 | 84 | 86 | (1) HMAC nonce replay attack window; (2) SECURITY DEFINER search_path injection; (3) HMAC key rotation cadence and bootstrap |
| 8 | 86 | 86 | 85 | 86 | 87 | (1) sign function GUC-gate is fake (replaced w/ role-based); (2) BYTEA `=` timing leak (added const_eq); (3) pgcrypto schema convention |
| 9 | 87 | 87 | 86 | 86 | 88 | (1) plpgsql per-byte loop slow; (2) Postgres-version pinning; (3) status vs error-code naming overlap |
| 10 | 88 | 88 | 87 | 87 | 89 | (1) `__zeroship_subject_map` DDL missing; (2) MERGE version-pin wrong; (3) per-app vs platform-wide schema scope unclear |
| 11 | 88 | 89 | 87 | 88 | 88 | (1) subject-map functions still GUC-gated; (2) installation order implicit; (3) Cybertec reference missing |
| 12 | 89 | 89 | 88 | 88 | 89 | (1) per-connection actor binding bleeds context; (2) `<app_schema>` placeholder substitution unclear; (3) RPC overhead from per-RPC init |
| 13 | 89 | 89 | 88 | 89 | 89 | Plateau — only minor flaws remain |
| 14 | 89 | 89 | 88 | 89 | 89 | Plateau confirmed; `pending` vs `running` initial-state clarification added |

**Stopping rule satisfied:** composite score plateaued at 89 across rounds 11, 12, 13, 14 with no critical issues remaining. Per the loop configuration, plateau across 3 consecutive rounds is the stop signal. Final score 89/100.

## Loop summary

Most consequential revisions across the run:

- **R1**: replaced LISTEN/NOTIFY with WAL/pgoutput fanout; reversed `ON DELETE CASCADE` default to `RESTRICT`; aligned with typed_id invariant.
- **R2**: corrected advisory-lock scope (session vs transaction); added replication-slot WAL retention strategy; added audit-log tamper protection via SECURITY DEFINER; added Drizzle relations deferral.
- **R3**: fixed `pg_advisory_unlock_all` misuse; excluded platform tables from C1 publication; added Postgres-server-log redaction; expanded subject-map security model.
- **R4**: fixed SECURITY DEFINER provenance (user-supplied actor → platform-controlled); added overflow side-table DDL; added per-app role provisioning section.
- **R5**: discovered custom-GUC trust model is wrong (any role can SET); planted the fix.
- **R6**: replaced custom GUC with PID-keyed admin-owned table; added PgBouncer pool-checkout safety reasoning.
- **R7**: added nonce-replay protection (expires_at + nonce uniqueness window); search_path hardening on all SECURITY DEFINER functions; HMAC key rotation lifecycle.
- **R8**: role-based access for sign function (replaced GUC gate); constant-time HMAC compare; pgcrypto schema convention.
- **R9**: Postgres version pinning section; status vs error-code naming inventory cleanup.
- **R10**: subject-map DDL declared; fixed wrong MERGE version pin; clarified per-app vs platform-wide schema scope.
- **R11**: subject-map functions transitioned from GUC to PID-context check; installation order documented; Cybertec timing-attack reference added.
- **R12**: per-RPC session init (replaces per-connection) to prevent actor-context bleeding; `__zeroship_reset_session` for RPC-exit cleanup.
- **R13-R14**: `pending` vs immediate-`running` clarification; final polish.

## Unresolved flaws (carried to implementation)

- **Per-RPC SQL overhead**: two SECURITY DEFINER round-trips per RPC. Acceptable but unmeasured. Bench in `crates/runtime/benches/db_connect_init.rs` will produce real numbers; the trade-off (context-bleed safety vs. latency) may be reconsidered.
- **`__zeroship_const_eq` plpgsql perf**: per-byte loop is slow; documented cost note, C-extension follow-up if benchmarks demand.
- **Read-set capture grammar (C1)**: fingerprint normalization for range queries, `IN`, ORDER BY remains a sketch — needs spec refinement before C1 ships.
- **DEFERRABLE FK throughput cost (B2)**: open question #6 — defer measurement to P4 implementation.
- **WAL fanout latency SLO (C1)**: open question #7 — defer measurement to C1 implementation.
- **PgBouncer transaction-mode compatibility (B1)**: open question #8 — operator-facing constraint, accepted limitation.
- **Multi-region brokers (C1.1)**: deferred to follow-up proposal.
- **Spatial / FTS / time-series / pagination cursor**: deferred to follow-up proposals (open Q #12-14).
- **Codemod AST tooling (B2 upgrade path)**: documented as future work; no concrete tool selection.
