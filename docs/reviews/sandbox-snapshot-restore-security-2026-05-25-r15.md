# Sandbox/snapshot-restore — security r15 review

Date: 2026-05-25 (UTC)
HEAD at audit: `2e9ae598` (24 commits past r14's `91ce9be5`).
Round 15 of N (security lens catching up; other lenses already r15).
Read-only. Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/scripts/**`.

## Summary

1 new CRITICAL (R15-S1, AEAD fail-OPEN in tiered+GCS mode still
unclosed — A1-FOLLOWUP open since arch-r9; `lib.rs:655-666` logs
`tracing::error!` then composes passthrough `AeadSnapshotStore`
and continues boot). 1 new IMPORTANT (R15-S2, snapshot
`config.json` path-injection: `nomad-vm-wrapper.sh:476-498` Python
rewriter passes through non-alloc-prefix absolute paths verbatim;
CH then opens them as virtio-blk under root — distinct from R9-S1
because reachable in current AEAD-OFF cluster via bucket-write).
1 new MINOR posture (R15-S3, C-8 fence cap to 30s is
security-positive but undocumented).

All r14 carry-forwards verified unchanged. R14-S1
`nomad_ch.rs:2002` Drop guard untouched. R13-S1/S2 + R12-S1
partial + R10-S1/S2/S3 + R9-S2/S3/S6/S7/S8 + R11-S3 unchanged.
R9-S1 unreachable cluster-side (AEAD OFF, gated by R15-S1).
Zero new `unsafe` since r14. The 24-commit window is C-7/C-8/C-8a/
C-8b fence tuning, R14-A6 retry-policy derivation, R15-Q1
thread-name cleanup — none touched the privilege boundary or
auth surface.

## Hunt-list disposition (security lens)

### 1. AEAD trust chain — tiered+GCS without KEK (hunt #1)

At `lib.rs:640-666` (HEAD `2e9ae598`), when
`config.snapshot_use_gcs=true` AND KEK env is unset: boot emits
`tracing::error!(..., "AEAD DISABLED — guest RAM plaintext on
disk + GCS")` and then `Arc::new(AeadSnapshotStore::new(tiered,
None))` — passthrough mode. Boot continues. `snapshot_handler.rs:417`
still stamps `dek_id="v1"` (R9-S3 carry). A1-FOLLOWUP at
`deferred.md:554-558` is explicit: *"return a hard configuration
error from `AppState::from_config` (boot panic, not log line)"*.
Not implemented at HEAD.

File-side gates (when KEK path IS set) at
`snapshot_aead.rs:185-217::RootKek::from_path` are correct:
mode 0o400 required (192-197), uid==0 required (198-204, R9-S4
closure), length==32 required (208-212). Wrong size/mode/uid →
`Err` propagates through `from_env` to the `?` at `lib.rs:628`
→ boot fails. The CRITICAL gap is the env-unset arm returning
`Ok(None)` and letting the tiered+GCS passthrough wrap proceed.
See R15-S1.

### 2. Snapshot integrity — `/_clock_resync` signed-bypass (hunt #2)

`sig.rs:440-518::verify_kind_inner` with `skip_skew_check=true`
disables ONLY timestamp skew. All other gates apply: nonce shape
(464-469), signature decode/length (472-480), body-hash bound
(485-489), `verify_strict` Ed25519 (503, rejects malleable),
nonce-replay LRU after signature (510-515, TTL 30s).
`handlers.rs:762-867::clock_resync` adds: `sandbox_id` must match
`boot_sandbox_id()` (R7-S1, 786-813), challenge shape 64
lowercase hex (819-834), separate `RESYNC_CHALLENGES` LRU
(`handlers.rs:68`, capacity 32, no TTL).

Verifying key loaded once from kernel cmdline
(`SANDBOX_AGENT_CONTROLLER_PUBKEY`); restart-required to rotate.

**Replay across reboot**: agent reboot empties the LRU; captured
controller-signed request could pass on fresh agent. Protocol
defense is **fresh random challenge per resync call** via `OsRng`
(`nomad_ch.rs::http_signed_async("/_clock_resync", ...)`). If
`OsRng` degrades, defense collapses to LRU's boot-only lifetime;
standard `getrandom` builds sound.

**Verdict**: no new finding. Posture only.

### 3. New admin endpoints since r14 (hunt #3)

10 admin handlers in `admin_handlers.rs`, all gated by
`admin_check(&req, &state)` (lines 310, 432, 490, 583, 653, 720,
888, 1224, 1424, 1476). `main.rs:194-235` registers 10 routes
1-to-1. **No new endpoint added since r14.** `/livez` `/readyz`
`/health` remain unauth (R9-S7). No `/metrics`. Per-bearer
rate-limit on heavy admin endpoints still missing — `MintRateLimiter`
gates creator share-mint only (R13-S2).

### 4. C-6 detach OS-thread security (hunt #4)

`admin_handlers.rs:1339-1392` (post-`7469118e`): `Arc::clone(&state)`
moves into a `std::thread::Builder::spawn`-ed thread that runs
`compio::runtime::Runtime::new().block_on(stop_inner.await)`.

`AppState` carries `admin_token: Zeroizing<String>` (`lib.rs:141`),
`persist: Arc<Persistence>` (AEAD key, `:106`), `database:
Arc<Database>` (pg DSN+pw, `:85`), `config.token: ApiToken`
(creator bearer, `:63`). On thread exit: Arc refcount decrements,
AppState keeps living (controller's Arc still alive), **no
zeroize fires**. OAuth bearer from metadata-server for Nomad-purge
HTTP lives in plain `String`, drops without zeroizing — **same
posture as every thread that calls GCS/Nomad metadata APIs** (the
main runtime included); not C-6-specific. uid/gid/cwd inherit
(uid 0 per raw_exec trust boundary). **No privilege boundary
changed.** Confirmed unchanged from r14 §5.

### 5. `#24 ZSBX_SANDBOX_ID` paperwork audit (hunt #5)

Deferred says R8-DEPLOY1 was applied "only to wrapper + docs".
**Falsified**: `nomad_ch.rs:2373` emits `"ZSBX_SANDBOX_ID": sandbox_id`
into every jobspec via `build_nomad_job_json_with` (the canonical
builder used by both create AND restore paths). Test pins:
`nomad_ch.rs:4056-4087` (B24 presence+format), `:4357-4358` (B24
driver-mode survival). R9-S5's raw_exec arm is also closed via the
same shared builder. **Mark deferred #24 + R9-S5 raw_exec arm
paperwork-only closures.** No security finding.

### 6. GCS bucket-level ACL post-C-5 (hunt #6)

`provision-gcp-cluster.sh:286` unchanged: `--scopes=storage-rw,...`
without `--service-account` → default Compute Engine SA's
`roles/editor` = `storage.objects.*` on every project bucket. With
AEAD OFF (R15-S1 unclosed), all L2 snapshots are plaintext and a
worker compromise leaks all snapshots cross-tenant within the
project. **R15-S1 → R13-S1 compound.** R13-S1 carry unchanged.

### 7. Snapshot config.json restore rewrite — path injection (hunt #7)

`nomad-vm-wrapper.sh:476-498` Python rewriter:

```python
ALLOC_PREFIX = re.compile(r"^/opt/nomad/data/alloc/[^/]+/[^/]+/local(/|$)")
def rewrite(value):
    ...
    if not m: return value   # ← non-matching absolute paths pass verbatim
```

A malicious `config.json` with `disks[].path = "/etc/shadow"`
(or `/dev/sda`, `/dev/kmem`) does NOT match → returned unchanged
→ wrapper execs `cloud-hypervisor --disk path=/etc/shadow` **as
root** (raw_exec trust boundary at `nomad-vm-wrapper.sh:247-248`).
CH attaches the host file as virtio-blk to the guest, granting
guest read/write to that host path.

AEAD authentication WOULD reject a tampered artifact at
`AeadSnapshotStore::get` — except `config.json` is deliberately
carved out (R9-S1, memory-ranges only). So the wrapper's anchored
regex is the SOLE structural defense in either AEAD posture. pg
sha256 blocks bucket-write-only attackers but not joint
controller+worker compromise. R9-S1 framed the AEAD-ON carve-out
(unreachable today); R15-S2 is the AEAD-OFF reachability via
bucket-write — wide open today.

### 8. `unsafe` audit since r14

Re-grep at HEAD: zero new unsafe. Production unsafe unchanged
(16+7+2+1 sandbox-agent including R9-S6 `settimeofday` at
`handlers.rs:893`); test-only unchanged (3+3 env-lock blocks).

## Findings (NEW since r14)

### [R15-S1] AEAD fail-OPEN in tiered+GCS mode: lib.rs:655-666 logs `tracing::error!` then composes passthrough wrapper and continues boot (CRITICAL, security-r15, A1-FOLLOWUP unclosed)

- **Files**: `crates/sandbox/src/lib.rs:640-666` (the
  `snapshot_use_gcs=true` arm of the snapshot-store composition).
- **Symptom**: when `config.snapshot_use_gcs=true` AND
  `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` is unset, boot logs
  `tracing::error!(..., "AEAD DISABLED — guest RAM plaintext on
  disk + GCS")` and then `Arc::new(AeadSnapshotStore::new(tiered,
  None))` returns a passthrough wrapper. Boot continues. Every
  snapshot writes guest RAM plaintext to GCS while
  `snapshot_handler.rs:417` stamps `dek_id="v1"` (R9-S3 carry).
  Deferred-doc A1-FOLLOWUP at `deferred.md:554-558` specifies:
  *"return a hard configuration error from
  `AppState::from_config` (boot panic, not log line)"*. Not
  implemented at HEAD.
- **Threat model**: any production deploy that omits the
  one-line `Environment=SANDBOX_SNAPSHOT_ROOT_KEK_PATH=...` in
  the systemd unit silently runs with full plaintext-to-GCS while
  pg audit trail claims encryption. Compounds with R13-S1: a
  project-editor SA on a compromised worker reads every snapshot
  in the project as plaintext.
- **Why CRITICAL not IMPORTANT**: (1) deferred-doc already marks
  CRITICAL with a concrete remediation; (2) silent failure mode
  (`dek_id="v1"` lies about posture); (3) reachable without an
  active attacker — every cluster-smoke run today is in this
  state; (4) fix is a 5-line `return Err(...)` insertion.
- **Action**:
  (a) Replace the `tracing::error!` + passthrough-wrap at
      `lib.rs:655-664` with `return Err(format!(...))`. Mirror
      the `RootKek::from_env Err` short-circuit pattern at
      `lib.rs:628`.
  (b) Add `SANDBOX_SNAPSHOT_ALLOW_UNENCRYPTED_REMOTE=1` escape
      hatch for non-prod (deferred-doc suggestion).
  (c) Close R9-S3 in the same commit: stamp `dek_id="none"` when
      `AeadSnapshotStore::is_active() == false`.

### [R15-S2] Snapshot config.json path-injection: wrapper passes through non-alloc-prefix absolute paths verbatim; CH opens them as virtio-blk under root (IMPORTANT, security-r15)

- **Files**: `crates/sandbox/scripts/nomad-vm-wrapper.sh:476-498`
  (Python `rewrite()` + `ALLOC_PREFIX` regex);
  `nomad-vm-wrapper.sh:247-248` (raw_exec runs as root);
  `crates/sandbox/src/restore_handler.rs:861-870`
  (`rewrite_config_json` only touches `net[].tap` / `net[].mac`).
- **Symptom**: a malicious `config.json` carrying `"disks":
  [{"path": "/etc/shadow"}]` does NOT match the alloc prefix →
  Python `rewrite()` line 490 returns unchanged → wrapper execs
  `cloud-hypervisor --disk path=/etc/shadow` as root. CH attaches
  the host file as a virtio-blk device exposed to the guest VM.
- **Threat model**: bucket-write between `store.put` and
  `store.get`. Realistic surface today: compromised worker via
  R13-S1's project-editor SA. AEAD ON would NOT block because
  `config.json` is deliberately carved out of the AEAD wrap
  (R9-S1, memory-ranges only). pg-stored sha256 blocks
  bucket-write-only attackers but not joint controller+worker
  compromise.
- **Why distinct from R9-S1**: R9-S1 framed the AEAD-ON carve-out
  (currently unreachable). R15-S2 is reachable in AEAD-OFF
  cluster posture **today** via the bucket-write surface.
- **Why IMPORTANT not CRITICAL**: requires bucket-write
  (R13-S1-gated); CH-as-root is the operator-accepted raw_exec
  trust boundary; guest must know how to interpret the smuggled
  path. Bounded-bad: two pre-conditions.
- **Action**:
  (a) Change `rewrite()` no-match branch from "return value
      unchanged" to "log + `sys.exit(1)`" when the value is an
      absolute path not under the alloc prefix. Whitelist:
      `/opt/nomad/data/alloc/<uuid>/<task>/local` only.
  (b) Belt: validate at controller-side `rewrite_config_json`
      (`restore_handler.rs:1106`) before passing to wrapper.
  (c) Long-term: extend AEAD wrap to `config.json` + `state.json`
      (closes R9-S1 + R15-S2 (b) + R15-S1's "but config.json is
      plaintext anyway" residual).

### [R15-S3] `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` lowers post-snapshot duplicate-agent window; security-positive but undocumented (MINOR posture, security-r15)

- **Files**: `crates/sandbox/scripts/gcp-worker-startup.sh:466-472`
  (C-8 fix `2afbb2dd`); `crates/sandbox/src/config.rs:401`
  (Rust default 120s).
- **Symptom**: `host_fence` is the window where source agent is
  still alive post-snapshot, carrying in-memory signing key + any
  cached secrets. A SHORTER fence reduces the duplicate-agent
  window where source + restore both hold the same key on the
  cluster. The C-8 commit justified the 30s cap purely on
  correctness (C-4 retry-budget envelope) with NO security
  analysis.
- **Why MINOR posture**: no new gap; if anything reduces a
  pre-existing window. Documentation gap only — future
  maintainers might bump the fence back without realizing the
  security side-effect.
- **Action**: add one sentence to the comment block at
  `gcp-worker-startup.sh:466-472`: *"Lower fence = smaller
  duplicate-agent window on cluster (security-favored
  direction). Future tuning should preserve fence ≤ 60s as
  defense-in-depth against same-key dual-agent state."* Doc-only.

## Verified open carry-forward (unchanged at HEAD `2e9ae598`)

- **R14-S1** (IMPORTANT) — `nomad_ch.rs:2002` Drop guard detach.
- **R14-S2** (MINOR posture) — pg-pool exhaustion amp.
- **R13-S1** (IMPORTANT) — default Compute Engine SA +
  `storage-rw` = project-wide bucket r/w.
- **R13-S2** (MINOR posture) — no per-bearer rate-limit
  snapshot/wake.
- **R12-S1** (IMPORTANT partial) — `db.rs::ENV_LOCK` +
  `TASK_DRIVER_ENV_LOCK` disjoint per-key mutexes.
- **R11-S3** (MINOR posture) — `chunk_aad` missing
  sandbox_id/taken_at.
- **R10-S1** (IMPORTANT) — 5 secret-file loaders
  symlink-following.
- **R10-S2** (IMPORTANT) — JoinError swallow at
  `restore_handler.rs:294-298`.
- **R10-S3** (MINOR) — `teardown_restore` step ordering.
- **R9-S1** (CRITICAL, UNREACHABLE) — gated by R15-S1.
- **R9-S2** (IMPORTANT) — DEK 1-sec granularity.
- **R9-S3** (IMPORTANT) — `dek_id="v1"` regardless of AEAD active.
  R15-S1 (c) couples the fix.
- **R9-S5** (CLOSED via shared jobspec builder) — paperwork only.
- **R9-S6/S7/S8** (MINOR) — agent journald leak, livez/readyz
  unauth, admin endpoints lack rate-limit.

## Closed by recent commits since r14

- **C-7** `493d6c1e` — retry budget 120s → 48s. Sec delta: none
  (reduces R14-S2 per-wake amp ~60%).
- **C-8 + C-8a** `2afbb2dd` — fence cap 30s + MIN-of-two ceilings.
  Sec delta: R15-S3 (doc-only).
- **C-8b** `64af1803` — 2× fence factor. Sec delta: none.
- **R14-A6 + R14-Q4** `c3edf968` — retry policy derived from cfg.
  Sec delta: none.
- **R15-Q1** `7469118e` — admin_handlers byte-slice form. Sec
  delta: none.
- **R14-Q2** `79b4d258` — `seal_filename_for_str` cfg-gated. Sec
  delta: minor positive (stringly-typed surface removed from prod).

## Counts

- CRITICAL: 1 new (R15-S1); carry: R9-S1 (unreachable
  cluster-side, becomes reachable once R15-S1 closes and AEAD
  turns ON).
- IMPORTANT: 1 new (R15-S2); carry: R14-S1, R13-S1, R12-S1
  partial, R10-S1, R10-S2, R9-S2, R9-S3.
- MINOR: 1 new (R15-S3, doc-only); carry: R14-S2, R13-S2,
  R11-S3, R10-S3, R9-S6/S7/S8.
- Total NEW this round: 3.

r14-closed at HEAD: 0 r14-S items closed. r9-S5 verified
code-closed (paperwork-update only). r9-carry: 6. r10-carry: 3.
r11-carry: 1. r12-carry: 1 (partial). r13-carry: 2. r14-carry: 2.

## Cross-lens consensus

- **arch-r15** (C-7-LT async wake, R15-A1): R15-S1+S2 orthogonal
  (boot-time + wrapper-script gates); not closed by C-7-LT.
- **concurrency-r15** (R15-I1, fence config drift): the 30s
  override is R15-I1's drift point. Add R15-S3 sentence to the
  same comment block.
- **test-cov-r15** (R15-T1, integration gap): R15-S1 catchable by
  an `AppState::from_config` boot-gate test (tiered+GCS + KEK
  unset → `Err`); not in today's unit-test arms.

## Lens hand-off

- **Architecture**: R15-S1 + R15-S2 both point at the AEAD wrap
  manifest being too narrow (memory-ranges only). Extending to
  `config.json` + `state.json` closes R9-S1, R15-S2 (b), and
  R15-S1's config.json residual. Weigh vs C-7-LT.
- **Concurrency**: R14-S1's `nomad_ch.rs:2002` Drop guard is 4
  commits stale; R15-Q3 proposed `detach_isolated` helper extract.
  Pin design + green-light.
- **Test-cov**: add boot-gate test for R15-S1 (`from_config`
  tiered+GCS+no-KEK → `Err`). Add path-injection test for R15-S2
  (synthetic `config.json` with `/etc/passwd` in `disks[].path`,
  expect Python rewriter non-zero exit).
