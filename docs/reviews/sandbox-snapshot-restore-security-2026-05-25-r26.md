# Sandbox/snapshot-restore — security r26 review

Date: 2026-05-25 (UTC)
HEAD at audit: `3d431eb8`. Lens: security (READ-ONLY).
Predecessor: r25 at `03d3470f`. Landings since r25 (22 commits):
- `7b5d84f5` — `AdminRole::{Full, ReadOnly}` + `admin_check_required` (T1 base)
- `97fcbcda` — `admin_ro_token` boot + AppState field + two-distinct-tokens guard (T1)
- `038ff3c7` — admin role authorization matrix e2e tests
- `61492e54` + `dd2079a9` — T-8b stress harness in-repo + SHA-pin (R24-T1)
- `d638b10f` — controller v34: leak host_dir on stop + verbatim driver-msg propagation
- `e82bffd7` — controller v34: `host_dir` GC sweeper task
- `c729c2b8` — driver v13→v14 + controller v33→v34 pin bump
- `79871194` + `022f778a` + `6476d18b` — `WakeErrorCode::StagingPathMissing` +
  typed `RestoreHandlerError::StagingPreflight` + close R23-API1 / R25-I1 / R25-I2 / R25-S1
- `28fa64d1` — restore-debug-playbook ADR
- `3e853cc6` — kernel-state-surface-inventory ADR
- `d0a744ce` — T-8b-stress-r3 cluster review (RED, 1/60 e2e OK)
- `3d431eb8` — Driver Failure events preferred over Alloc Unhealthy in
  verbatim msg propagation (T-8b-stress-r3 r3-C)
- `883df7fe` — `AppState::local_nomad_node_id` cached at boot via
  `GET /v1/agent/self` (r3-A precursor; **jobspec Constraints emit
  still in flight**)

## Summary

**r26 produces ZERO new CRITICALs and ZERO new IMPORTANTs.** Two carries
were CLOSED (R25-S1 by `022f778a`/`6476d18b`; T1 boot guard verified
correct). Two new attack surfaces were introduced and triaged below
the IMPORTANT bar:

- **R25-S1 — CLOSED**. Typed `StagingPreflight` variant + path-free
  Display impl + `log_detail` tracing-side capture closes the
  controller-side preflight path-leak. The §10.0 envelope `message`
  field stays path-free across all three callers (sync POST,
  async wake poll, sync mode).
- **r3-A precursor (`883df7fe`) — NEW LATENT SURFACE, BELOW IMPORTANT
  THRESHOLD UNTIL EMIT LANDS**. `fetch_local_nomad_node_id` already
  caches the local node ID at boot, but the Constraints emit in the
  jobspec builders is the IN-FLIGHT half. The brief flags two
  attack vectors that activate **only when the emit lands**:
  tampered local Nomad agent → wrong node_id → DoS;
  misconfigured `NOMAD_ADDR` pointing remote → cross-cluster
  placement. r26 documents both as r26-S1 (post-landing) carries.
- **R22-S1 carry — ELEVATED to ACTIVE via driver-msg propagation
  (`d638b10f` + `3d431eb8`)**. The verbatim driver TaskEvent
  DisplayMessage path now reliably plumbs path-bearing strings into
  `wake_jobs.error_message` via `RestoreHandlerError::Backend(s)` →
  `Display` → `sanitize_error_message` (which still strips IPs only,
  not paths). Same column, same RO-bearer audience. R25-S1's
  StagingPathMissing closure does NOT cover this axis. **See r26-S2.**

Net carry status (vs r25):

- **R25-S1 (controller-side path leak via assert_disk_image_present)**
  — **CLOSED at `022f778a` / `6476d18b`**. The typed
  `RestoreHandlerError::StagingPreflight` variant has a path-free
  Display (`"staging image missing: {which} for {sandbox_id_typed}"`)
  and the controller-side `SubmitRestoreError::log_detail` captures
  the verbatim host path + source error via tracing **before** the
  path-free typed map crosses the wake-machine boundary. Verified
  end-to-end across all three callers (see Focal #1).
- **R22-S1 (driver-msg leak)** — **ELEVATED to ACTIVE, NEW WIRE PATH**.
  `d638b10f` + `3d431eb8` (the v34/r3-C verbatim-driver-msg
  propagation) is now harvesting driver TaskEvent `DisplayMessage`
  strings — including the v14 driver's `StartTask: disk[1]
  workspace.img does not exist` shape — and folding them into
  `SubmitRestoreError::Other(s)` → `RestoreHandlerError::Backend(s)` →
  `wake_jobs.error_message`. The StagingPathMissing closure does NOT
  cover this axis because the driver msg flows through the OTHER arm
  of the `submit_result` match (`Err(SubmitRestoreError::Other(s))`),
  not the `Preflight` arm. **See r26-S2 below.**
- **R20-S3 (driver SHA256 verify in gcp-worker-startup.sh)** —
  **UNCHANGED, STILL OPEN, STILL PRE-CUTOVER BLOCKER**.
  `gcp-worker-startup.sh:180` pulls `nomad-driver-ch.v14` via the
  no-verify `gs_pull` helper. The R24-T1 SHA-pin pattern landed at
  `dd2079a9` for `snapshot_stress.py` only (lines 217, 234-249) —
  the driver binary download was not extended with the same pattern
  in the v13→v14 bump (`c729c2b8`). See Focal #3 for the exact
  fix shape.
- **R21-S1 (restore-path typed_id validation)** — UNCHANGED. Defense-
  in-depth gap; DB CHECK regex still the structural guard.
- **R19-S1 (driver `EvalSymlinks` for rootfs_source/restore_from
  paths)** — UNCHANGED. Cross-worktree carry. Driver v13→v14 bump
  did not address. See Focal #6.
- **R18-S1 (sanitize_error_message coverage)** — **LOAD-BEARING via
  R22-S1 reactivation**. The function still matches IPv4/IPv6 only
  (`wake_machine.rs:812-830` plus `match_rfc1918_at` at `:846-891`);
  NO filesystem-path / NO typed-id stripping. With R22-S1 now
  actively wiring driver paths into `Backend(s)`, the sanitize gap
  is the load-bearing leak.
- **T1 admin_ro role boot guard** — **VERIFIED CORRECT**. See Focal #4.

**Wire-surface focal checks** (refreshed since r25):

- **WakeErrorCode wire enum** — Extended by `79871194` to
  `staging_path_missing` (pg form) / `staging_image_missing` (wire
  form). Domain CHECK migration `0013_wake_jobs_staging_path_missing_code.sql`
  enforces at the pg layer. Wire forms still name failure CLASS only
  (`db.rs:1578` + `:1653`). **Leak-safe.**
- **§10.0 envelope on `GET /admin/sandboxes/{id}/wake/{wake_id}`** —
  `render_wake_poll_response` at `admin_handlers.rs:1971-2019`
  renders `row.error_message` verbatim into the envelope `message`
  field. Sanitization happens at WRITE-time
  (`wake_machine.rs:158-181`, line 165 `sanitize_error_message`
  applied before `update_wake_job_state`). The new
  `StagingPathMissing` shape writes a path-free Display string at
  source; the sanitize layer is a defense-in-depth pass over it.
- **`assert_distinct_admin_tokens` boot guard** — **VERIFIED at
  `lib.rs:1359-1385`**. Uses `subtle::ConstantTimeEq::ct_eq` on the
  byte slices, returning the canonical `subtle::Choice` masked into
  a `bool` via `.into()`. The `Choice` type guarantees constant-time
  compare AND equal-length-required semantics (slices of unequal
  length short-circuit to `Choice(0)` via length compare without an
  early-exit branch in user code). **No early-exit; no plaintext
  comparator; correctness verified.** See Focal #4.
- **r3-A precursor `883df7fe` cached node_id** — net new surface
  this round. `AppState::local_nomad_node_id` is the cache slot
  (`lib.rs:225`); jobspec emit not yet present. See Focal #2.

## CRITICAL

None.

## IMPORTANT

None new. Carries (all OPEN):

### [R22-S1] ELEVATED — verbatim driver TaskEvent paths now plumb into `wake_jobs.error_message` via `Backend(s)`; StagingPathMissing closure does not cover this axis

- **File**:
  - `crates/sandbox/src/backend/nomad_ch.rs:2660-2700`
    (`wait_for_alloc_running_blocking` — terminal-failure path,
    composes `extract_failed_task_event_msgs` output into the
    returned `String`)
  - `crates/sandbox/src/backend/nomad_ch.rs:2817-2899`
    (`extract_failed_task_event_msgs` — walks `TaskStates[*].Events[]`,
    prefers Driver Failure / Task Setup Failure / Killing events
    over Alloc Unhealthy)
  - `crates/sandbox/src/restore_handler.rs:2287-2295`, `:2302`
    (`submit_restore_job` — `wait_for_alloc_running_blocking` Err
    folds into `SubmitRestoreError::Other(s)`)
  - `crates/sandbox/src/wake_machine.rs:426-430`
    (`Err(SubmitRestoreError::Other(s)) => RestoreHandlerError::Backend(s)`)
  - `crates/sandbox/src/wake_machine.rs:623-626`
    (`rollback_and_classify` → `Phase::Failed { code:
    classify_failure(&err), message: err.to_string() }` — calls
    `Display` on `RestoreHandlerError::Backend("...")` which renders
    `"backend: ..."` verbatim, paths intact)
  - `crates/sandbox/src/wake_machine.rs:812-830`
    (`sanitize_error_message` — IPv4/IPv6 only; **no path stripping**)

- **Quote + chain** (driver-msg propagation, NEW this round):
  ```rust
  // nomad_ch.rs:2688-2697 — wait_for_alloc_running_blocking
  let driver_msgs = extract_failed_task_event_msgs(a);
  let composed = format!(
      "nomad alloc terminal status={cs}: {desc}: {}",
      driver_msgs.join(" | "),
  );
  return Err(composed);
  ```
  Chain: driver v14 `StartTask` emits `Driver Failure` TaskEvent
  with DisplayMessage `"StartTask: disk[1] /var/zeroship/ch/<sid>/
  workspace.img does not exist ..."` → `extract_failed_task_event_msgs`
  composes verbatim into `Err(String)` → `submit_restore_job` wraps
  as `SubmitRestoreError::Other(s)` (`restore_handler.rs:2302`) →
  wake_machine folds into `RestoreHandlerError::Backend(s)`
  (`wake_machine.rs:426-430`) → `rollback_and_classify` Display →
  `set_state` Phase::Failed calls `sanitize_error_message` (IPs
  only, NO path stripping) → `wake_jobs.error_message` →
  `render_wake_poll_response` verbatim into §10.0 `message` field.
  **`AdminRole::ReadOnly` is sufficient.**

- **Why R25-S1 closure does NOT cover this**: R25-S1's fix is
  upstream of alloc submission — `submit_restore_job`'s
  controller-side `assert_disk_image_present` short-circuits to
  `SubmitRestoreError::Preflight` BEFORE the Nomad POST (path-free
  by construction). The DRIVER's preflight (cross-worktree) fires
  AFTER the controller's POST succeeded, reaches the controller via
  `Events[].DisplayMessage`, and lands in
  `SubmitRestoreError::Other(s)` — the OTHER arm of the
  `submit_result` match. Closure was on the controller-side fork
  only; the race window (CreateGuard::drop on a sibling alloc's
  failure unlinks host_dir mid-flight; driver preflight then
  ENOENTs) is documented in d638b10f / r24-A3 ADR and is the
  expected verbatim-observable.

- **What's leaked**: driver TaskEvent DisplayMessage strings —
  v14 carries verbatim host paths (`/var/zeroship/ch/<sid>/
  workspace.img`); whatever other paths the v14 driver embeds
  (rootfs_source, restore_from, jailer chroot if used) — all into
  `wake_jobs.error_message`, RO-bearer-readable via §10.0.

- **Threat model + practical impact**: identical to R25-S1's
  pre-closure shape (admin-bearer-gated, not tenant-exploitable);
  lifts the RO bearer's horizon from "wake outcomes" to
  "filesystem topology + tenant-id enumeration via repeat polling".
  See r25 § R25-S1 for the full shape; r26 reaffirms under the
  new active vector.

- **Fix shape** (NOT prescribing; same shape as r25 documented for
  R25-S1):
  - **Mode A** — extend `sanitize_error_message` with path-stripping
    + typed-id stripping passes (~30 LOC + 6-8 tests; mirror
    `match_rfc1918_at`). **Catches BOTH the closed R25-S1 axis AND
    this newly-active R22-S1 axis** because both flow through the
    same `update_wake_job_state` write site. Highest-leverage single
    patch in the carry-forward list.
  - **Mode B** — split driver-msg propagation: tracing-side verbatim
    (operator-only journald) + column-side stripped (paths replaced
    with `<workspace.img>` / `<user_home.img>` / `<rootfs.img>`
    classifier tags). Higher mechanism cost; preserves operator
    observability contract.
  - **Mode C** (r24-A3 lens) — the verbatim msg IS the operator
    observable; right fix is to *route* to a different sink. journald
    path already populated via unsanitized `tracing::warn!`
    (`wake_machine.rs:166-172`); pg column carries structured
    `error_code` + sanitized message. Same density, no path leak.

- **Severity**: **IMPORTANT** (carries forward from R25-S1; leak
  active again via OTHER fork of same wire path). Pre-cutover
  priority below R20-S3 but inside the same v34/v14 bundle.

### [r26-S1] r3-A `fetch_local_nomad_node_id` precursor introduces local-Nomad-trust + NOMAD_ADDR-misconfig attack surfaces (latent until Constraints emit lands)

- **Status**: NEW surface (`883df7fe`), **NOT YET WEAPONISED** —
  precursor caches node_id; jobspec Constraints emit (IN-FLIGHT
  half) hasn't landed. Documenting so the IMPORTANT bar is set
  BEFORE the emit lands.
- **File**: `nomad_ch.rs:3080-3093` (`fetch_local_nomad_node_id` —
  `GET /v1/agent/self`); `:3106-3135` (`parse_nomad_agent_self_node_id`);
  `lib.rs:670-698` (boot-time fetch; non-fatal demotion to counter
  + WARN); `lib.rs:225` (`AppState.local_nomad_node_id` cache slot).
- **Threat model** (per brief):
  - **(1) Tampered local Nomad agent → wrong node_id → DoS**: a
    compromised process on the worker host (e.g., a Nomad-plugin-side
    compromise, the very thing R20-S3 admits today via the
    un-SHA-pinned driver download) can substitute `/v1/agent/self`
    responses. Once the Constraints emit lands, every CREATE/restore
    alloc pins to the attacker-chosen node → if that node doesn't
    exist, every alloc unschedulable. **DoS-scoped**. Network MITM
    out of scope (nomad_addr is `127.0.0.1:4646`).
  - **(2) `NOMAD_ADDR` misconfig to remote agent**: boot-time fetch
    returns the remote agent's node_id; controller pins all allocs
    to *that* remote node. Allocs land cross-cluster; controller's
    local staging dir is irrelevant; driver-side preflight on the
    remote ENOENTs. **Operator-trust boundary** — not an adversary.
    Widens the blast radius of misconfig from "fail at probe" to
    "fail silently per-alloc".
- **Why below IMPORTANT bar**: (1) is gated on the driver SHA pin
  landing (R20-S3); attacker who substitutes the driver binary
  doesn't need this surface. (2) is operator-trust, not adversary
  — UX regression rather than security regression.
- **Forward-looking fix shape** (NOT prescribing):
  - Refuse to boot when `nomad_addr` host is not `127.0.0.1`/`::1`/
    `localhost`/unix path (closes vector 2).
  - After successful node_id fetch, cross-check by hitting `/v1/node/
    <node_id>` and verifying `Node.HTTPAddr` against the controller's
    own PRIVATE_IP / hostname (closes vector 1 modulo full Nomad
    compromise).
  - Both defense-in-depth; neither required pre-emit-landing.
- **Re-classify to IMPORTANT** in the round the Constraints emit
  lands iff EITHER guard is missing.

### [r26-S2] CI does not enforce r24-A3 "verbatim observable before defense" — policy-only

- **Status**: process-side gap; MINOR-equivalent. Brief asked to
  verify.
- **Evidence**: grepped `.github/workflows/ci.yml` (the only
  workflow) for `observabil|verbatim|playbook|debug-playbook` — no
  matches. The ADR codifies a discipline applied at review time
  but is not machine-enforced.
- **Why this matters for security**: a PR landing a silent path-
  stripping pass in `extract_failed_task_event_msgs` (the obvious
  "fix" to R22-S1 above) would close the leak surface AND close
  the operator-observable surface that d638b10f added. The ADR's
  rule "route it to a different sink, not silence it" is the
  load-bearing invariant; without CI enforcement, an out-of-context
  fixer might pick the wrong axis.
- **Out of scope for r26**: a CI hook (PR-body check / commit-msg
  check / lint) requiring verbatim-observable citation for changes
  to `sanitize_error_message`, `extract_failed_task_event_msgs`,
  or `wake_machine::set_state`'s Phase::Failed arm. Ops/CI
  deliverable; security flags policy-only.

## MINOR

None new. Carries unchanged.

## Focal-list checks (per r26 brief)

### Focal #1 — R25-S1 closure: typed `StagingPathMissing` wire shape across all 3 callers

**Status**: **CLOSED**. Verified by tracing the path-free invariant
end-to-end through the three wire termini:

1. **Sync POST `/admin/sandboxes/{id}/wake`** (`wake_sandbox_sync_inner`
   at `admin_handlers.rs:1617-1656`). The handler calls
   `restore_handler::restore_sandbox` (`do_restore_inner` at
   `restore_handler.rs:990-1037`); a preflight failure returns
   `Err(RestoreHandlerError::StagingPreflight { which,
   sandbox_id_typed })`. `map_restore_error` (`admin_handlers.rs:1305-
   1316`) emits a §10.0 envelope with `message =
   "staging image missing: {which} for {sandbox_id_typed}"` —
   `which` is `"workspace.img"`/`"user_home.img"` (operator-facing
   resource name, `&'static str`); `sandbox_id_typed` is the
   `sbx_<base62>` form (typed-id, NOT a host path).
   **Path-free.**
2. **Async POST `/admin/sandboxes/{id}/wake` + 202** (`wake_sandbox_async_inner`
   at `admin_handlers.rs:1673-...`). The 202 body has no error message
   (it's the in-flight wake_id + poll URL). **Trivially path-free**.
3. **Poll `GET /admin/sandboxes/{id}/wake/{wake_id}`**
   (`render_wake_poll_response` at `admin_handlers.rs:1971-2019`).
   Reads `row.error_message` and emits verbatim. The
   `error_message` column was written at
   `wake_machine.rs:158-181` via `sanitize_error_message(message)`
   where `message = err.to_string()` (line 625 in
   `rollback_and_classify`). For
   `RestoreHandlerError::StagingPreflight { which, sandbox_id_typed }`,
   the Display impl is `#[error("staging image missing: {which} for
   {sandbox_id_typed}")]` (`restore_handler.rs:116`). **Path-free at
   the source; the sanitize layer is defense-in-depth (it has nothing
   to redact in this shape).**

The wake-machine boundary captures the verbatim host path + source
error via `SubmitRestoreError::log_detail` (`restore_handler.rs:212-
223`) — a `tracing::warn!` event with
`target: "sandbox::wake::preflight"` and the full
`path = %path.display(), source = %source` fields. This routes to
operator-only journald, NOT to any RO-bearer-readable surface.

**The path-bearing capture site is ONE: tracing only. The wire-
surfacing sites are THREE: all three render the path-free Display
form.** Closure verified.

**Test coverage**: `wake_machine.rs:1128-1140`
(`staging_preflight_display_is_path_free`) asserts the Display
output has no `/` characters. `db.rs:3675`
(`StagingPathMissing → "staging_image_missing"` wire-form pin)
tests against rename drift on either side. `admin_handlers.rs:2462-
2503` (`r16_api1_failed_state_renders_every_wake_error_code`)
exercises the StagingPathMissing wire-code in the response body.
Migration `0013_wake_jobs_staging_path_missing_code.sql` extends
the CHECK domain to admit the new pg form (`staging_path_missing`)
— forward-only, idempotent.

**Hand-off**: closure is structural (typed variant + path-free
Display), not behavioural (no sanitize-time stripping needed). A
future refactor that adds a `path: PathBuf` field to the
`StagingPreflight` variant Display impl would re-open the leak;
the regression test at `wake_machine.rs:1128-1140` pins this.

### Focal #2 — r3-A node-affinity new attack surface (latent until emit lands)

**Status**: **NEW LATENT SURFACE per `883df7fe`** — see [r26-S1]
above for the full triage. Summary:

- `fetch_local_nomad_node_id` (`nomad_ch.rs:3080-3093`) makes an
  unauthenticated `GET /v1/agent/self` to `nomad_addr` at boot.
- The result caches into `AppState.local_nomad_node_id`
  (`lib.rs:225`, set at `:670-698`).
- **The jobspec Constraints emit is NOT YET LANDED**. Grepping for
  `Constraints` + `node.unique.id` returns only the doc-comment
  references in `lib.rs` + `metrics.rs` + `nomad_ch.rs:3076-3079`;
  no `format!(... "Constraints"...)` site exists in
  `build_restore_nomad_job_json` / `build_create_nomad_job_json`.
  When that lands, the threat model in r26-S1 activates.

The brief notes both attack vectors (tampered local Nomad agent
returns wrong node_id → all CREATE allocs pin to wrong node →
DoS; operator misconfig of `NOMAD_ADDR` → cross-cluster placement).
r26 confirms both vectors are real **post-emit-landing** and
documents the boot-time fingerprint cross-check + `nomad_addr`
loopback-enforcement as the forward-looking fix shape. Neither
is required pre-cutover because pre-cutover the emit isn't there.

**Severity**: BELOW IMPORTANT until emit lands. **Re-classify to
IMPORTANT in the same round the emit lands** if the `nomad_addr =
loopback` invariant is not enforced AND a node-fingerprint cross-
check is not added.

### Focal #3 — R20-S3 driver SHA256 verify in gcp-worker-startup.sh

**Status**: **OPEN, UNCHANGED, PRE-CUTOVER BLOCKER**. The R24-T1
fix (`dd2079a9`) added SHA-pinning for `snapshot_stress.py` only.
Re-verified at HEAD `3d431eb8`:

```bash
# gcp-worker-startup.sh:150-163 — gs_pull helper (no integrity check)
gs_pull() {
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

# gcp-worker-startup.sh:177-180 — driver download in v14 bundle
if [ "$INSTALL_CH_PLUGIN_DRIVER" = "1" ]; then
  echo "[startup] INSTALL_CH_PLUGIN_DRIVER=1 — installing nomad-driver-ch"
  mkdir -p /etc/zeroship/nomad-plugins
  gs_pull nomad-driver-ch.v14 /etc/zeroship/nomad-plugins/nomad-driver-ch 0755
  #       ^^^^^^^^^^^^^^^^^^^ — driver binary STILL pulled with NO SHA verify
```

The pattern that DOES exist for `snapshot_stress.py` (lines 217,
234-249) is the template:

```bash
# Lines 234-249 (snapshot_stress.py — SHA-pinned per R24-T1)
SNAPSHOT_STRESS_SHA256="89ba229e2c8544bc648b46f4963e57cf524cd7edfd7af82093a1217afb123d43"
...
if gsutil -q stat "gs://$ARTIFACT_BUCKET/stress/snapshot_stress.py" 2>/dev/null; then
  gsutil -q cp "gs://$ARTIFACT_BUCKET/stress/snapshot_stress.py" /opt/stress/snapshot_stress.py
  chmod 0755 /opt/stress/snapshot_stress.py
  got=$(sha256sum /opt/stress/snapshot_stress.py | awk '{print $1}')
  if [ "$got" != "$SNAPSHOT_STRESS_SHA256" ]; then
    echo "[startup] FATAL: snapshot_stress.py SHA mismatch" >&2
    ...
    exit 1
  fi
  echo "[startup] snapshot_stress.py SHA OK ($SNAPSHOT_STRESS_SHA256)"
```

**Exact fix shape for the driver binary (per brief)**:

1. Add `NOMAD_DRIVER_CH_V14_SHA256="<literal>"` constant near line
   180 (mirror `SNAPSHOT_STRESS_SHA256`; v14 SHA is in the
   `c729c2b8` commit body but NOT yet codified in-script).
2. After `gs_pull nomad-driver-ch.v14 ...` and BEFORE
   `chown root:root` (line 181), insert `sha256sum -c` against the
   literal with FATAL-on-mismatch + exit 1.
3. Update the SHA literal in the same PR that bumps the driver
   pin — one SHA bump per driver pin bump, identical to the
   `SNAPSHOT_STRESS_SHA256` maintenance pattern.

Alternative (lower-friction): generalise `gs_pull` to accept an
optional 4th arg `expected_sha256`. When provided, run sha256sum
inline. All integrity-sensitive callers (`cloud-hypervisor.v51.1`,
`ch-remote.v51.1`, `virtiofsd`, `vmlinuz`, the rootfs image,
`nomad-vm-wrapper.sh`, `$CONTROLLER_OBJECT`, `nomad-driver-ch.v14`)
gain integrity in one patch.

**Threat model** (unchanged from r22-r25): TLS protects in-flight;
trust boundary is GCS + the GCE worker SA's write scope. R13-S1
(`--scopes=storage-rw`) means every worker has WRITE access to the
bucket — single worker compromise rewrites the binary for the
entire fleet's next bootstrap. **Pre-cutover blocker.**

### Focal #4 — T1 admin_ro role boot-guard constant-time comparator

**Status**: **VERIFIED CORRECT**. `lib.rs:1359-1385`:

```rust
pub(crate) fn assert_distinct_admin_tokens(
    full: Option<&str>, ro: Option<&str>,
) -> Result<(), String> {
    let (Some(f), Some(r)) = (full, ro) else { return Ok(()); };
    use subtle::ConstantTimeEq;
    if f.as_bytes().ct_eq(r.as_bytes()).into() {
        return Err("FATAL: ... identical secrets ...".to_string());
    }
    Ok(())
}
```

**Constant-time correctness** (`subtle::ConstantTimeEq for [u8]`):
unequal-length slices return `Choice(0)` via a length compare (single
`usize` cmp, no byte-level branch); equal-length slices run a full
byte-by-byte XOR-OR loop over `O(len)` with NO early-exit. `.into()`
masks `Choice` to `bool` without branching. **Equal-length-required:
YES. No early-exit: YES. Correct.**

**Boot-path posture**: called from `AppState::from_config`
(`lib.rs:797-800`) AFTER both `load_admin_token` calls succeed. The
`let-else` early-return handles Option-emptiness cases before the
comparator — the comparator only runs with both-Some, where
equality is the sole failure-eligible shape. Error message names
both env vars + lists three remediation paths; no partial-match
information leak.

**Test coverage** (`lib.rs:2065-2135`): both-None / Full-only /
RO-only / distinct-contents / equal-contents-Err /
one-byte-difference-OK. `subtle` is Dalek-cryptography-maintained
— Rust crypto's standard constant-time comparator. **No issue.**

### Focal #5 — r25-A2 kernel-state surface inventory ADR: sweeper read-of-tenant-data

**Status**: **VERIFIED CLEAN**. Read `run_host_dir_gc_once` at
`sweep.rs:909-1100`. The eligibility check uses ONLY:

1. **Filesystem metadata** via `std::fs::read_dir` (`:927`) and
   `entry.metadata()` (`:986`). The `metadata.modified()` call
   reads mtime; no file content is touched.
2. **Filename parse** to `Uuid` (`:981` `Uuid::parse_str(name)`) —
   filename only, no file body read.
3. **pg state** via `db.get_sandbox_row(sandbox_uuid)` (`:1038`)
   — reads sandbox table rows.
4. **pg state** via `db.find_pending_wake_for_sandbox(...)`
   (`:1078`) — reads wake_jobs table rows.

**`workspace.img` is never opened, read, or stat'd for content** in
the sweeper's eligibility path. Confirmed at the source level.

The reap action is `remove_dir_all(<sandbox_id>/...)` (at the
implementation site below the eligibility check). The reap
deletes the whole subtree atomically; it does NOT read
file content as part of the decision.

**Conclusion**: the brief's "are any sweepers READING tenant data
during their eligibility check?" — **NO, for host_dir.** No
content-read in eligibility. No cross-tenant inference channel
introduced by the sweeper.

**Hand-off**: the ADR's per-surface-sweeper pattern (each future
sweeper gets its own EEXIST-safe re-entry path + orphan counter
+ optional sweeper task) inherits this invariant by default — the
template at `run_host_dir_gc_once` does not touch file content.
A future sweeper that DOES need to read content for eligibility
(e.g., a mount-ns sweeper that checks a process-table view) needs
a separate security audit at that landing.

### Focal #6 — R19-S1 driver `EvalSymlinks` for rootfs_source / restore_from paths

**Status**: **OPEN, UNCHANGED**. Cross-worktree. Driver v13→v14
bump (`c729c2b8`) did not add `EvalSymlinks` per the
inventory ADR's r19-S1 carry. The controller still emits
operator-trust raw paths at `restore_handler.rs:2398`
(`rootfs_source`) and `:2406` (`restore_from`). The driver-side
`filepath.Clean`-only validator resolves `..` but NOT symlinks;
a symlink anywhere along the operator-supplied path lets the
driver hardlink/copy from a different file than the operator
intended.

**Flagging but not proposing a fix** per brief ("don't propose
driver changes"). The next driver bump (v14 → v15) is the right
cut for the `EvalSymlinks` upgrade + the cross-worktree
sibling carries (R22-S1 `ch_stderr_tail=%q` drop,
R20-S2 `isTypedID(cfg.SandboxId/UserId)`, R21-S2 validator-call
symmetry).

## Carry-forward open at HEAD `3d431eb8`

| ID         | Sev       | File:line                                                                                  | Status at r26                                                                                          |
|------------|-----------|--------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------|
| **r26-S1** | **LATENT-IMPORTANT** | `nomad_ch.rs:3080-3093, :3106-3135`; `lib.rs:225, :670-698`                                  | **NEW** — r3-A precursor cached node_id introduces local-Nomad-trust + NOMAD_ADDR-misconfig surfaces. Activates when jobspec Constraints emit lands. |
| **r26-S2** | **POLICY** | `docs/decisions/2026-05-25-restore-debug-playbook.md`; `.github/workflows/ci.yml` | **NEW** — ADR's "verbatim observable before defense" rule is policy-only; not CI-enforced. Process-side; not security-critical.            |
| R22-S1     | **IMPORTANT (ACTIVE)** | `nomad_ch.rs:2660-2700, :2817-2899`; `wake_machine.rs:158-181, :426-430, :623-626, :812-830`              | **ELEVATED — newly active via d638b10f/3d431eb8 driver-msg propagation**. R25-S1's closure does not cover this fork. Mode-A sanitize widening closes both axes. |
| R25-S1     | (closed)  | n/a                                                                                        | **CLOSED at `022f778a`/`6476d18b`** — typed `RestoreHandlerError::StagingPreflight` + path-free Display + `log_detail` tracing capture.    |
| R21-S1     | IMPORTANT | `restore_handler.rs:2256-2259, :2368`                                                       | OPEN — DB CHECK regex remains the structural guard; defense-in-depth gap unchanged.                    |
| R21-S2     | IMPORTANT | (driver-side, cross-worktree)                                                              | OPEN — driver validator-call symmetry not audited.                                                     |
| R20-S3     | **IMPORTANT** | `crates/sandbox/scripts/gcp-worker-startup.sh:150-180`                                     | OPEN — v13→v14 bump (`c729c2b8`) landed without adding SHA256 verify; R24-T1 added SHA verify for stress harness only. **Pre-cutover blocker.** |
| R20-S2     | IMPORTANT | (driver-side, cross-worktree)                                                              | OPEN — driver `cfg.SandboxId` validator not audited.                                                   |
| R19-S1     | IMPORTANT | (driver-side, cross-worktree); controller emit at `restore_handler.rs:2398, :2406`         | OPEN — driver `filepath.Clean`-only; controller emits raw paths without symlink resolution.            |
| R18-S1     | IMPORTANT | `wake_machine.rs:812-830` (code); doc carry r24-M1                                          | **PARTIAL — load-bearing per R22-S1 active reactivation.** Code matches 5 IP ranges; no path / typed-id coverage.   |
| R13-S1     | IMPORTANT | `crates/sandbox/scripts/provision-gcp-cluster.sh:286`                                       | OPEN — `--scopes=storage-rw` + default GCE SA unchanged. ≥12 rounds open.                              |
| R9-S3      | IMPORTANT | `crates/sandbox/src/snapshot_handler.rs:417`                                                | OPEN — `Some("v1")` stamp regardless of AEAD posture; not re-examined this round.                      |
| R20-S1     | MINOR     | (config)                                                                                   | OPEN — `ContentAddressedRootfsRoots` slot empty.                                                       |
| R15-S3     | MINOR     | (config / docs)                                                                            | OPEN — 30s fence cap undocumented.                                                                     |
| R17-S2     | MINOR     | (KEK provisioning)                                                                         | OPEN — no explicit `chown root:root`.                                                                  |
| R18-S2     | MINOR     | (logging)                                                                                  | OPEN — fence-error IP leak into scoped log.                                                            |
| R24-M1     | MINOR     | `wake_machine.rs:740-748` (doc-comment)                                                    | OPEN — doc-comment understates `match_rfc1918_at` coverage (carry from r24).                           |
| R24-M2     | MINOR     | `restore_handler.rs:2352, :2371`                                                            | OPEN — operator-trust footprint widened by one driver-Config path field (carry from r24).              |

## Cross-lens consensus

- **architecture r25**: r25-A2 (kernel-state inventory → ADR) and
  r25-A4 (debug-playbook → ADR) CLOSED. r25-A1 (typed-variant
  schema) CLOSED via Focal #1. r26 adds one IMPORTANT-equivalent
  (R22-S1 elevated) intersecting r25's wire-shape track.
- **cluster T-8b-stress-r3** (RED, 1/60 e2e OK): the failing chain
  exercises exactly the `extract_failed_task_event_msgs` →
  `Backend(s)` → `wake_jobs.error_message` path R22-S1 identifies
  — **empirical evidence the leak is active on every failed alloc**.
- **test-coverage r25**: R24-T1 stress harness in-repo + SHA-pinned;
  driver-binary parity gap (R20-S3) is the remaining pre-cutover
  blocker.
- **api-surface r24**: §10.0 envelope `message` is the wire terminus
  of R22-S1; fix is upstream (sanitize the column write), not at
  the wire.
- **concurrency r25 / code-quality r25 / perf r24**: no security
  overlap.

## Lens hand-off

- **Sandbox controller (Rust)** — **R22-S1 sanitize widening (Mode A)
  is the highest-leverage fix this round** (~30 LOC: path-stripping
  + typed-id stripping, mirror `match_rfc1918_at` pattern; 6-8 test
  cases). Closes BOTH R22-S1 (driver-msg propagation) AND any future
  R25-S1-shaped regression at the sanitize-write boundary.
  Lower-risk than R21-S1 (`parse_with_prefix` plumbing through
  `submit_restore_job` / `do_restore_inner`).

- **Ops / cluster bring-up (R13-S1 + R20-S3)** — pre-cutover blockers
  unchanged. R20-S3 driver SHA pin: exact fix shape in Focal #3.
  R13-S1 carries unchanged.

- **nomad-driver-ch maintainers (cross-worktree)** — v14 landed
  defensive tap cleanup + Driver Failure preference without
  addressing R22-S1 driver-side / R20-S2 / R21-S2 / R19-S1.
  Next pin bump (v15) is the right cut for: driver-side
  `ch_stderr_tail=%q` drop, `isTypedID(cfg.SandboxId/UserId)`,
  `EvalSymlinks` upgrade, validator-call symmetry. Controller-side
  R22-S1 Mode A closes the leak axis regardless.

- **Forward-looking (r3-A Constraints emit)** — re-engage r26-S1
  audit. Required gates: (a) `nomad_addr` loopback enforcement;
  (b) node-fingerprint cross-check (`/v1/node/<node_id>` vs.
  controller's own PRIVATE_IP). Escalate r26-S1 to IMPORTANT if
  either gate is missing when emit lands.

- **CI (r26-S2)** — r24-A3 ADR is policy-only; future PR-body /
  commit-msg lint could enforce verbatim-observable citation for
  changes to `sanitize_error_message`,
  `extract_failed_task_event_msgs`, or wake_machine Phase::Failed
  handling. Out of scope for security.

## Counts

- CRITICAL: 0 new; carry: 0.
- IMPORTANT: 0 new (R25-S1 CLOSED; R22-S1 ELEVATED to ACTIVE but
  not "new" — it's been an open carry since r22); carry: R22-S1
  (now active again via a new fork), R21-S1, R21-S2, R20-S3,
  R20-S2, R19-S1, R18-S1, R13-S1, R9-S3.
- LATENT-IMPORTANT: 1 new (**r26-S1** r3-A precursor; activates
  when Constraints emit lands).
- MINOR / POLICY: 1 new (**r26-S2** r24-A3 not CI-enforced);
  carry: R20-S1, R15-S3, R17-S2, R18-S2, R24-M1, R24-M2.
- Total NEW this round: 0 IMPORTANT, 1 LATENT-IMPORTANT, 1 POLICY.
- **Cutover gate**: **R20-S3 (driver binary integrity)** + **R13-S1
  (storage-rw scope)** remain pre-cutover blockers; landings since
  r25 did not address either. **R22-S1 elevation is post-cutover
  acceptable** (admin-bearer-gated, not tenant-exploitable) but
  should land inside the next bundle since the path-leak is now
  active on every driver-side preflight failure (which is most of
  the stress-r3 RED failure modes).
