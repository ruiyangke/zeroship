# Sandbox/snapshot-restore — security r13 review

Date: 2026-05-25 (UTC)
HEAD at audit: `0053e8b6`
Round 13 of N (security lens). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/scripts/**`.

## Summary

1 new IMPORTANT (R13-S1, the worker-VM GCS scope is project-wide on
the **default** Compute Engine SA — C-5's narrow-scope claim depends
on operator-side bucket-IAM hardening that the provisioning script
does NOT assert); 1 new MINOR posture (R13-S2, snapshot/wake DoS via
12-slot vm_index exhaustion is now genuinely reachable post-C-3 with
no per-bearer rate-limit). No CRITICAL elevation this round.

The C-3 fix (`c890c015`, `std::thread::Builder::spawn` for L2
detach) and the R13-Q1 ENV_LOCK unify (`c5b9cb9d`,
`TASK_DRIVER_ENV_LOCK` promoted to a crate-shared file-scope mutex)
introduced **zero new `unsafe` blocks** in `crates/sandbox/**` or
`crates/sandbox-agent/**` — `std::thread::Builder::spawn` is fully
safe, and the env-mutation unsafe blocks merely **moved** from
`#[cfg(test)] mod tests` into a sibling `#[cfg(test)] pub(crate) mod
test_env_lock` at file scope. The R12-S1 cross-module-cross-key
env-mutation UB risk is **partially closed** for `SANDBOX_TASK_DRIVER`
across nomad_ch + restore_handler but **still open** between
`db.rs::ENV_LOCK` (9 keys incl. `SANDBOX_HOST_ID`, `SANDBOX_HA_*`)
and the new `TASK_DRIVER_ENV_LOCK` — two locks; one named key each;
disjoint-key UB per the Rust-2024 stdlib `set_var` contract still
reachable test-side.

R9-S1 / R9-S2 / R9-S3 carry forward unchanged. R9-S1 (config.json
plaintext exemption inside the AEAD layer) is **still unreachable**
in the current cluster because `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` is
not set by `gcp-worker-startup.sh` — production runs AEAD in
passthrough mode while `snapshot_aead_dek_id="v1"` is still stamped
unconditionally into the DB (R9-S3 / A1 in deferred). Post-C-3 the
SNAPSHOT path **does** now reach `store.put` successfully on the
cluster, so the attacker-tooling reachability nuance shifts: the
write path is no longer panic-blocked, only AEAD-OFF-blocked.

R10-S1 (5 symlink-follow loaders), R10-S2 (JoinError swallow),
R10-S3 (teardown_restore step ordering), R11-S3 (chunk_aad missing
sandbox_id+taken_at), R9-S5 partial-close, R9-S6/S7/S8 (livez,
metering bearer rate-limit, clock_resync log leak) all unchanged.

## Hunt-list disposition (security lens)

### 1. C-5 scope audit — `storage-rw` on which SA, scoped to which buckets?

`d7740b03` upgraded `crates/sandbox/scripts/provision-gcp-cluster.sh:
286` from `--scopes=storage-ro,…` to `--scopes=storage-rw,…`. The
gcloud alias maps to `https://www.googleapis.com/auth/
devstorage.read_write`. **What that grant actually means in
production depends on which service-account identity the VM runs
as** — and that is the security gap the deferred-doc summary glosses.

The provisioning command at `provision-gcp-cluster.sh:275-289` does
**not** pass `--service-account`. From `gcloud compute instances
create --help`: when `--service-account` is omitted, the instance
uses the **project's default Compute Engine service account**
(`<PROJECT_NUMBER>-compute@developer.gserviceaccount.com`). The
default Compute Engine SA has the **`roles/editor` primitive role on
the project** by default — meaning bucket-level IAM is NOT the only
fence; the SA's project-wide `roles/editor` grants `storage.objects.
*` on every bucket in the project.

So `storage-rw` is **OAuth-scope** narrowing (the scope token can
only call `devstorage.*` APIs, not e.g. `compute.*`) — it is NOT a
**resource** scope. The OAuth scope and the SA's IAM permissions
intersect; with the default SA's `roles/editor`, that intersection
on `devstorage.*` is "read/write any bucket the project owns".

Practical impact:
- A worker compromise reads/writes ANY bucket in the project
  (artifact-bucket, snapshot-bucket, and any unrelated bucket
  belonging to the same GCP project — terraform state, audit logs,
  customer-data buckets if co-located, etc.).
- The cluster smoke uses `ARTIFACT_BUCKET=suger-dev-zsbx-artifacts`
  and `SNAPSHOT_BUCKET=$ARTIFACT_BUCKET` (one bucket); in a less
  hygienic project, the same provision command pulls in any other
  bucket the project happens to host.

Deferred-doc note at `docs/reviews/sandbox-snapshot-restore-
deferred.md:88` says: *"Worker still has no IAM permission outside
the project's snapshot bucket (per existing service-account-level
grants)."* This is **true only if the operator has manually
hardened the default Compute Engine SA's IAM to remove
`roles/editor` and grant only `roles/storage.objectAdmin` on the
two specific buckets**. The provisioning script does NOT do this; it
inherits whatever the project happens to have.

See R13-S1 below for the actionable shape.

### 2. C-3 `std::thread::Builder::spawn` vs `compio::runtime::spawn_blocking` — security boundary

`c890c015` switches the L2-upload detach path from
`compio::runtime::spawn_blocking(...).detach()` to
`std::thread::Builder::new().name(...).spawn(move || …)`.

Security delta — both primitives:
- Run on a **fresh OS thread** in the controller process.
- Inherit the controller's **uid/gid/cwd/umask** (no `setresuid` /
  `setresgid` / `unshare` / `chroot` between spawn and run).
- Have **full access to controller's process memory** (the closure
  is a `Box<dyn FnOnce + Send>` over the controller's heap).
- Have **full access to controller's filesystem view** (no fs
  namespace).

`std::thread::Builder::spawn` does NOT lower any privilege; the
thread is fungible with the parent except in scheduling. The OAuth
token used by `GcsSnapshotStore::put` is fetched on-thread via
`google-cloud-auth`'s metadata-server call (uses `ureq`), which
reads the **VM's** instance-metadata identity token — same identity
either way.

**Verdict**: no security delta. C-3 is a runtime-affinity fix
(extract a sync primitive from a compio-required context), not a
privilege change. Confirmed.

### 3. R9-S1 reachability shift after C-3 SNAPSHOT now works on cluster

Pre-C-3: cluster snapshot panicked at `store.put` with `"not in a
compio runtime"`, so the write side never reached the AEAD layer.
Post-C-3: `store.put` returns Ok on the cluster, the L2 upload
detaches successfully, AND `snapshot_handler.rs:417` still stamps
`Some("v1")` into pg unconditionally.

But R9-S1 (config.json plaintext exemption inside AEAD) is
**unreachable in cluster** because:
- `crates/sandbox/scripts/gcp-worker-startup.sh` (full re-read at
  HEAD): no `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` env var written to the
  zsbx-ctl systemd unit (cross-checked with `Grep
  SANDBOX_SNAPSHOT_ROOT_KEK_PATH crates/sandbox/scripts/`: zero
  matches).
- `crates/sandbox/src/lib.rs:316-345` (A1 in deferred): the
  production wire-up bare-`LocalDiskSnapshotStore` or
  `TieredSnapshotStore<LocalDisk, Gcs>`; `AeadSnapshotStore` is
  never composed.

So in current cluster runs, the AEAD layer is **completely bypassed**
— `config.json` plaintext is the LEAST of the issues; the
`memory-ranges` itself goes to GCS plaintext. R9-S1 the finding is
**MORE exploitable in principle** post-C-3 (the write path now
reaches the store), but **LESS observably exploitable** in the
current cluster because AEAD is OFF (A1 still pending in deferred).
Once A1 lands and turns AEAD ON, R9-S1 becomes immediately
attacker-reachable on every snapshot. **No new finding; carry-forward
unchanged.**

### 4. `process::Stdio::null()` on `cloud-hypervisor` spawn

The controller (`crates/sandbox/src/**.rs`) does NOT spawn
`cloud-hypervisor` directly. Search proves it (`Grep Command::new\
(.*cloud-hypervisor` on the crate: 0 hits; only doc-comment refs).
CH is spawned by either:
- the bash wrapper `nomad-vm-wrapper.sh` (raw_exec mode) — captures
  stderr via shell redirection per cluster review r5.
- the Go driver `nomad-driver-ch` (ChPlugin mode, OUT of Rust review
  scope) — captures stderr into a 1 KiB ring buffer per cluster
  review r5.

`ch-remote` (the API client) IS spawned controller-side via
`snapshot_handler.rs:622-654` (`run_with_timeout`) — that function
correctly sets `cmd.stderr(Stdio::piped())` at line 631 AND drains
on failure at lines 643-647, surfacing the stderr in the returned
error message. No `Stdio::null()` on the controller-side
ch-remote path. Confirmed.

### 5. Wake-path vm_index race (C-4) — DoS via sustained snapshot+wake cycles

C-4 (`5399b6b7`, cluster review r5) was a correctness fix
(60 × 2 s = 120 s retry budget to envelope the source-teardown
release). Re-reading the security implication under the post-C-3
cluster (where SNAPSHOT now succeeds):

- 12 vm_index slots per worker (`gcp-worker-startup.sh:89`, default
  `VM_INDEX_CEIL=12`).
- Each snapshot+wake cycle holds the source slot for ~90 s
  (host_fence ~60 s + Nomad purge ~30 s).
- No per-bearer rate-limit on snapshot/wake endpoints (R9-S6 carry).
- Sustained attacker throughput per worker:
  `12 slots / 90 s ≈ 0.13 wakes/s ≈ 8 wakes/min`.

At cluster size 12 × 5 workers = 60 slots, the global ceiling is
`60 / 90 s ≈ 0.67 wakes/s` before all slots are saturated. An
attacker with one valid `sandbox-token` who can drive both
CREATE/SNAPSHOT (admin-side) and POST `/sandboxes/<id>/restore`
(user-side) can:
- Slot-park 12 sandboxes per worker for ~90 s by repeatedly
  snapshotting them.
- Force legitimate wakes onto the 120-s caller-retry budget
  (legitimate wakes still succeed eventually, just delayed).
- If the attacker has admin token: drive a sustained ~0.67 wakes/s
  cluster-wide, eventually exhausting every slot. Legitimate
  CREATE returns 503 with `VmIndexUnavailable`.

The 503 surface is bounded-bad (no data exfiltration, no
authentication bypass, no privilege escalation). But it IS a
DoS class that is **newly reachable post-C-3** because before C-3
the SNAPSHOT path itself was 500-blocked by the compio panic.
Worth surfacing as posture — see R13-S2 below.

### 6. `unsafe` audit — re-run since r12

`Grep unsafe\s*\{` on `crates/sandbox/src/` + `crates/sandbox-agent/
src/` at HEAD `0053e8b6`:

Production unsafe blocks (all pre-existing, none added by C-3 / C-4
/ R13-Q1):
- `crates/sandbox-agent/src/dropuser.rs` — 16 unsafe blocks
  (libc setuid/setgid/prctl/setrlimit/etc.; module-scoped
  `#![allow(unsafe_code)]`; explicit `unsafe fn pre_exec_lockdown`).
  Pre-existing.
- `crates/sandbox-agent/src/files.rs` — 7 unsafe blocks
  (OwnedFd::from_raw_fd around openat2). Pre-existing.
- `crates/sandbox-agent/src/exec.rs` — 2 unsafe blocks
  (Command::pre_exec closure, geteuid). Pre-existing.
- `crates/sandbox-agent/src/handlers.rs:893` — `libc::settimeofday`
  in /_clock_resync. Pre-existing (R9-S6 surface).

Test-only unsafe blocks (the env-mutation set; R13-Q1 RELOCATED but
did NOT add new ones):
- `crates/sandbox/src/db.rs:2811-2820` (3 unsafe blocks; ENV_LOCK
  mutex; `SANDBOX_HOST_ID` + 8 other HA keys). Pre-existing.
- `crates/sandbox/src/db.rs:2868-2871` (2 unsafe blocks; nested
  inside `with_env_clean`). Pre-existing.
- `crates/sandbox/src/backend/nomad_ch.rs:3463-3469`
  (3 unsafe blocks; relocated from `mod tests` to sibling
  `pub(crate) mod test_env_lock`). MOVED, not new.

Diff `42212c5c..0053e8b6` confined to env-block moves
(`-` and `+` mirror each other for the unsafe set). **Zero new
unsafe surface.** C-3 used safe `std::thread::Builder::spawn`; C-4
used safe `compio::time::sleep` + safe `VmIndexAllocator::reserve`.
Confirmed.

## Findings (NEW since r12)

### [R13-S1] Worker VM GCS scope `storage-rw` is project-wide on the default Compute Engine SA — narrow-bucket-scope claim depends on unenforced operator-side IAM hardening (IMPORTANT, security-r13)

- **File**: `crates/sandbox/scripts/provision-gcp-cluster.sh:275-289`
  (worker creation block, no `--service-account` flag).
- **Symptom**: `d7740b03` upgraded the worker scope from
  `storage-ro` to `storage-rw`. The deferred-doc summary at
  `docs/reviews/sandbox-snapshot-restore-deferred.md:88` claims
  *"Worker still has no IAM permission outside the project's
  snapshot bucket (per existing service-account-level grants)."*
  That assertion is **true only under a hardened SA**; the
  provisioning script does NOT pass `--service-account`, so the
  worker runs as the project's **default Compute Engine service
  account** (`<PROJECT_NUMBER>-compute@developer.gserviceaccount.
  com`). The default SA carries `roles/editor` on the project by
  default — granting `storage.objects.*` on every bucket the
  project owns. The `storage-rw` OAuth scope narrows API surface
  (only `devstorage.*`, not `compute.*` etc.) but does NOT narrow
  the **resource scope**; resource-scope is set by IAM, and the
  default IAM is "project-wide editor".
- **Threat model**: worker-VM compromise → attacker has
  read+write on every bucket in the GCP project. In a test
  project that hosts only `suger-dev-zsbx-artifacts`, this is one
  bucket. In a real production project that may host audit logs,
  terraform state, customer-data buckets, or unrelated apps'
  artifacts, this is **all of them**. The blast radius is set by
  project tenancy, not by code.
- **Why this is IMPORTANT not MINOR**: the deferred-doc summary
  asserts a security property the code does NOT enforce. An
  operator reading the deferred-doc would reasonably believe
  the bucket-narrowing was wired up; it is not. The C-5 fix
  closes the L2-upload-403 functional bug (correctly), but
  leaves the scope-narrowing claim conditional on unstated
  operator IAM hygiene. Any future operator deploying to a
  multi-tenant project inherits the project's default SA grants.
- **Action**:
  (a) Add `--service-account=zsbx-worker@$PROJECT.iam.
      gserviceaccount.com` to the worker `gcloud compute instances
      create` invocation. Create the SA in the provisioning script
      idempotently, grant it `roles/storage.objectAdmin` on
      **only** `$ARTIFACT_BUCKET` and `$SNAPSHOT_BUCKET`, and
      `roles/logging.logWriter` + `roles/monitoring.metricWriter`
      project-wide for the existing logging/monitoring scopes.
  (b) Update the deferred-doc note at
      `sandbox-snapshot-restore-deferred.md:88` to reflect what
      IAM the provisioning script actually configures.
  (c) Optionally: add a startup-script preflight that calls
      `gcloud iam service-accounts get-iam-policy
      $(gcloud config get-value project)-compute@…` and
      **refuses to start** the controller if the SA still has
      `roles/editor` or `roles/owner`. Belt-and-suspenders.
  (d) Server VM at line 250 (`--scopes=storage-ro,…`) inherits
      the same project-default SA — same caveat applies, but the
      blast radius there is read-only on `devstorage.*`.

### [R13-S2] Snapshot+wake DoS via 12-slot vm_index exhaustion newly reachable post-C-3 — no per-bearer rate-limit (MINOR posture, security-r13)

- **Files**: `crates/sandbox/src/restore_handler.rs:128-160`
  (`VmIndexRetryPolicy`, 60 × 2 s = ~120 s budget);
  `crates/sandbox/scripts/gcp-worker-startup.sh:89` (default
  `VM_INDEX_CEIL=12`); `crates/sandbox/src/lib.rs:69,399,757`
  (only `mint_rate_limiter` is wired; no snapshot/wake limiter).
- **Symptom**: post-`c890c015`, SNAPSHOT no longer panics
  end-to-end on the cluster (was the C-3 surface). With no
  per-bearer rate-limit on the snapshot/wake endpoints (R9-S6 / S8
  carry-forward), an attacker holding a valid `sandbox-token` (or
  `sandbox-admin-token`) can drive sustained snapshot+wake cycles
  at ~0.13 wakes/s per worker (12 slots / 90 s teardown hold),
  saturating vm_index slots cluster-wide.
- **Threat model**: legitimate CREATE returns 503
  `VmIndexUnavailable`; legitimate wake takes the 120-s
  caller-retry path; no data exfiltration. Bounded-bad DoS.
  Pre-C-3 this was masked by the snapshot-itself failing
  end-to-end; post-C-3 it is **newly reachable** as a sustained
  cluster-degradation vector.
- **Why MINOR posture and not IMPORTANT**: (1) sandbox-token /
  sandbox-admin-token are operator-issued bearer credentials, not
  end-user-derived; the threat model is "compromised creator
  bearer", not "anonymous attacker". (2) The retry budget at
  120 s envelope's the worst-case teardown, so legitimate wakes
  succeed bounded-late, not bounded-wrong. (3) No CRITICAL fence
  is violated — host_fence (the FM-F defense) still clears
  before slot release. The harm is SLO-class.
- **Action**:
  Per-bearer token-bucket on `POST /sandboxes/*/snapshot` and
  `POST /sandboxes/*/restore`; default ~5 cycles/min/bearer
  (10 % of the saturation envelope at cluster size 12 × 5
  workers). The existing `MintRateLimiter` shape
  (`preview_share_handlers.rs:23,206`) is a workable template;
  same per-key sliding-window state structure, different keys.
  Belt: emit a counter
  `vm_index_unavailable_total{bearer_hash}` so an operator can
  detect the attack pattern from the metrics surface.

## Verified open carry-forward (unchanged at HEAD)

- **R9-S1** (CRITICAL → currently UNREACHABLE) —
  `nomad-vm-wrapper.sh:476-498` anchored prefix regex on the AEAD
  config.json carve-out. AEAD layer is OFF in cluster
  (`SANDBOX_SNAPSHOT_ROOT_KEK_PATH` not set by
  `gcp-worker-startup.sh`; A1 in deferred). Once A1 lands the
  finding becomes immediately reachable. Carry forward.
- **R9-S2** (IMPORTANT) — `snapshot_aead.rs::derive_dek` /
  `derive_nonce_prefix` keyed on 1-second timestamp. Re-snapshot
  within same wall-clock-second → nonce reuse. Unchanged at HEAD.
- **R9-S3** (IMPORTANT) — `snapshot_handler.rs:417` writes
  `Some("v1")` into `snapshot_aead_dek_id` regardless of whether
  AEAD root KEK is present. With cluster AEAD OFF, every snapshot
  row in pg is mislabeled as `dek_id=v1` while the artifact is
  plaintext. Unchanged.
- **R9-S5** (IMPORTANT → partially closed) — restore env block
  lacks `ZSBX_SANDBOX_ID` under raw_exec mode
  (`restore_handler.rs:1334-1349`). ChPlugin partial-close via
  typed `sandbox_id` Config field at `:1403` (r12 finding).
  Unchanged.
- **R10-S1** (IMPORTANT) — 5 secret-file loaders all use
  `std::fs::metadata` (follows symlinks) then a separate
  `read`/`open` that re-resolves. R10-S1 unchanged at HEAD.
- **R10-S2** (IMPORTANT) — `restore_handler.rs:294-298`
  `let _ = spawn_blocking(...).await`. JoinError swallow
  unchanged.
- **R10-S3** (MINOR) — `restore_handler.rs:1168-1211`
  `teardown_restore` step (3) `release_vm_index` runs regardless
  of step (1) `nomad_delete_blocking` outcome. Unchanged.
- **R11-S3** (MINOR posture) — `snapshot_aead.rs::chunk_aad`
  binds only `"zsbx-snap" || chunk_index`; not sandbox_id /
  taken_at. Unchanged.
- **R12-S1** (IMPORTANT → partially closed) — see § 7 below.
- **R12-S2** (MINOR) — see § 6 carry-forward of R9-S5.
- **R9-S6 / S7 / S8** (MINOR) — `/_clock_resync` agent-body
  journald leak, `/livez|/readyz|/metrics` unauthenticated,
  admin endpoints lack per-bearer rate-limit. Unchanged.

## R12-S1 partial close at `c5b9cb9d`

`c5b9cb9d` unified `T7_ENV_LOCK` + `R12_I1_ENV_LOCK` into a single
`TASK_DRIVER_ENV_LOCK` at `nomad_ch.rs:3445` inside a
`pub(crate) mod test_env_lock` so both `nomad_ch::tests` and
`restore_handler::r12_i1_tests` use the same mutex when mutating
`SANDBOX_TASK_DRIVER`.

**Partial close because**: the unified lock is **per-env-key**, not
**per-process**. The Rust-2024 stdlib `set_var` contract (per
finding-r12 reasoning) is that the **env table itself** is
thread-unsafe — concurrent `set_var(A) + set_var(B)` is UB even
when A ≠ B. Three module-local mutexes were the r12 problem;
`c5b9cb9d` reduced that to two:

| Lock | Module | Keys serialised |
|---|---|---|
| `db.rs:2790::ENV_LOCK` | `db::tests` | `SANDBOX_HA_*` (4 keys), `SANDBOX_HOST_ID`, `SANDBOX_PERSIST_DIR`, `SANDBOX_DATABASE_URL`, `SANDBOX_DATABASE_PASSWORD_PATH`, `SANDBOX_PG_*` (2 keys) — 9 keys total |
| `nomad_ch.rs:3445::TASK_DRIVER_ENV_LOCK` (NEW SCOPE) | `nomad_ch::tests` + `restore_handler::r12_i1_tests` | `SANDBOX_TASK_DRIVER` — 1 key |

Two locks; disjoint key sets; concurrent `set_var("SANDBOX_HOST_ID",
"x")` (db::tests) + `set_var("SANDBOX_TASK_DRIVER", "y")`
(nomad_ch::tests) is **still UB** per the Rust-2024 contract.
**R12-S1 partial close; full close requires promoting BOTH to a
single crate-wide static** (e.g. `crate::tests::ENV_LOCK` in a
`src/tests/support.rs` module), with both mutating helpers
unconditionally taking it.

## Closed by recent commits since r12

- **C-3** (cluster review r4) at `c890c015` — `Tiered::put`'s L2
  upload detach now via `std::thread::Builder::spawn`. Security
  delta: none (no privilege change). See § 2 above.
- **C-5** (cluster review r5) at `d7740b03` — worker VM scope
  upgraded `storage-ro` → `storage-rw`. Functional close. R13-S1
  above flags the residual security-posture nuance about SA-level
  resource-scope.
- **R13-Q1** at `c5b9cb9d` — `SANDBOX_TASK_DRIVER` env-lock
  unified across nomad_ch + restore_handler. R12-S1 partial
  close; db.rs::ENV_LOCK still separate.

## Counts

- CRITICAL: 0 new (R9-S1 carry, currently unreachable due to AEAD
  being OFF on cluster).
- IMPORTANT: 1 new (R13-S1); carry: R9-S2, R9-S3, R9-S5 (raw_exec
  arm only), R10-S1, R10-S2, R12-S1 (now partially closed by
  `c5b9cb9d`).
- MINOR: 1 new (R13-S2); carry: R10-S3, R10-S6, R11-S3,
  R9-S6/S7/S8, R12-S2.
- Total NEW this round: 2.

r12-closed at HEAD: 0 (R12-S1 only **partially** closed at
`c5b9cb9d` — `SANDBOX_TASK_DRIVER` arm only). r9-carry: 7
(R9-S1/S2/S3/S5/S6/S7/S8). r10-carry: 4 (R10-S1/S2/S3/S6). r11-carry:
1 (R11-S3). r12-carry: 2 (R12-S1 partial, R12-S2).
