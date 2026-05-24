# Sandbox/snapshot-restore — security r18 review

Date: 2026-05-25 (UTC)
HEAD at audit: `b8654600` (per task brief; tree HEAD is `406a366a`,
one docs-only commit ahead). Catchup r18 — security lens was 2
rounds behind at r17.
Scope: R17-S1 sanitizer extension; C-7-LT-2 PR1+PR2; R18-I1
fixture-only; smoke-r12 KEK hot-patch; R19-A1 cross-lens; GATE-C2
cross-tenant existence; r17 carry-forward. Read-only.

## Summary

**R17-S1 CLOSED** at `3c75a8ce`: `match_rfc1918_at` extends to
169.254/16 and 100.64/10 with explicit 64..=127 second-octet gate;
helper doc updated to enumerate all five prefixes; 6 new tests pin
both CGNAT edges and two negative pins (100.63.x.y, 100.128.x.y).
Sanitizer is now policy-aligned with `runtime/src/transport/ssrf.rs`
on the two prefixes r17 flagged.

**C-7-LT-2 PR1+PR2** introduce **no new security gaps**. The
`probe_agent_reachable_tcp` connect-only probe targets a
controller-derived, worker-internal RFC1918 address
(`http://10.<cfg>.<100+vm_index>.2:7777`) with no tenant input —
MITM is N/A on a worker-local subnet. The new
`sandbox_vm_index_leaks_total{reason}` counter is `&'static
str`-typed with two valid labels and an unknown-label fold; cardinality
is bounded. The new `sandbox::teardown::leak` log target carries
`vm_index`, `sandbox_id`, `job_id`, `reason`, `error` — no
user_id/project_id/token leakage.

**Smoke-r12 KEK hot-patch (`f9a9c5f0`)**: mode 0o400 ✓, /dev/urandom
(non-blocking) ✓, exactly 32 bytes ✓, idempotent via `[ ! -s … ]` ✓.
Owner remains implicit-root (R17-S2 carry — startup script header
says "Runs as root"; zsbx-ctl unit has no `User=`). No explicit
`chown root:root` added — R17-S2 finding posture is unchanged.

**Carry-forward**: R13-S1 (`--scopes=storage-rw` no
`--service-account` at `provision-gcp-cluster.sh:286`),
R9-S3 (`Some("v1")` hard-coded at `snapshot_handler.rs:417`),
R17-S2 (KEK implicit-root owner) all UNCHANGED.

**New findings**: 2 (one IMPORTANT — sanitizer parity gap with the
larger SSRF blocklist; one MINOR — leak-log echoes fence-error text
that contains the worker-internal IP).

## Findings (NEW since r17)

### [R18-S1] Sanitizer covers only 5 of the ~10 SSRF-blocked IPv4 ranges (IMPORTANT)

- **Files**: `crates/sandbox/src/wake_machine.rs:737-779`
  (the updated `match_rfc1918_at` prefix table) vs
  `crates/runtime/src/transport/ssrf.rs:42-55` (the sibling
  blocklist).
- **Symptom**: r17-S1 added 169.254/16 and 100.64/10 to bring two of
  the five major gaps to parity. The sanitizer still does NOT cover
  five other ranges the SSRF guard treats as private/blocked:
  - **127/8** loopback (`v4.is_loopback()`)
  - **0.0.0.0/8** "this network" (`octets[0]==0`)
  - **192.0.2/24, 198.51.100/24, 203.0.113/24** documentation
    (`v4.is_documentation()`)
  - **192.0.0/24** IETF protocol assignments
  - **198.18/15** benchmarking (`(octets[1] & 0xFE) == 18`)
  - **240/4** reserved + 255.255.255.255 broadcast
    (`v4.octets()[0] >= 240`)
  - All IPv6 except `fe80::/10` (the SSRF guard also blocks
    `::1`, `::`, `ff00::/8`, `fc00::/7` ULA, `::ffff:0:0/96`
    v4-mapped, `2001:db8::/32` doc, `2001:2::/48` benchmark).
- **Threat model**: the wake-error sanitizer's job is to redact
  worker-internal topology before the message lands in a column
  SELECT-able by `sandbox_app` (R16-S1 revert). The most realistic
  leaks from `restore_handler` / `backend.*` failure paths are:
  - **127.0.0.1:<port>** — local-loopback connect errors from
    same-host services (e.g. agent-on-loopback in dev,
    controller-internal probes). These land verbatim today.
  - **fc00::/7** ULA — some k8s/Nomad CNI plugins use ULA addresses
    for service IPs.
  - **240/4** — reserved space sometimes used in test fixtures.
  The 169.254/100.64 additions in r17-S1 closed the highest-value
  gaps (IMDS, AWS CGNAT), but loopback in particular is the most
  common shape for any "connect to local service failed" error.
- **Why IMPORTANT-not-MINOR**: the doctrine is now explicit — the
  sanitizer is meant to match the SSRF blocklist (the r17 review
  pinned this explicitly via reference to
  `ssrf.rs:45-51`). Shipping a strict subset of `ssrf.rs`'s
  blocklist after r17's premise was "match the sibling guard"
  perpetuates a doctrine drift that future contributors are likely
  to extend by reflex (a third partial sync). Single source of
  truth would close this permanently.
- **Action**: refactor `match_rfc1918_at` (or rename the module's
  helper to `match_private_or_reserved_ipv4_at`) to either (a)
  consume the same `is_blocked_ip` predicate via a small "parse +
  classify" wrapper, or (b) add the missing arms — `127.`, `0.`,
  `192.0.2.`, `198.51.100.`, `203.0.113.`, `192.0.0.`,
  `198.(18|19).`, `240.` through `255.`. Mirror the SSRF guard for
  IPv6 in the existing IPv6-literal pass at `wake_machine.rs:706+`
  (today only `fe80::/10`). Two new sibling-guard tests (loopback
  redacted; one ipv6-ULA redacted) pin the parity contract.

### [R18-S2] Host-fence error string echoed into leak log contains worker-internal IP (MINOR posture)

- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:1180-1185`
  (the `host_fence_timeout` leak log) +
  `wait_for_agent_silent:3364-3369` (the format that bakes
  `agent_url` into the error string).
- **Symptom**: when host_fence times out, `stop_inner` logs
  `error = %fence_err.as_deref()` (line 1183) where `fence_err`
  contains the agent base_url verbatim — e.g.
  `"agent at http://10.99.105.2:7777 still answering at fence
  deadline (probes=300, last_http_status=None,
  consecutive_misses=0); leaking vm_index to avoid handing out a
  live IP"`. The base_url is a worker-internal RFC1918 literal
  (10.X.Y.2 by construction; not tenant-influenced), so this is
  **topology data, not PII**. But the new
  `target: "sandbox::teardown::leak"` scope means an operator
  shipping just this log target to a central aggregator (the
  intended use per the PR2 commit message) will also ship those
  internal subnet IPs.
- **Why MINOR**: (a) it's worker-cluster topology, not tenant
  data; (b) the operator who configured `RUST_LOG=
  sandbox::teardown::leak=warn` is by definition same-org as the
  worker; (c) the receiving aggregator already sees the worker's
  outbound IP. Practically, this only matters if the leak log is
  ever forwarded to a less-trusted consumer (third-party APM,
  customer-visible debug surface).
- **Action**: drop a comment at line 1183 noting "error includes
  worker-internal subnet IP; do not forward this log target
  off-cluster as-is", OR pass the *sanitized* fence error to the
  log line. The sanitizer (`sanitize_error_message`) is in the
  same crate and now (post-R17-S1) redacts 10/8. A one-line
  `error = %sanitize_error_message(fence_err.as_deref().unwrap_or("<unknown>"))`
  would make the log target safe to forward.

## Hunt disposition (terse)

1. **C-7-LT-2 probe MITM?** No. `probe_agent_reachable_tcp` is a
   pure `TcpStream::connect(addr)` to a `SocketAddr` derived from a
   controller-built URL of shape `http://10.<subnet_second_octet>.<100+vm_index>.2:7777`
   (`nomad_ch.rs:791-795`). The `subnet_second_octet` comes from
   worker config; `vm_index` is allocator-internal. No tenant input
   reaches `parse_agent_probe_addr`. The dst address is
   worker-local (10/8 by construction). MITM is structurally N/A
   on a TAP-routed worker-internal subnet.
2. **C-7-LT-2 leak counter cardinality?** Bounded.
   `inc_vm_index_leak(reason: &'static str)` matches on two
   compile-time-known labels; unknowns fold into the
   `host_fence_timeout` bucket with a WARN. Prometheus cardinality
   ≤ 2. Test `vm_index_leak_counter_per_reason_monotonic` pins
   the unknown-fold contract.
3. **`sandbox::teardown::leak` log PII?** No tenant data
   (user_id/project_id/token). `sandbox_id` (typed-id), `vm_index`
   (small int), `job_id` (Nomad allocator id), `reason` (static).
   `error` field carries worker-internal IP — see R18-S2.
4. **KEK provisioning verification**: `f9a9c5f0`'s
   `gcp-worker-startup.sh:431-439` — mode 0o400 ✓, /dev/urandom
   (non-blocking) ✓, exactly 32 bytes via `head -c 32` ✓,
   idempotent via `[ ! -s "$ROOT_KEK_PATH" ]` ✓. Startup script
   runs as root (per script header line 6: "Runs as root on first
   boot"), so EUID at write is root. `zsbx-ctl.service` unit
   block (lines 462+) has no `User=`, so the controller process
   reads the file as root. **R17-S2 finding posture is unchanged**
   — no explicit `chown root:root` was added, ownership remains
   implicit. The R17-S2 action item (defensive `chown root:root`
   + doc comment) has not been addressed.
5. **R19-A1 leaked-vm_index recovery vs FM-F race?** The leak
   semantics are structurally safe. `release(sandbox.vm_index)` is
   ONLY called inside `if fence_passed { … }` at
   `nomad_ch.rs:1134-1143`. On fence-timeout the vm_index is NEVER
   added to `freed`, so `VmIndexAllocator::alloc()` cannot return
   it. A new tenant CANNOT bind to the leaked IP within the same
   controller process. At next controller boot
   `cleanup_orphans_at_startup` purges orphan Nomad jobs before
   the allocator hands out any index — provided
   `startup_orphan_cleanup=true` (the default per
   `nomad_ch.rs:432`). If an operator sets this to false, a
   re-issue after process restart could race a still-running
   orphan agent — but that's a config-disabled-the-guard footgun,
   not a runtime bug. The architecture-r19 R19-A1 "recovery is a
   comment" is a capacity loss (the index stays leaked across
   process restarts only if orphan-cleanup is disabled), not a
   security incident — leak-without-reuse is the fail-CLOSED
   posture.
6. **GATE-C2 cross-tenant existence inference?** N/A. The admin
   API (`admin_handlers.rs:144-159` `admin_check`) is
   single-trust-level — there is no per-tenant authz at the
   wake/poll layer. An admin caller is already trusted to specify
   any `sandbox_id`. The partial UNIQUE INDEX
   `wake_jobs_sandbox_pending_uniq` (`0011_wake_jobs_unique.sql`)
   exposes existence inference *within the admin role*, which has
   no per-tenant scope; the inference is meaningless. r17's verdict
   carries — no new probe surface.
7. **R13-S1 (carry, IMPORTANT)** —
   `provision-gcp-cluster.sh:286` still
   `--scopes=storage-rw,logging-write,monitoring-write` with no
   `--service-account=…`. UNCHANGED.
8. **R9-S3 (carry, IMPORTANT)** — `snapshot_handler.rs:417` still
   stamps `Some("v1")` unconditionally. UNCHANGED.
9. **R17-S2 (carry, MINOR)** — no `chown root:root` in
   `f9a9c5f0`; no doc comment about implicit-root in the unit.
   UNCHANGED.
10. **R18-I1 (`531db5c3`)** — fixture-only; no security surface.
   Skip.

## Carry-forward open at HEAD `b8654600`

R13-S1 (IMPORTANT) · R12-S1 partial (IMPORTANT) ·
R10-S1/S2 (IMPORTANT) · R9-S2/S3 (IMPORTANT) ·
R14-S2 · R13-S2 · R11-S3 · R10-S3 · R9-S6/S7/S8 · R15-S3 ·
R17-S2 (all MINOR). R9-S1 partially closed by R15-S2.

Closed since r17: R17-S1 (`3c75a8ce`).

## Counts

- CRITICAL: 0 new; carry: R9-S1 partial.
- IMPORTANT: 1 new (R18-S1); carry: R13-S1, R12-S1 partial,
  R10-S1, R10-S2, R9-S2, R9-S3.
- MINOR: 1 new (R18-S2); carry: R17-S2, R14-S2, R13-S2, R11-S3,
  R10-S3, R9-S6/S7/S8, R15-S3.
- Total NEW this round: 2.

## Cross-lens consensus

- **architecture-r19 (R19-A1)**: leaked-vm_index recovery being
  comment-only is a CAPACITY concern, not a security concern.
  Security posture is fail-CLOSED — the leaked index stays out of
  the free list, so no tenant-collision race exists within a
  process lifetime. If arch-r19 wants a sweep, security would
  prefer that sweep gate on a fresh fence-pass per index (re-probe
  the leaked TAP, only return to `freed` if connect-refused twice
  in a row) — i.e. the sweep itself must use
  `wait_for_agent_silent`'s contract, not a wall-clock delay.
- **concurrency**: R18-S1 is the same parity gap the r17 cross-lens
  noted (concurrency-r18 M1). Promoting to a single source of
  truth (helper that delegates to `is_blocked_ip`) closes the
  doctrine drift permanently.
- **test-cov**: R18-S1 closes with 2 new sanitize tests
  (loopback `127.0.0.1`, doc-net `192.0.2.0`); R18-S2 has no
  test surface (log-format-only).
- **api-surface**: no new admin/wake API surface; `agent_url`
  CHECK regex (R16-S3) still tight.

## Lens hand-off

- **Architecture / impl**: R18-S1 — refactor
  `match_rfc1918_at` to delegate to (or test-parity with)
  `runtime/src/transport/ssrf.rs::is_blocked_ip`. Same-crate
  helper is fine if a workspace dep would be awkward. Land with
  ≥2 sibling-guard regression tests.
- **Operator / doc**: R18-S2 — either sanitize the `error =` field
  in the leak log warn at `nomad_ch.rs:1183` (one-line; uses the
  in-crate `sanitize_error_message`), OR add a comment on the
  same line noting the log target carries internal-subnet IPs
  and should not be forwarded off-cluster as-is.
- **Carry-forward**: R13-S1 (storage-rw scope) + R9-S3 ("v1"
  hard-code) remain open; both are IMPORTANT-tier and unresolved
  for ≥4 rounds. Recommend prioritising R13-S1 if any production
  rollout to GCP is imminent — the default-SA + storage-rw combo
  is a single-VM-compromise → cross-tenant-bucket-read path.
