# Composite r28 Validation Review — Sandbox Snapshot/Restore

**Scope**: Validate that recent landings since `add6d5ef` compose correctly. NOT a broad-lens refresh.

**Worktree HEAD**: `e3a95a28` (one scripts-only commit ahead of `8f512153`). Lib **512 passed / 0 failed / 1 ignored**.

---

## Cross-Cutting Concern Verdicts

| # | Concern | Verdict | Evidence |
|---|---------|---------|----------|
| 1 | Sanitize composition (5 strip passes + idempotency) | **CLEAN** | `wake_machine.rs:790-815` composes left-to-right. `sanitize_idempotent` test at `:1684-1700` pins all 3 redaction tokens as non-matching against subsequent passes. Char-boundary truncation at `:809-814`. |
| 2 | /metrics route + AdminRole + envelope consistency | **CLEAN** | `admin_handlers.rs:2055-2064` delegates to `admin_check_required(_, ReadOnly)` — identical machinery as every `/admin/*` GET. 401/503 §10.0 envelopes; 200 Prometheus text. Tests at `tests/sandbox_admin_e2e.rs:1229-1320`. |
| 3 | spawn_blocking + CreateGuard rollback | **CLEAN** | `backend/nomad_ch.rs:774-792` moves sync-IO bundle into spawn_blocking. **Guard mutation stays on calling thread** at `:793` AFTER spawn_blocking returns Ok. Owned PathBuf clones cross closure boundary; guard reference does not. |
| 4 | r27-S1 loopback + r3-A boot ordering | **CLEAN** | `nomad_ch.validate()` at `config.rs:908` runs `validate_nomad_addr_loopback` BEFORE `AppState::from_config`. Non-loopback → fast fail boot; loopback-valid + agent down → r3-A degrades gracefully with WARN + counter. |
| 5 | Staging-locality ADR scope vs code | **CLEAN** | ADR Phase 3 targets `create_ext4_image_if_missing` + `materialize_rootfs`, both invoked from `try_create` step 3 at `nomad_ch.rs:777-789`. R26-I2 spawn_blocking refactor makes Phase-3 cutover cleaner (sync-IO body is now a single owned-data closure mapping trivially to driver StartTask). |

---

## Closure Verification Table

| Backlog ID | Commit(s) | Verified |
|---|---|---|
| R26-I1 SnapshotRowMeta DRY | `b5ec01a1`, `6c475c30` | YES |
| r27-S1 nomad_addr loopback | `069dd277` | YES |
| r27-M1 sanitize whitelist + UUIDs | `4f0f2259` | YES |
| r27-M2 char-boundary truncation | `dfccd049` | YES |
| R22-S1 sanitize widening Mode A | `7647cd4d` | YES |
| R26-I2 spawn_blocking try_create | `73725aa3` | YES |
| R26-API2 /metrics route | `3ec2762d`, `05224bd1` | YES |
| Staging-locality ADR | `bbadbe68` | YES |

---

## Findings

### CRITICAL — none
### MAJOR — none

### MINOR (4)

1. **`#[doc(hidden)]` annotations stale on metrics accessors** — `metrics.rs:262, 296, 316, 333, 339, 345, 385, 391, 398, 429, 435, 442` carry `#[doc(hidden)]` despite now being production accessors consumed by `metrics_export::render()`. Either drop or add justifying comment.

2. **`pub mod metrics_export` could tighten to `pub(crate)`** — sole production consumer is `admin_handlers::metrics_endpoint`; sole test consumer is the module's unit tests. Symmetric with `pub mod metrics` (also overdue), so not net-new debt.

3. **Missing `/metrics` 503 test coverage** — covers 401/200 but not 503-when-no-admin-tokens-configured. Shared `admin_check_required` makes this structurally redundant but symmetric with `/admin/*` 503 tests. Add `metrics_503_when_no_admin_tokens_configured`.

4. **`sandbox_corrupt_id_total` help text scopes narrowly** — `metrics_export.rs:69` help reads restore-specific but counter increments from all typed_id parse failures. Broaden to "decode of stored sandbox_id as typed-id failed at any read site".

---

## Spot Checks

- **No new `unwrap()`/`expect()` in production** since `add6d5ef` — 8 new sites all in test fns (7 config.rs r27-S1 tests + 1 nomad_ch.rs r27-M2 test).
- **`metrics_export` writer correctness** — escapes `\`, `"`, `\n` per Prometheus spec; body terminates with `\n`; NaN renders as Prometheus sentinel.

---

## Composition Verdict

**ALL FIVE CROSS-CUTTING CONCERNS: CLEAN.**

The recent landings compose correctly. No regression introduced by the sum of changes. Rust dimension scores:
- **Correctness**: 92
- **Performance**: 90
- **Security**: 91
- **API Design**: 87
- **Rust Idioms**: 90

**Recommendation**: ship the 4 MINOR cleanups as a single tidy-up commit when convenient. No release-blocking issues.
