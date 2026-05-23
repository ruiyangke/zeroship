# Sandbox/snapshot-restore — security r9 review

Date: 2026-05-25 (UTC)
HEAD at audit: `10bddc20` (working tree). Prompt referenced
`e7b3278b`; branch has since advanced by 1 commit (`10bddc20`
"R8-API1 full" — `init_sandbox_id_from_env` `pub`→`pub(crate)`),
so the audit covers both states.
Round 9 of N (security lens). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/scripts/**`.

## Summary

8 findings (1 critical, 4 important, 3 minor). 5 r8 findings closed
by recent commits (W1, A1, R5-S5, R8-DEPLOY1 deployment gap,
R8-API1 — both halves now); 4 r8 findings carry forward (F2 /livez,
R5-S4 restore-internal body excerpt, T2 admin rate-limit, B-AGENT-A
`boot_sandbox_id` `Option`). The W1 Python rewrite is sound for the
controller-trusted NOMAD_TASK_DIR input but introduces a NEW
path-traversal surface against bucket-write attackers (S1) because
`config.json` remains AEAD-unwrapped on L2. AEAD timestamp
granularity (1 s) creates a nonce-reuse hazard on same-second
re-snapshots (S2). The `snapshot_aead_dek_id="v1"` stamp is hard-
coded regardless of whether AEAD is actually active, decoupling pg
truth from disk truth (S4).

## Findings

### [R9-S1] AEAD does not wrap `config.json`; Python rewrite faithfully propagates attacker-controlled paths (CRITICAL, security-r9)
- **Files**: `crates/sandbox/src/snapshot_aead.rs:28-35` (scope
  comment "Only the dominant `memory-ranges` file"); wrapper
  `crates/sandbox/scripts/nomad-vm-wrapper.sh:476-498` (Python
  `ALLOC_PREFIX` regex + `rewrite()` helper).
- **Symptom**: W1's structural fix moved from unanchored sed to a
  JSON-aware anchored-prefix Python rewrite — sound vs. the
  shell-metacharacter injection r2/r3/r4/r5/r6/r7/r8 chased. But
  the Python script's `rewrite()` (`:481-498`) preserves verbatim
  the suffix after the matched `…/local(/|$)` boundary —
  `task_dir + m.group(1) + value[m.end():]`. Any `..` segments,
  symlink-poison paths, or non-canonical Unix path syntax inside
  the snapshot's `config.json` survive the rewrite and land in
  CH's per-restore configuration unmodified. Because
  `snapshot_aead.rs:28-35` exempts `config.json` from AEAD wrap
  ("encrypting it would force a re-encrypt on every restore"), a
  bucket-write attacker on L2 can mutate `config.json` between
  `put` and `get` with arbitrary `disks[].path`, `serial.file`,
  or `console.file` values. The Python prefix-match only catches
  values matching `^/opt/nomad/data/alloc/[^/]+/[^/]+/local(/|$)`;
  anything else passes through untouched.
- **Threat model**: GCS bucket-write principal (compromised
  worker SA, leaked GCS HMAC key, or misconfigured IAM that
  granted write on `snapshots/v1/<sid>/`). The attacker
  substitutes `state.json`-paired `config.json` so that on the
  next wake, CH (running as raw_exec root) opens an attacker-
  chosen host file as a guest disk → guest can read arbitrary
  host bytes — and on virtio-blk this includes the rootfs
  template's signing key persistence layer. `serial.file`
  pointed at a host file means raw_exec's CH writes guest
  console output over that path.
- **Why it matters**: AEAD on `memory-ranges` defends the
  dominant byte-volume secret (guest RAM) but leaves the
  smallest-but-most-load-bearing artifact (`config.json` ≈ 2.4
  KB) unauthenticated. The canonical SHA-256 (`x-goog-meta-
  zsbx-canonical-sha256` on `state.json`, see
  `snapshot_store_gcs.rs:85-92`) covers the artifact's bytes,
  but `verify_metadata_only` ITSELF reads the same metadata the
  attacker controls (`snapshot_store_gcs.rs:705-711` explicitly
  acknowledges: "A bucket-write attacker can substitute both the
  body and any metadata they control"). The deep `verify` at
  `get`-time (`snapshot_store.rs:277-283`) catches the
  substitution — but ONLY if the controller doesn't also accept
  the corresponding bucket-substituted `expected_sha256` from
  pg (which it can't directly here, but a separate pg-write
  attacker chain compounds).
- **Action**:
  (a) Extend the AEAD wrap to `config.json` and `state.json`
      both — re-encrypt-on-restore is acceptable for files this
      small (≤ 110 KB combined). The proposal's § 4.3 trust
      chain explicitly leaves `state.json` unwrapped citing "no
      tenant-secret material" but `config.json`'s threat is
      structural (path injection), not confidentiality.
  (b) Alternately: add a controller-side per-field allow-list
      on rewritten paths in `restore_handler::rewrite_config_json`
      (extending `:642-680`) that rejects any non-canonical path
      segment (`..`, symlink, world-writable parent) BEFORE
      handing to the wrapper. The wrapper-side Python is the
      LAST line of defense; the FIRST should be controller-side
      authentication of the JSON structure.
  (c) At minimum, document the gap in the AEAD module's docstring
      so the next reviewer knows the boundary.

### [R9-S2] AEAD DEK derivation has 1-second timestamp granularity → nonce reuse on same-second re-snapshot (IMPORTANT, security-r9)
- **Files**: `crates/sandbox/src/snapshot_aead.rs:592-598`
  (`snapshot_taken_at = SystemTime::now().as_secs()`),
  `:270-282` (`derive_dek` — salt is `sandbox_id ||
  snapshot_taken_at_be_u64`), `:293-299` (`derive_nonce_prefix`
  — deterministic on DEK).
- **Symptom**: The DEK is determined by `(sandbox_id,
  snapshot_taken_at_unix_secs)` and the nonce-prefix is a
  deterministic function of the DEK. Two snapshots of the SAME
  sandbox within the SAME wall-clock second produce identical
  DEK + identical nonce-prefix + identical AAD per chunk index.
  ChaCha20-Poly1305 with nonce reuse is catastrophic: an
  attacker who recovers both ciphertexts can XOR them to obtain
  the XOR of two plaintexts, leaking guest RAM contents.
- **Threat model**: Triggering two same-second snapshots of the
  same sandbox is hard but not impossible — the snapshot
  pause+dump itself takes ~50 s (per `snapshot_handler.rs:340-
  379`), but a quick failure-retry pattern (controller crashes
  mid-snapshot, restarts, retries within the same wall-clock
  second window of the second snapshot's effective start) plus
  NTP-induced clock rewind (e.g., a non-monotonic correction)
  can collapse two starts into the same `as_secs()`. The
  `dek_rotates_with_snapshot_taken_at` unit test at `:942-951`
  asserts ts=1_000_000 vs 1_000_001 differ but does not pin the
  same-ts behavior; the put path will happily produce the
  same DEK + nonce-prefix.
- **Why it matters**: The module docstring (`:13-22`) claims
  "re-snapshot bumps `snapshot_taken_at` so the only way to
  repeat a nonce is to repeat (sandbox_id, snapshot_taken_at,
  chunk-counter) which the put-side can never do" — that's
  false at second granularity. The proposal's choice to use
  ChaCha20-Poly1305 (instead of AES-256-GCM-SIV) was justified
  by "deterministic nonce schedule" — which holds only if the
  timestamp source is strictly monotonic per-(sandbox_id,
  snapshot operation). It isn't.
- **Action**:
  (a) Add `snapshot_taken_at_unix_nanos` (or `_micros`) to the
      DEK salt and AEAD header to push the collision window
      well below realistic retry intervals. Header is currently
      32 bytes; bumping `taken_at` from u64 BE to u128 BE adds
      8 bytes (the `reserved u16` at `:65` plus a header
      version bump for back-compat).
  (b) OR: read the pg-side `snapshot_taken_at` (`now()` clock
      from pg) BEFORE the wrap so two same-host-clock retries
      can't collide. pg `now()` advances per-statement, not
      per-second; even a same-millisecond retry sees a
      different value.
  (c) OR (proposal's stated v2 path): swap to AES-256-GCM-SIV
      for nonce-misuse resistance. The cipher tag byte at
      `:120` already supports the discriminator.

### [R9-S3] `snapshot_aead_dek_id` always stamped as `"v1"` regardless of AEAD state — pg metadata diverges from artifact truth (IMPORTANT, security-r9)
- **Files**: `crates/sandbox/src/snapshot_handler.rs:417`
  (`Some("v1")` hard-coded passed to
  `update_snapshot_metadata`); `snapshot_aead.rs:611-615`
  (suffix `+aead-cc20p1305` appended to `ch_version` ONLY when
  `self.root.is_some()`).
- **Symptom**: When AEAD is in passthrough mode
  (`SANDBOX_SNAPSHOT_ROOT_KEK_PATH` unset → guest RAM written
  to L1/L2 in clear), the put path still records
  `snapshot_aead_dek_id="v1"` in pg. The only on-the-wire
  signal that AEAD is active is the `+aead-cc20p1305` suffix in
  `ch_version`. An operator querying "which snapshots are
  encrypted?" via pg `WHERE snapshot_aead_dek_id IS NOT NULL`
  gets every snapshot, including the plaintext ones.
- **Threat model**: Operator forensics / incident response. The
  schema field carries the SEMANTIC promise "this DEK ID
  identifies the key used to encrypt the artifact" but the
  contract is broken in passthrough mode. An incident
  responder restoring confidence after a leaked-disk event
  would mis-categorize plaintext artifacts as encrypted and
  fail to flag them for re-encryption.
- **Why it matters**: A1 (commit `18e2034b`) closed the
  fail-OPEN shape that earlier rounds flagged (the prod wrap
  is now unconditional). But the metadata stamp wasn't
  threaded through — the snapshot handler hard-codes "v1"
  because it doesn't know the inner store's AEAD posture. The
  R8 finding #2 explicitly named this: "stamps
  `snapshot_aead_dek_id="v1"` while guest RAM ships to GCS in
  plaintext." That half is still open.
- **Action**:
  (a) Pass `aead_dek_id` from `AeadSnapshotStore::is_active()`
      through `SnapshotMetadata` — extend the struct with
      `aead_dek_id: Option<&'static str>` and have the AEAD
      layer set it to `Some("v1")` only when wrapping. The
      handler then reads `meta.aead_dek_id` instead of
      hard-coding.
  (b) Alternately: drop `Some("v1")` to `None` at the handler
      and rely on the `ch_version` suffix as the canonical
      indicator. This is the smaller blast radius but loses
      structured-query convenience.

### [R9-S4] AEAD root KEK file: mode 0o400 enforced but owner uid not checked (IMPORTANT, security-r9)
- **Files**: `crates/sandbox/src/snapshot_aead.rs:179-203`
  (`RootKek::from_path`); cf. `crates/sandbox/src/persist.rs:
  326-360` (`AeadKey::from_path` — same shape).
- **Symptom**: The KEK loader checks `mode & 0o777 == 0o400`
  but does NOT verify `meta.uid() == geteuid()`. A file with
  mode 0o400 owned by a different unprivileged user is
  unreadable to the controller (read fails with EACCES → load
  fails loud), so the practical attack surface is limited.
  But the controller commonly runs as root (per the wrapper's
  `set_caps`/`CAP_NET_ADMIN` requirement at
  `scripts/nomad-vm-wrapper.sh:247`), and a root controller
  can read ANY mode-0o400 file regardless of owner. A
  misconfigured KEK_PATH pointing at e.g. `/proc/<pid>/auxv`
  or a file controlled by a less-trusted local user would
  load 32 bytes of attacker-influenced material as the KEK
  without warning.
- **Threat model**: Misconfiguration / typo / config-rendering
  bug pointing the env var at the wrong path. The 32-byte
  length check is necessary but not sufficient — `/dev/zero`
  truncated to 32 bytes via `head -c 32` would load
  successfully and silently downgrade the KEK to all-zero.
- **Why it matters**: A1's "wrap unconditionally + visible
  posture" hardening assumes the loaded KEK is the
  operator-intended material. Adding an `meta.uid() ==
  geteuid()` check, or refusing to load when uid != root and
  the running process is root (defense in depth), would close
  the typo class.
- **Action**: After the mode check, verify
  `meta.uid() == nix::unistd::geteuid().as_raw()` (or the
  equivalent libc path). Same guard belongs on
  `AeadKey::from_path` for symmetry.

### [R9-S5] Restore-branch `ZSBX_SANDBOX_ID` flow asymmetric: cold-boot validates `[0-9a-zA-Z_]`; restore branch logs informationally only (IMPORTANT, security-r9)
- **Files**: `crates/sandbox/scripts/nomad-vm-wrapper.sh:221-
  226` (cold-boot strict validator); `:394-396` (restore
  branch — just `echo "informational"`); restore-side env
  payload at `crates/sandbox/src/restore_handler.rs:1275-
  1305` — does NOT emit `ZSBX_SANDBOX_ID` at all.
- **Symptom**: The wrapper's cold-boot path hard-validates
  `ZSBX_SANDBOX_ID` chars before embedding in the kernel
  cmdline (`SANDBOX_AGENT_SANDBOX_ID=...`). On the restore
  branch the wrapper accepts the env if set (logs
  informationally) and skips both validation and injection
  because the agent's `SANDBOX_ID` `OnceLock` is preserved
  inside the snapshot's memory image. The asymmetry is fine
  IF the snapshot was always taken by an R7-S1-or-later agent
  (which BIND the OnceLock at cold-boot). But the wrapper has
  NO way to verify this property of the snapshot — and the
  controller's restore-job builder
  (`restore_handler.rs:1275-1305`) doesn't pass
  `ZSBX_SANDBOX_ID` even when it knows it. A snapshot taken
  on a pre-R7-S1 agent would restore an agent with an unbound
  OnceLock → every `/_clock_resync` 500s
  (`handlers.rs:755-769`) → every cluster wake fails.
- **Threat model**: Mostly an operational footgun, but with a
  security flavor: a pre-R7-S1 snapshot replayed against a
  post-R7-S1 controller produces an agent that REJECTS every
  resync. The controller falls back to "no clock skew"
  signaling, and a captured-pre-R7-S1-resync replayed against
  the new agent will pass the controller's body-hash gate
  (because the new agent's OnceLock is unset → the
  `sandbox_id` field comparison in `handlers.rs:770-782`
  fails → 401, NOT a skew-bypass). So the agent fails closed
  — good. But the restore wedges silently for a non-security
  reason and the wrapper's "informational" log hides it.
- **Why it matters**: The agent fails closed (correct), but
  the SYMMETRIC fix — controller emits `ZSBX_SANDBOX_ID` on
  the restore branch too, and the wrapper applies the same
  validator + kernel-cmdline injection — would close two
  classes at once: (a) pre-R7-S1 snapshots are auto-bound to
  the controller's known id on wake; (b) the wrapper's strict
  char validator runs on BOTH branches, so a future code-path
  that overrides the snapshot's preserved OnceLock has a
  defense-in-depth gate.
- **Action**: Add `"ZSBX_SANDBOX_ID": sandbox_id_simple` to
  `build_restore_nomad_job_json` (`restore_handler.rs:1275-
  1305`), mirroring the cold-boot payload at
  `nomad_ch.rs:2288`. Extend the wrapper's restore branch to
  run the same `[!0-9a-zA-Z_]` validator as cold-boot.

### [R9-S6] Restore-internal `/_clock_resync` error path still embeds 256 chars of agent body verbatim (MINOR, security-r9, r8 finding #6 carry)
- **Files**: `crates/sandbox/src/restore_handler.rs:1598-1601`
  + `:1605-1608` — `body_excerpt.chars().take(256).collect()`.
- **Symptom**: When agent returns non-200 to `/_clock_resync`,
  the controller embeds 256 chars of the response body
  verbatim into the `RestoreHandlerError::Backend` string.
  R8's audit confirmed this propagates upward through the
  admin handler's `err_safe("restore_backend_failed",
  "restore backend error", s)` mapping, so the WIRE body is
  sanitized ("restore backend error" only). But the raw is
  logged via `tracing::warn!(..., error = %e, ...)` in
  `admin_handlers.rs:1397-1401` ("admin/wake: handler
  failed").
- **Threat model**: Same as R5-S4. Agent body in journald is
  visible to anyone with log access. Per agent's A4 envelope
  (`handlers.rs:201-244`), error responses are
  `{"error":"...","message":"..."}` — bounded shapes, no
  raw driver text. Body excerpt is mostly safe today, but
  the 256-char window is wide enough to carry e.g. an
  internal IP from agent-side `derive_agent_url` mirror calls
  or path fragments from `verify_signed`'s audit-log line.
- **Why it matters**: r8 flagged this as MINOR; no commit
  since. The fix is a one-line `err_safe` wrap (or just don't
  embed the body excerpt — agent's status code + audit log
  is enough for diagnosis).
- **Action**: At `restore_handler.rs:1599-1608` replace
  `body_excerpt` with a fixed message; rely on the agent-side
  audit log for diagnostic detail.

### [R9-S7] `/livez` + `/readyz` + `/metrics` unauthenticated; reaper-health and drain state leak to anyone with port reach (MINOR, security-r9, F2 carry)
- **Files**: `crates/sandbox-agent/src/main.rs:171-178` +
  `handlers.rs:468-485` (livez/readyz) + `:489-494`
  (metrics).
- **Symptom**: F2 standing carry across r4-r8. `/livez`
  reports `{"status":"ok"}` without auth. `/readyz` reports
  drain state and reaper health
  (`{"status":"reaper-down"}`). `/metrics` (Prometheus
  exposition) exposes every counter the agent records,
  including `sbx_auth_fail_total{reason="..."}` per-cause
  cardinality — anyone scraping :7777 can correlate auth-
  failure spikes to attack waves.
- **Threat model**: Cluster NetworkPolicy is the primary
  gate; the runbook
  (`docs/runbooks/sandbox-nomad-ch.md`) names it as such for
  `/metrics` (`handlers.rs:487-494` cites it). For
  `/livez`/`/readyz`/`/metrics` to leak, an attacker must
  already be inside the cluster network or have routed to
  the VM IP. Information disclosure: process-age,
  drain-state, reaper-health, per-reason auth-fail counts.
- **Why it matters**: r5+r6+r7+r8 all carried this. The fix
  shape isn't obvious — k8s readiness probes can't carry a
  bearer, and the controller's network-policy assumption is
  documented. Worth noting that the metrics surface in
  particular now carries R7-S1-era counters
  (`inc_auth_fail("challenge-replayed")`,
  `inc_auth_fail("sandbox-id-mismatch")` — see
  `handlers.rs:766,780,801,832`) which directly map to
  attack-attempt classes — a much richer signal than pre-
  R7-S1.
- **Action**: Status quo (NetworkPolicy as the gate) is the
  pragmatic path. If hardening is desired, gate `/metrics`
  on the same Ed25519 signature as `/version` (r4 already
  added it to `/version`) — controllers carry the key and
  the scrape interval is low.

### [R9-S8] Admin endpoints lack rate-limit on heavyweight operations (MINOR, security-r9, T2 carry)
- **Files**: `crates/sandbox/src/admin_handlers.rs` — no
  `rate_limit`/`throttle`/per-bearer counter anywhere; cf.
  ripgrep yields zero hits.
- **Symptom**: `POST /admin/sandboxes/{id}/snapshot` (`:1219-
  1349`) and `POST /admin/sandboxes/{id}/wake` (`:1351-1405`)
  are the heaviest endpoints in the crate — snapshot is a
  ~2 GB memory dump (`snapshot_handler.rs:340-379`), wake is
  a tiered L1/L2 fetch + decrypt + CH restore. Both gate on
  the admin bearer (`admin_check` constant-time-compare)
  and the snapshot-feature flag, but nothing else. A
  compromised admin token (or an admin operating under
  duress) can DoS the worker by spamming snapshot/wake. The
  GDPR `DELETE /admin/users/{user_id}` (`:883-1031`) is
  equally heavy — a per-user pg transaction with multiple
  DELETEs + sealed-record unlink walk.
- **Threat model**: Compromised admin bearer (file
  exfiltration, leaked deployment secret). Constant-time
  compare on the bearer is good but does not bound
  per-bearer request rate.
- **Why it matters**: T2 standing carry; r4-r8 all flagged.
  No commit since. The fix shape is a per-token
  counter-with-window + 429 response; lives behind the
  Phase-5 JWT migration noted at
  `admin_handlers.rs:23-42`.
- **Action**: Add an in-memory `HashMap<bearer_hash,
  (window_start, count)>` at `AppState` and refuse new
  snapshot/wake/delete-user requests above N/min. Wire as a
  middleware tier above `admin_check`. (Note: a process-
  local counter survives only single-host; for multi-
  controller deployments, the gate is necessarily best-
  effort.)

## Closed by recent commits

1. **W1 unanchored `sed` → Python rewrite** (r1#5 / r2#5 /
   r3-r8 carry) — `f0ebf783` (R8-DEPLOY1 + W1). The Python
   heredoc is single-quoted (`<<'PY'`) so no shell
   interpolation; env passed via `NOMAD_TASK_DIR="$NOMAD_TASK_DIR"`
   (bash quoting is honest); regex is `^/opt/nomad/data/alloc/
   [^/]+/[^/]+/local(/|$)` — anchored at both ends; atomic
   tmpfile + fsync + rename. **The injection vector r1-r8
   tracked is closed.** New residual is R9-S1 (config.json
   plaintext on L2 → bucket-write attacker forges paths
   that survive the rewrite because they don't match the
   anchored prefix → rewriter is a no-op on those values).
2. **A1 fail-OPEN AEAD** (r6/r7/r8 CRITICAL #2) —
   `18e2034b`. `lib.rs:599-680` now wraps the inner store in
   `AeadSnapshotStore` unconditionally; `RootKek::from_env`
   absence → passthrough with `tracing::error!` (with-GCS) or
   `tracing::warn!` (L1-only). Boot-time posture visible in
   journald. The metadata stamp residual is R9-S3.
3. **R5-S5 hard_link aliasing** (r5#5 / r6#4 / r7 / r8) —
   `e7ecbbd6`. `snapshot_store.rs:323-327` chmod 0o444 on
   each alloc-side hard link. Test at `:614-640`
   (`local_disk_get_makes_alloc_side_read_only`) pins. CH
   writeback through MAP_SHARED is now blocked at the FS
   layer.
4. **R8-DEPLOY1 deployment gap** (r8 CRITICAL #1) —
   `a4c481e1` (env wiring) + `f0ebf783` (wrapper-side
   handling). The wrapper now embeds `ZSBX_SANDBOX_ID`
   verbatim in the kernel cmdline as
   `SANDBOX_AGENT_SANDBOX_ID=<value>` on the cold-boot
   branch only; the agent reads it via
   `init_sandbox_id_from_env`. Cluster wake is unbroken.
   Restore-branch asymmetry is R9-S5.
5. **R8-API1** `init_sandbox_id_from_env` `pub`→`pub(crate)`
   (r8 finding #7 partial — `ResyncBody` at `df1e756c`;
   full fix at `10bddc20` for `init_sandbox_id_from_env`).
   Lib wrapper `boot_init_sandbox_id` is now the single
   pub surface. `test_set_sandbox_id` remains `pub fn` but
   is `#[cfg(test)]`-gated (no prod-build surface).

## Carry-forward

- **R5-S4 restore-internal body excerpt** — see R9-S6 above.
  WIRE leak is sanitized; journald leak is the residual.
- **F2 unsigned probe surfaces** (`/livez`,`/readyz`,
  `/metrics`) — see R9-S7.
- **T2 admin rate limit absence** — see R9-S8.
- **B-AGENT-A `boot_sandbox_id` returns `None`** (r8 #5) —
  unchanged at `handlers.rs:138-140`. Hard error at
  init-time (`main.rs:97-102`) makes `None` unreachable in
  practice; a refactor that removes the explicit init call
  would not be caught at compile time. Type-system fix
  remains: move id into `AppState` and require it at the
  type level.
- **Hex validator lowercase-only** (r8 #8, minor) — same
  shape at `handlers.rs:792` (`(b'a'..=b'f').contains(&b)`)
  matches controller's `format!("{b:02x}")` at
  `restore_handler.rs:1635-1642`. Tight today; one-line
  comment recommending lowercase-only contract would
  guard against a future controller refactor.

## Counts

- CRITICAL: 1 (R9-S1)
- IMPORTANT: 4 (R9-S2, R9-S3, R9-S4, R9-S5)
- MINOR: 3 (R9-S6, R9-S7, R9-S8)
- Total: 8

r8-closed: 5 (W1, A1, R5-S5, R8-DEPLOY1, R8-API1 both halves).
r8-carry: 4 listed in §carry-forward.
