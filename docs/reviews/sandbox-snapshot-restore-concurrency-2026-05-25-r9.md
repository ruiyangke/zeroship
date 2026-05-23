# Sandbox snapshot-restore — Concurrency Review r9 (2026-05-25)

Branch `feat/sandbox-snapshot-restore` @ `dfd1a43d`. Read-only.
Audits the round-8 critical-fix sweep. r1-r8 carry-overs re-checked.

---

## PER-FIX AUDIT VERDICTS

| Fix | Verdict |
| --- | --- |
| **C1** lessee_updated_at wiring (`de3523c4`) | **HALF-INSTALLED** — sweep finds rows but cannot recover them (finding 1) |
| **A1** boot-time AEAD wrap (`18e2034b`) | **NEUTRAL** — single-threaded init, no runtime surface |
| **A2b** `verify_metadata_only` (`b925ad0d`) | **SAFE-BUT-LATENT** — sync ureq; no async callers yet (finding 5) |
| **R8-A3-5** spawn_blocking submit+livez (`64cbb447`) | **C3 WIDENED 6TH TIME** (finding 2) |
| **R8-A4** ErrorEnvelope (`fc3e9972`) | **NEUTRAL** — synchronous response construction |
| **R8-CONC2** LRU 4→32 (`f1bed99a`) | **CLOSES r8 finding 4** |
| **R7-API1 / R7-S2 / R5-S5 / R8-DEPLOY1+W1** | **NEUTRAL** |

---

## FINDINGS

### CRITICAL

1. **C1 sweep recovery still broken — host_id fence mismatch.**
   `db.rs:1758-1762` stamps `lessee_updated_at` and the sweep query
   at `db.rs:2397-2420` now returns abandoned rows. But the recovery
   CAS at `sweep.rs:167-169` calls `db.update_sandbox_status(...)`
   which delegates to `update_sandbox_status_with_host(..., self.host_id(), ...)`
   (`db.rs:1690-1697`). The CAS fences `AND host_id = $5::TEXT`
   (`db.rs:1773`) against the **sweeping** controller's host_id —
   but the row's host_id belongs to the **crashed** controller.
   CAS misses every iteration; sweep logs `CasLost (peer took
   over)` at `sweep.rs:180-185` while the row stays wedged.
   `take_dead_host_sandboxes` (`db.rs:1556-1573`) DOES rotate
   host_id but explicitly filters `status IN ('starting','running','unreachable')`
   — transient states are excluded. The C1 regression test
   (`sandbox_pg_e2e.rs:2202+`) only covers same-host transitions;
   cross-host takeover is untested. **§6.1 remains dead code.**
   Fix: have `run_transient_takeover_once` first issue a host_id-
   rewriting UPDATE (the private `update_lessee` at `db.rs:2344+`
   is the right shape but still has zero callers), or widen
   `take_dead_host_sandboxes`' status filter.

2. **C3 widened a 6TH time by R8-A3-5 (`restore_handler.rs:489-517`).**
   The r9 summary claim "should be neutral — same number of await
   points, just spawn_blocking wraps" is wrong. Pre-r8,
   `submit_restore_job` and `wait_for_livez` were sync function
   calls with **no** executor yield; the surrounding future was
   non-droppable across them (worker parked, but atomically
   committed). Post-r8 each is `spawn_blocking ... .await` — two
   NEW yield points. Cancel-unsafety window grew from 5 awaits
   (r8) to 7: `store.get` (`:405`), submit (`:497`), livez
   (`:515`), unseal (`:558`), clock_resync (`:569`),
   `update_sandbox_status` (`:601`), `clear_snapshot_metadata`
   (`:603`). Drop between submit-Ok and the final CAS now leaves
   a running Nomad alloc + vm_index reservation + pg row in
   `Restoring` — and per finding 1 the sweep cannot reach it.
   R4-A2 RAII overdue by 5 cycles.

### MAJOR

3. **R4-A2 LeasedVmSlot RAII — 5th cycle open.** With C3 at 7 awaits
   AND C1 recovery broken (finding 1), this is now incident-class.
   `backend/nomad_ch.rs:944-952,1126-1129` +
   `snapshot_handler.rs:269-407` + `restore_handler.rs:341-615`.

4. **R7-C1 unbounded detached teardown — 3rd cycle open.**
   `admin_handlers.rs:1310-1324` still detaches with no
   JoinSet / semaphore. Cap with `Semaphore`.

### MINOR

5. **A2b `verify_metadata_only` carries blocking ureq — latent
   ntex park.** `snapshot_store_gcs.rs:693-719` →
   `head_object_metadata_value` (sync ureq HEAD). Trait doc at
   `snapshot_store.rs:91-96` requires `spawn_blocking`; default
   impl at `:159-161` falls through to deep `verify` (full 1 GB
   egress on caller thread). No async callers exist yet, so latent.
   When the verify-sweep lands, the wrapper MUST `spawn_blocking`.

6. **C1 SET→sweep race is benign (audited safe).** Transient
   entry stamps `lessee_updated_at = now()` in the same UPDATE
   that bumps generation. Sweep filters `< now() - interval`, so
   a fresh stamp never qualifies. No race.

7. **R8-A3-5 spawn_blocking panic surface.** `unwrap_or_else(|p|
   Err(format!("{p:?}")))` at `restore_handler.rs:498,516`
   renders `Box<dyn Any>` as `Any { .. }`. Same deferred minor
   as r8 finding 7; centralize in `format_spawn_blocking_panic`.

---

## DETACHED-TASK / OUTSTANDING WORK

| ID | Status |
| --- | --- |
| C1 | OPEN — half-installed (finding 1); the "CLOSED" claim in deferred is premature |
| C3 | OPEN — 6 widenings (r2, r4, r5, r7, r8, **r9**) |
| R4-A2 | OPEN — 5th cycle; subsumes C3 + R5-A2 + R6-P1 |
| R7-C1 | OPEN — unbounded detached teardown (3rd cycle) |
| R7-P1, R7-S1, R7-S2, R7-API1, R5-S5, R8-CONC2 | CLOSED |
