# Sandbox/snapshot-restore — security r19 review

Date: 2026-05-25 (UTC)
HEAD at audit: `dea68995`. Catchup r19 — security lens was one
round behind at r18.
Scope: R19-C1 (PR1 + PR2 takeover sweep), R19-I1 (livez two-phase
probe), C-7-LT-1 (async retry budget), C-7-LT-4 (Go path allow-list,
driver-side), C-7-LT-5 (SetBinaries ChRemoteBin fix), driver v6
upload, controller v30 pin bump; six standing carry-forwards.
Read-only.

## Summary

**R19-C1 takeover sweep (`1d3724fe` + `8d163d58`)** introduces no
authz or trust-boundary regressions. `claim_orphan_wake_for_recovery`
is a single indexed UPDATE with `WHERE state NOT IN ('ok','failed')
AND lessee_updated_at < now() - interval`; pg row-locks serialise
concurrent claims (no double-claim). `WakeErrorCode::WakeWorkerAborted`
is a structurally enumerable variant; migration 0012's CHECK extension
is correctly additive (still rejects any unknown value at the column
level). The sweep runs on `detach_isolated("wake-takeover", …)` —
no ntex starvation. The threshold floor of 30 s correctly exceeds
the longest mid-flight wake stage (no row-stealing race against a
healthy in-flight wake).

**R19-I1 two-phase probe (`82478a6b`)** mirrors C-7-LT-2-PR1's
proven shape: compio-native TCP-connect gate (`CONNECT_TIMEOUT=150ms`)
fronts the ureq `/livez` call. `parse_agent_probe_addr` is reused
verbatim; the destination is the same controller-derived RFC1918
literal (`10.<subnet>.<100+vm_index>.2:7777`) — no tenant input
reaches the probe. Port-scanning oracle: **not a real surface** —
the address space is fully controller-built and the caller
(`create()` at `nomad_ch.rs:791`) is admin-token-gated.

**C-7-LT-4 Go path allow-list (`c1df13d3`, driver-side)** ports
the wrapper's R15-S2 hardening to `rewriteAndAssertUnderTaskDir` —
empty-string reject, non-absolute reject, `..`-component reject,
post-rewrite containment check via `filepath.Clean`. **Documented
parity gap**: the Go side does NOT do a `realpath` / symlink
resolution (the bash wrapper does, via `os.path.realpath`). The
Go file documents this explicitly as deferred to "operator's
alloc-dir hygiene policy". See **R19-S1** below.

**C-7-LT-5 SetBinaries (`cffb5c8e`, driver-side)**: **NOT a security
issue, functional only**. The bug aliased `c.chRemoteBin` (the
SECOND arg of `SetBinaries`) to `VirtiofsdBin`. `Client.Shutdown`/
`Resume`/`Probe` shelled out to virtiofsd; virtiofsd rejected the
argv. The first arg (CH binary) was always correct — no CH swap,
no privilege escalation, no sandbox escape. SIGTERM fallback kept
the termination contract. **Security verdict: clean.**

**C-7-LT-1 async retry budget (`f9996fcf`)** widens the budget
only in `WakeResponseMode::Async`; sync mode preserves the MIN
contract. No auth surface; the budget governs wall-time on a
controller-internal allocator call.

**Carry-forwards**: R13-S1 (IMPORTANT — `storage-rw` scope +
default GCE SA at `provision-gcp-cluster.sh:286`), R9-S3
(IMPORTANT — `Some("v1")` dek_id at `snapshot_handler.rs:417`),
R15-S3 (MINOR — fence-cap-30s undocumented), R17-S2 (MINOR —
KEK implicit-root), R18-S1 (IMPORTANT — sanitizer parity with
`is_blocked_ip`), R18-S2 (MINOR — fence-error IP leak into
scoped log target) ALL UNCHANGED.

**New findings**: 1 (R19-S1, IMPORTANT — Go-side path allow-list
missing the bash wrapper's `realpath` symlink-resolution step).

## Findings (NEW since r18)

### [R19-S1] Driver-side Go path allow-list omits `realpath` symlink-resolution; bash wrapper does it (IMPORTANT)

- **Files**: `nomad-driver-ch/ch/config_rewrite.go:80-145`
  (`rewriteAndAssertUnderTaskDir`) vs
  `crates/sandbox/scripts/nomad-vm-wrapper.sh:537-583`
  (`assert_under_task_dir`).
- **Symptom**: the bash wrapper resolves symlinks via
  `os.path.realpath(value)` and compares against
  `TASK_DIR_REAL = os.path.realpath(task_dir)`. The Go port uses
  `filepath.Clean` only (no symlink resolution). The Go file
  documents this explicitly at lines 99-107: *"We deliberately
  do NOT call os.Stat / filepath.EvalSymlinks here"*.
- **Threat model**: a snapshot's `config.json` carries
  attacker-influenceable paths under the rewriter. With
  `filepath.Clean` alone, an attacker who can:
  1. Pre-place a symlink at a path that lexically resolves under
     `taskDir` (e.g. `<taskDir>/local/disks/0` → `/etc/shadow`),
     AND
  2. Have CH open that path during `--restore`,
  bypasses the allow-list — `filepath.Clean(<taskDir>/local/disks/0)`
  yields `<taskDir>/local/disks/0` (containment passes), but
  CH's `open()` follows the symlink to `/etc/shadow`. The bash
  wrapper closes this via `os.path.realpath` before comparing.
- **Why IMPORTANT-not-CRITICAL**: the pre-condition (write a
  symlink under `taskDir/local/`) requires either (a) prior code
  execution inside the alloc dir, or (b) a malicious operator
  with filesystem access — both of which carry their own
  compromise envelope. But the restore-from-snapshot path is
  expressly the place where attacker-controlled `config.json`
  hits a sanitiser, and R15-S2's whole point was to close that
  vector. Shipping a Go port that is documented-strict-subset
  of the bash wrapper's defence reopens half of the gap R15-S2
  closed (the half exercised when the driver bypasses the
  wrapper — which `cffb5c8e`'s commit message confirms is the
  production path on the restore branch).
- **Action**: extend `rewriteAndAssertUnderTaskDir` with a
  best-effort `filepath.EvalSymlinks` step on the cleaned
  path, OR document the operational requirement that
  `taskDir/local/` is freshly created per-alloc with no
  symlinks (Nomad's alloc-dir lifecycle nominally guarantees
  this, but the bash wrapper's choice to belt-and-brace it
  suggests the team didn't trust the guarantee). The Go
  comment at lines 99-107 lays out the design seam — a
  swappable `os.Stat` hook for tests — so adding the realpath
  layer is straightforward. Two new sibling-port tests
  (`TestRewriteRestoreConfigPaths_RejectsSymlinkOutsideAlloc`,
  `TestRewriteRestoreConfigPaths_AllowsSymlinkInsideAlloc`)
  pin the contract.

## Hunt disposition (terse)

1. **R19-API1 takeover error_message leak (from api-surface r19)**:
   the literal `"controller lessee abandoned this wake (R19-C1
   takeover sweep)"` at `db.rs:3303-3309` is rendered verbatim by
   `render_wake_poll_response` (`admin_handlers.rs:1856-1859`)
   into the §10.0 envelope's `message` field. **Surface is admin-
   only**: the route `/admin/sandboxes/{id}/wake/{wake_id}` is
   under `/admin/*` (`admin_handlers.rs:46`), gated by the
   admin-token middleware (`state.admin_token`). No tenant
   reaches this body. Severity: **MARGINAL info-disclosure, NOT
   a real PII/security issue**. The "lessee" term and review-ID
   leak are operator-facing copy hygiene (api-surface's framing
   is correct — minor doctrine-drift, not a security
   regression). Recommend api-surface's fix (move review ID to
   `target: "sandbox::wake::takeover"` warn; keep the body
   message client-facing) — but security would not block on it.
   No new security finding.
2. **R18-S1 SSRF parity (carry, IMPORTANT)**: `match_rfc1918_at`
   still covers 5 of ~10 IPv4 ranges that `runtime/src/
   transport/ssrf.rs::is_blocked_ip` blocks (loopback 127/8,
   0.0.0.0/8, doc-nets, IETF-protocol, benchmarking 198.18/15,
   reserved 240/4, IPv6-non-fe80). r18's recommendation stands:
   refactor to delegate (or add the missing arms). UNCHANGED.
3. **R19-I1 phase-1 as port-scanning oracle**: NO. The
   destination address is built from `cfg.nomad_ch.
   subnet_second_octet` (worker-config) + allocator-internal
   `vm_index` (`nomad_ch.rs:791-795`). No tenant input reaches
   `parse_agent_probe_addr`. The caller chain is admin-only
   (`POST /admin/sandboxes/{id}/wake` → backend `create()`).
   The probe's outer compio timeout caps a stuck SYN at 150 ms;
   loop budget is bounded by `agent_livez_timeout`. Even an
   admin "scan" via repeated wake POSTs would observe only
   the {alive, dead} bit on a single controller-derived
   RFC1918 literal per call — no useful oracle.
4. **C-7-LT-5 SetBinaries security verdict**: **NOT a security
   issue**. `c.chBin` (first arg) was always correct; only
   `c.chRemoteBin` (second arg) was aliased to virtiofsd. CH
   binary was never swapped. ch-remote is used only for
   `shutdown-vmm`/`resume`/`info` against a local API socket
   (`ch_client.go:241,273,316`); virtiofsd rejected the argv
   (functional bug), the SIGTERM fallback in `StopTask` handled
   termination. No privilege escalation, no sandbox escape, no
   trust-boundary crossing. Verdict: clean functional fix.
5. **R19-C1 takeover-sweep race vs in-flight wake**: NONE.
   Threshold floor 30 s exceeds the longest single wake-stage
   timeout (~30 s); a healthy in-flight wake's
   `lessee_updated_at` is bumped on every state transition
   (R17-A1) so it cannot drift older than threshold under
   normal progression. The non-terminal predicate inside the
   UPDATE WHERE clause means a peer that just claimed sees a
   no-op (state already `failed`); pg row-locks serialise.
6. **R13-S1 (carry, IMPORTANT)** —
   `provision-gcp-cluster.sh:286` still
   `--scopes=storage-rw,logging-write,monitoring-write` with no
   `--service-account=…`. UNCHANGED for ≥6 rounds.
7. **R9-S3 (carry, IMPORTANT)** — `snapshot_handler.rs:417`
   still stamps `Some("v1")` unconditionally. UNCHANGED for
   ≥10 rounds.
8. **R15-S3 (carry, MINOR)** — fence cap of 30 s in
   `gcp-worker-startup.sh:466-472` is still security-positive
   but undocumented. UNCHANGED.
9. **R17-S2 (carry, MINOR)** — no explicit `chown root:root`
   on the KEK file in `gcp-worker-startup.sh`. UNCHANGED.
10. **C-7-LT-1 async retry budget security review**: clean. The
   widened budget governs only the allocator's
   `reserve_vm_index` retry loop on the controller side
   (`restore_handler.rs`); no auth surface, no
   tenant-controllable input bound by the deadline. The
   `WakeResponseMode` enum is config-derived
   (`SANDBOX_WAKE_RESPONSE_MODE` env), not request-derived.

## Carry-forward open at HEAD `dea68995`

R13-S1 · R10-S1/S2 · R9-S2/S3 · R18-S1 · R12-S1 partial (all
IMPORTANT) · R14-S2 · R13-S2 · R11-S3 · R10-S3 · R9-S6/S7/S8 ·
R15-S3 · R17-S2 · R18-S2 (all MINOR). R9-S1 closed by R15-S2 +
C-7-LT-4 (subject to R19-S1). Closed since r18: none on the
security ledger.

## Counts

- CRITICAL: 0 new; carry: none (R9-S1 closed by R15-S2 + C-7-LT-4).
- IMPORTANT: 1 new (R19-S1); carry: R13-S1, R12-S1 partial,
  R10-S1, R10-S2, R9-S2, R9-S3, R18-S1.
- MINOR: 0 new; carry: R17-S2, R14-S2, R13-S2, R11-S3, R10-S3,
  R9-S6/S7/S8, R15-S3, R18-S2.
- Total NEW this round: 1.

## Cross-lens consensus

- **api-surface-r19 (R19-API1)**: security agrees the
  takeover-sweep error_message leak is operator-facing copy
  hygiene, NOT a security finding. The admin-only surface
  scopes the disclosure to a single trust level; "lessee"
  vocabulary and review-ID leakage are doctrine drift, not PII.
  api-surface's fix (move review ID to scoped warn; keep body
  client-facing) is the right path; security would not block
  on it but supports the cleanup.
- **concurrency-r19 (R19-C1 closure)**: takeover sweep is
  structurally sound. Threshold floor 30 s correctly
  exceeds the longest in-flight wake stage; pg row-locks
  serialise concurrent claims. Security has no objection to
  the design and confirms the `WakeWorkerAborted` variant
  carries no new auth surface.
- **architecture (driver-side C-7-LT-4 / C-7-LT-5)**: security
  confirms C-7-LT-5 is functional-only. C-7-LT-4's Go port
  is a real (documented) parity gap with the bash wrapper —
  R19-S1 above. Architecture should pin the realpath
  decision: either close the gap or document the alloc-dir
  hygiene assumption as a security-relevant operational
  invariant.

## Lens hand-off

- **Architecture / driver maintainers**: R19-S1 — extend
  `rewriteAndAssertUnderTaskDir` with `filepath.EvalSymlinks`
  (the design comment at `config_rewrite.go:99-107` already
  names the swappable `os.Stat` seam for tests). Alternative:
  document the alloc-dir-no-symlinks invariant as a
  security-relevant operational precondition.
- **Operator / doc**: R13-S1 + R9-S3 are the two oldest open
  IMPORTANT-tier finds (≥6 / ≥10 rounds). R13-S1 remains the
  highest-impact pre-GCP-rollout item: default-SA + `storage-rw`
  = single-VM-compromise → cross-tenant bucket read.
- **api-surface r20**: R19-API1 cleanup (review-ID out of
  body) is the cheapest open lane. Security supports but does
  not block.
- **Carry-forward**: R18-S1, R17-S2, R18-S2 remain open;
  none escalate at r19. R18-S1's refactor-to-`is_blocked_ip`
  remains the cleanest closure.
