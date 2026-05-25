# Sandbox snapshot-restore architecture review — 2026-05-25 r24

**Reviewer**: architecture-r24 (post-T-8b-smoke-r23 GREEN, T-8b-stress RED 2/60 end-to-end OK 3.3%, controller v33 + driver v13 staged for re-stress)
**HEAD**: `30960451` (`feat/sandbox-snapshot-restore`, controller-side disk-image preflight). Driver-worktree `3d03cb90` (tap pre-delete on EEXIST). Pin bump `364ead22` (driver v12→v13, controller v32→v33).
**Predecessor**: r23 at `0e0eeffa`. Diff since r23: parity contract test (`b6c55d93`, R22-T1), R22-T1+R21-API2 backlog close (`c351f341`), smoke-r23 GREEN (`082e6ddb`), R23-I1 pg-gated wake-machine counter e2e (`234c3bdf` + `ede57778`), T-8b-stress RED (`e6363fce`), controller-side preflight (`30960451`), driver tap pre-delete (`3d03cb90`), scripts pin bump (`364ead22`).
**Lens**: architecture (READ-ONLY).

## Summary

r23-A2's "wrapper retirement UNBLOCKED pending smoke-r23 GREEN" forecast was **REFUTED THE CYCLE AFTER IT LANDED**. Smoke-r23 went GREEN (082e6ddb, first end-to-end in 23 cycles); T-8b-stress went RED at 3.3% end-to-end OK and surfaced **TWO new bugs the single-cycle smoke could not exercise**: C-N-W1 (49× CREATE — workspace.img not visible to driver preflight at the staging/submit boundary) and C-N-W2 (9× WAKE — tap interfaces leak across cycles because `DestroyTask` races or fails). The same meta-lesson r23-A3 codified (smoke-cycle observability is downstream of empirical contact with load) is now empirically validated: GREEN at N=1 is NOT a proxy for GREEN at N=20×3.

The 30960451 fix is **defense-in-depth without root-cause closure**. `fsync_dir` + `assert_disk_image_present` is plausible (a missing fsync(dirfd) IS a known footgun on ext4 between two cooperating processes that don't share a write-then-read happens-before edge) BUT the commit message itself admits "the file ends up missing at preflight time anyway, suggesting a tight staging-and-submit window" — the controller is fixing for a symptom whose mechanism is inferred, not proven. The right structural fix is a **TYPED staging contract** with a driver-side handshake, not a fsync-then-pray pattern repeated per-image.

C-N-W2's tap-leak shape is **not unique**. The driver owns at least four kernel-state surfaces (tap, cgroup, mount-ns, loop-device-ish for disk-image-files) and the EEXIST→clean→retry pattern that closes C-N-W2 must be either replicated per-surface or replaced with a single top-level pre-create sweep. r23-A2's "wrapper retirement r20-A1 STRUCTURALLY UNBLOCKED" must be **RE-BLOCKED** until the cutover plan accounts for every kernel-state-leak surface the wrapper currently cleans deterministically.

r23-A3's restore-debug-playbook ADR did **NOT land** between r23 and r24 (no new file under `docs/decisions/`). The C-N-W1 staging-window failure is the second post-r23 incident where a verbatim observable (the driver preflight string `disk[1] /var/.../workspace.img does not exist (controller must stage before spawn)`) is what reclassified the failure from "outside our codebase" to "in our codebase" — exactly the pattern the ADR exists to codify. r24 re-escalates ADR landing to a hard gate before T-8b-stress-r2.

Six findings (2 CRITICAL, 2 IMPORTANT, 2 MINOR). r23-A1 (parity contract test) CLOSED at `b6c55d93`. r23-A2 (wrapper retirement) **RE-OPENED** — structural-equivalence claim was true for the rootfs.img dimension; stress flushed a SECOND staging-contract dimension (workspace.img) and a kernel-state-leak class (tap) the wrapper handled implicitly. r23-A3 (ADR) carried unchanged.

---

## CRITICAL

### [r24-A1] Staging/submit boundary is implicit + per-image; promote to a TYPED `StagingManifest` contract verified by a driver-side handshake before CH spawn

- **Where**:
  - `crates/sandbox/src/backend/nomad_ch.rs:670-741` — `create_sandbox` step 3: `create_ext4_image_if_missing(workspace_img, ...)` then `create_ext4_image_if_missing(user_home_img, ...)` then `build_nomad_job_json(...)` then `submit_nomad_job(...)`. No explicit "ready-to-spawn" signal exists between staging and submit; the **filesystem path itself** is the contract.
  - `crates/sandbox/src/restore_handler.rs:2046-2089` — `submit_restore_job` step: re-assert `workspace.img` + `user_home.img` exist before submit, then build job JSON, then submit.
  - `crates/sandbox/src/backend/nomad_ch.rs:3525-3601` — `create_ext4_image_if_missing` now ends with `fsync_dir(parent) → assert_disk_image_present(path)`.
  - Driver: `preflightDiskPaths` (per stress retro § "Fix surface (driver)") — driver-side three-check post-receive: exists / is-file / size>0.

- **The problem**: the controller and driver are two cooperating processes communicating across a Nomad job submit. The **only** shape they agree on is "these paths in this filesystem will be there when you stat them." That contract is:
  1. **Implicit**: no struct or schema names "the set of paths the driver will preflight." The driver hard-codes which paths to check based on the job's `Config.disks[*].path` array; the controller hard-codes which paths to stage based on `create_sandbox` step 3 / `submit_restore_job` preamble. There is no single source of truth that says "this is the staging manifest." Drift between which paths the controller stages and which paths the driver preflights is a one-commit window every time either side adds a new disk.
  2. **Per-image**: every new disk image added to a future job (e.g., a third `user_data.img`, a scratch image, a CRIU-state image post-snapshot-v2) requires:
     - controller adds `create_ext4_image_if_missing(new_img, ...)` + post-staging assertion + fsync,
     - driver adds the path to its `preflightDiskPaths` list,
     - **AND** the parity test from r23-A1 / `b6c55d93` does NOT cover this dimension — r23-A1 is JSON-field parity (cold-boot Config keys ↔ restore-path Config keys), NOT host-fs-path-set parity between controller-staging and driver-preflight. The cross-emitter drift surface has **TWO dimensions** and only one has a contract test.
  3. **Fsync-then-pray**: the 30960451 fix message says "the file ends up missing at preflight time anyway, suggesting a tight staging-and-submit window." That sentence is a confession that the root cause is **not pinpointed**. `fsync(dirfd)` plausibly fixes a missing-durability bug between two processes on the same host that don't share a write-then-read edge — but on Linux ext4 with the default journal mode, the in-memory dentry is visible to a concurrent `stat()` from a peer process well before fsync. If the driver's preflight is actually seeing ENOENT on a path the controller just created, the more likely mechanism is:
     - Nomad's allocation pre-stage logic running the driver in a different mount namespace (sandbox-isolated, bind-mount delayed), OR
     - the controller's `create_ext4_image_if_missing` was racing **itself** (two concurrent CREATEs for the same user_id colliding on `home.img` mkdir), OR
     - Nomad rescheduling the alloc to a different worker between submit and start (host_state_dir is worker-local, not network FS — would explain 49/60 random distribution).
     None of these is fixed by fsync. We are **fixing for a mechanism we have not proven**.

- **Why this is CRITICAL**: T-8b-stress proved that "two cycles since the contract drift surface was last seen" (user_id at C-7-LT-7, rootfs_source at C-7-LT-12a) is a streak measured in **commits**, not weeks. The host-fs-staging dimension is the **third strike** of the same cross-emitter-drift pattern, and the only fix proposed so far is yet another defense-in-depth assertion at the staging site. Asserting "the file is there after I created it" does not solve the staging-and-submit window — it makes the controller more **alert** to the failure, but does not change the **mechanism** of failure. A second observation from the same shape will land in T-8b-stress-r2 unless the contract is made explicit.

- **Fix shape (~60-100 LOC, no driver dependency for the FIRST half)**:

  **Phase 1 (controller-side, can land before driver coordinates)**: Introduce a typed `StagingManifest` struct in `crates/sandbox/src/backend/nomad_ch.rs`:

  ```rust
  /// The set of host-filesystem paths the driver will preflight-stat
  /// before spawning CH. The controller MUST stage every path here
  /// (create + fsync_dir + assert_disk_image_present) before
  /// submit_nomad_job; the driver-side preflightDiskPaths list MUST
  /// match this set exactly. Drift is caught by
  /// `staging_manifest_parity_contract_test`.
  ///
  /// Successor (Phase 2): the controller will serialize this manifest
  /// into the Nomad job Config under `staging_manifest_json`; the
  /// driver will deserialize, verify each entry matches the disks[*]
  /// paths it would otherwise hard-code, and ACK or refuse to spawn.
  #[derive(Debug, Clone)]
  pub(crate) struct StagingManifest {
      pub workspace_img: PathBuf,
      pub user_home_img: PathBuf,
      // (future: rootfs_source for restore-path symmetry; today the
      // controller emits `rootfs_source` as a Config field, the driver
      // stages it into runDir — that's a different layer)
  }

  impl StagingManifest {
      /// Stage every path: create-if-missing, fsync_dir, then assert.
      /// Returns the manifest itself on success so the caller threads
      /// it into both the job builder AND a future driver handshake.
      pub fn stage_all(&self, size_gb: u32) -> Result<&Self, String> {
          create_ext4_image_if_missing(&self.workspace_img, size_gb)
              .map_err(|e| format!("workspace.img: {e}"))?;
          create_ext4_image_if_missing(&self.user_home_img, size_gb)
              .map_err(|e| format!("home.img: {e}"))?;
          Ok(self)
      }

      /// Assert all paths are present (used by restore-path which does
      /// NOT create — only verifies that snapshot teardown preserved
      /// them across the wake).
      pub fn assert_all_present(&self) -> Result<(), String> {
          assert_disk_image_present(&self.workspace_img)
              .map_err(|e| format!("workspace.img: {e}"))?;
          assert_disk_image_present(&self.user_home_img)
              .map_err(|e| format!("home.img: {e}"))?;
          Ok(())
      }
  }
  ```

  Then `create_sandbox` (l. 670-703) and `submit_restore_job` (l. 2046-2089) BOTH build a `StagingManifest` from the same source-of-truth helper (`StagingManifest::for_sandbox(&cfg, sandbox_id, user_id)`), and the job builder takes a `&StagingManifest` instead of `&Path, &Path` separately. This collapses the two-dimensional drift surface into a single Rust struct: adding a new disk now requires adding a field to `StagingManifest`, which compiler-fails every caller until both staging paths and the job builder agree.

  Add `staging_manifest_parity_contract_test`: builds `StagingManifest::for_sandbox(...)`, collects all of its `PathBuf` fields via the upcoming `paths()` method, and asserts the set matches the disks[*].path values emitted by both `build_nomad_job_json_with` AND `build_restore_nomad_job_json` (the existing r23-A1 / `b6c55d93` test handles JSON-key parity; this new test handles host-fs-path parity).

  **Phase 2 (cross-worktree, after driver coordination)**: extend the Nomad job Config with a `staging_manifest` JSON field. The driver's `StartTask`:
  - deserializes `staging_manifest`,
  - cross-checks against its own `disks[*].path` list (refuses to spawn on mismatch — surfaces as `error_code=staging_manifest_drift` with the diff in the error message),
  - performs `preflightDiskPaths` against the manifest paths,
  - on success, emits a structured Nomad task event `staging_confirmed{manifest_sha=...}` BEFORE `cloud-hypervisor` is spawned.

  The handshake closes the "staging-and-submit window" by making the **driver** the canonical "ready" signal, not the controller. If the window persists, the driver will see ENOENT, return a typed error with the manifest entry that failed, and the controller can retry deterministically OR fail-fast with the offending path in the wire envelope (which the T-8b-stress retro already flagged as a P2 observability gap).

- **Why this beats per-image defense-in-depth**:
  1. Adding a third / fourth / Nth disk image is a single struct field, not three coordinated edits (controller stage + controller assert + driver preflight) that compile independently and can drift.
  2. The handshake gives both sides a typed `Result` instead of a paths-on-disk side-channel. `error_code=staging_manifest_drift` is grep-able; `Failed tasks` rollup is not.
  3. If the actual mechanism of C-N-W1 turns out to be **mount-namespace isolation** or **alloc rescheduling to a different worker** (neither of which fsync_dir fixes), the handshake will surface that mismatch deterministically because the driver will report which path it could not see.

- **Severity**: **CRITICAL**. The 30960451 patch is correct as defense-in-depth but does not move the staging/submit boundary from "implicit per-image filesystem agreement" to "explicit typed contract." T-8b-stress-r2 with the current patch may pass for the **same reason r23 smoke passed** (incidental timing on the test cluster), not because the mechanism is fixed. Phase 1 alone is ~40 LOC controller-only, lands without driver coordination, and immediately turns "third strike of cross-emitter drift" into "compile-time impossible." Block T-8b-stress-r2 on Phase 1; Phase 2 ships in the cycle after to harden the handshake.

---

### [r24-A2] Driver-owned kernel-state surfaces beyond tap (cgroup, mount namespaces, loop-device-ish disk images, network namespaces) need either a per-surface EEXIST→clean→retry pattern OR a single top-level pre-spawn sweep — wrapper retirement r20-A1 must NOT cut over until this is enumerated

- **Where**:
  - Driver-worktree `3d03cb90` (out of scope for edits, in scope for arch): tap-add now pre-deletes on EEXIST.
  - Wrapper script (DO NOT EDIT, in scope for analysis): `crates/sandbox/scripts/nomad-vm-wrapper.sh` historically owned **deterministic** teardown of: tap (`ip link del`), per-sandbox host_dir (`rm -rf`), any mount points the wrapper bind-mounted into the guest jail, any cgroup the wrapper created via raw-exec inheritance. The Go driver replicates **some** of this in `DestroyTask`; the smoke-r1 → smoke-r23 → stress arc has revealed that "some" leaves at least one surface leaking (tap).
  - Test surface: there is no comprehensive **"enumerate all kernel-state surfaces the driver owns and assert each has an EEXIST-safe re-entry"** test today.

- **The pattern**: T-8b-stress flushed C-N-W2 (tap leak) because under sequential load with vm_index reuse, two cycles bind the same `zsbx-nm-<idx>`. The driver's `init` net hook treated tuntap-add EEXIST as idempotent success — but the leftover tap was misconfigured (no IP, prior alloc's MAC), and CH exited -1 mid-resume. Pre-delete on EEXIST closes the immediate symptom.

  The architectural question is **not** "is pre-delete-on-EEXIST correct for tap" — it is correct, that's well-trodden ground. The question is **what other kernel-state surfaces does the driver own?** For each, the same shape applies: a stranded resource from a prior alloc's incomplete teardown is silently re-used by the next alloc with stale config and breaks mid-spawn.

- **Enumeration of likely surfaces (driver owns, controller does NOT touch)**:

  | Surface | Allocated by | Cleared by (today) | EEXIST-safe re-entry? | Stress-detectable? |
  |---|---|---|---|---|
  | **tap interface** `zsbx-nm-<idx>` | driver init net hook | driver `DestroyTask` (best-effort) — known-flaky per `3d03cb90` | NOW yes (pre-delete on collision) | YES — visible via `ip link` post-stress (9 stranded on w1) |
  | **cgroup hierarchy** (CH process scope) | nomad exec driver inheritance | nomad alloc teardown (presumed) | UNKNOWN — no audit | Symptom would be CH-process resource-limit drift; not directly checked by stress |
  | **mount namespaces** for guest jail | driver may bind-mount workspace.img / home.img into a chroot or netns | driver `DestroyTask` (presumed) | UNKNOWN — stress run did not check `/proc/*/mountinfo` post-cycle | NOT checked today; orphan mounts would show as "device or resource busy" on next stage |
  | **loop devices** for raw disk image files | NOT directly created (CH attaches `.img` as virtio-blk through file-backed device, no losetup) | n/a | n/a | n/a — only applies if the driver ever switches to losetup |
  | **network namespaces** (if driver creates a netns per VM rather than attaching the tap to host bridge) | driver init net hook (depends on impl) | driver `DestroyTask` | UNKNOWN — depends on whether driver uses a separate netns per VM | Symptom: stranded `ip netns ls` entries |
  | **vsock CID allocation** | CH config (each VM gets a CID) | implicit when CH process dies | EEXIST shape: if vsock CIDs are tracked separately and not re-used safely, two VMs colliding on a CID would fail at CH spawn | Stress could detect via `ss -K` or CH stderr |
  | **VFIO / device passthrough handles** | n/a (no GPU/PCI passthrough today) | n/a | n/a | n/a |
  | **firecracker/ch jailer chroot** | driver setup hook (if jailer is used) | driver teardown | UNKNOWN | Symptom: stale `/var/lib/.../jailer-<vmid>` dirs |
  | **PID files / lock files** for CH process | driver may write `<runDir>/ch.pid` | driver `DestroyTask` (best-effort) | EEXIST shape: stale pidfile with re-used PID could mis-target a kill | Stress with PID-recycling could detect |
  | **systemd transient units** (if driver uses `systemd-run --scope`) | driver setup | systemd cleanup | EEXIST shape: stale unit name on reuse | Symptom: `systemctl list-units --state=failed` post-stress |

- **What this means**:
  1. **C-N-W2 fix at `3d03cb90` is necessary but not sufficient**. It closes ONE surface. The wrapper had a single sledgehammer ("`rm -rf $host_dir && ip link del <tap>`") that was correct by being **comprehensive at one point in time**. The Go driver's `DestroyTask` is per-surface — and "best-effort + idempotent re-entry" for one surface (tap) is not the same as a comprehensive sweep. Every other surface in the table above is a latent C-N-W3, C-N-W4, ... candidate.
  2. **Wrapper retirement (r20-A1) MUST NOT cut over** until we have a positive enumeration of every kernel-state surface the wrapper currently cleans (explicit OR implicit-via-rm-rf), with each surface mapped to either:
     - the driver's equivalent `DestroyTask` call (audited for EEXIST safety on the next alloc), OR
     - a top-level pre-spawn sweep at the worker level that runs **before** any driver `StartTask` and reaps stranded surfaces by name pattern.
  3. The **top-level pre-spawn sweep** approach is the architectural equivalent of `cleanup_orphans_at_startup` (nomad_ch.rs:431-464), but at the worker level, for kernel state instead of Nomad allocs. It runs once per worker boot AND once per N stranded-tap detections (closed-loop). Strong preference for this over per-surface EEXIST-cleanup because it is one place to audit, one place to instrument, one place to fix when a new surface is added.

- **Concrete recommendations**:

  1. **Pre-cutover audit task** (architecture, ~2 hours): walk `nomad-vm-wrapper.sh` line by line, identify every shell command that creates, modifies, or destroys kernel state (`ip link`, `mount`, `umount`, `mkdir`, `rm`, `systemd-run`, `mknod`, `losetup`, `iptables`, `nft`, etc.). Map each to the driver's equivalent (or note "no equivalent — silently dropped"). Output: a single markdown table that becomes the cutover gate.

  2. **Driver-side audit task** (cross-worktree): for each surface in the audit, confirm `DestroyTask` calls the cleanup AND that `StartTask` re-entry is EEXIST-safe (either `ip link del` before add, `umount` before mount, etc.). Drift between "the wrapper cleans X" and "the driver does NOT clean X" is the cutover blocker list.

  3. **Worker-level sweep loop** (controller-side, ~50 LOC): a periodic task (every 60 s, or on every cluster-wide stranded-tap-counter threshold) that:
     - Lists all `zsbx-nm-<idx>` taps via `ip link show type tun`,
     - cross-checks each `<idx>` against the controller's active vm_index reservations,
     - for any tap whose `<idx>` is NOT in the active set AND has been stranded for ≥ T seconds (T = e.g. 5 × `host_fence_timeout_secs`), emit a metric AND `ip link del`.
     This is the architectural equivalent of `cleanup_orphans_at_startup` for kernel state. Even with `3d03cb90` closing the immediate collision, a leaking surface that grows unboundedly is a worker-resource exhaustion vector (kernel has a finite tap-interface namespace; sysctl `net.core.somaxconn`-style limits apply).

  4. **Stress-harness instrumentation** (test/process, ~20 LOC): the T-8b-stress retro already flagged "9 stranded tap interfaces post-stress" as a NEW observable that should join the standard post-stress sweep. Codify this as a script step: after every stress run, snapshot `ip -j link show type tun | jq '...'`, `mount | grep zsbx`, `cgroup | grep zsbx` (etc., one line per surface), and check ALL are zero. Any non-zero is a regression that blocks cutover even if end-to-end OK rate is ≥95%.

- **Why this is CRITICAL**: r23-A2 promoted "wrapper retirement structurally unblocked pending GREEN smoke." Stress shipped TWO independent regressions the smoke could not see — one (workspace.img staging) is in scope for r24-A1, the other (tap leak) is one of N surfaces in this enumeration. **Cutover with N-1 surfaces still latent is one stress cycle away from another RED**. The cutover plan needs the full enumeration in writing, AND positive evidence that each surface is either replicated in the driver or covered by a top-level sweep, BEFORE wrapper deletion lands.

- **Severity**: **CRITICAL**. The wrapper retirement is a one-way migration; lost behaviour on a kernel-state surface that wasn't enumerated would surface as a latent leak that compounds over days, NOT a fast-fail at smoke. The audit task IS the cutover gate and currently does not exist as a written deliverable.

---

## IMPORTANT

### [r24-A3] r23-A3 ADR `2026-05-25-restore-debug-playbook.md` did NOT land — re-escalate as a hard gate before T-8b-stress-r2

- **Where**:
  - `docs/decisions/` — current contents: `2026-04-20-kernel-cut.md`, `2026-05-05-sandbox-admin-shared-bearer.md`, `2026-05-25-vm-index-retry-policy.md`. No `2026-05-25-restore-debug-playbook.md`.
  - r23-A3 prescribed this ADR with three sections (triage tier ladder + observability-before-architecture rule + r22-A2 retro). The code-quality r23 lens hand-off (r23-A5) bundled the writing of this ADR with r19-A3 + r20-A3 + r20-A4 for a single ADR pass. Bundle has **not landed** between r23 and r24.

- **Why this re-escalates now**: T-8b-stress is the **second** consecutive cluster cycle where a verbatim driver-emitted observable was the reclassification trigger:
  - smoke-r22: `DeviceManager(Disk(NotFound))` → reclassified "CH-internal" to "driver staging" in one cycle, saved CH-version-sweep (the r23-A3 origin story).
  - T-8b-stress: `disk[1] /var/zeroship/ch/<id>/workspace.img does not exist (controller must stage before spawn)` → reclassified "Failed tasks rollup" to "controller-staging window" in one stress cycle, saved an indeterminate number of cycles chasing the wrong layer (a CH version sweep AGAIN would have been on the table without that string).

  This is **two** for two. The triage tier ladder is empirically validated as a force multiplier (~5 LOC of stderr capture closes multi-cycle uncertainty), and the rule "before patching at layer N+1, capture layer N's verbatim error first" has now been validated under two distinct failure classes (one in-codebase / in-runtime, one cross-process-staging-window).

- **What changes about r23-A3 in light of T-8b-stress**: the ADR should ALSO codify:
  3. **Stress-cycle gates** as the canonical falsification step for any "GREEN smoke ⇒ contract validated" claim. r23-A2's structural-equivalence claim was true for one dimension (rootfs.img) and was REFUTED for two more dimensions (workspace.img staging, tap teardown) within one cycle. The rule: an N=1 smoke proves NOTHING about a contract that operates under N>1 alloc reuse, vm_index recycling, or concurrent CREATEs. Stress is the empirical falsification step; promote it to a gate equal to smoke in the cluster cycle.
  4. **Wire-envelope verbatim-cause field** (already flagged as P2 in T-8b-stress retro): `backend_create_failed` / `restore_backend_failed` MUST carry the driver's actual failure string (e.g., `disk[1] /var/.../workspace.img does not exist`), not the generic `Failed tasks` rollup. Codify in the ADR alongside the stderr-capture lesson; both are observability investments with multi-cycle payoff.

- **Fix shape (~1 file, ~120 lines of markdown, no code)**: ship `docs/decisions/2026-05-25-restore-debug-playbook.md` with five sections:
  1. Triage tier ladder (validator → wire → CH input contract → CH internal → agent boot); 1-cycle diagnostic budget per tier.
  2. Observability-before-architecture rule: capture verbatim error at layer N before designing a fix at layer N+1.
  3. r22-A2 retro: "CH-internal" hypothesis REFUTED by stderr capture.
  4. **r23-A2 retro: "wrapper retirement unblocked" claim REFUTED by stress at N=20 (multi-dimensional contract drift, two latent regressions in one cycle)**.
  5. **Stress-as-gate rule**: smoke proves a single happy-path traversal; stress proves the contract under load AND under resource reuse. Cutover-class decisions require stress GREEN, not smoke GREEN.

- **Severity**: **IMPORTANT** (escalated within IMPORTANT-rank — would CRITICAL if not for r24-A1/A2 being load-bearing for the same outcome). One markdown file, no code change, codifies a process rule that has now demonstrated 2/2 force-multiplier evidence. Block T-8b-stress-r2 on this AND the worker-level sweep enumeration from r24-A2.

---

### [r24-A4] `SnapshotRowMeta` (restore_handler) and `WakeSnapshotMeta` (wake_machine) duplication is a sub-instance of a broader DRY surface — pg-row-readers across the sandbox crate share field-list + null-handling shape

- **Where**:
  - `crates/sandbox/src/restore_handler.rs:607-665` — `SnapshotRowMeta { artifact_path, sha256, vm_index, user_id }` + `read_snapshot_row(db, sandbox_id) -> Result<SnapshotRowMeta, RestoreHandlerError>`.
  - `crates/sandbox/src/wake_machine.rs:657-714` — `WakeSnapshotMeta { sha256, vm_index, user_id }` + `read_snapshot_row(db, sandbox_id) -> Result<WakeSnapshotMeta, String>`. Note the **same function name** in two modules with two **different return types and different error types**.
  - The wake_machine comment at l. 657-670 candidly admits: "The `SnapshotRowMeta` struct is `pub(super)` (not exported), so we can't reuse the function directly without a visibility bump that ripples through the sync path. We re-implement the same SELECT here against the same columns. If the snapshot-row schema evolves both paths need to update; the proposal § 7 phase 5 deletes the sync path entirely, after which this becomes the sole reader."

- **The broader DRY shape**: this is the **third** instance of the "two emitters / two readers, one column" drift surface in the sandbox crate, and the only one without a contract test:

  | Drift dimension | Cold-boot side | Restore/wake side | Contract test |
  |---|---|---|---|
  | Job-JSON Config field list | `build_nomad_job_json` (nomad_ch.rs) | `build_restore_nomad_job_json` (restore_handler.rs) | YES — `ch_plugin_config_field_list_parity` at `b6c55d93` (R22-T1) |
  | Host-fs staging-path set | `create_sandbox` step 3 (nomad_ch.rs:670-703) | `submit_restore_job` preamble (restore_handler.rs:2046-2089) | NO — covered by r24-A1 above |
  | snapshot-row reader (pg SELECT) | `restore_handler::read_snapshot_row` (returns `SnapshotRowMeta { artifact_path, sha256, vm_index, user_id }`) | `wake_machine::read_snapshot_row` (returns `WakeSnapshotMeta { sha256, vm_index, user_id }`) | NO |

  The third dimension already drifted by ONE FIELD: `WakeSnapshotMeta` does NOT carry `artifact_path`. That's intentional (wake-machine doesn't need it because the sync path through `restore_handler::do_restore_inner` is the one that consumes `artifact_path`) — but it means the two-row-readers each parse a **different column set** from the same logical row. If a future migration drops a column (or worse, renames `snapshot_vm_index` to something else), both readers must update in lockstep, with no test catching drift.

- **Why this matters more than "just refactor the duplication"**: the comment at wake_machine.rs:657-670 cites "proposal § 7 phase 5 deletes the sync path entirely" as the planned closure. That makes this **scheduled** code debt, not random duplication. But the closure depends on a phase that has not landed; meanwhile every column rename / addition is a two-place edit. The fix is either:
  1. **Accelerate the sync-path deletion** (proposal phase 5) so wake_machine's reader becomes canonical, OR
  2. **Bump `SnapshotRowMeta` to `pub(crate)`** and have `wake_machine::read_snapshot_row` call `restore_handler::read_snapshot_row` (or a refactored neutral location), accepting that `WakeSnapshotMeta` then discards `artifact_path` post-read,
  3. **Land a contract test** that calls both readers against an in-memory pg fixture and asserts the (sha256, vm_index, user_id) triple agrees AND that the column-list-actually-selected matches a fixture-row's column set.

- **Compared to r24-A1**: the staging-path drift is CRITICAL because it's actively breaking the cluster (49× CREATE fail). This row-reader drift is IMPORTANT because it's latent — both readers happen to be correct today, but the same shape (two emitters, no contract test) is the same shape that caused user_id and rootfs_source to drift previously. The pattern-recognition cost of NOT codifying it now is: when phase 5 lands and the sync path goes away, will somebody also remember to delete `SnapshotRowMeta`? Today, no test fails if `SnapshotRowMeta` is left orphaned.

- **Fix shape (~30 LOC)**: visibility-bump path. Promote `SnapshotRowMeta` to `pub(crate)` (or to a neutral `crates/sandbox/src/db.rs` module), have `wake_machine::read_snapshot_row` delegate to the canonical reader, project to `WakeSnapshotMeta` at the call site. ~30 LOC delta, one function name disambiguated, drift surface collapsed.

- **Severity**: IMPORTANT — latent risk, not currently breaking; but the same shape as user_id + rootfs_source + workspace.img and IS the third dimension of the cross-emitter pattern. Bundle into the same r24 cleanup PR as r24-A1 Phase 1.

---

## MINOR

### [r24-A5] r19-A1 leak ledger — T-8b-stress is the 10th consecutive cycle with `vm_index_leaks_total = 0` AND `fence_passed=true probes=2 elapsed_ms=300`. Stress widens the falsification window (vm_index reuse across 20 sequential cycles per worker × 3 workers, 60 total). Demotion track to MINOR-permanent on track for r26 closure.

- **Where**: same site as r23-A4 — `cleanup_orphans_at_startup` (nomad_ch.rs:431-464), three lying-comment sites at `:1081/:1162/:1184`. The T-8b-stress retro counter-deltas table: `vm_index leak counter: 0 → 0 (no inc_vm_index_leak emitted in any of the three worker logs)` AND `host_fence: cleared … fence_passed=true elapsed_ms=300 observed on every successful STOP (11 cycles), all elapsed_ms=300`.

- **What this adds over r23-A4**: stress under N=60 cycles with sequential vm_index reuse is the **strongest empirical falsification yet** of any pending leak. The C-N-W2 tap leak surfaced at the kernel level (9 stranded interfaces) WITHOUT incrementing `vm_index_leaks_total` — which is correct, because the leak in C-N-W2 is at the **driver-DestroyTask** layer, not at the controller-vm_index-release layer. The leak counter is wired to fire on `wait_for_job_gone` failure (nomad_ch.rs:1145-1162); stress exercised that path **eleven times** (11 successful STOPs) and zero of them tripped the counter. The leak-counter wiring continues to have no signal under exactly the load it was designed for.

- **Demotion progress**: r23-A4 set the bar at 5 consecutive cycles in the r21–r25 window (r21 + r22 + r23 + stress = 4 of 5, all zero). The remaining 1 cycle (T-8b-stress-r2, gated on r24-A1 + r24-A2 + r24-A3 landing) is the closing cycle. r25+ writes the "remove dead leak-counter wiring" ADR alongside the three lying-comment-site cleanups.

- **Action**: unchanged from r23-A4 — carry through the closing cycle, ADR at r25+, cleanup PR.

- **Severity**: MINOR — on track for closure, no runtime risk.

---

### [r24-A6] r20-A3 / r19-A3 / r20-A4 ADR bundle still open; r20-A5 (R19-I1 livez_polling verification) now CLOSED-ON-FIRST-CONTACT by smoke-r23 GREEN

- **Where**:
  - r20-A3: ADR bundle for timing constants (`wait_for_agent_silent` + `wait_for_job_gone` + takeover) — no commits since r19. Bundle target: code-quality r23+.
  - r19-A3: per-phase wall-time `stop()` metric — no commits since r19.
  - r20-A4: C-7-LT-2 invariant + regression test (9-cycle fence_passed=true; now 10-cycle post-stress). Integration test still not landed.
  - r20-A5: R19-I1 unverified-in-prod — **closed**. Smoke-r23 GREEN traversed `pending → reserving_slot → restoring → ok` with the wake state machine fully exercised; livez_polling fired and succeeded. T-8b-stress confirmed the same traversal twice (the 2 OK wakes) with poll-count = 46. R19-I1 is empirically verified.

- **What's left**: the same three items (r20-A3 / r19-A3 / r20-A4) bundle for the next code-quality cycle. T-8b-stress added one new ADR-worthy datum: `WAKE p50 = 46990 ms` is **identical** across smoke-r23 single-shot and stress-r1 sequential, within 0.2% noise. The "33 s reserving_slot phase" perf concern was structurally NOT what failed under load — that timing budget held under both shapes. Codify in the r20-A3 ADR alongside the host_fence_timeout derivation.

- **Severity**: MINOR — process backlog; no runtime risk. r20-A5 dropped from carry table.

---

## Cross-lens consensus

- **cluster-stress (T-8b-stress retro)**: Bug 1 (workspace.img staging window) and Bug 2 (tap teardown leak) flushed under 60-cycle load. Architecture endorses the retro's "driver+controller fix pair MUST ship before re-stress" gate AND extends it with r24-A1 (typed staging manifest) + r24-A2 (kernel-state surface enumeration) as the structural fixes — defense-in-depth without root-cause closure invites a second observation from the same shape.
- **test-coverage r24 (anticipated)**: the r23-A1 / `b6c55d93` parity contract test handles JSON-field parity only. r24-A1 expands the parity-test scope to host-fs paths (StagingManifest); r24-A4 adds the third dimension (snapshot-row reader). Three dimensions of cross-emitter drift, three contract tests OR one structural collapse (typed struct that compile-fails on drift).
- **code-quality r23/r24 (anticipated)**: r20-A3 + r19-A3 + r20-A4 bundle remains open per r23-A5 / r24-A6. r23-A3 ADR re-escalated to hard gate (r24-A3); bundle this with the code-quality ADR pass.
- **api-surface r22 (anticipated)**: a typed `StagingManifest` exposed via `pub(crate)` is an internal contract, NOT a public-API change — but if the Phase 2 driver-side handshake serializes the manifest into the Nomad job Config, that IS a wire-format addition. Coordinate with api-surface lens before Phase 2 ships.
- **security r21 (R21-S1 + R21-S2)**: unchanged from r23 carry. user_id + SandboxId unvalidated on restore boundary still open. The staging manifest from r24-A1 is a useful place to add an `assert_user_id_format()` call site (one path, one spot), but that's a side-effect rather than the primary motivation.

---

## Lens hand-off

1. **Implementer (r24-A1 Phase 1, immediate, P0)**: ship `StagingManifest` struct + collapse both staging sites (cold-boot + restore) to one source-of-truth helper + add host-fs-path parity contract test. ~40-60 LOC controller-only, no driver dependency, no cluster cycle needed for unit-test verification. **Block T-8b-stress-r2 on this.** Phase 2 (cross-worktree handshake) ships in the cycle after.
2. **Architect / pilot (r24-A2 audit, immediate, P0)**: walk `nomad-vm-wrapper.sh` line by line and produce the kernel-state-surface enumeration table. Cross-worktree: map each surface to driver `DestroyTask` equivalent. Output is one markdown table — it IS the cutover gate. **Block wrapper retirement (r20-A1) on this.**
3. **Implementer (r24-A2 worker sweep, P1)**: ~50 LOC controller-side periodic loop that detects stranded `zsbx-nm-<idx>` taps and (after a settle window) `ip link del`s them. Architectural equivalent of `cleanup_orphans_at_startup` for kernel state. Independent of driver fix; complementary defense layer.
4. **Doc landing (r24-A3, P0)**: write `docs/decisions/2026-05-25-restore-debug-playbook.md` with five sections (triage ladder + observability-before-architecture + r22-A2 retro + r23-A2 retro + stress-as-gate). **Block T-8b-stress-r2 on this** alongside r24-A1 Phase 1.
5. **Implementer (r24-A4, P2)**: visibility-bump `SnapshotRowMeta` to `pub(crate)`, have wake_machine delegate. ~30 LOC, no behavior change. Bundle with r24-A1 Phase 1 PR.
6. **Cluster (T-8b-stress-r2)**: gated on driver v13 (`3d03cb90`) + controller v33 (`30960451`) + r24-A1 Phase 1 + r24-A2 sweep + r24-A3 ADR. **Falsification criterion**: end-to-end OK rate ≥ 95% at 60-cycle 3+3 shape AND zero stranded kernel-state surfaces (taps, mounts, cgroups — enumerate per r24-A2) post-stress.
7. **Architecture r25**: gated on T-8b-stress-r2 outcome. GREEN ≥ 95% AND zero stranded surfaces → r25 pivots to cutover planning (wrapper retirement, post-cutover stability budget). RED → r25 scopes the new failure layer per the now-landed triage tier ladder.
8. **Code-quality r23/r24**: bundle r19-A3 + r20-A3 + r20-A4 (r24-A6). Single ADR + 2 PRs.

---

## r19-A1..A5 + r20-A1..A5 + r21-A1..A5 + r22-A1..A5 + r23-A1..A5 carry status

| Finding | r24 status |
|---|---|
| r19-A1 (vm_index leak ledger + reaper) | OPEN, MINOR — DEMOTION TRACK (r24-A5). 10/15 cycles zero counter (stress added 1). On track for r25 ADR closure. |
| r19-A2 / r19-A4 / r19-A5 | CLOSED (r20). |
| r19-A3 (per-phase wall-time stop metric) | OPEN — bundle for code-quality r23/r24 (r24-A6). |
| r20-A1 (three-rewriter coexistence + wrapper retirement) | **RE-BLOCKED** (r24-A2). r23-A2's "structurally unblocked" claim REFUTED by T-8b-stress: rootfs.img dimension was correct (`stageRootfsForRestore` works), workspace.img + tap-teardown dimensions had latent regressions. Gate now: r24-A2 audit + r24-A1 typed staging contract + ≥95% stress GREEN. |
| r20-A2 (deferred) / r20-A3 / r20-A4 | OPEN — code-quality bundle (r24-A6). |
| r20-A5 (R19-I1 unverified-in-prod) | **CLOSED** by smoke-r23 + stress (R19-I1 livez_polling exercised in 3 OK cycles total). |
| r21-A1..r21-A5 | r21-A1 CLOSED at `fcac5355`; remainder per r22 mapping. |
| r22-A1 (driver/wrapper restore-input contract divergence) | CLOSED at r23-A2 for rootfs.img dimension. r24-A1 opens the workspace.img + user_home.img dimension as a SEPARATE contract surface; not a regression on r22-A1, an EXPANSION of the contract-surface inventory. |
| r22-A2 (next failure class CH-internal) | SUPERSEDED / REFUTED — codified in r24-A3 ADR alongside r23-A2 refutation. |
| r22-A3 / r23-A1 (R21-API2 field-list parity contract test) | **CLOSED** at `b6c55d93` (R22-T1 landed). r24-A1 + r24-A4 add the second + third parity dimensions. |
| r22-A4 / r23-A4 | → r24-A5. 10/15 cycles. |
| r22-A5 / r23-A5 (ADR bundle backlog) | → r24-A6. R23-I1 closed (`234c3bdf` + `ede57778`); bundle remains open. |
| r23-A2 (wrapper retirement structurally unblocked) | **RE-OPENED / REFUTED** by T-8b-stress. See r24-A2; gate now: enumeration audit + typed staging contract + stress GREEN ≥95%. |
| r23-A3 (restore-debug-playbook ADR) | OPEN — did NOT land between r23 and r24. RE-ESCALATED in r24-A3 with additional sections (r23-A2 retro + stress-as-gate rule). Hard gate for T-8b-stress-r2. |
| r23-A5 (R22-I1 wake-machine terminal-overwrite counter) | CLOSED at `f98611fb`; e2e tests landed at `234c3bdf`. |
