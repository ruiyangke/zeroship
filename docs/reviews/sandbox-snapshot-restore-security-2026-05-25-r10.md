# Sandbox/snapshot-restore — security r10 review

Date: 2026-05-25 (UTC)
HEAD at audit: `040812f2`
Round 10 of N (security lens). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/scripts/**`.

## Summary

6 findings (0 critical, 2 important, 4 minor). 2 r9 findings CLOSED
(R9-S4 KEK uid==0 check landed at `cca1e74d`; the prior round's
test-only R9-T7 init tests landed at `73a263a2` and surfaced
R9-T7-FOLLOWUP which is itself a posture observation, not a directly
exploitable defect). 5 r9 findings carry forward (R9-S1, R9-S2,
R9-S3, R9-S5, R9-S6/S7/S8). R9-S4b (persist.rs uid asymmetry just
filed in r9 follow-up) confirmed open at HEAD. NEW residuals: R10-S1
(R9-S4 fix uses follow-symlinks `metadata()` — symlink-redirect
weakness via operator misconfigured KEK path), R10-S2
(spawn_blocking rollback swallows panics → partial cleanup),
R10-S3 (best-effort Nomad DELETE ordering can release a vm_index
while CH still binds the tap+IP), R10-S4 (registry uses naked
`.unwrap()` on RwLocks while peers use `unwrap_or_else(into_inner)`
— inconsistency).

## Findings (NEW since r9)

### [R10-S1] `RootKek::from_path` follows symlinks; operator-typo'd KEK path can be silently redirected by a non-root attacker (IMPORTANT, security-r10)
- **Files**: `crates/sandbox/src/snapshot_aead.rs:185-217`
  (the R9-S4 fix at `cca1e74d`); same shape at
  `crates/sandbox/src/persist.rs:326-354` (`AeadKey::from_path`,
  R9-S4b carry).
- **Symptom**: The KEK loader uses `std::fs::metadata(path)` which
  follows symlinks to perform the mode + uid checks, then
  `std::fs::read(path)` which also follows symlinks. The two
  syscalls re-resolve the symlink independently — a non-root
  attacker who controls the parent directory of the KEK path (e.g.
  because the operator typo'd `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` to
  `/tmp/kek` instead of `/etc/zeroship/kek`) can:
  (a) point `/tmp/kek` → `/etc/zeroship/real_kek` so the
      stat check returns mode 0o400 + uid 0 + length 32 (pass);
  (b) flip the symlink between `metadata` and `read` to point at a
      different root-owned 0o400 file of length 32 (e.g. another
      operator-managed key in `/etc/`). Because both files must be
      root-owned mode 0o400, the non-root attacker CANNOT supply
      their own bytes — but they CAN cause the controller to load a
      DIFFERENT key than the operator intended. This is a
      cross-deployment confusion class: the controller silently
      encrypts with the wrong KEK and the operator's snapshots
      become undecryptable on the original cluster (or, worse,
      decryptable on the attacker's separate cluster that happened
      to have access to the same KEK file via a different config).
  (c) variant: a symlink-followed file whose target is `/dev/zero`
      truncated by an admin tool to exactly 32 bytes (e.g.
      `truncate -s 32 /tmp/foo` then `chmod 0400 /tmp/foo; chown
      root:root /tmp/foo`) loads as an all-zero KEK. The current
      32-length check is necessary but not sufficient — content is
      not validated for entropy.
- **Threat model**: Local non-root attacker + operator
  misconfiguration. Attack requires:
  - The operator to set `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` to a path
    in a world-writable or attacker-writable parent (`/tmp/`,
    `/var/tmp/`, an attacker-owned directory under
    `/opt/zeroship/`, or a directory the controller-runner UID
    owns but a less-trusted local sidecar can drop files into).
  - The controller process to start with that env (root-owned
    process is typical for raw_exec).
  This is squarely defense-in-depth — the operator's path choice
  is the primary gate.
- **Why it matters**: R9-S4 closed the "non-root attacker who can
  PRE-CREATE a chmod-400 file gets their bytes loaded" class. The
  symlink-redirect class is narrower (still requires root-owned
  target files) but the symmetric defenses cost two LOC each:
  - `std::fs::symlink_metadata(path)?.file_type().is_symlink()`
    → refuse, OR
  - Use `nix::fcntl::open(path, O_NOFOLLOW | O_RDONLY)` then
    `fstat` on the fd + `read` from the same fd — eliminates the
    TOCTOU window AND blocks symlink redirects in one stroke.
- **Action**:
  (a) Refuse symlinks at the loader: read
      `symlink_metadata(path)` first; if `is_symlink()` return Err
      with a message that names the link target so the operator
      can self-diagnose.
  (b) Stronger: switch to `O_NOFOLLOW` open + `fstat` on the fd to
      close the residual TOCTOU window (between `metadata()` and
      `read()`) — the same fd is used for stat + read, no
      double-resolution.
  (c) Replicate to `AeadKey::from_path` (R9-S4b) — the persistence
      key has the same loader shape and the same threat surface.
  (d) Optional content guard: refuse loads when the bytes are
      all-zero or contain a long run of identical bytes (poor-
      man's entropy check); useful against the `head -c 32
      /dev/zero` typo class.

### [R10-S2] Rollback `spawn_blocking(teardown_restore)` discards `JoinHandle` errors → cleanup panics silenced (IMPORTANT, security-r10)
- **Files**: `crates/sandbox/src/restore_handler.rs:291-298`
  (the R10-C2 fix at `be246395`).
- **Symptom**:
  ```rust
  let _ = compio::runtime::spawn_blocking(move || {
      backend_for_teardown
          .teardown_restore(sandbox_id_for_teardown, snap_vm_index);
  })
  .await;
  ```
  The `let _ =` discards a JoinError. `teardown_restore` calls
  three side-effects in sequence on the blocking thread:
  (1) Nomad DELETE `/v1/job/...?purge=true` (best-effort,
      already swallowed by the impl);
  (2) `nomad_handle.unregister_restored(sandbox_id)` (state-map
      remove);
  (3) `self.release_vm_index(vm_index)`.
  If a panic fires inside (2) or (3) (e.g. an `unwrap()` in a
  registry-poisoning case, or a future change adds an
  `expect()`), the spawn_blocking returns Err(JoinError) with
  the panic payload — and the `let _ =` throws it away
  silently. The caller proceeds to `update_sandbox_status` with
  the assumption that the source alloc is gone, but the
  state-map entry may still be present + the slot may still be
  reserved → ghost state.
- **Threat model**: Cleanup-availability. An attacker who can
  influence the rollback path (e.g. by inducing a controlled
  registry RwLock poison via a panic in a peer code path —
  R10-S4 below) and then triggers a restore failure can leave
  the state-map populated with a record pointing at a tap+IP
  that the next `create()` will reuse. The next tenant on that
  vm_index then receives traffic for both the ghost record AND
  the live record (the in-memory dispatcher's
  `boot_sandbox_id` checking on the agent side fails closed —
  see R9 finding analysis — so this is a stuck-state DoS, not
  a cross-tenant leak; but worth pinning).
- **Why it matters**: The R10-C2 fix correctly moved the sync
  10s ureq DELETE off the ntex worker. The follow-on hygiene
  (logging the join error) is a 4-line patch.
- **Action**:
  ```rust
  if let Err(e) = compio::runtime::spawn_blocking(move || {
      backend_for_teardown
          .teardown_restore(sandbox_id_for_teardown, snap_vm_index);
  })
  .await {
      tracing::error!(
          sandbox_id = %sandbox_id,
          rollback_join_error = ?e,
          "restore rollback: teardown_restore panicked or cancelled — \
           state-map / vm_index slot may be wedged; sweep will recover"
      );
  }
  ```
  Even on panic, the rollback continues to the
  `update_sandbox_status` CAS so pg is consistent; the sweep at
  `sweep.rs:127` recovers the wedge after `threshold_secs` (120
  default). The find here is the missing log line.

### [R10-S3] `teardown_restore` order: Nomad DELETE (best-effort) → state-map remove → vm_index release; failed DELETE can produce port-rebind races (MINOR, security-r10)
- **Files**: `crates/sandbox/src/restore_handler.rs:1159-1202`.
- **Symptom**: The order at lines 1163-1201 is:
  (1) `nomad_delete_blocking` (10s budget, errors logged as
      `non-fatal` and swallowed);
  (2) `unregister_restored` (drop state-map entry);
  (3) `release_vm_index` (return slot to the allocator).
  If step (1) times out (Nomad unreachable, slow control plane),
  the Nomad-managed CH process keeps running with its
  tap=`zsbx-nm-<vm_index>` + IP=`10.X.<100+vm_index>.2` alive.
  Steps (2)+(3) release the slot. The next `create()` reserves
  the same vm_index → wrapper attempts `ip link set zsbx-nm-N up`
  (idempotent on an already-up tap, ok) → CH spawns with the SAME
  api_socket path (`$ZSBX_RUNTIME/ch.sock`) — but wait: the
  NOMAD_TASK_DIR is per-alloc and the api_socket lives there, so
  the two CH processes have DISTINCT api_sockets. The tap is
  shared by both VMs simultaneously (same MAC `12:34:56:78:9b:%02x`
  on both — bridge-layer behaviour undefined; expect packet
  storms or ARP confusion).
- **Threat model**: Operational footgun, not a security primitive
  by itself. But under sustained Nomad outage (control-plane
  partition while raw_exec keeps running), the slot allocator
  desync from running CH processes is unbounded. Eventually the
  sweep's orphan-prune logic catches it, but in the meantime any
  end-user traffic to `10.X.<100+N>.2:7777` reaches WHICHEVER CH
  the tap routes to (ARP cache wins).
- **Why it matters**: A captured/replayed `/_clock_resync` or
  agent-signed RPC for the OLD tenant arrives on the wire and is
  delivered to whatever VM ARPs first. The agent's R7-S1 binding
  (boot-time sandbox_id) ensures the WRONG tenant's agent returns
  401 — fail-closed — but the load balancer / dispatcher would
  see oscillating timeouts. Defensive: invert the order so step
  (1) becomes blocking-with-retry OR introduce a "VM is alive on
  this tap" probe before step (3) hands the slot back.
- **Action**:
  (a) Reverse-order step (3) before step (1) is NOT correct —
      the slot must be released so a peer can move on. Instead,
      poll the tap (`/sys/class/net/$TAP/operstate`) after the
      DELETE and only release the slot once the kernel has torn
      down the tap (Nomad's task-shutdown reaper closes the tun
      fd, kernel auto-deletes the tap). A 5s budget with a
      fallback "force-release with explicit log" gives the
      operator a sweep-recoverable trace.
  (b) OR (cleaner): split the slot allocator into
      "reserved-pending-cleanup" vs. "free" states, where
      cleanup-pending slots are not eligible for new creates
      until the orphan-prune sweep confirms the host tap is
      gone. This is the structural cure; (a) is interim.
  (c) Document the residual in the R10-C1 comment block so the
      next contributor knows.

### [R10-S4] `registry.rs` uses naked `.unwrap()` on every RwLock; peer code (`backend/nomad_ch.rs`, `snapshot_aead.rs`, etc.) uses `unwrap_or_else(into_inner)` — inconsistent poison-recover policy is itself a DoS surface (MINOR, security-r10)
- **Files**: `crates/sandbox/src/registry.rs:239,251,298,299,308,319,
  331,347,349,350,363,365,385,390,428,430,464,474,476,483,487,496,
  499,511,513,527,533,539,552,559,563,573,584` (every RwLock
  access). Peers using poison-recover at
  `crates/sandbox/src/backend/nomad_ch.rs:1709,488,509,536,652,
  835,4838+`; `crates/sandbox/src/snapshot_handler.rs:151,721`;
  `crates/sandbox/src/sweep.rs:294,776`;
  `crates/sandbox/src/restore_handler.rs:755,762,1063,1068,1081,
  1088`.
- **Symptom**: An attacker who can induce a panic while ANY
  registry writer holds the lock (e.g. by feeding a malformed
  input to a handler that triggers a panic upstream of the
  registry mutation — `info.user_id.clone()` is the closest
  candidate but is pure; a future change is the bigger risk)
  poisons the lock. Every subsequent `.read().unwrap()` or
  `.write().unwrap()` in the registry panics, taking down the
  whole controller-internal sandbox lookup. The agent's per-VM
  state survives, but the controller can't lookup, list,
  preview, or stop any sandbox until restart.
  Peer code uses `unwrap_or_else(|p| p.into_inner())` which
  RECOVERS the inner data on poison — partial state may be
  observable but the process keeps running. The policies are
  inconsistent and the registry's choice is the more
  conservative on correctness (no partial-state observation) but
  the less robust on availability.
- **Threat model**: The DoS arm requires inducing a controller-
  side panic while a registry mutex is held. The registry's
  mutations are tight (mostly single-line `.insert` /
  `.remove`); no obvious panic vector with attacker-influenced
  input today. But if a future change e.g. adds a `format!` with
  attacker-controlled UTF-8 inside a held lock and that input
  later trips a `String::from_utf8` panic, the whole controller
  wedges. The peer's poison-recover is the safer DEFAULT for
  long-running services.
- **Why it matters**: The cluster_smoke story (multi-hour
  cycles) is exactly the class where poison-and-stay-down would
  burn an entire cycle on a single bad sandbox. The fix is
  mechanical: replace every `.read().unwrap()` /
  `.write().unwrap()` in `registry.rs` with the
  `.unwrap_or_else(|p| p.into_inner())` form already used in
  `backend/nomad_ch.rs`. Pinned by a `clippy::disallowed_methods`
  on `RwLock::read().unwrap` / `.write().unwrap` (or a crate-
  local lint).
- **Action**:
  (a) Mechanical sweep of `registry.rs` to mirror the peer
      pattern. The unit tests already exercise the read/write
      paths; behaviour is unchanged on the non-poisoned path.
  (b) Stretch goal: add a `pub(crate) fn read_guard()` /
      `write_guard()` helper that hides the recover decision
      inside the registry, then everyone uses the helper. Avoids
      the temptation for a future contributor to copy the
      `.unwrap()` shape.

### [R10-S5] AEAD posture leak: `snapshot_aead_dek_id="v1"` stamp in pg + ch_version suffix gap leaks AEAD-active vs. AEAD-passthrough to anyone with pg read (MINOR, security-r10, refinement of R9-S3)
- **Files**: `crates/sandbox/src/snapshot_handler.rs:417`
  (`Some("v1")` hard-coded passed to `update_snapshot_metadata`);
  `crates/sandbox/src/snapshot_aead.rs:627-629` (suffix
  `+aead-cc20p1305` appended ONLY when `self.root.is_some()`).
- **Symptom**: R9-S3 already noted that `snapshot_aead_dek_id`
  is always stamped `"v1"` regardless of whether AEAD is active.
  The complement: `ch_version` carries the `+aead-cc20p1305`
  suffix ONLY when active. An operator (or an attacker with pg
  read) can detect which controller / which boot was running in
  passthrough mode by querying
  `WHERE snapshot_aead_dek_id IS NOT NULL AND ch_version NOT LIKE
  '%+aead-cc20p1305%'` — every row matching is a known-plaintext
  artifact. The same pg query reveals which tenants /
  date-ranges are decryptable without the KEK. This is an audit-
  trail confidentiality issue: the pg metadata reveals the
  AEAD posture of every artifact, AND the discrepancy reveals
  the boundary between "this controller had AEAD on" and "this
  one didn't" — useful for an attacker prioritising which L2
  bucket prefixes to exfiltrate.
- **Threat model**: pg-read principal (read-only audit user, BI
  user, leaked DB password). The information leaked is not
  catastrophic (the artifacts themselves are wrapped /
  unwrapped on disk) but it IS a confidentiality-classification
  discrepancy that a regulator-audit shop would flag (HIPAA
  §164.312(b) audit-trail integrity).
- **Why it matters**: R9-S3's fix shape — propagate AEAD posture
  through `SnapshotMetadata` so the stamp matches reality —
  closes both R9-S3 and this finding in one patch.
- **Action**: See R9-S3 (a)/(b) — extend `SnapshotMetadata` with
  an `aead_dek_id: Option<&'static str>`, set in
  `AeadSnapshotStore::put` from `self.root.is_some()`. Handler
  reads `meta.aead_dek_id` instead of hard-coding.

### [R10-S6] R9-T7-FOLLOWUP UUID-shape pin is documented behaviour but the wrapper-side asymmetry (R9-S5) leaves a gap where a future code-path could surface the unvalidated id (MINOR, security-r10)
- **Files**: `crates/sandbox-agent/src/handlers.rs:106-155`
  (`init_sandbox_id_from_env` + `read_sandbox_id_from_sources` —
  no UUID-shape guard; pinned by
  `r9t7_read_env_var_arbitrary_string_accepted_no_shape_guard`
  at `:2127-2140`); wrapper cold-boot validator at
  `crates/sandbox/scripts/nomad-vm-wrapper.sh:221-226` rejects
  `[!0-9a-zA-Z_]` so a path-traversal-shaped id (e.g.
  `..%2fetc%2fshadow`) can't reach the agent via the cmdline
  injection path. Restore branch (`:394-396`) doesn't validate
  but also doesn't pass `ZSBX_SANDBOX_ID` into the kernel
  cmdline (CH `--restore` ignores `--cmdline`, agent reads from
  preserved OnceLock in memory image).
- **Symptom**: Today the agent's
  `boot_sandbox_id() -> Option<&'static str>` value flows only
  into a string-equality check at `handlers.rs:801` (vs.
  controller's signed `parsed.sandbox_id`). No path
  construction. **No directly exploitable defect.** The risk is
  that a future code path (e.g. agent persists per-sandbox
  artifacts under a directory derived from
  `boot_sandbox_id()`) would inherit the unvalidated value and
  open a real path-traversal vector. The wrapper-side validator
  is the controller-side fence; symmetric validation on the
  agent side is defense in depth.
- **Threat model**: Future-proofing. A reviewer in r12-r15
  who's adding e.g. an audit-log persistence path that uses
  the sandbox_id as a filename component would inherit the gap.
- **Why it matters**: The R9-T7 init tests landed at
  `73a263a2` pin the current behaviour explicitly (the test
  name itself — `r9t7_read_env_var_arbitrary_string_accepted_
  no_shape_guard` — flags the gap). A 3-line addition to
  `read_sandbox_id_from_sources` would close this for good:
  ```rust
  if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
      return Err(format!("sandbox_id contains invalid chars: {id:?}"));
  }
  ```
  Mirrors the wrapper's cold-boot validator at line 221-226.
- **Action**: Add the `[0-9a-zA-Z_]` validator to
  `read_sandbox_id_from_sources` AFTER the empty check. Update
  the test name + body to reflect the new contract; the existing
  test becomes `…_arbitrary_string_REJECTED_…` and asserts an
  Err return on `"not-a-uuid"`.

## Closed by recent commits

1. **R9-S4 KEK uid==0 check** (security-r9 IMPORTANT) — `cca1e74d`.
   `snapshot_aead.rs:198-204` adds `meta.uid() != 0` check after
   the existing mode check. Residual: symlink-follow vector
   (R10-S1 above); persist.rs::AeadKey::from_path still missing
   the symmetric guard (R9-S4b carry).
2. **R10-C1 + R10-C2 teardown_restore state-map + spawn_blocking
   wrap** — `be246395`. State-map remove via
   `unregister_restored` (`nomad_ch.rs:1706-1712`) + spawn_blocking
   wrap in rollback (`restore_handler.rs:294-298`). Test pins at
   `restore_handler.rs:2186-2360`. Residuals: R10-S2 (join error
   discard) + R10-S3 (ordering).
3. **C1-FOLLOWUP** `claim_orphan_transient_for_recovery` —
   `0e71e5c4`. CAS predicate at `db.rs:2552-2578` correctly
   fences `host_id = expected_host_id` (the crashed peer's id,
   not self). Defensive self-host_id refusal at `:2518-2524`.
   Threshold is pg-side `now() - threshold_secs` (default 120s,
   env-tunable). Reviewed for attacker-clock-skew: both SELECT
   (`db.rs:2429`) and CAS (`db.rs:2565`) use pg `now()` — no host
   clock involvement → not skew-attackable from a single
   controller. The env-var threshold IS operator-tunable; a
   malicious operator could lower it to e.g. 1 second to grief
   peer recoveries, but that's a trust-boundary already
   surrendered (env access = root on controller).
4. **R9-T7 init tests for `init_sandbox_id_from_env`** —
   `73a263a2`. Pinned the absent UUID-shape guard as
   `r9t7_read_env_var_arbitrary_string_accepted_no_shape_guard`.
   The MISSING guard itself is R10-S6 (above, MINOR — not
   directly exploitable today).

## Carry-forward (from r9, unchanged at HEAD)

- **R9-S1** (CRITICAL) — AEAD does not wrap `config.json`;
  bucket-write attacker on L2 can forge `disks[].path` /
  `serial.file` to point CH at arbitrary host files. Verified
  open at `snapshot_aead.rs:28-35` (scope comment) and Python
  rewriter at `nomad-vm-wrapper.sh:476-498` (only matches the
  anchored Nomad alloc prefix; non-matching paths pass through).
- **R9-S2** (IMPORTANT) — AEAD DEK derivation has 1-second
  timestamp granularity. Verified open at `snapshot_aead.rs:609-
  612` (`d.as_secs()`). Same-second re-snapshot → nonce reuse
  → catastrophic on ChaCha20-Poly1305.
- **R9-S3 / R10-S5** (IMPORTANT) — `snapshot_aead_dek_id="v1"`
  hard-coded regardless of AEAD state. Verified open at
  `snapshot_handler.rs:417`. Operator forensics is misled in
  passthrough mode + pg audit trail leaks AEAD posture by
  schema-discrepancy comparison.
- **R9-S4b** (IMPORTANT) — `persist.rs::AeadKey::from_path`
  carries the same uid-vs-mode-only bug R9-S4 closed for
  `RootKek::from_path`. Verified open at `persist.rs:326-354`;
  only the mode check is present (line 339-346), no uid check.
  Same one-line fix.
- **R9-S5** (IMPORTANT) — Restore-branch wrapper asymmetric
  `ZSBX_SANDBOX_ID` handling. Verified open at
  `nomad-vm-wrapper.sh:394-396` (informational echo only) and
  `restore_handler.rs:1327-1356` (no `ZSBX_SANDBOX_ID` in the
  restore-branch env payload). The R9 action — emit + validate on
  both branches — would close the gap and the (more theoretical)
  pre-R7-S1-snapshot wedge case in one stroke.
- **R9-S6** (MINOR) — `/_clock_resync` error path still embeds
  256 chars of agent body verbatim into the
  `RestoreHandlerError::Backend` string at
  `restore_handler.rs:1650-1660`. WIRE body is sanitized; journald
  leak is the residual.
- **R9-S7** (MINOR) — `/livez`, `/readyz`, `/metrics` remain
  unauthenticated. Verified open at `handlers.rs:494-...`.
  NetworkPolicy is the documented gate.
- **R9-S8** (MINOR) — Admin endpoints still lack per-bearer rate
  limit. Verified open by absence — no `rate_limit` /
  `throttle` symbols anywhere in `admin_handlers.rs`. Standing
  T2 carry across r4-r10.

## Out-of-scope check (informational)

Attempted `gcloud compute instances list --filter='name~"^zsbx-"'`
in the reviewer's GCP account (`suger-dev`). Project does not host
zsbx instances (lists controller / github-runner / dev VMs only;
none with the `zsbx-` prefix). The cluster smoke deploys appear to
live in a different GCP project the reviewer does not have access
to. Recommend the on-call run the same query in the
cluster-smoke project and post the result in a separate cycle —
known prior cycles fired multiple short-lived `zsbx-*` allocs.

## Counts

- CRITICAL: 0
- IMPORTANT: 2 (R10-S1, R10-S2)
- MINOR: 4 (R10-S3, R10-S4, R10-S5, R10-S6)
- Total: 6

r9-closed: 2 (R9-S4 KEK uid check; R9-T7 test landing — though the
underlying shape-guard gap is itself pinned as R10-S6).
r9-carry: 8 (R9-S1, R9-S2, R9-S3, R9-S4b, R9-S5, R9-S6, R9-S7,
R9-S8).
