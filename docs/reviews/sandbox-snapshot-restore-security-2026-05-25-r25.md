# Sandbox/snapshot-restore — security r25 review

Date: 2026-05-25 (UTC)
HEAD at audit: `03d3470f`. Lens: security (READ-ONLY).
Predecessor: r24 at `e6363fce`. Landings since r24:
- `30960451` — controller-side disk-image preflight (T-8b-stress Bug1)
- `3d03cb90` — driver tap pre-delete on EEXIST (cross-worktree Bug2)
- `1c255a00` — wake_machine WARN now carries error_code+error_message (R24-I1)
- `364ead22` — driver v12→v13 + controller v32→v33 pin bump
- `10d4200b` — T-8b-stress-r2 RED (2/60)
- `03d3470f` — pilot round-24 reviewer artifacts

Two parallel fixers in flight (v14/v34 bundle, T1 admin_ro) — NOT
touched this round. Driver lives cross-worktree.

## Summary

**r25 produces ONE NEW IMPORTANT finding and ZERO new CRITICALs.** The
landing of `30960451` (controller-side `assert_disk_image_present`)
introduces a new **CONTROLLER-SIDE** filesystem-path leak surface into
`wake_jobs.error_message` that was previously driver-side only (R22-S1).
This **lifts R22-S1 from "latent / driver-only" to "active on every
restore-path stage failure"** — the controller now itself sources the
exact `disk[1] /var/zeroship/ch/<sandbox_id>/workspace.img does not
exist` string-shape into the SELECT-able pg column, independent of
whether the driver's verbose error is ever propagated. This is the
focal-list "v34 verbatim driver-msg propagation" finding evaluated
against current code rather than the planned v34 surface.

Net carry status (vs r24):

- **R22-S1 (CH stderr-tail / driver-msg leak)** — ELEVATED. Was OPEN
  on the driver-verbose-string axis (cross-worktree, not audited
  this round). NOW also OPEN on the CONTROLLER-EMITTED-path axis
  (`30960451` adds `assert_disk_image_present` whose Err strings
  embed full host filesystem paths AND the `sandbox_id` typed-id,
  reaching `wake_jobs.error_message` via `RestoreHandlerError::
  Backend(e).to_string()` → `sanitize_error_message` → pg write).
  See [R25-S1] below.
- **R21-S1 (restore-path typed_id validation)** — UNCHANGED. DB CHECK
  regex still the structural guard; no Rust-side `parse_with_prefix
  ("usr")` in `submit_restore_job` / `do_restore_inner`.
- **R20-S3 (driver SHA256 verify)** — UNCHANGED. `gs_pull` at
  `gcp-worker-startup.sh:146-159` still does NO integrity check; the
  v12→v13 driver bump (`364ead22`, `gcp-worker-startup.sh:176`)
  landed without any verify step. **Pre-cutover blocker** per r22.
- **R19-S1 (driver `EvalSymlinks` for rootfs_source/restore_from
  paths)** — UNCHANGED. `cfg.runtime_dir` flows directly into Config
  fields (`restore_handler.rs:2398, :2417`); the driver's path
  validator is `filepath.Clean`-only per the carry. Operator-trust
  surface unchanged.
- **R18-S1 (sanitize_error_message coverage)** — UNCHANGED. Doc-drift
  is r24-M1 (not re-logged here). Code at `wake_machine.rs:797-843`
  still matches IPv4/IPv6 only; **NO filesystem-path / NO typed-id
  stripping**. R25-S1 is the load-bearing consequence.
- **R20-C1 / WakeJobs `lessee_updated_at` write integrity** — CLEAN.
  R24-I1 (`1c255a00`) added error_code+error_message to the
  terminal-overwrite WARN but used the **unsanitized** `message`
  variable (`wake_machine.rs:200`). This is correct per the comment
  at `:185-194` (the WARN routes to operator-only journald; the
  sanitization invariant is "pg column only, retained T_KEEP=5min")
  — but it widens the journald-leak surface by one line. Not a new
  finding; documented in r24-M1 territory.

**Wire-surface focal checks** (refreshed since r24):

- **WakeErrorCode `error_code` wire field** — UNCHANGED. Enum variants
  at `db.rs:1493-1523` still name failure CLASS only. Leak-safe.
- **§10.0 envelope on `GET /admin/sandboxes/{id}/wake/{wake_id}`** —
  UNCHANGED. `render_wake_poll_response` at `admin_handlers.rs:1971-
  1989` renders `error_message` verbatim into the §10.0 envelope's
  `message` field. **The sanitized value is what lands here**; the
  R25-S1 path is "the sanitization is insufficient", not "no
  sanitization."
- **`assert_distinct_admin_tokens` boot guard** — VERIFIED.
  `lib.rs:1276-1302` refuses equal contents at boot. T1 contract
  observed. **Runtime rotation is NOT supported** — token reload
  requires controller restart (R25-A1 carry-note below).
- **C-N-W1 / `30960451` controller-side preflight** — net new behaviour
  this round. Three `format!(...)` sites at `nomad_ch.rs:3638-3661`
  and one `submit_restore_job` site at `restore_handler.rs:2071-2089`
  embed `path.display()` + `sandbox_id` + `user_id` into Err strings
  that flow into `wake_jobs.error_message`. See [R25-S1].

## CRITICAL

None.

## IMPORTANT

### [R25-S1] Controller-side preflight `30960451` now sources filesystem paths + typed_ids into `wake_jobs.error_message` (SELECT-able by `sandbox_audit`); `sanitize_error_message` has no path/typed-id coverage — R22-S1 elevated from latent to active

- **File**:
  - `crates/sandbox/src/backend/nomad_ch.rs:3638-3661`
    (`assert_disk_image_present` — three `format!` paths embedding
    `path.display()`)
  - `crates/sandbox/src/backend/nomad_ch.rs:3554-3613`
    (`create_ext4_image_if_missing` — five `format!` paths, four
    embed `path.display()`)
  - `crates/sandbox/src/backend/nomad_ch.rs:3679-3685`
    (`fsync_dir` — two `format!` paths embedding `dir.display()`)
  - `crates/sandbox/src/restore_handler.rs:2071-2089`
    (`submit_restore_job` preamble — embeds **both** `sandbox_id`
    (typed-id) AND `user_id` (typed-id) into the Err string)
  - `crates/sandbox/src/wake_machine.rs:769-787`
    (`sanitize_error_message` — IPv4/IPv6/agent-URL coverage only;
    NO filesystem-path stripping, NO typed-id stripping)
  - `crates/sandbox/src/admin_handlers.rs:1971-1989`
    (`render_wake_poll_response::Failed` — emits the sanitized
    `error_message` verbatim into the §10.0 envelope's `message`
    field on `GET /admin/sandboxes/{id}/wake/{wake_id}`)
  - `crates/sandbox/migrations/0009_wake_jobs.sql:126, 129`
    (`GRANT SELECT, INSERT, UPDATE, DELETE ... TO sandbox_app;
     GRANT SELECT ... TO sandbox_audit`)

- **Quote** (controller-side preflight, NEW this round at `30960451`):
  ```rust
  // restore_handler.rs:2071-2089
  crate::backend::nomad_ch::assert_disk_image_present(&workspace_img).map_err(|e| {
      format!(
          "restore submit: workspace.img missing for sandbox {} \
           (snapshot teardown should have preserved it via \
           stop_preserving_state; controller will not submit \
           restore job that the driver's preflight would reject \
           with a generic Failed-tasks rollup): {e}",
          sandbox_id
      )
  })?;
  crate::backend::nomad_ch::assert_disk_image_present(&user_home_img).map_err(|e| {
      format!(
          "restore submit: user_home.img missing for sandbox {} \
           user {} (per-user image should persist across the user's \
           sandboxes — source create() mkfs'd it; only host disk \
           corruption or out-of-band rm would explain this): {e}",
          sandbox_id, user_id
      )
  })?;
  ```
  and the inner `{e}` from `assert_disk_image_present`
  (`nomad_ch.rs:3639-3645`):
  ```rust
  let md = std::fs::metadata(path).map_err(|e| {
      format!(
          "disk image post-stage stat failed: {} ({e}); \
           controller-side parity check for driver preflight",
          path.display()
      )
  })?;
  ```

- **The chain end-to-end** (verified path-by-path this round):
  1. `submit_restore_job` Err — string of shape:
     `"restore submit: user_home.img missing for sandbox 0192abcd...
     user usr_01ABC123XYZ... (...): disk image post-stage stat
     failed: /var/zeroship/ch/users/usr_01ABC123XYZ/home.img (No
     such file or directory); controller-side parity check for
     driver preflight"`.
  2. `do_restore_inner` (`restore_handler.rs:867-868`) wraps into
     `RestoreHandlerError::Backend(e)`; `thiserror` `#[error("backend:
     {0}")]` at `:82-83` prepends `"backend: "`.
  3. `wake_machine::rollback_and_classify` (`wake_machine.rs:590-593`)
     emits `Phase::Failed { code: RestoreFailed, message: err.
     to_string() }`. The `message` carries the **full path-bearing
     prefix from step 1**.
  4. `wake_machine::set_state` Phase::Failed arm (`:158-181`) calls
     `sanitize_error_message(message)` — strips IPs but **NOT
     filesystem paths or typed_ids** — then writes to
     `wake_jobs.error_message` via `update_wake_job_state`.
  5. `GET /admin/sandboxes/{id}/wake/{wake_id}` (`admin_handlers.rs:
     1971-1989`) reads the row, emits the **sanitized message
     verbatim** into the §10.0 envelope `message` field. The
     endpoint is `AdminRole::ReadOnly` (`:1869`) — **the RO bearer
     reads it**.
  6. At the pg layer, `sandbox_audit` GRANTed SELECT
     (`migrations/0009_wake_jobs.sql:129`) — the role exists for
     non-controller audit consumers (Grafana, ad-hoc SQL probes,
     CI). The retained T_KEEP=5min window means recently-failed
     wakes are queryable.

- **Why r25 elevates R22-S1**: r22-r24 classified R22-S1 as latent
  because (a) the driver's `ClientDescription` string is only one
  vector path and (b) the v12/v13 driver was not audited cross-
  worktree. `30960451` **adds a parallel vector that does not depend
  on the driver at all**. Every controller-side staging-failure
  exit (workspace.img missing, home.img missing, mkfs failure,
  truncate failure, fsync failure, parent stat failure) now sources
  a host filesystem path + the sandbox_id typed-id into the same
  pg column. The carry "still latent until driver propagates" is no
  longer accurate; the leak is **active on every restore-path
  staging failure**.

- **What's leaked**:
  - `path.display()` — full absolute host filesystem path. Includes:
    - `/var/zeroship/ch/<sandbox_id>/workspace.img` (host_state_dir
      + sandbox UUID-simple)
    - `/var/zeroship/ch/users/<user_id>/home.img` (user_home_dir_root
      + typed_id `usr_…`)
    - `cfg.host_state_dir` if mistyped, plus parent dirs via
      `fsync_dir`.
  - `sandbox_id` (typed-id form) — directly into both `submit_restore
    _job` error messages.
  - `user_id` (typed-id form) — directly into the `user_home.img`
    error message.
  - The OS-level error (`{e}` from `std::fs::metadata`) — typically
    `(No such file or directory)`, low-information, but can be
    EACCES (which betrays mode/ownership), ENOTDIR (path-traversal
    surface), or ENAMETOOLONG (low entropy but confirms shape).

- **Threat model + practical impact**:
  - The **§10.0 envelope** is admin-bearer-gated (`AdminRole::
    ReadOnly` — the RO bearer is sufficient). The RO bearer is
    designed for "fleet enumeration, no write actions" per
    `admin_handlers.rs:155-159`. A leaked RO bearer now grants the
    attacker:
    1. The set of all `sandbox_id → user_id` mappings whose recent
       wakes failed (via the embedded typed_ids in
       `error_message`). The list cardinality is bounded by the GC
       sweep (T_KEEP=5min), but observation is cumulative — a
       sustained polling attacker over N×5min can enumerate the
       full user_id ↔ sandbox_id graph for any user whose wake
       hits the preflight path.
    2. The platform's filesystem layout (path roots, per-user
       sharding strategy) — useful for any future privilege-
       escalation that touches the host filesystem.
    3. Indication of staging-failure rate per user (a flooded
       `error_message` queue is itself a side-channel: which users
       are seeing repeat failures, which are seeing none).
  - The **pg `sandbox_audit` role** SELECT GRANT is the broader
    surface. The role exists for legitimate audit consumers; the
    expectation per the column comment is that SELECTs see
    operationally-useful (sanitized) failure shapes, not raw
    host-fs paths or typed_ids. A leaked audit credential pulls
    every wake_jobs row written in the T_KEEP window — the
    cross-tenant 404 guard at `admin_handlers.rs:1926-1934` does
    NOT apply at the pg layer.
  - **NOT in the threat model**: end-user / tenant-controlled JS in
    the sandbox runtime. End-user code has no path to
    `wake_jobs.error_message`. The leak is **admin-tier internal**
    — controller-internal staging diagnostics escape one trust
    band (operator bearer, audit role) wider than they need to.
  - **The leak does NOT compromise tenant-isolation** (tap CID,
    vsock CID, mount-ns, jailer chroot all stay driver-owned). The
    r24-A2 enumeration is a separate surface; R25-S1 is purely a
    diagnostic-string leak into an admin-tier column.

- **Why `sanitize_error_message` doesn't already catch this**: the
  function is byte-scan + match against 5 IPv4 prefixes + IPv6 LL.
  The TODO at `wake_machine.rs:752-757` already calls out the
  expansion list: "kerberos tickets, pubkey fingerprints, jwt
  suffixes, GCS signed-URL query strings". **Filesystem paths and
  typed_ids are not on that list**. R22-S1 noted this as a carry;
  R25-S1 makes it load-bearing because `30960451` actively writes
  exactly these shapes into the column.

- **Two-mode fix shape** (NOT prescribing — this is a security
  finding, not an implementation):
  - **Mode A** (sanitize widening, ~30 LOC): add two passes to
    `sanitize_error_message` — (a) strip `/var/zeroship/ch/...`
    absolute paths and any path beginning with `/` followed by 2+
    `/`-separated components (over-redact rather than under-
    redact), (b) strip typed_id literals matching the regex
    `(usr|sbx|wak|app|ses|prj)_[0-9A-Za-z]{20,40}`. The byte-scan
    pattern is the same shape as the existing RFC1918 matcher;
    adding it costs ~30 LOC + ~6 test cases.
  - **Mode B** (controller-side `err_safe` pattern): mirror the
    `admin_handlers::err_safe` shape (`admin_handlers.rs:328-345`)
    in the wake-machine pg-write path — log the unredacted error
    via `tracing::error!` and write a fixed string to the column.
    The cost is loss of fine-grained diagnostic info in the column
    (operators recover it from journald), trade-off for zero
    structural leak.
  - **Tertiary** (defense-in-depth): the `format!` sites at
    `restore_handler.rs:2071-2089` could omit the typed_ids
    entirely — the surrounding `tracing::error!` already carries
    `sandbox_id` + `user_id` as separate structured fields. Drop
    them from the `format!` template, keep them only in the
    structured-tracing event.

- **Severity**: **IMPORTANT** — admin-bearer-gated, not exploitable
  by tenant code, but the leak is now active on every controller-
  side staging-failure path (was latent / driver-side-only in r24).
  Pre-cutover priority is below R20-S3 (binary integrity) but above
  R21-S1 (defense-in-depth typed_id validation) because the new
  controller-emitted leak path is unconditional on every failure
  whereas R21-S1 is a hypothetical CHECK-bypass.

## MINOR

None new this round. (R24-M1 / R24-M2 carry unchanged.)

## Focal-list checks (per r25 brief)

### Focal #1 — v34 verbatim driver-msg propagation: sanitize coverage

**Status**: **OPEN** per R25-S1 above. The brief asks whether the
sanitize layer "still covers" the driver's `disk[1] /var/zeroship/
ch/<sandbox_id>/workspace.img does not exist` string. Answer: it
DOES NOT — `sanitize_error_message` strips IPs only, not paths or
typed_ids. The brief notes "sanitize_error_message already covers IP
ranges but NOT filesystem paths (R22-S1 carry)"; r25 confirms this
**and elevates it from carry to active** because `30960451` now
writes path-shaped strings into the pg column from the controller
side, not just from a hypothetical driver propagation. The v34
landing will add a parallel driver-side vector; r25's finding
applies to both.

The "path-leak becomes more important" framing in the brief is
correct: with `30960451` landed, the path-leak is ALREADY important
(controller-side write); v34 just doubles the source surface.

### Focal #2 — T1 admin_ro role: rotation / Full-bearer-delete behaviour

**Status**: **DOCUMENTED-LIMITATION, NOT A FINDING**. Walkthrough:

- Boot-time guard at `lib.rs:1276-1302` (`assert_distinct_admin_
  tokens`) refuses to boot when both files resolve to the same
  contents. **CORRECT.**
- Both tokens are loaded ONCE at boot into `state.admin_token` /
  `state.admin_ro_token` (`lib.rs:692-705`). There is no runtime
  reload path (`Grep "admin_token\s*=|reload"` returns only the
  builders and the boot-load site).
- **What happens when Full bearer is deleted on disk (rotation
  scenario)?**
  1. The file at `SANDBOX_ADMIN_TOKEN_PATH` is `rm`'d.
  2. The running controller's `state.admin_token` still holds the
     OLD secret in `Zeroizing<String>`.
  3. `admin_check_required` (`admin_handlers.rs:170-273`) reads
     `state.admin_token` — still `Some(OLD)`. **The old bearer
     still works**. No lockout, no fallback — but no rotation
     either.
  4. To complete rotation, operator MUST restart the controller.
     On restart, `load_admin_token` (`lib.rs:693`) returns
     `Ok(None)` (file missing). `admin_token` is `None` post-boot.
  5. `admin_check_required` for `AdminRole::Full` returns 503
     `admin_api_disabled` (`:194-201`).
  6. `admin_check_required` for `AdminRole::ReadOnly` requires
     `full.is_none() && ro.is_none()` for the 503 path (`:204`);
     since RO is still `Some`, the RO endpoints continue working.
     **Graceful fallback**: read endpoints stay up on the RO
     bearer alone.

  Verdict: the brief's question "is there a graceful fallback to
  ReadOnly?" — **YES, on controller restart**. Runtime hot-swap
  is unsupported (no inotify, no SIGHUP handler, no reload
  endpoint). The Round-3 design comment at `:156-159` explicitly
  cites this: "reads from the boot-cached state.admin_token …
  instead of stat()+read()'ing the file per request. Bounded
  amplification at 10k req/s; no slow-FS DoS; no fail-open on
  chmod-error." The trade-off is intentional.

- **What about runtime token rotation?** The boot-time
  `assert_distinct_admin_tokens` guard is the only equal-token
  defense. There is no equivalent runtime guard, because there is
  no runtime reload. If an operator (a) replaces the contents of
  `SANDBOX_ADMIN_RO_TOKEN_PATH` with the contents of
  `SANDBOX_ADMIN_TOKEN_PATH` while the controller is running, then
  (b) restarts the controller, **the boot guard catches it**. There
  is no window where the controller is running with equal tokens.
- **What if the operator hot-edits `SANDBOX_ADMIN_TOKEN_PATH` to
  equal `SANDBOX_ADMIN_RO_TOKEN_PATH` contents WITHOUT restarting?**
  The cached in-memory copies stay distinct; the equality-check
  guard is bypassed only because it doesn't run at request-time.
  After restart, the guard catches it.

  **This is fine** — the threat model "operator who maliciously
  hot-edits both token files to equal contents and DOES NOT
  restart" is degenerate (the operator already has root and can
  do anything). The boot-time guard is the right place for this
  invariant.

- **Documented-limitation, not a new finding**: there is no admin
  bearer rotation runbook in `docs/`; an operator going through
  this exercise without context might mis-stage. A future runbook
  could codify the "rm Full file → wait for restart → controller
  comes up Full=None, RO-only ⇒ /admin reads keep working" path.
  Out of scope for security r25.

### Focal #3 — R24-A1 typed StagingManifest: content-hash integrity (planned)

**Status**: **NOT-YET-LANDED, FORWARD-LOOKING NOTE**.

r24-A1 (architecture-r24, lines 24-113) describes a Phase 2 driver-
side handshake that includes `staging_manifest_sha=...`. The brief
asks: "if the manifest carries content-hashes, are those hashes
integrity-checked at the driver side, OR does the driver trust the
controller blindly (operator trust model)?"

**Answer**: r24-A1 prescribes the **manifest itself** carries an
SHA fingerprint (`staging_confirmed{manifest_sha=...}` task event,
arch-r24 line 104) — that's the integrity of the MANIFEST, not the
disk-image contents. The disk-image content-hashing surface is a
SEPARATE design dimension and is **NOT in the r24-A1 prescription**.

For r24-A1 as written:
- The manifest is operator-trust (controller emits, driver
  deserializes). No tenant-controllable bytes enter the manifest.
- The disk-image bytes are operator-trust today (workspace.img is
  freshly mkfs'd; home.img persists across user sandboxes but is
  driver-controlled-via-bind-mount). No content-hashing today.
- If a future R24-A1 Phase 3 adds disk-image content-hashing
  (e.g., to detect host-disk-corruption between source-snapshot
  and restore), the integrity check would have to land DRIVER-
  SIDE (controller doesn't read the file in the staging path —
  `assert_disk_image_present` only stats size>0).

**Forward-looking lens for the implementer**: if Phase 2 ships,
the manifest itself should carry a `controller_signed: hex(sig)`
field so the driver doesn't accept manifests forged by any other
process on the worker host (e.g., a compromised process with write
access to the alloc dir). Today the Nomad jobspec is the trust
boundary — manifests INSIDE the jobspec inherit that trust. If
manifests escape the jobspec (e.g., written to a sidecar file the
driver reads), they need their own integrity proof. **Recommend
keeping the manifest INSIDE the jobspec Config** to avoid this
class entirely; arch-r24 line 100 hints at this ("`staging_
manifest` JSON field" in the jobspec) so the design is on the
right axis.

### Focal #4 — R24-A2 kernel-state surface audit: tenant-isolation vs operator-only

**Status**: **NOT-YET-LANDED, FORWARD-LOOKING NOTE**.

r24-A2 (architecture-r24, lines 117-166) enumerates kernel-state
surfaces: tap, cgroup, mount-ns, vsock CID, jailer chroot, PID
files, systemd transient units. The brief asks: "which of these
are tenant-isolation surfaces vs operator-only? A leak across
vm_index reuse where two SANDBOXES share the same vm_index N
(impossible per design but worth confirming) would be a sandbox-
escape primitive."

**Audit by surface** (from a security lens; arch-r24's table is the
operational lens):

| Surface | Tenant-iso? | If leaked across vm_index reuse |
|---|---|---|
| tap interface `zsbx-nm-N` | **YES** | New sandbox sees prior sandbox's IP/MAC + any pending packets. **Sandbox-escape primitive if the prior sandbox's agent socket was still bound to the host IP**. C-N-W2 fix at `3d03cb90` is correct on tenant-isolation grounds; pre-delete on EEXIST closes the cross-cycle bleed-through. |
| cgroup hierarchy | weak — affects resource-accounting, not direct iso | Operator-only (CH process resource limits). No tenant-data path through cgroup state. |
| mount-namespaces | **YES if bind-mounts persist across alloc** | Stale bind-mount could expose prior sandbox's `workspace.img` contents to new sandbox. Per the per-sandbox `host_dir = host_state_dir/<sandbox-id>` shape (`nomad_ch.rs:2300`), workspace.img is sandbox_id-keyed, not vm_index-keyed — so a vm_index-N reuse does NOT collide on workspace.img path. **Iso preserved by the sandbox_id partition, NOT by vm_index reuse safety per se.** |
| vsock CID | **YES** | Two VMs colliding on a CID would fail at CH spawn (CH refuses) — fail-fast is iso-preserving. The brief's "impossible per design" is correct because vsock CID is derived from vm_index at CH config time (`nomad-vm-wrapper.sh` line ~ vm_index → cid mapping). **Iso preserved by vm_index-serialised allocation.** |
| jailer chroot | **YES if jailer is used** | Stale chroot dir would expose prior sandbox's files. Per the nomad-vm-wrapper.sh and ch-plugin Config inspection, the controller emits `kernel`/`workspace_img`/`user_home_img`/`rootfs_source` paths but NOT a jailer-chroot path; the driver may or may not use jailer internally. **Audit deferred to driver-side (cross-worktree per R19-S1 carry)**. |
| PID files / lock files | **YES if PID re-use mis-targets a kill** | Stale `<runDir>/ch.pid` with re-used PID could mis-target signals. Cross-iso impact: SIGKILL to wrong process. **Severity bounded by runDir per-alloc partition** (Nomad allocates fresh `NOMAD_TASK_DIR`). |
| systemd transient units | **YES if unit-name re-used** | Stale unit could re-execute on systemd reload. Bounded by whether driver uses `systemd-run --scope`. Operator-only impact most likely. |

**vm_index reuse across SANDBOXES collision check**: the brief notes
"impossible per design but worth confirming." Reviewed
`VmIndexAllocator` (`nomad_ch.rs:1944` ff.) — vm_index is reserved
for the LIFETIME of the sandbox (cold-boot through delete), with
`host_fence` (30s in cluster) interlocking release. Two **distinct
sandbox_ids** holding the same vm_index simultaneously would
require (a) the source sandbox's vm_index reservation to be
released BEFORE the host-fence clears OR (b) `VmIndexAllocator`
itself to race. (a) is structurally prevented by
`stop_preserving_state`'s fence-gated release; (b) is a mutex
inside the allocator. **Confirmed iso-safe at the controller
level.** The kernel-state leaks r24-A2 enumerates are post-release-
of-vm_index hazards (the slot is free, the new sandbox claims it,
the kernel-state from the OLD sandbox still references the
`zsbx-nm-N` tap with the old IP). Iso preservation depends on
DRIVER-side cleanup completing before NEW alloc's StartTask fires
— which is exactly what `3d03cb90` (pre-delete tap on EEXIST)
addresses.

**Recommendation for the r24-A2 audit deliverable**: explicitly
mark each surface (tap, cgroup, mount-ns, vsock CID, jailer
chroot, PID files, systemd units) as **tenant-isolation** vs
**operator-only**. The cutover-blocker list should be the
tenant-isolation subset; operator-only surfaces can ship with
"known stale dir, opportunistic cleanup" without blocking.

### Focal #5 — R20-S3 driver SHA256 verify (carry)

**Status**: **OPEN, UNCHANGED**. Re-verified at HEAD `03d3470f`:

```bash
# crates/sandbox/scripts/gcp-worker-startup.sh
gs_pull() {           # :146-159 — no integrity check, just retry-on-error
  local src=$1 dst=$2 mode=${3:-0755}
  ...
  for attempt in 1 2 3 4 5; do
    if gsutil -q cp "gs://$ARTIFACT_BUCKET/$src" "$dst"; then
      chmod "$mode" "$dst"
      return 0          # ← returns WITHOUT verifying sha256
    fi
    ...
  done
  exit 1
}

# Line 176, in the INSTALL_CH_PLUGIN_DRIVER block:
gs_pull nomad-driver-ch.v13 /etc/zeroship/nomad-plugins/nomad-driver-ch 0755
#       ^^^^^^^^^^^^^^^^^^^ — the driver binary pulled with NO SHA verify
```

The v12→v13 pin bump (`364ead22`) landed without adding any verify
step. Commit message at `364ead22` even cites the expected
SHA256: `b34a7a65b91fb40b902da1535d3ba7cacefcbe70ef1f9fa1920c
7a6a75ea5a4b` — that string is in the commit body but NOT in any
script that would check it post-pull. **The integrity check is
asserted in the commit log, not enforced at runtime.**

Threat model: a worker host pulls the binary every first boot.
The transport is `gs://` (HTTPS to GCS) — TLS protects in-flight.
The trust boundary is **GCS itself + the GCE worker SA**. If the
bucket is compromised (write access to `gs://suger-dev-zsbx-
artifacts/nomad-driver-ch.v13`), every worker bootstrap pulls and
runs the attacker-controlled binary as root in the worker node.
Storage-rw scope (R13-S1) compounds this: every worker has WRITE
access to the same bucket, so a single worker compromise lets the
attacker rewrite the binary for the entire fleet's next bootstrap.

**Pre-cutover blocker. Same severity, same fix shape as r24
(pin a `nomad-driver-ch.v13.sha256` sibling object; `gs_pull`
fetches and verifies before chmod).**

### Focal #6 — R22-S1 controller-side error sanitization for filesystem paths

**Status**: **OPEN, ELEVATED to ACTIVE per R25-S1**. See R25-S1
above for the full chain; this focal-list item is the same
finding from a different framing.

### Focal #7 — R19-S1 driver EvalSymlinks for rootfs_source / restore_from paths

**Status**: **OPEN, UNCHANGED**. The controller emits
`rootfs_source = cfg.runtime_dir.join("rootfs-slim.img")` at
`restore_handler.rs:2398` and `restore_from = alloc_dir.display()
.to_string()` at `:2406`. Both are operator-trust paths. The
driver-side `filepath.Clean` resolves `..` but NOT symlinks; a
symlinked component anywhere along the path would let the driver
hardlink/copy from a different file than the operator intended.
**Cross-worktree, not audited this round** — same status as r24.

## Carry-forward open at HEAD `03d3470f`

| ID         | Sev       | File:line                                                                                  | Status at r25                                                                                          |
|------------|-----------|--------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------|
| **R25-S1** | IMPORTANT | `restore_handler.rs:2071-2089`; `nomad_ch.rs:3638-3661, :3554-3613, :3679-3685`; `wake_machine.rs:769-787`; `admin_handlers.rs:1971-1989`; `migrations/0009_wake_jobs.sql:126, 129` | **NEW this round** — `30960451` controller-side preflight sources host filesystem paths + typed_ids into `wake_jobs.error_message`. R22-S1 elevated from latent to active. |
| R22-S1     | IMPORTANT | `nomad-driver-ch/ch/restore_task.go:529-554` (driver, cross-wt); `nomad_ch.rs:2614-2620`; `wake_machine.rs:757-775`     | OPEN — driver v13 not audited cross-wt; controller-side now active per R25-S1. Sanitize widening covers both. |
| R21-S1     | IMPORTANT | `restore_handler.rs:2256-2259, :2368`                                                       | OPEN — DB CHECK regex remains the structural guard; defense-in-depth gap unchanged.                    |
| R21-S2     | IMPORTANT | (driver-side, cross-worktree)                                                              | OPEN — driver validator-call symmetry not audited.                                                     |
| R20-S3     | IMPORTANT | `crates/sandbox/scripts/gcp-worker-startup.sh:146-176`                                     | OPEN — v12→v13 bump (`364ead22`) landed without adding SHA256 verify. **Pre-cutover blocker.**        |
| R20-S2     | IMPORTANT | (driver-side, cross-worktree)                                                              | OPEN — driver `cfg.SandboxId` validator not audited.                                                   |
| R19-S1     | IMPORTANT | (driver-side, cross-worktree); controller emit at `restore_handler.rs:2398, :2406`         | OPEN — driver `filepath.Clean`-only; controller emits raw paths without symlink resolution.            |
| R18-S1     | IMPORTANT | `wake_machine.rs:740-748` (doc); `:797-843` (code)                                          | PARTIAL — code matches 5 IP ranges; doc names 3 (r24-M1); typed_id / path coverage **load-bearing per R25-S1**. |
| R13-S1     | IMPORTANT | `crates/sandbox/scripts/provision-gcp-cluster.sh:286`                                       | OPEN — `--scopes=storage-rw` + default GCE SA unchanged. ≥11 rounds open.                              |
| R9-S3      | IMPORTANT | `crates/sandbox/src/snapshot_handler.rs:417`                                                | OPEN — `Some("v1")` stamp regardless of AEAD posture; not re-examined this round.                      |
| R20-S1     | MINOR     | (config)                                                                                   | OPEN — `ContentAddressedRootfsRoots` slot empty.                                                       |
| R15-S3     | MINOR     | (config / docs)                                                                            | OPEN — 30s fence cap undocumented.                                                                     |
| R17-S2     | MINOR     | (KEK provisioning)                                                                         | OPEN — no explicit `chown root:root`.                                                                  |
| R18-S2     | MINOR     | (logging)                                                                                  | OPEN — fence-error IP leak into scoped log.                                                            |
| R24-M1     | MINOR     | `wake_machine.rs:740-748`                                                                  | OPEN — doc-comment understates `match_rfc1918_at` coverage (carry from r24).                           |
| R24-M2     | MINOR     | `restore_handler.rs:2352, :2371`                                                            | OPEN — operator-trust footprint widened by one driver-Config path field (carry from r24).              |

## Cross-lens consensus

- **architecture r24** (`docs/reviews/sandbox-snapshot-restore-
  architecture-2026-05-25-r24.md`): r24-A1 (typed StagingManifest)
  + r24-A2 (kernel-state surface audit) are the two CRITICALs
  blocking cutover. Security agrees both are correctly scoped as
  CRITICAL; r24-A1 Phase 2 is the right place to formalise the
  manifest's content-hash integrity question (focal #3 above); r24-
  A2 needs the tenant-iso-vs-operator-only split I sketched in
  focal #4. **Security adds no new CRITICAL this round.**

- **test-coverage r24** (`...-test-coverage-2026-05-25-r24.md`,
  cited from filename): R24-T1 (stress harness not in-repo) is the
  CRITICAL blocker on iterating R25-S1's fix — a regression test for
  "preflight failure produces a path-free `error_message`" needs
  the stress harness or a synthetic unit test. The unit-test path
  is cheaper (mock `assert_disk_image_present` to return Err with
  a known path; assert the post-`sanitize_error_message` string
  has no `/` characters or typed_id literals).

- **code-quality r24**: R24-I1 (`1c255a00`) added error_code +
  error_message to the terminal-overwrite WARN — uses the
  **unsanitized** `message` per the comment at `wake_machine.rs:
  185-194`. Security agrees the journald destination is operator-
  only and the sanitization invariant applies only to the pg
  column. **No new finding from R24-I1.**

- **smoke-r23 GREEN** + **stress-r2 RED (2/60)**: smoke success
  proves no cleartext tenant data leaks through the §10.0-
  enveloped failed body (no `failed` rows on success). Stress RED
  is the **load-bearing** evidence for R25-S1: the failures are
  exactly the preflight path where `assert_disk_image_present`
  fires, which is exactly the surface that now writes
  path-bearing strings into `wake_jobs.error_message`. The
  empirical surface area for R25-S1 is non-zero — this is what
  elevates R22-S1 from latent to active.

## Lens hand-off

- **Sandbox controller (Rust)**: R25-S1 is the cheapest defense-in-
  depth fix this round — ~30 LOC widening `sanitize_error_message`
  with path + typed_id stripping passes, plus 6 test cases mirroring
  the existing RFC1918 test pattern. Pair with R22-S1's eventual
  driver-side `ch_stderr_tail=%q` drop for the symmetric closure.
  Lower-risk than R21-S1 (which requires `parse_with_prefix` plumbing
  through `submit_restore_job` / `do_restore_inner`).

- **nomad-driver-ch maintainers (R22-S1 + R20-S2 + R21-S2 + R19-S1)**:
  cross-worktree backlog unchanged from r24. Driver v13 landed at
  `3d03cb90` (tap pre-delete on EEXIST) without addressing any of
  the four security carries. Next driver pin bump (v14 per focal-
  list) is the right cut for: `ch_stderr_tail=%q` drop,
  `isTypedID(cfg.SandboxId/UserId)`, `EvalSymlinks` upgrade, and
  validator-call symmetry test.

- **Ops / cluster bring-up (R13-S1 + R20-S3)**: WORM-propagation
  loop and `gs_pull` SHA256 gap unchanged across the v12→v13 bump.
  Both pre-cutover blockers. Driver v13 ships without integrity
  enforcement at runtime; the SHA in the commit message is operator-
  trust, not machine-enforced.

- **api-surface lens**: §10.0 envelope's `message` field is the
  wire-visible terminus of R25-S1's leak chain. The contract
  intentionally surfaces error context to operators — the fix is
  upstream of the wire (sanitize the column write) rather than at
  the wire (which would lose information uniformly). No api-
  surface action.

- **Documentation**: a future operator runbook should codify the
  "graceful Full-bearer rotation on controller restart" flow
  documented in focal #2. Out of scope for security but worth a
  hand-off to ops docs.

## Counts

- CRITICAL: 0 new; carry: 0.
- IMPORTANT: 1 new (**R25-S1** controller-side path+typed_id leak
  into `wake_jobs.error_message`); carry: R22-S1 (now elevated),
  R21-S1, R21-S2, R20-S3, R20-S2, R19-S1, R18-S1, R13-S1, R9-S3.
- MINOR: 0 new; carry: R20-S1, R15-S3, R17-S2, R18-S2, R24-M1,
  R24-M2.
- Total NEW this round: 1 IMPORTANT.
- **Cutover gate**: R20-S3 (driver binary integrity) + R13-S1
  (storage-rw scope) remain pre-cutover blockers; landings since
  r24 did not address either. R25-S1 is **post-cutover acceptable**
  (admin-bearer-gated, not tenant-exploitable) but should land
  inside the v34 bundle alongside the planned driver-msg
  propagation since both write into the same column.
