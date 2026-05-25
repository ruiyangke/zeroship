# Sandbox/snapshot-restore — security r31 review

Date: 2026-05-25 (UTC). HEAD: `729f22dd` (branch `feat/sandbox-snapshot-restore`).
Predecessor: r30 at `37a53878` (`docs/reviews/sandbox-snapshot-restore-security-2026-05-25-r30.md`).
Scope: `crates/sandbox/**` and `crates/sandbox-agent/**`. READ-ONLY.

Landings since r30 (in-scope):

- `ade8fb46` — NomadStopPermits global semaphore on AppState (r30-A1 closure).
- `cdcd670d` — T-7 + T-8 cutover. Delete `TaskDriverMode`, `task_driver_mode_from_env`, `wrapper_path`, raw_exec branch. Both job builders unconditionally emit `Driver: "ch"` + typed `Config`.
- `c3670845` — scripts: remove wrapper install + `SANDBOX_NOMAD_CH_WRAPPER_PATH` + `INSTALL_CH_PLUGIN_DRIVER` gate + `SANDBOX_TASK_DRIVER` line.
- `b75728ce` — `vm_index_ceil` 12→20, `vm_index_release_delay_secs` 5→2.
- `4d10ba45` — stale rustdoc cleanup.
- `29a2dc95` — concurrency r31 docs (out-of-scope).
- `729f22dd` — controller pin v38→v39.

## Summary

**r31 produces ZERO new CRITICAL, ZERO new IMPORTANT, ONE new MINOR.**

R13-S1 (worker SA `storage-rw`) remains the SOLE pre-cutover Ops blocker.

## CRITICAL

None.

## IMPORTANT

None.

## MINOR

### [r31-S1] Nomad client config retains `driver.raw_exec.enable = "1"` after controller-side cutover

- **File**: `crates/sandbox/scripts/gcp-worker-startup.sh:351-362`.

```hcl
client {
  options = {
    "driver.raw_exec.enable" = "1"
    "user.blacklist"         = ""
  }
}
```

**What's wrong**: post-cdcd670d, the controller no longer emits a single raw_exec jobspec — the branch is structurally deleted (pinned by `nomad_job_spec_always_uses_ch_driver` at `nomad_ch.rs:5786` and `nomad_restore_job_spec_always_uses_ch_driver` at `restore_handler.rs:4825`). But the Nomad agent still loads the raw_exec driver, leaving a latent root-equivalent execution capability the controller no longer needs.

**Why it matters**: any actor with Nomad `/v1/jobs` submit rights (operator-level bearer; any future code path that bypasses `build_nomad_job_json_with`; in-tree compromise of the controller binary) can submit a `Driver: "raw_exec"` job that runs an arbitrary command on the host. `user.blacklist = ""` does NOT blacklist root. The driver-binary SHA-pin (R20-S3) protects only the ch path — raw_exec uses Nomad's built-in driver and bypasses that guard.

This is MINOR (not IMPORTANT) because the Nomad agent listens on the private network only (no TLS, but firewalled to the VPC by GCP `--scopes` shape) and the controller is the documented sole submitter.

**Concrete fix**: drop the `"driver.raw_exec.enable" = "1"` line at `gcp-worker-startup.sh:359`. Nomad's default for raw_exec is disabled. The ch plugin loads via the explicit `plugin "nomad-driver-ch" { config {} }` stanza at lines 198-203 and is unaffected.

**Cross-lens**: not previously raised — the cutover deletions made this surface dead-capability for the first time at r31.

---

## "Verified closed since r30" section

Nothing was open as CRITICAL or IMPORTANT at the security lens entering r31. The r30 carry table shape is unchanged. r30-FOCAL-I1 / R30-I1 (panic-amplification on gc_stopper) remains open as concurrency IMPORTANT; security lens confirmed CLEAN last round.

---

## Focal-list checks (per r31 brief)

### Focal #1 — Admin auth on every handler

**CLEAN.** All 12 public handlers in `admin_handlers.rs` call `admin_check_required` as the first non-arg statement:

| Handler | Line | Role |
|---|---|---|
| `list_all_sandboxes` | 411 | ReadOnly |
| `get_sandbox_detail` | 534 | ReadOnly |
| `list_user_sandboxes` | 593 | ReadOnly |
| `list_user_shares` | 687 | ReadOnly |
| `list_hosts` | 758 | ReadOnly |
| `export_user` | 835 | Full |
| `delete_user` | 1004 | Full |
| `snapshot_sandbox` | 1361 | Full |
| `wake_sandbox` | 1570 | Full |
| `poll_wake` | 1888 | ReadOnly |
| `cold_boot_sandbox` | 2024 | Full |
| `metrics_endpoint` | 2056 | ReadOnly |

The cutover commits did not touch `admin_handlers.rs`; the auth-gate matrix and T1 symmetric-timing logic are unchanged from r30.

### Focal #2 — AEAD snapshot integrity / trust chain / fence

**CLEAN.** `git diff ade8fb46^..729f22dd -- crates/sandbox/src/snapshot_aead.rs` is empty. HKDF / ChaCha20-Poly1305 wire shape (cipher tag `0x01`, per-chunk monotonic nonce, root KEK from `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` mode 0o400) is unchanged. The host_fence (FM-F, `nomad_ch.rs:1555-1600`) is unchanged this round.

### Focal #3 — Identifier-quoting/injection on typed "ch" jobspec

**CLEAN (improved over the deleted wrapper path).** Both emitters use `serde_json::json!` macros throughout — `build_nomad_job_json_with` (`nomad_ch.rs:2769-2968`) and `build_restore_nomad_job_json` (`restore_handler.rs:2496-2668`). No string interpolation routes `user_id`/`sandbox_id`/`project_id`/`vm_index` into a shell-quoted form; serde handles JSON escapes.

`user_id` is the only identifier that flows into a host path (`user_home_dir_root.join(user_id)` at `restore_handler.rs:2528-2531`). Path-injection is bounded by:
1. Admin handlers validate `user_id` against `zeroship_core::typed_id::parse_with_prefix(.., "usr")` at every entry (`admin_handlers.rs:424,598,692,840,1009`).
2. Typed-ID shape `^usr_[0-9A-HJ-NP-Za-km-z]{22}$` — no `/`, `..`, NUL.

The deleted bash wrapper required shell-quoting discipline at the wrapper boundary; the typed path eliminates that surface entirely. Strict improvement.

### Focal #4 — vm_index 2 s release: TOCTOU?

**CLEAN.** The 5→2 s change at `nomad_ch.rs:1626-1635` fires ONLY after `wait_for_agent_silent` (`:1568-1602`) returns `Ok(())` — the previous tenant's agent has stopped answering `/livez` for the fence-quorum window. The 2 s is kernel-level breathing room (tap netdev eviction + fcntl drain); the cross-tenant security guard is the fence above, not the post-fence delay. The driver-side r24-A2-S2 tuntap-add verify gate closes the worker-side reuse window in parallel. The `if fence_passed` branch at line 1604 is the only release call — no path releases without fence pass (or routes to leak-on-timeout). No new race.

### Focal #5 — `nomad_vm_wrapper.sh` deletion residual

**CLEAN except for the new [r31-S1].** c3670845 removes the `gs_pull nomad-vm-wrapper.sh`, the `SANDBOX_NOMAD_CH_WRAPPER_PATH` env entry, the `INSTALL_CH_PLUGIN_DRIVER` gate + conditional install block, and the `SANDBOX_TASK_DRIVER` line. cdcd670d deletes the `wrapper_path` field on `NomadCHConfig` and the `task_driver_mode_from_env` reader. No residual key paths, secrets surface, or permission grants tied to the wrapper survive in the controller crate.

The new residual surface ([r31-S1]) is the Nomad `driver.raw_exec.enable` option — capability the wrapper relied on still loaded in the agent.

### Focal #6 — Post-cutover SA / IAM

**CLEAN.** `provision-gcp-cluster.sh:252` (server SA) `storage-ro,logging-write,monitoring-write` — unchanged. `:293` (worker SA) `storage-rw,logging-write,monitoring-write` — unchanged (R13-S1 carry). Cutover REDUCES per-worker GCS pulls by one object (`nomad-vm-wrapper.sh` no longer fetched). No new permissions.

---

## Carry table at r31

| Finding | r30 | r31 |
|---|---|---|
| **r30-carry-HEREDOC** unquoted heredocs | CARRY | CARRY |
| **r30-carry-r28-M1** T5 body control-char | CARRY | CARRY |
| **r30-carry-S1** Guard B node-id | CARRY | CARRY |
| **r30-carry-M3** wake-poll RFC1918 | CARRY | CARRY |
| **r30-carry-M4** artifact_path Full-bearer | CARRY | CARRY |
| **r30-carry-M5** CI policy | CARRY | CARRY |
| **R22-S1 / r27-S1 / r27-M1 / r27-M2** | CLOSED | Re-verified CLOSED |
| **R20-S3** driver SHA pin | v22 carry | v24 SHA `7bb90576…` at `gcp-worker-startup.sh:172`; pin enforcement intact |
| **R13-S1** worker storage-rw scope | OPEN IMPORTANT | OPEN — **SOLE pre-cutover Ops blocker** |
| **R21-S1 / R21-S2 / R20-S2 / R19-S1 / R9-S3** | OPEN IMPORTANT | OPEN — unchanged |
| **[r31-S1]** raw_exec enabled in Nomad client | — | **NEW MINOR** |

---

## Counts

- CRITICAL: 0 new; 0 carry.
- IMPORTANT: 0 new. Carries: R21-S1, R21-S2, R20-S2, R19-S1, **R13-S1**, R9-S3.
- MINOR: **1 new ([r31-S1]).** Carries: r30-carry-HEREDOC, r30-carry-r28-M1, r30-carry-S1, r30-carry-M3, r30-carry-M4, r30-carry-M5.
- Closed this round: 0 (nothing was open at security entering r31).
- Total NEW this round: **0 CRITICAL, 0 IMPORTANT, 1 MINOR.**
- **Cutover gate: R13-S1 remains the SOLE pre-cutover Ops blocker.** [r31-S1] is defense-in-depth, not cutover-gating.

## Lens hand-off

- **Sandbox controller (Rust)**: pre-cutover CLEAR of IMPORTANTs.
- **Ops**: R13-S1 carries. New [r31-S1] is a one-line edit to `gcp-worker-startup.sh` (drop raw_exec option); bundle with next worker provision.
- **Concurrency r31 owners**: R30-I1 (`catch_unwind` on gc_stopper) still open; security defers.
