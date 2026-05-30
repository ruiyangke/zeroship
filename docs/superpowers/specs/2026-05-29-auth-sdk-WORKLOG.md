# @zeroship/auth build — loop WORKLOG

Running log for the `feat/auth-sdk-popup` autonomous pilot loop. The spec workflow rewrites
`2026-05-29-auth-sdk-design.md`; THIS file is loop state. Append newest at the bottom.
**Pilot mode (user offline 2026-05-29): decide forks myself, don't wait for review gate,
commit-only NEVER push.**

## State machine
`seed ✔` → `spec author+harden ✔ (61→62→72)` → `lock forks ✔` →
`convergence harden (running)` → `writing-plans` → impl slice 1 (foundation) → review → …
→ slice 5 (relay) → review → e2e → done.

## Decisions log (forks resolved as pilot — best-architecture rulings)
- **O8 — where app scopes are declared → MANIFEST.** Creators declare `auth.scopes` in the app
  manifest (alongside routes), mirrored to control-plane `app_scope_defs` + the per-app Hydra
  client allowlist atomically on deploy. Rationale: scopes are an app-shape concern, belong with
  the code that defines them; single source of truth; matches how routes are declared.
- **O7 — relay reply routing (v1) → BOUNCE with "replies not yet supported".** Clearer failure
  than silently dropping; two-way re-injection is v2. (Relay = separate sub-spec.)
- **S1 — Bearer binding → bind on the `client_id` claim (RFC 9068).** Confirmed by a live-Hydra
  spike in Slice 1c; if Hydra omits `client_id`, fall back to per-client `audience=[client_id]` +
  `aud` binding. Decided-by-spike, not assumed.
- **S2 — pairwise mechanism → ship F4-B (gateway HMAC projection).** Hydra stays
  `subject_type: public`; gateway derives `pws_=base62(HMAC(salt, global_user||sector))` at the
  ZeroShip-User boundary + mints the browser wrapper token with `sub=pws_`. F4-A (Hydra-native
  pairwise + sector_identifier_uri) deferred behind an optional spike. Rationale: zero change to
  accept_login/accept_consent + no UUID-parsing consumer touched; reversible later.
- **Relay (subsystem 5)** is split to its own spec `2026-05-29-relay-email-design.md`; in pilot
  mode I author that sub-spec + build it after slices 1-4 (no user gate).

## Timeline
- 2026-05-29: worktree created off `f051cc87`; seed committed `bdf04426`.
- 2026-05-29: research complete (discovery + 3 grounded facts + Supabase + Auth0 internals).
  Architecture spine = client-held tokens via SAME-ORIGIN gateway endpoints (not cross-site
  iframe). User went offline → FULL PILOT authority; review gate skipped.
- 2026-05-29: spec author+harden workflow `w9eoznlnh` done — 3 critic→reviser rounds, 61→62→72,
  detailed 5-subsystem design (~180KB). Open forks O7/O8/S1/S2 resolved above.
- 2026-05-29: launching convergence-harden (lock forks + 2 critic→reviser + final critic) to
  drive blockers→0 before implementing.

## Next actions when woken
1. Read convergence result + final score; commit the converged spec.
2. Invoke writing-plans (or author the impl plan) from section-6 slice order (1a/1b/1c/1d → 2 → 3 → 4 → 5).
3. Implement slice-by-slice via background opus subagents; review each (diff+tests+decisions)
   before next; commit per slice. Faithful e2e; regression test per behavior. NEVER push.
