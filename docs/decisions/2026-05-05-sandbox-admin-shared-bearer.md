# ADR — Sandbox admin API ships with shared bearer; per-operator JWT deferred to Phase 5

- **Date:** 2026-05-05
- **Status:** Accepted
- **References:** `docs/proposals/sandbox-pg-state.md` § II.0 §4 + Q-12; round-4 admin-API review IMPORTANT #6.

## Context

Phase 3 of the sandbox controller's pg-state work ships the operator-facing admin/GDPR API (`/admin/*`) under `crates/sandbox/src/admin_handlers.rs`. The API is gated on a single shared bearer token mounted from `SANDBOX_ADMIN_TOKEN_PATH` and read once at boot (Round-3 CRITICAL #3). Every admin request re-uses the same token; there is no per-operator identity at the auth layer.

The GDPR `DELETE /admin/users/{user_id}` handler writes an audit row into `sandbox.events` with `kind = 'gdpr.delete_user'`. That row's `data.admin_id` is currently the literal string `"operator"` because the controller has no way to recover an operator identity from the bearer.

GDPR Art. 30 (Records of Processing Activities) requires the platform to identify *who* performed each processing operation — not just *that* it was performed.

## Decision

Ship Phase 3 with the shared-bearer + `admin_id = "operator"` shape. Defer per-operator JWT (with `admin_id` / `scopes` / `step_up` claims) to Phase 5 production hardening.

## Consequences

**Pros**
- Phase 3 is unblocked. The JWT verifier lives in `crates/control/`; coupling the sandbox crate to it before that contract is finalized is premature, and the rest of Phase 3 (read endpoints, exports, GDPR cascade SQL, role split) is independently shippable.
- The bearer-from-file shape mirrors `SANDBOX_TOKEN`'s wire format. Operators already understand it.
- Nothing in the Phase-3 wire format blocks the JWT shape. Every handler still takes `&HttpRequest`; the auth path can evolve from "match bearer" to "verify JWT + check scope" without touching SQL or response shapes.

**Cons**
- GDPR Art. 30 requires identifying who performed processing. With a shared bearer the audit cannot distinguish operators. If multiple humans share the bearer, the audit row truthfully says "an operator did this," not "Alice did this."
- The mitigation chain below has weak links: process-level audit relies on host-OS log retention, which not every deployment configures.

**Mitigations (Phase 3)**
1. The shared bearer is mounted from a secret store with limited-distribution access controls (operator role on the secret, not the controller).
2. Physical/process access to the controller host is itself audit-logged via host-level mechanisms (sshd, sudo, IAM session logs); a determined investigator can correlate `gdpr.delete_user` event timestamps with host-session logs to identify the operator.
3. `SANDBOX_ADMIN_TOKEN_PATH` is mode 0o400 root-readable only; rotating the token requires a coordinated secret-store update + a rolling restart.

**Phase 5 plan**
1. Replace `admin_check`'s shared-bearer compare with a JWT verifier that pulls `admin_id` + `scopes` + `step_up` claims.
2. Replace `audit_admin_action`'s hard-coded `"operator"` actor with the JWT's `admin_id` claim.
3. Add per-endpoint scope checks at the start of each handler; add `WWW-Authenticate: Step-Up max_age=300` on 403 from destructive endpoints when `step_up` is missing/stale.
4. Backfill historical audit rows is out of scope — pre-Phase-5 rows stay `admin_id = "operator"` permanently.

## Notes for operators

- Until Phase 5 ships: rotating the shared bearer is the only mitigation if the bearer leaks. Rotation requires a rolling restart of every controller replica (the bearer is read once at boot; there is no reload).
- Limit the bearer's distribution to the smallest operator pool consistent with on-call coverage.
