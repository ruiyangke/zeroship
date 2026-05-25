# Sandbox/snapshot-restore — security r27 review

Date: 2026-05-25 (UTC)
HEAD at audit: `01288c18` (`d0d7abd6` per brief; latest landings since
are R26-I1 type dedup + cluster-r4 review artifacts — no security
delta). Lens: security (READ-ONLY).
Predecessor: r26 at `3d431eb8`. Landings since r26 (8 commits):

- `883df7fe` — boot-time `fetch_local_nomad_node_id` (cached node_id;
  precursor; landed pre-r26 already, flagged latent in r26-S1)
- `9b623f44` — sandbox/nomad-ch: emit Constraints block pinning placement
  to staging worker (r3-A cold-boot half — **POST-r26**)
- `d258cd6b` — close R25-T4 + R24-A1 in deferred backlog
- `34b52cf1` — sandbox/sweep: tighten `HOST_DIR_GC_GRACE_SECS` 3600→600
- `901dfbf2` — sweep unit tests
- `d71f1a8c` — sandbox/restore-handler: emit Constraints block pinning
  placement to staging worker (r3-A restore-path half — **POST-r26**)
- `b562d3a1` — close r3-A in deferred backlog
- `2ead52c2` — `gcp-worker-startup.sh`: bump driver v14→v15 +
  controller v34→v35 + **R20-S3 driver SHA256 verify**
- `7647cd4d` — **R22-S1 closure**: widen `sanitize_error_message`
  to mask filesystem paths + typed-IDs (Mode A)
- `cb2836a0` — close R22-S1 in deferred backlog
- `d0d7abd6` — round-30 reviewer artifacts

## Summary

**r27 produces ZERO new CRITICALs and ONE new IMPORTANT.** The
landing-density spike since r26 closed THREE previously-open carries
but elevated **r26-S1 from LATENT to ACTIVE IMPORTANT** because the
r3-A Constraints emit (both halves) has now landed at
`nomad_ch.rs:2570-2578` and `restore_handler.rs:2693-2700`. The
companion guards r26 documented as required for promotion to remain
LATENT (`nomad_addr` loopback enforcement; node-id fingerprint
cross-check) have NOT landed. Per r26 lens hand-off: *"Re-engage
r26-S1 audit. Required gates: (a) `nomad_addr` loopback enforcement;
(b) node-fingerprint cross-check (`/v1/node/<node_id>` vs.
controller's own PRIVATE_IP). Escalate r26-S1 to IMPORTANT if either
gate is missing when emit lands."* Both are missing. Promoting.

Net carry status (vs r26):

- **R22-S1 (driver-msg propagation path-leak)** — **CLOSED at
  `7647cd4d`** (Mode A widening). `sanitize_error_message` now runs
  five passes: agent-urls → RFC1918 → IPv6-LL → filesystem-paths
  (whitelist `/var/zeroship/`, `/opt/nomad/`, `/etc/zeroship/`) →
  typed-IDs (`^[a-z]{3}_[A-Za-z0-9]{18,32}$` with word-boundary
  semantics). Path-bearing driver TaskEvent DisplayMessages
  propagating via `RestoreHandlerError::Backend(s)` are stripped at
  the single write site (`wake_machine.rs:165` before
  `update_wake_job_state`). Verified end-to-end (see Focal #1). Two
  follow-up surfaces below the IMPORTANT bar logged as r27-M1
  (whitelist gap: `/var/lib/zeroship/`, `/run/zeroship/`, operator-
  configurable `host_state_dir`) and r27-M2 (`extract_failed_task_
  event_msgs` UTF-8 truncation panic surface at
  `nomad_ch.rs:2896`).
- **R20-S3 (driver SHA256 verify)** — **CLOSED at `2ead52c2`**.
  `gcp-worker-startup.sh:187-199` adds `DRIVER_BINARY_SHA256` literal
  + `sha256sum -c` against `nomad-driver-ch.v15`; FATAL on mismatch
  per the R24-T1 pattern. Verified at HEAD (see Focal #3).
- **r26-S1 (r3-A node-affinity attack surface)** — **PROMOTED to
  ACTIVE IMPORTANT** per r26's pre-stated promotion criteria.
  Constraints emit landed; neither loopback-enforcement nor node-id
  fingerprint cross-check landed. **See [r27-S1] below.**
- **R25-S1 (controller-side preflight path-leak)** — CLOSED at r25
  (unchanged this round). The `RestoreHandlerError::StagingPreflight`
  typed variant has a path-free Display by construction; the Mode A
  widening is now a defense-in-depth pass over that closure.
- **R21-S1 (restore-path typed_id validation)** — UNCHANGED. DB
  CHECK regex remains the structural guard.
- **R19-S1 (driver `EvalSymlinks`)** — UNCHANGED. Cross-worktree
  driver-side carry; driver v14→v15 bump did not address.
- **R18-S1 (sanitize coverage)** — **CLOSED for the R22-S1 axis by
  Mode A widening**, but the underlying pattern (whitelist-by-prefix)
  has a one-leak-class-per-prefix shape. r27-M1 documents the
  prefix gap.
- **T1 admin_ro role boot guard** — VERIFIED CORRECT (carry from
  r26 Focal #4; subtle::ConstantTimeEq with equal-length-required
  semantics + no early-exit branch).

**Wire-surface focal checks** (refreshed since r26):

- **WakeErrorCode wire enum** — unchanged. `staging_path_missing`
  pg form / `staging_image_missing` wire form CHECK domain extended
  via migration `0013`. **Leak-safe.**
- **§10.0 envelope on `GET /admin/sandboxes/{id}/wake/{wake_id}`** —
  `render_wake_poll_response` at `admin_handlers.rs:1971-2019`. RO
  bearer-readable. `row.error_message` plumbed verbatim; the
  sanitization is at WRITE-time via `sanitize_error_message`
  composition (`wake_machine.rs:825-850`). Path-bearing driver
  TaskEvent DisplayMessages → `<redacted-path>` token; typed-IDs →
  `<redacted-typed-id>` token. **R22-S1 closure verified.**
- **`assert_distinct_admin_tokens` boot guard** — re-verified at
  `lib.rs:1359-1385`. No change. Constant-time correct.
- **r3-A precursor + emit** — Constraints block landed at
  `nomad_ch.rs:2570-2578` and `restore_handler.rs:2693-2700`. Boot-
  time `fetch_local_nomad_node_id` populates
  `AppState.local_nomad_node_id` (`lib.rs:670-698`). No loopback
  enforcement on `nomad_addr` (verified at `config.rs:418-472`); no
  node-id cross-check on `/v1/node/<node_id>`. **See r27-S1.**
- **`gcp-worker-startup.sh` driver SHA256 verify** —
  `DRIVER_BINARY_SHA256="2d5618adb82bfcdc5e098e2bfd4b22f4b7cf590e7c5b1f4f9a86304f57ce826c"`
  at line 187; FATAL-on-mismatch at lines 191-197; verify happens
  BEFORE the binary is executed at line 201
  (`--version` invocation). **R20-S3 CLOSED.**

## CRITICAL

None.

## IMPORTANT

### [r27-S1] r3-A node-affinity ACTIVE — Constraints emit landed without loopback enforcement or node-id fingerprint cross-check (promoted from r26-S1 LATENT)

- **Status**: **NEW IMPORTANT** (promotion from r26 LATENT). The two
  prerequisites r26 flagged ("required gates" for the threat model
  to be weaponised) are now satisfied:
  1. r3-A Constraints emit landed at `nomad_ch.rs:2570-2578`
     (`9b623f44`, cold-boot) and `restore_handler.rs:2693-2700`
     (`d71f1a8c`, restore-path). Every CREATE/RESTORE jobspec now
     emits `Constraints: [{ LTarget: "${node.unique.id}", Operand:
     "=", RTarget: <node_id> }]` pinning placement to the cached
     local node_id.
  2. Neither defensive guard r26 specified has landed:
     - `nomad_addr` loopback enforcement: `config.rs:418-472`
       (`NomadCHConfig::validate`) checks scheme (`http://` /
       `https://`) and trims trailing `/` but does NOT enforce the
       host part is loopback. Greppable: `nomad_addr.*127\.0\.0\.1`
       matches only literals in test fixtures and a doc-comment
       default at `config.rs:272`; no validation code path.
     - Node-id fingerprint cross-check: no code path queries
       `/v1/node/<node_id>` after `fetch_local_nomad_node_id` to
       confirm the returned node's `Node.HTTPAddr` belongs to the
       controller's own host. The cached `node_id` is taken on
       face value.

- **File**:
  - `crates/sandbox/src/backend/nomad_ch.rs:3144-3157`
    (`fetch_local_nomad_node_id` — `GET /v1/agent/self` against
    `nomad_addr`, no host validation)
  - `crates/sandbox/src/backend/nomad_ch.rs:2570-2578`
    (`build_create_nomad_job_json` — emits the Constraints block
    when `local_nomad_node_id.is_some()`)
  - `crates/sandbox/src/restore_handler.rs:2693-2700`
    (`build_restore_nomad_job_json` — same shape, restore path)
  - `crates/sandbox/src/lib.rs:670-698` (boot-time fetch; non-fatal
    demotion to WARN + counter; populates `AppState.local_nomad_
    node_id`)
  - `crates/sandbox/src/config.rs:418-472`
    (`NomadCHConfig::validate` — no loopback assertion on
    `nomad_addr`)

- **Quote + chain** (vector 1: tampered local Nomad agent):
  ```rust
  // nomad_ch.rs:3144-3157
  pub(crate) async fn fetch_local_nomad_node_id(
      nomad_addr: &str,
  ) -> Result<String, String> {
      let url = format!("{nomad_addr}/v1/agent/self");
      let resp = http_get_unsigned(&url, Duration::from_secs(5)).await?;
      if resp.status != 200 {
          return Err(format!(
              "GET {url} → status {}: {}",
              resp.status, resp.body.trim()
          ));
      }
      parse_nomad_agent_self_node_id(&resp.body)
  }
  ```
  Chain: a process on the worker host that has write access to a
  port in `nomad_addr` (default `http://127.0.0.1:4646`) — e.g.,
  a sidecar that escapes its sandbox, a debug-only proxy
  inadvertently left bound to 4646, OR a Nomad-plugin-side compromise
  that allows TaskEvent injection — can substitute the
  `/v1/agent/self` response. `parse_nomad_agent_self_node_id` accepts
  any non-empty string at `stats.client.node_id`. The cached value
  goes into every CREATE/RESTORE jobspec as the RTarget of a strict-
  equality constraint. Two outcomes:
  - **(a) Attacker-chosen ID matches no existing node**: Nomad's
    scheduler rejects the alloc as unplaceable; every create / wake
    fails with a placement-error. **Sandbox-create DoS for the entire
    cluster.** No tenant data leak; service-availability hit only.
  - **(b) Attacker-chosen ID matches a DIFFERENT existing node**:
    allocs schedule on the wrong node; the driver on THAT node's
    preflight stats `<host_state_dir>/<sandbox-id>/workspace.img`
    locally and ENOENTs (the controller staged the image on THIS
    host). **Same DoS shape but with a more confusing error
    surface — looks like a controller bug to operators, not a
    placement attack.**

- **Quote + chain** (vector 2: `NOMAD_ADDR` misconfig to remote):
  Operator (mis)points `SANDBOX_NOMAD_ADDR` at a peer worker's Nomad
  HTTP endpoint (e.g., a copy-paste error mixing
  `http://10.99.101.0:4646` with `http://10.99.102.0:4646`).
  `fetch_local_nomad_node_id` succeeds against that peer; the cached
  `node_id` is for THE OTHER WORKER. Every alloc pins to THAT
  worker. THIS controller staged the disk image locally; the alloc
  runs on the OTHER worker, whose host_state_dir has no
  workspace.img. Driver preflight fails identically to vector (b),
  but the misconfig is silent — operator sees per-alloc failures,
  not a probe failure at boot. **Operator-trust boundary, not
  adversary**; widens the blast radius of misconfig from "fail at
  probe" to "fail silently per-alloc, every alloc, no recovery
  surface short of restarting the controller against a corrected
  `nomad_addr`".

- **Why this matters more post-r3-A-emit**: pre-r3-A, the
  controller's cached `node_id` was unused; the failure mode was
  "scheduler picks any node, driver's host check fails on the wrong
  node" — recoverable by Nomad reschedule + retry. Post-r3-A, the
  Constraint pins the alloc to the attacker/misconfig node and
  Nomad's reschedule lands on the SAME wrong node. Reschedule
  doesn't recover. **The threat model now closes a loop that
  previously self-healed.**

- **What's exploitable**:
  - Vector (1a): cluster-wide create/wake DoS until controller
    restart against a non-tampered agent. Attack requires write
    access to `127.0.0.1:4646` on the worker host. R20-S3 closure
    raises this bar (a malicious driver binary can no longer land
    on the worker), but a *post-boot* compromise of the Nomad
    agent or any other root-runnable process on the worker still
    opens vector 1. Worker-host root remains the trust boundary
    (acceptable; the worker is the kernel).
  - Vector (1b): same DoS shape with more confusing error surface.
  - Vector (2): operator misconfig → silent per-alloc failure. UX
    regression vs. fail-at-probe.

- **Threat model bar**: vector (1) requires worker-host code-exec
  at root (or 4646 socket write) — high bar given R20-S3 closure.
  Vector (2) is operator-trust. Neither is tenant-exploitable.
  **However**: the brief explicitly asked for promotion to IMPORTANT
  post-emit-landing, and r26 lens hand-off pre-committed to the
  promotion if either guard was missing. Both are. **Promoting.**

- **Fix shape** (NOT prescribing; r26 already documented):
  - **Guard A** — `NomadCHConfig::validate` (`config.rs:425`) refuses
    to boot when the URL host is NOT `127.0.0.1` / `::1` /
    `localhost` / a unix-socket path. ~10 LOC; closes vector (2)
    cleanly. Mirrors the existing scheme check at lines 465-472.
  - **Guard B** — after successful `fetch_local_nomad_node_id`,
    `GET /v1/node/<node_id>` (Nomad's per-node API) and verify
    `Node.HTTPAddr` matches the controller's own PRIVATE_IP /
    hostname. Closes vector (1) modulo a fully-Nomad-compromised
    agent. ~30 LOC + an additional HTTP roundtrip at boot.
  - Both are defense-in-depth; Guard A is the cheaper / clearer
    win and stops the operator-trust vector unconditionally.

- **Severity**: **IMPORTANT** (admin-bearer-gated for the operator-
  trust vector; worker-host-root for the adversary vector; both
  cluster-wide DoS). Pre-cutover priority: alongside R13-S1
  (storage-rw scope) and any remaining v15 driver-side carries.

## MINOR

### [r27-M1] sanitize widening whitelist gaps: `/var/lib/zeroship/`, `/run/zeroship/`, operator-configurable `host_state_dir`

- **Status**: **NEW MINOR**. The R22-S1 closure at `7647cd4d` is
  whitelist-by-prefix on three roots (`/var/zeroship/`,
  `/opt/nomad/`, `/etc/zeroship/`). Other roots the codebase
  writes/reads from are NOT in the strip set:
  - `/var/lib/zeroship/ch/` — `runtime_dir` default (`config.rs:644`).
    Carries `vmlinuz` (`restore_handler.rs:4094, :2625`),
    `rootfs-slim.img` (`:4119, :2635`). The kernel path emitted to the
    driver (`nomad_ch.rs:2490, :2498`) and to the restore-path
    builder (`restore_handler.rs:2640`) lives under this root. If
    the driver returns `kernel not found: /var/lib/zeroship/ch/
    vmlinuz`, the host path **leaks verbatim** through R22-S1's
    closed wire.
  - `/var/lib/zeroship/sandbox/` — `persist_dir` default
    (`db.rs:1166`, `persist.rs:632`). Sealed-record material lives
    here. Less exposed to driver TaskEvents (controller-internal
    only), but `RestoreHandlerError::Internal("post-wake unseal
    sandbox {sandbox_id}: {e}")` (`restore_handler.rs:1132-1136`)
    plumbs `Persistence::unseal` errors that may carry this prefix
    on filesystem IO failures.
  - `/run/zeroship/ch/` — CH api-socket directory referenced in
    `handlers.rs:1315-1327` (test fixture). If a runtime ever emits
    a CH-socket-bearing error message into the wake-machine's
    `RestoreHandlerError::Backend(s)`, the `/run/zeroship/` prefix
    is not stripped.
  - **Operator-configurable `host_state_dir`** — defaults to
    `/var/zeroship/ch` (covered) but `SANDBOX_NOMAD_CH_HOST_STATE_DIR`
    accepts any absolute path (`config.rs:741-755`). An operator
    that re-roots to `/srv/zeroship/ch` or `/data/sandbox/` for any
    reason (different fs mount, distro convention) escapes the
    whitelist. Mode A's whitelist is correct for the *default*
    configuration; operator-configurability widens the surface
    silently.

- **File**:
  - `crates/sandbox/src/wake_machine.rs:1099`
    (`strip_filesystem_paths::ROOTS` constant — three prefixes)
  - `crates/sandbox/src/config.rs:644, :741-755, :1282`
    (`runtime_dir` / `host_state_dir` defaults + env var override)
  - `crates/sandbox/src/restore_handler.rs:1132-1136`
    (`unseal` error string carrying potential persist-dir path)
  - `crates/sandbox/src/wake_machine.rs:328-344` (Internal
    variants format alloc_dir — host_state_dir-rooted; covered when
    operator uses default, leaks when not)

- **Why MINOR not IMPORTANT**:
  - The default deployment uses `/var/zeroship/` which IS covered.
  - `/var/lib/zeroship/` paths in driver-side TaskEvents are
    operator-discoverable from the `gcp-worker-startup.sh` script
    (`ART=/etc/zeroship`, `mkdir -p /var/lib/zeroship/ch …`) which is
    in-repo and public — not a true secret.
  - The threat model that R22-S1 closure addressed (RO bearer
    enumerating tenant topology via driver path leaks) is closed
    against the *tenant-identifying* segments (typed-ID strip
    handles the `<sandbox-id>` / `<user-id>` suffix even on a
    non-whitelisted root path). A leaked `/var/lib/zeroship/ch/
    vmlinuz` carries no tenant identity.

- **Fix shape** (NOT prescribing):
  - **Mode A1** — extend `ROOTS` to include `/var/lib/zeroship/` and
    `/run/zeroship/`. ~2 LOC + 2 tests.
  - **Mode A2** — replace the whitelist with a generalised
    "absolute path with ≥3 segments containing `zeroship` OR
    `nomad`" matcher. Catches operator-relocated roots; risks more
    false positives (operator-debuggable error text could include
    `/usr/lib/zeroship-tools/` which would be redacted unnecessarily).
  - **Mode A3** — read the actual `runtime_dir` / `host_state_dir`
    / `persist_dir` values from `AppState` at sanitize time, and
    redact based on the *runtime configuration* rather than a
    static list. Closes operator-relocation case; threads
    `AppState` into the sanitize call site (currently a free fn).
    Highest-leverage but largest mechanism cost.

- **Severity**: **MINOR** — known whitelist gap; no tenant data
  leak under default config; operator-relocation case is operator-
  controllable and operator-discoverable. Re-classify if a follow-up
  surface emerges in cluster runs.

### [r27-M2] `extract_failed_task_event_msgs` byte-indexed truncation can panic on UTF-8 multibyte boundary

- **Status**: **NEW MINOR** — latent panic surface, not a leak.
- **File**: `crates/sandbox/src/backend/nomad_ch.rs:2890-2901`
- **Quote**:
  ```rust
  if let Some(trimmed) = picked {
      const PER_TASK_CAP: usize = 2048;
      let bounded: String = if trimmed.len() > PER_TASK_CAP {
          format!("{}…(truncated)", &trimmed[..PER_TASK_CAP])
          //                       ^^^^^^^^^^^^^^^^^^^^^^^^^
          //                       PANIC if 2048 is mid-char
      } else {
          trimmed.to_string()
      };
      out.push(format!("{task_name}: {bounded}"));
  }
  ```
- **Threat model**: the driver TaskEvent `DisplayMessage` is JSON-
  parsed via `serde_json` so the input is guaranteed valid UTF-8.
  However, byte-indexed slicing at a fixed offset (2048) is not
  guaranteed to fall on a char boundary. If any multibyte char's
  bytes straddle offset 2048, `&trimmed[..2048]` panics with
  `byte index 2048 is not a char boundary`. Driver-controlled input
  + admin-bearer-readable failure path = a worker that emits a
  carefully-shaped DisplayMessage can panic the wake-machine task
  → 500 to the polling RO admin → eventual takeover-sweep
  reclamation. **Local DoS**; bounded by the wake-takeover sweep.
- **Compare**: `sanitize_error_message`'s own truncation at
  `wake_machine.rs:844-849` does char-boundary handling correctly:
  ```rust
  let mut end = ERROR_MESSAGE_MAX_BYTES;
  while end > 0 && !s.is_char_boundary(end) {
      end -= 1;
  }
  s[..end].to_string()
  ```
  The driver-msg cap path predates the sanitize widening and was
  missed by the Mode A pass.
- **Fix shape**: replicate the `is_char_boundary` walk-back loop
  from `sanitize_error_message:844-849`. ~5 LOC; identical pattern.
- **Severity**: **MINOR** — driver-side input control is admin-
  trust-equivalent; wake-machine takeover-sweep recovers. Mirrors
  the pattern fix in sanitize but on a different code path.

### [r27-M3] `agent_url` (RFC1918 IP) in terminal=ok wake-poll response — operator-trust topology disclosure (pre-existing carry, re-noted)

- **Status**: pre-existing; not new. Re-flagging because the R22-S1
  closure shifted the leak surface from `error_message` (write-time
  sanitized) to `agent_url` (NOT sanitized, NOT in `sanitize_error_
  message`'s call path).
- **File**: `crates/sandbox/src/admin_handlers.rs:1988`
  (`render_wake_poll_response` terminal=ok arm — renders
  `row.agent_url` verbatim into the response body).
- **Quote**:
  ```rust
  WakeJobState::Ok => HttpResponse::Ok().json(&serde_json::json!({
      "state": "ok",
      "wake_id": row.wake_id,
      "sandbox_id": row.sandbox_id,
      "ready_at": row.ready_at_secs,
      "agent_url": row.agent_url,  // ← RFC1918 IP, RO-readable
  })),
  ```
- **Threat model**: RO bearer polling a successful wake gets the
  worker's internal IP + agent port. The IP belongs to the CHWBL-
  routed 10.99.{100+idx}.2 range (`restore_handler.rs:2392-2395`),
  which is private and not externally routable; the IP is the
  derived form `http://10.99.<100+idx>.2:7777`. Enumeration ladder:
  successful wakes for sandboxes the RO bearer has wake_ids for
  enumerate the `vm_index` allocation. **Operator-trust shape** —
  admin bearers are expected to see internal topology.
- **Why this matters**: the operative threat model for the wake-poll
  endpoint pre-R22-S1 was "leak nothing about internal topology to
  RO bearers". R22-S1 closure achieved that for `error_message`; the
  `agent_url` field still carries internal IP. The closure is
  consistent if we declare "agent_url is a contract field, not a
  leak"; inconsistent if we declare "RO bearers should learn
  nothing operator-internal from a terminal=ok response". Process-
  side resolution; not advocating a code change without that policy
  call.
- **Severity**: **MINOR** — operator-trust boundary; not new this
  round.

### [r27-M4] `artifact_path` (host filesystem path) in snapshot success response — Full bearer only, but path-bearing wire surface

- **Status**: pre-existing; not new. Re-flagging because R22-S1
  closure clarified the policy that host paths should not cross
  RO bearer surfaces; this surface is Full-bearer-gated but still
  emits a host path on the wire.
- **File**: `crates/sandbox/src/admin_handlers.rs:1515`
  (`snapshot_sandbox` success arm).
- **Quote**:
  ```rust
  "snapshot": {
      "artifact_path": o.metadata.artifact_path,  // ← host fs path
      "sha256_hex": hex::encode(o.metadata.sha256),
      "ch_version": o.metadata.ch_version,
      "bytes": o.metadata.bytes,
  },
  ```
- **Why MINOR**: handler requires `AdminRole::Full` (`admin_handlers.
  rs:1361`); RO bearers cannot reach it. Full bearer effectively has
  worker-host access (cold-boot / wake control). Treating
  `artifact_path` as a Full-only field is consistent with the
  existing Full / RO split.
- **Compare**: the wake-poll terminal=ok response (r27-M3) leaks
  the agent_url IP to RO. Snapshot-success leaks the artifact_path
  to Full. Different audiences, different surfaces.
- **Severity**: **MINOR** — Full-bearer-gated; not a leak per the
  current Full role's threat model.

### [r27-M5] No CI enforcement of r24-A3 ADR (carry from r26-S2)

- **Status**: **CARRY**. Unchanged since r26. The ADR's "verbatim
  observable before defense" rule is policy-only; not machine-
  enforced. Greppable in `.github/workflows/ci.yml`: no `verbatim`
  / `playbook` / `observability` / `sanitize_error_message`
  reference.
- **Why this matters**: the R22-S1 Mode A closure at `7647cd4d`
  passes the r24-A3 test (operator observability preserved via
  `tracing::warn!` at `wake_machine.rs:166-172`; only the pg
  column is sanitized; journald keeps the verbatim). The future-
  regression concern remains: a contributor adding a stripping
  pass to `extract_failed_task_event_msgs` (the source-of-truth for
  the verbatim msg) WOULD violate the ADR; CI wouldn't catch it.
- **Severity**: **MINOR (POLICY)** — process-side; out of scope
  for security.

## Focal-list checks (per r27 brief)

### Focal #1 — R22-S1 closure verification: sanitize_error_message at HEAD

**Status**: **CLOSED** for the documented threat model. Mode A
widening adds two passes:

1. `strip_filesystem_paths` (`wake_machine.rs:1098-1138`):
   - Whitelist: `/var/zeroship/`, `/opt/nomad/`, `/etc/zeroship/`
   - Greedy body consumption up to whitespace / quote / `,` / `;` /
     `)` / `]`
   - Replaces with `<redacted-path>`
2. `strip_typed_ids` (`wake_machine.rs:1153-1196`):
   - Pattern: `^[a-z]{3}_[A-Za-z0-9]{18,32}$` with word-boundary
     on both ends (non-alnum / non-underscore lookahead/lookbehind)
   - Replaces with `<redacted-typed-id>`

Composition order (`wake_machine.rs:834-838`): URL → RFC1918 →
IPv6-LL → fs-paths → typed-IDs. Paths-before-typed-IDs is correct:
embedded typed-id segments inside `/var/zeroship/ch/<sbx-id>/…` are
consumed as part of the path body, avoiding double-redaction.

**Path-prefix list audit** (per brief — "Are the 3 path prefixes …
sufficient? Any sibling leak surfaces?"):

| Path root | Coverage | Notes |
|---|---|---|
| `/var/zeroship/` | ✅ | Default `host_state_dir`, `workspace_root`, snapshot L1 root, key material |
| `/opt/nomad/` | ✅ | Nomad client `data_dir` — alloc dirs, task secrets |
| `/etc/zeroship/` | ✅ | Controller config, TLS material, wrapper path, nomad-driver-ch plugin path |
| `/var/lib/zeroship/` | ❌ | `runtime_dir` default — vmlinuz, rootfs-slim.img source; persist_dir for sealed records |
| `/run/zeroship/` | ❌ | CH api-socket dir referenced in handlers.rs:1315 |
| `/var/log/` | n/a | Operator-debug logs; not a tenant-topology surface |
| `/run/secrets/` | n/a | Not used by sandbox code; k8s-specific (`backend/k8s.rs:1177`) |
| `/home/...` | n/a | Not used; userhome is `<host_state_dir>/users/<usr>/home.img` (under /var/zeroship — covered) |
| Operator override | ❌ | `SANDBOX_NOMAD_CH_HOST_STATE_DIR` accepts any abs path; default-only coverage |

**Recommendation**: r27-M1 documents the `/var/lib/zeroship/` +
`/run/zeroship/` + operator-override gaps. Below IMPORTANT bar
because (a) default config is covered, (b) tenant-id stripping
catches the per-sandbox segments even on non-whitelisted roots,
(c) the in-repo gcp-worker-startup.sh exposes the default paths
publicly anyway. **r27 acknowledges the gap and pins fix-shape in
r27-M1.**

**Typed-ID prefix audit** (per brief — "Are there typed-id prefixes
(NOT in the masked list) that could leak?"):

Canonical list in `crates/core/src/typed_id.rs:152-154`: `usr_`,
`app_`, `ses_`. Sandbox crate uses: `sbx_` (handlers.rs:106),
`wak_` (wake_id), `evt_` (events). Preview-share handlers add
`tok_` (preview_share_handlers.rs:274) and `shr_` (lines 305, 331).

Mode A's `strip_typed_ids` matches the generic shape
`^[a-z]{3}_[A-Za-z0-9]{18,32}$` (not a hard-coded prefix list), so
all of these match the canonical 22-char base62 case. **However**:

- `tok_` / `shr_` use **base64url** (`-` / `_` allowed), not base62.
  Their raw `tid` claim may include `-` or `_` characters. The
  Mode A matcher only consumes `[A-Za-z0-9]` for the trailing run,
  so a base64url tid containing `-` would terminate the match
  prematurely:
  - `shr_abc-DEF_GHI` → matches `shr_abc`, but only if `abc` is in
    the 18-32 range (it's not — 3 chars), so no redaction fires.
  - In practice, preview-share token_ids don't appear in
    `wake_jobs.error_message` (different code paths), so this is
    a non-leak.
- `evt_` event_ids ARE base62 per `crates/sandbox/src/db.rs:3442`
  validation (`parse_with_prefix(event_id, "evt")`). If an event
  table query error surfaces in wake-machine error_message, the
  evt_ prefix matches Mode A's shape. Good.
- Hyphenated UUID form (`f6d8d847-d9d5-7e5b-...`): used in
  `restore_handler.rs:1134` (`unseal sandbox {sandbox_id}` with
  `sandbox_id: Uuid` whose Display is hyphenated). This is **NOT**
  caught by `strip_typed_ids` (no `xxx_` prefix). Mode A leaves
  raw UUIDs in error messages intact. **Real leak surface**:
  enumeration via hyphenated-UUID form is functionally equivalent
  to typed-id enumeration. Logged as part of r27-M1 (whitelist
  gap analysis is the symmetric fix; alternative is to add a
  separate `strip_hyphenated_uuid` pass).

**No-typed-ID-substring guard**: Mode A's word-boundary check
(`bytes[i-1]` non-alnum-non-underscore; `bytes[j]` non-alnum-non-
underscore-OR-EOS) correctly excludes:
- `_sbx_<base62>` (leading underscore — fails boundary at `i`)
- `Xsbx_<base62>` (leading alphanumeric — fails boundary at `i`)
- `sbx_<base62>X` (trailing alphanumeric — extends the run, fails
  length bound or trailing-boundary check)
- `sbx_<base62>_X` (trailing underscore — fails trailing-boundary)

**Conclusion**: R22-S1 CLOSED for the documented threat model
(driver-msg propagation → `wake_jobs.error_message` → RO-bearer
read). Whitelist gaps (r27-M1) and hyphenated-UUID gap (folded
into r27-M1) are below the IMPORTANT bar; tenant-topology
enumeration via repeat polling stays bounded because the typed-
id stripping fires on most tenant-identifying segments.

### Focal #2 — R20-S3 closure verification: driver SHA256 verify

**Status**: **CLOSED** at `2ead52c2`. `gcp-worker-startup.sh:177-202`:

```bash
if [ "$INSTALL_CH_PLUGIN_DRIVER" = "1" ]; then
  echo "[startup] INSTALL_CH_PLUGIN_DRIVER=1 — installing nomad-driver-ch"
  mkdir -p /etc/zeroship/nomad-plugins
  DRIVER_BINARY_SHA256="2d5618adb82bfcdc5e098e2bfd4b22f4b7cf590e7c5b1f4f9a86304f57ce826c"
  gs_pull nomad-driver-ch.v15 /etc/zeroship/nomad-plugins/nomad-driver-ch 0755
  chown root:root /etc/zeroship/nomad-plugins/nomad-driver-ch
  got=$(sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch | awk '{print $1}')
  if [ "$got" != "$DRIVER_BINARY_SHA256" ]; then
    echo "[startup] FATAL: nomad-driver-ch SHA mismatch" >&2
    echo "[startup]   expected: $DRIVER_BINARY_SHA256" >&2
    echo "[startup]   got:      $got" >&2
    echo "[startup]   GCS object: gs://$ARTIFACT_BUCKET/nomad-driver-ch.v15" >&2
    echo "[startup]   Rebuild via nomad-driver-ch/scripts/build-binary.sh --verify and re-upload." >&2
    exit 1
  fi
  echo "[startup] nomad-driver-ch SHA OK ($DRIVER_BINARY_SHA256)"
  /etc/zeroship/nomad-plugins/nomad-driver-ch --version || true
  …
fi
```

**Sequence audit**:
1. `gs_pull` downloads the binary (no integrity check on the helper
   itself — same as before)
2. `chown root:root` (cosmetic; root was the writer)
3. `sha256sum` computes the on-disk hash
4. Comparison against the literal `DRIVER_BINARY_SHA256`
5. FATAL + `exit 1` on mismatch (worker refuses to start)
6. **Binary execution at line 201** happens AFTER the SHA check —
   any tampering between download and check is caught before exec

**TOCTOU window**: between `chown` (line 189) and `sha256sum`
(line 190), a process with write to `/etc/zeroship/nomad-plugins/
nomad-driver-ch` could swap the file. The path's directory is
created with default permissions at line 179 (root-owned, no
explicit chmod — inherits from process umask, typically 0o022 →
mode 0o755). At boot, the only writers are root processes. The
TOCTOU window is bounded by:
- Boot phase: no tenant code is running yet
- Network: SSH / metadata-server inbound is policy-gated by R13-S1
- Local: any local process with root has already won

**Acceptable** as a defense-in-depth boundary. R20-S3 CLOSED.

**Pattern consistency check**: matches the R24-T1
`SNAPSHOT_STRESS_SHA256` pattern at lines 235-264. Same fail-mode,
same error message shape, same "in-repo source + GCS mirror + SHA
pin move together" maintenance contract. **No drift.**

### Focal #3 — r26-S1 promotion: r3-A node-affinity surface activation

**Status**: **PROMOTED** to IMPORTANT [r27-S1] above. Details:

- Emit landings: `9b623f44` (cold-boot, `nomad_ch.rs:2570-2578`),
  `d71f1a8c` (restore-path, `restore_handler.rs:2693-2700`). Both
  emit the same `Constraints: [{ LTarget: "${node.unique.id}",
  Operand: "=", RTarget: <node_id> }]` shape.
- `fetch_local_nomad_node_id` at `nomad_ch.rs:3144-3157` — NO host
  validation; accepts any reachable Nomad agent on `nomad_addr`.
- `NomadCHConfig::validate` at `config.rs:418-472` — NO loopback
  check; only scheme (`http://` / `https://`) + trailing-slash
  trim.
- No node-id cross-check site — greppable: `/v1/node/` matches only
  doc-comments in `nomad_ch.rs:3076-3079`; no live RPC.

Both attack vectors (tampered local agent / `NOMAD_ADDR` misconfig)
are live. See r27-S1 for full triage.

### Focal #4 — R22-S1 follow-up: do other code paths besides wake_jobs.error_message also leak paths?

**Status**: **MIXED**. Auditing the operator-visible (admin-bearer-
readable) surfaces:

1. **`render_wake_poll_response` terminal=ok arm**
   (`admin_handlers.rs:1983-1989`) — emits `row.agent_url` verbatim.
   This is an RFC1918 IP (not a filesystem path); operator-trust
   topology shape. **Not a path leak; logged as r27-M3.**
2. **`render_wake_poll_response` terminal=failed arm**
   (`admin_handlers.rs:1996-2007`) — emits `row.error_message` after
   sanitization at write-time. **Covered by R22-S1 closure.**
3. **`snapshot_sandbox` success arm**
   (`admin_handlers.rs:1515`) — emits `o.metadata.artifact_path`.
   Full-bearer only; logged as r27-M4.
4. **`err_safe` wrapper** (`admin_handlers.rs:328-345`) — all 500
   errors funnel through this; raw error → `tracing::error!` (only
   journald) + fixed public message on the wire. **Path-safe.**
5. **`tracing::warn!` / `tracing::info!` lines in admin_handlers**
   (12 sites; greppable):
   - All use `error = %e` / `error = %err` shape → routes to
     journald (operator-only), NOT the wire response body.
   - The brief asked specifically about "admin_handlers logging at
     INFO/DEBUG level" — there are NO `tracing::info!` or
     `tracing::debug!` lines in admin_handlers (zero matches on
     `tracing::(info|debug)!`). Only `warn!` and `error!`, both
     of which route through journald with the same operator-only
     scope.
6. **`render_wake_poll_response` intermediate-state arm**
   (`admin_handlers.rs:1972-1979`) — no error_message field;
   no leak.
7. **`row.lessee` / `row.lessee_updated_at_secs`** — `lessee` is a
   host_id (typed-id form? no — it's the database's `host_id()`
   value, which is set by the controller boot). NOT emitted by
   `render_wake_poll_response`. **Not on the wire.**

**Conclusion**: the R22-S1 closure is complete for the documented
operator-visible surfaces. The only "still-leaks" shapes (r27-M3
agent_url, r27-M4 artifact_path) are operator-trust contract
fields, not exfiltration channels.

### Focal #5 — AdminRole RO bearer post-R22-S1: filesystem path freedom verified

**Status**: **VERIFIED**. RO bearer's wake-poll surface chain:

1. `poll_wake` at `admin_handlers.rs:1882-1956` calls
   `admin_check_required(&req, &state, AdminRole::ReadOnly)` at
   line 1888 — accepts RO or Full.
2. Reads `wake_jobs` row via
   `db.get_wake_job(wake_uuid)` — pg SELECT.
3. Calls `render_wake_poll_response(&row)` at line 1955.
4. For terminal=failed (the worst-case path-leak surface):
   - `row.error_message` was written at
     `wake_machine.rs:175-181` with `sanitized.as_str()` (line 179);
     `sanitized` is the post-Mode-A output of
     `sanitize_error_message(message)` (line 165).
   - Mode A applies five passes including `strip_filesystem_paths`
     (whitelist roots) and `strip_typed_ids`.
   - **No fs path under the three whitelist roots reaches the wire.**
   - **No canonical typed-id (`xxx_<22 base62>`) reaches the wire.**

The only fs paths an RO bearer can still see in `error_message`
post-R22-S1 are:
- Non-whitelist roots (`/var/lib/zeroship/...`, `/run/zeroship/...`,
  operator-overridden `host_state_dir` outside the default) — see
  r27-M1.
- Hyphenated UUID form (`f6d8...-...-...`) — see r27-M1.
- Non-typed-id ASCII identifiers (e.g., a raw Nomad alloc_id —
  these are 36-char hex with hyphens; do NOT match Mode A's
  base62 pattern). Alloc-id leak is operator-trust shape; the IDs
  carry no tenant identity by themselves.

**Hand-off**: closure is structural for the leak axes R22-S1
identified. Residual surfaces are tenant-identity-free.

### Focal #6 — r26-A1 ADR (BackendFailureDetail) — pre-emptive defensive design call

**Status**: **NOT LANDED**. Code-quality r26 deferred this; greppable
in the entire worktree, `BackendFailureDetail` returns 0 matches in
the Rust source tree. Only documentation references in
`docs/reviews/sandbox-snapshot-restore-{api-surface,code-quality,
architecture}-2026-05-25-r25.md` and `…-r26.md`.

**Defensive design observation** (per brief): IF `BackendFailureDetail`
lands as a typed struct with fields like:
```rust
pub struct BackendFailureDetail {
    pub kind: BackendFailureKind,
    pub host_path: Option<PathBuf>,
    pub alloc_id: Option<String>,
    pub task_event_msg: Option<String>,
}
```
and IF the `Display` impl interpolates `host_path` into the rendered
form, AND IF the wake-machine error_message column receives the
`Display` output:
- The current Mode A pipeline (`sanitize_error_message`) WILL still
  catch host paths if and only if they fall under one of the three
  whitelist roots.
- The current pipeline will catch typed-IDs in the canonical shape.
- **Structured fields rendered to JSON instead of free text would
  bypass `sanitize_error_message` entirely**, since the
  sanitization is applied to the `error_message` TEXT column write
  but NOT to any structured side-channel (e.g., a hypothetical
  `error_detail JSONB` column).

**Recommendation for the eventual landing PR**:
- Either render `BackendFailureDetail` through `Display` →
  `sanitize_error_message` (current shape works; whitelist gap
  per r27-M1 applies)
- Or add the sanitize pipeline to any new structured-field
  serialization site (`render_wake_poll_response` extension,
  the new `error_detail JSONB`, etc.)

**Severity classification**: nothing to flag now (the type doesn't
exist). The defensive design recommendation lands in the lens
hand-off below.

## Carry-forward open at HEAD `01288c18`

| ID         | Sev       | File:line                                                                                  | Status at r27                                                                                          |
|------------|-----------|--------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------|
| **r27-S1** | **IMPORTANT (NEW)** | `nomad_ch.rs:2570-2578, :3144-3157`; `restore_handler.rs:2693-2700`; `lib.rs:670-698`; `config.rs:418-472` | **NEW — promoted from r26-S1 LATENT.** r3-A Constraints emit landed without loopback enforcement or node-id cross-check. Cluster-wide DoS via tampered local agent or `NOMAD_ADDR` misconfig. |
| **r27-M1** | **MINOR (NEW)** | `wake_machine.rs:1099`; `config.rs:741-755`; `restore_handler.rs:1132-1136` | **NEW — whitelist gap.** `strip_filesystem_paths` covers default config; misses `/var/lib/zeroship/`, `/run/zeroship/`, operator-overridden `host_state_dir`, raw hyphenated UUID. |
| **r27-M2** | **MINOR (NEW)** | `nomad_ch.rs:2890-2901` | **NEW — UTF-8 panic surface.** `&trimmed[..PER_TASK_CAP]` byte-indexes without char-boundary check; driver-controlled input can panic the wake-machine task. |
| **r27-M3** | **MINOR (carry)** | `admin_handlers.rs:1988` | OPEN — wake-poll terminal=ok emits agent_url (RFC1918 IP) to RO bearers. Operator-trust shape; flagged as policy-call. |
| **r27-M4** | **MINOR (carry)** | `admin_handlers.rs:1515` | OPEN — snapshot success emits artifact_path. Full-bearer-only; consistent with Full role's threat model. |
| r26-S2 / **r27-M5** | **POLICY (carry)** | `docs/decisions/2026-05-25-restore-debug-playbook.md`; `.github/workflows/ci.yml` | OPEN — ADR not CI-enforced. Process-side. |
| R22-S1     | (closed)  | n/a                                                                                        | **CLOSED at `7647cd4d`** — Mode A sanitize widening (filesystem paths + typed-IDs). Mode A1 follow-up for r27-M1 whitelist gap is incremental. |
| R20-S3     | (closed)  | n/a                                                                                        | **CLOSED at `2ead52c2`** — `DRIVER_BINARY_SHA256` pin + FATAL-on-mismatch in `gcp-worker-startup.sh:187-199` (R24-T1 pattern). |
| R25-S1     | (closed)  | n/a                                                                                        | CLOSED at r25; carry status unchanged. |
| R21-S1     | IMPORTANT | `restore_handler.rs:2256-2259, :2368`                                                       | OPEN — DB CHECK regex remains the structural guard.                                                    |
| R21-S2     | IMPORTANT | (driver-side, cross-worktree)                                                              | OPEN — driver validator-call symmetry not audited.                                                     |
| R20-S2     | IMPORTANT | (driver-side, cross-worktree)                                                              | OPEN — driver `cfg.SandboxId` validator not audited.                                                   |
| R19-S1     | IMPORTANT | (driver-side, cross-worktree); controller emit at `restore_handler.rs:2398, :2406`         | OPEN — driver `filepath.Clean`-only; v14→v15 bump did not address.                                     |
| R18-S1     | IMPORTANT | `wake_machine.rs:825-850` (code)                                                             | **PARTIAL — R22-S1 axis CLOSED; whitelist gap r27-M1.**                                                |
| R13-S1     | IMPORTANT | `crates/sandbox/scripts/provision-gcp-cluster.sh:286`                                       | OPEN — `--scopes=storage-rw` + default GCE SA unchanged. ≥13 rounds open.                              |
| R9-S3      | IMPORTANT | `crates/sandbox/src/snapshot_handler.rs:417`                                                | OPEN — `Some("v1")` stamp regardless of AEAD posture.                                                  |
| R20-S1     | MINOR     | (config)                                                                                   | OPEN — `ContentAddressedRootfsRoots` slot empty.                                                       |
| R15-S3     | MINOR     | (config / docs)                                                                            | OPEN — 30s fence cap undocumented.                                                                     |
| R17-S2     | MINOR     | (KEK provisioning)                                                                         | OPEN — no explicit `chown root:root`.                                                                  |
| R18-S2     | MINOR     | (logging)                                                                                  | OPEN — fence-error IP leak into scoped log.                                                            |
| R24-M1     | MINOR     | `wake_machine.rs:740-748` (doc-comment)                                                    | OPEN — doc-comment understates `match_rfc1918_at` coverage.                                            |
| R24-M2     | MINOR     | `restore_handler.rs:2352, :2371`                                                            | OPEN — operator-trust footprint widened by one driver-Config path field.                               |

## Cross-lens consensus

- **r3-A landing (architecture / api-surface r26)**: the Constraints
  emit is the architecturally correct fix for cross-node placement
  (T-8b-stress-r3's 78% failure rate). Security adds r27-S1 — the
  emit needs companion guards. **No conflict with architecture
  lens**; r27-S1 is a pre-shipped-feature defense layer.
- **R22-S1 closure** (security r26 → r27): the Mode A widening at
  `7647cd4d` lands cleanly with operator-observability preserved
  (`tracing::warn!` carries verbatim; pg column carries sanitized).
  **r24-A3 ADR compliance verified** at `wake_machine.rs:166-172`.
- **cluster T-8b-stress-r4** (RED, 3/60 e2e OK per `01288c18`): the
  failing chain no longer exercises the cross-node placement gap
  (post r3-A), but the residual 57/60 RED is downstream of the
  placement fix. Per the cluster review's "node-affinity validation"
  framing, the Constraints emit IS routing correctly — the
  remaining failures are different shapes (likely host_dir GC race
  + driver-side preflight per r24-A3 ADR).
- **code-quality r26 / api-surface r25**: BackendFailureDetail
  deferred. r27 Focal #6 documents the defensive design rec for
  the eventual landing.

## Lens hand-off

- **Sandbox controller (Rust)** — **r27-S1 is the highest-leverage
  fix this round**. Guard A (`nomad_addr` loopback enforcement in
  `NomadCHConfig::validate`) is ~10 LOC; closes the operator-
  trust vector (2) unconditionally. Guard B (node-id cross-check
  via `/v1/node/<node_id>`) is ~30 LOC + one HTTP roundtrip at
  boot; closes adversary vector (1) modulo full-Nomad compromise.
  **Pre-cutover blocker.**

  Mode A1 follow-up for r27-M1 (extend `ROOTS` to include
  `/var/lib/zeroship/` + `/run/zeroship/`) is ~5 LOC + 2 tests;
  closes the whitelist gap for the default + common-deployment
  case. Lower priority than r27-S1.

  r27-M2 (UTF-8 char-boundary cap in
  `extract_failed_task_event_msgs`) is ~5 LOC; mirrors the existing
  pattern at `sanitize_error_message:844-849`. Pre-existing latent
  panic surface, not a leak; bottom of the priority list.

- **Ops / cluster bring-up** — **R13-S1 (`--scopes=storage-rw`)
  remains the sole pre-cutover Ops blocker** now that R20-S3 has
  CLOSED. The post-R20-S3 trust boundary is "every worker can
  rewrite `nomad-driver-ch.v15` for the next worker's boot"; the
  SHA-pin makes that rewrite *detectable* (worker refuses to start)
  but not *prevented*. Hardening R13-S1 (drop write scope; sign
  binaries with a cosign key whose pub-half ships in the boot
  image) is the next layer.

- **nomad-driver-ch maintainers (cross-worktree)** — v15 landed.
  Driver-side carries R19-S1 / R20-S2 / R21-S2 not addressed in the
  v14→v15 bump. Next v16 cut is the right window for the
  `EvalSymlinks` upgrade + `isTypedID(cfg.SandboxId/UserId)` +
  validator-call symmetry. r27 does not propose driver changes per
  brief.

- **Forward-looking (BackendFailureDetail ADR)** — when this lands,
  the `Display` → `sanitize_error_message` path MUST be the
  rendering site for the wake-machine error_message column write.
  Structured side-channels (e.g., `error_detail JSONB`) need
  their own sanitization application or a typed-by-construction
  path-free shape (the `StagingPreflight` typed variant is the
  template). r27 Focal #6 pins the design call.

- **CI (r27-M5 / r26-S2)** — unchanged. Process-side; out of scope
  for security.

## Counts

- CRITICAL: 0 new; carry: 0.
- IMPORTANT: 1 new (**r27-S1** r3-A node-affinity ACTIVE; promoted
  from r26-S1 LATENT). Carries: R21-S1, R21-S2, R20-S2, R19-S1,
  R18-S1 (partial), R13-S1, R9-S3.
- MINOR: 2 new (**r27-M1** sanitize whitelist gap; **r27-M2**
  UTF-8 truncation panic). Re-flagged carries (r27-M3 agent_url
  RFC1918, r27-M4 artifact_path Full-only). Policy carry: r27-M5
  (r26-S2 CI not enforcing r24-A3).
- Closed this round: **R22-S1** (Mode A sanitize widening at
  `7647cd4d`), **R20-S3** (driver SHA256 verify at `2ead52c2`).
- LATENT-IMPORTANT: 0 (r26-S1 promoted).
- Total NEW this round: 1 IMPORTANT, 2 MINOR.
- **Cutover gate**: **R13-S1 (storage-rw scope)** is now the SOLE
  pre-cutover Ops blocker (R20-S3 closed). **r27-S1 (r3-A
  hardening)** is the controller-side pre-cutover blocker — Guard A
  is cheap and clearly worth the LOC. r27-M1 (whitelist gap) and
  r27-M2 (UTF-8 panic) are post-cutover acceptable.
