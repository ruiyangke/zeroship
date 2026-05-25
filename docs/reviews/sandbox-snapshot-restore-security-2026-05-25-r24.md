# Sandbox/snapshot-restore — security r24 review

Date: 2026-05-25 (UTC)
HEAD at audit: `e6363fce`. Catchup r24 — landings since r22
(`8b366b6d`): C-7-LT-12a `rootfs_source` emit (`7fd661c9`),
field-list parity contract test (`b6c55d93`), driver v11→v12 +
controller v31→v32 pin bump (`429f2a47`), WakeMachine
terminal-overwrite counter + WARN (`f98611fb`), pg-gated counter
e2e (`234c3bdf`). Smoke-r23 GREEN; stress RED (2/60). Read-only;
nomad_ch.rs + driver tap idempotency in flight C-N-W1+W2 — NOT
touched this round.

## Summary

**r24 produces ZERO new CRITICAL / IMPORTANT / MINOR findings.**
Every landing since r22 is security-clean on the focal lens
(WakeErrorCode wire surface, §10.0 envelope on polling, restore-
path Config emit, terminal-overwrite race surface, sanitizer
exhaustiveness).

**Net carry status**:
- **R22-S1 (CH stderr-tail leak)** — UNCHANGED. Driver v12
  binary not audited this round (sandbox crate is read-only;
  driver lives cross-worktree). No controller-side sanitizer
  widening landed either. Still latent until any future change
  lifts `ClientDescription` (or the driver's verbose error
  string) into wake_jobs.error_message.
- **R21-S1 (restore-path typed_id validation)** — UNCHANGED.
  `user_id` flows from `sandboxes.user_id` (DB CHECK regex
  `^usr_[0-9A-Za-z]{20,40}$` enforces shape at the column —
  see `migrations/0001_*.sql:67-68`) into `cfg.user_home_dir_root
  .join(user_id)` at `restore_handler.rs:2256-2259` and into the
  driver Config at `:2368` without a Rust-side
  `parse_with_prefix("usr")`. Defense-in-depth gap remains — the
  DB CHECK is the structural guard today, so this is correctly
  classified IMPORTANT-defense-in-depth, not CRITICAL.
- **R20-S3 (driver SHA256 verify)** — UNCHANGED + REGRESSED-IN-
  SCOPE. `gs_pull` at `gcp-worker-startup.sh:146-159` still does
  no integrity check; the v11→v12 driver bump (`429f2a47`,
  `gcp-worker-startup.sh:176`) landed without adding any verify
  step. **Pre-cutover blocker per r22 cluster lens; still open.**
- **R18-S1 (sanitize_error_message coverage)** — partially
  closed in code but doc-comment out of sync. Code at `wake_
  machine.rs:797-843` matches 5 IPv4 ranges (10/8, 192.168/16,
  172.16/12, 169.254/16, 100.64/10) + IPv6-link-local; doc-
  comment at `:740-748` still names only 3 RFC1918 ranges.
  Coverage of 100.64 / 169.254 fe80 is **EXHAUSTIVE** per r22
  focal-list criterion; carry remains because it doesn't cover
  typed_ids / filesystem paths (R22-S1 controller-side
  recommendation).
- **R20-C1 / WakeJobs `lessee_updated_at` write integrity** —
  CLEAN. The R22-I1 counter (`f98611fb`) makes the guard-fire
  visible without altering the SQL guard; takeover sweep at
  `db.rs:3370-3389` and `update_wake_job_state` at `:3222-3247`
  share the `state NOT IN ('ok','failed')` predicate — pg
  row-locking serializes the race per the function docstring at
  `:3339-3346`. No new race surface introduced by R22-I1.

**Wire-surface focal checks**:
- **WakeErrorCode `error_code` wire field** — no new branch since
  r22; enum unchanged at `db.rs:1493-1523`; wire-code mapping at
  `:1588-1607` stable. Variants name the failure CLASS only — no
  embedded paths / IDs / messages. The free-form `error_message`
  field is the leak vector R22-S1 traces; the `error_code`
  itself is leak-safe.
- **§10.0 envelope on `GET /admin/sandboxes/{id}/wake/{wake_id}`**
  — at `admin_handlers.rs:1849-1879` (`render_wake_poll_response`)
  the failed-state body uses `ErrorEnvelope::new(...)` + `.with_
  extra(...)` per R16-API1 #1. typed_id parse on BOTH path
  parameters at `:1755-1762`; 400 on bad shape; 404 (envelope)
  on wrong-sandbox cross-tenant probe at `:1805-1813`. Clean.
- **AEAD chain on `rootfs_source`** — N/A. `rootfs_source =
  cfg.runtime_dir.join("rootfs-slim.img")` at `restore_handler.
  rs:2352` is the operator-staged base image (not snapshot-
  bound), pulled by `gs_pull rootfs-slim.img.virtio-blk-v5` at
  `gcp-worker-startup.sh:165`. Not an artifact whose integrity
  the snapshot AEAD chain protects — see R20-S3 carry for the
  bucket-trust gap.
- **wake_id typed_id validation on POST mint path** — server-
  minted via `typed_id::new_wake_id()` at `admin_handlers.rs:
  1660`; attacker has zero control. Validation on poll path
  covered at `:1759-1762`.

## CRITICAL

None.

## IMPORTANT

None new this round.

## MINOR

### [R24-M1] `sanitize_error_message` doc-comment understates code coverage — auditor sees "3 ranges" but code matches 5 (DOC HYGIENE)

- **File**: `crates/sandbox/src/wake_machine.rs:740-748`.
- **Quote**:
  ```rust
  /// Strategy (conservative starting set; **TODO**: expand as new leak
  /// surfaces emerge during PR2 cluster smoke — kerberos tickets,
  /// pubkey fingerprints, jwt suffixes, GCS signed-URL query strings):
  /// 1. Strip agent-shape URLs (`http(s)://<rfc1918-host>:port/...`)
  ///    as a unit, so the redaction reads as one `[redacted]` rather
  ///    than `http://[redacted]:7000/[redacted]`.
  /// 2. Strip bare RFC1918 IPv4 addresses (10/8, 172.16/12,
  ///    192.168/16), optionally followed by `:port`.
  /// 3. Strip IPv6 link-local prefixes (`fe80::/10`).
  /// 4. Truncate to [`ERROR_MESSAGE_MAX_BYTES`] (char-boundary safe).
  ```
- **Why**: The `match_rfc1918_at` matcher at `:791-843` actually
  scans **5** prefixes — `10.`, `192.168.`, `169.254.` (RFC 3927
  link-local + IMDS), `172.16/12`, and `100.64/10` (RFC 6598
  CGNAT). The doc-comment names only the three RFC1918 ranges
  and omits 169.254 + 100.64. An auditor reading the strategy
  block from the top of the function will conclude IMDS
  exposure is unredacted and file a finding — same conclusion
  R18-S1 originally reached. The accompanying tests at `:1185-
  1213` confirm 169.254 + 100.64 ARE matched (passing). Doc
  drift, not code drift.
- **Cross-lens note**: api-surface / code-quality should pick
  this up at the next round — the §10.0 envelope IS leak-clean
  but the inline doc misrepresents coverage.
- **Severity**: MINOR — doc-only; no leak surface; only audit
  productivity tax. Fix is a 3-line comment edit.

### [R24-M2] Restore-path `rootfs_source` field accepts any path the controller cfg names — operator-trust footprint widened by one path (DEFENSE-IN-DEPTH)

- **File**: `crates/sandbox/src/restore_handler.rs:2352`,
  `:2371`.
- **Quote**:
  ```rust
  let rootfs_source = cfg.runtime_dir.join("rootfs-slim.img");
  // ...
  "rootfs_source": rootfs_source.display().to_string(),
  ```
- **Why**: The driver (per the C-7-LT-12a docstring at `:2343-
  2351`) hardlinks `rootfs_source` into runDir before CH spawn.
  `cfg.runtime_dir` is operator-supplied via
  `SANDBOX_NOMAD_CH_RUNTIME_DIR` (`config.rs:749-752`), defaulted
  to `/var/lib/zeroship/ch`. No tenant-controllable bytes flow
  in. **However**, the driver Config now carries an
  operator-controllable host-path that the driver dereferences
  with elevated privileges (root) — if `SANDBOX_NOMAD_CH_RUNTIME_
  DIR` ever gets misconfigured or env-injected to point at a
  path containing a symlink to host-sensitive bytes (e.g.
  `/etc/shadow.img`), the driver hardlinks that into the
  sandbox's runDir. This is **operator-induced, not tenant-
  induced**, and matches existing `kernel`, `workspace_img`,
  `user_home_img` exposure. No new threat class — defense-in-
  depth carry of R19-S1 (driver `filepath.Clean` only, no
  `EvalSymlinks`).
- **Cross-lens note**: r22 noted the symlinking introduced in
  C-7-LT-10 is security-clean because src/dst are driver-owned
  filename-constants. C-7-LT-12a widens that to one path the
  CONTROLLER names — same operator-trust band, broader controller-
  to-driver path surface. If/when the driver's PathFieldDisk
  validator gets the `EvalSymlinks` upgrade (R19-S1 carry), this
  field needs to be in its allow-list.
- **Severity**: MINOR — operator-trust; not exploitable by
  tenant. Documentation of the assumption + R19-S1 closure are
  the right path.

## Cross-lens consensus

- **architecture r23** (`docs/reviews/sandbox-snapshot-restore-
  architecture-2026-05-25-r23.md`, not re-read this round —
  cited from filename only): r23 should confirm the field-list
  parity contract test (`b6c55d93`) closes the symmetric
  divergence class R22-T1/R21-API2 flagged. Security agrees:
  the test at `restore_handler.rs:3742-3853` pins both the
  intentional asymmetry (`rootfs_source` restore-only) and
  asserts every other field is symmetric. The error-shape
  symmetry recommendation from r22 (cross-lens) remains
  unimplemented but no new error-shape divergences landed
  either.
- **smoke-r23 GREEN** (`082e6ddb`): first end-to-end success.
  The success path proves no cleartext tenant data leaks through
  the §10.0-enveloped failed body (no `failed` rows in the
  successful smoke). Stress-r24 RED (2/60) — the failures are
  cluster-correctness (C-N-W1+W2), not security; out of scope.
- **wake_machine R22-I1 counter** (`f98611fb`): counter labels
  contain ZERO tenant data (process-global atomic — see
  `metrics.rs:141-148`). WARN log target
  `sandbox::wake::terminal_overwrite_blocked` emits `wake_id` +
  `attempted_state` only — `wake_id` is a server-minted
  typed_id, `attempted_state` is the enum. No leak surface.
- **WakeJobs `lessee` retention after takeover** (`db.rs:3370-
  3389`): the takeover sweep at `:3370` does NOT overwrite the
  `lessee` column when marking a row `failed` — preserves the
  dead-controller's host_id for audit. `lessee_updated_at` is
  bumped to NOW. Semantic is "lessee field names dead owner,
  lessee_updated_at is takeover-time"; intentional, not a write-
  integrity hazard. Cross-confirmed against concurrency r23.

## Carry-forward open at HEAD `e6363fce`

| ID       | Sev       | File:line                                                       | Status at r24                              |
|----------|-----------|-----------------------------------------------------------------|--------------------------------------------|
| R22-S1   | IMPORTANT | `nomad-driver-ch/ch/restore_task.go:529-554` (driver, cross-wt); `crates/sandbox/src/backend/nomad_ch.rs:2614-2620`; `wake_machine.rs:757-775` | OPEN — driver v12 not audited; no controller-side sanitizer widening landed. |
| R21-S1   | IMPORTANT | `crates/sandbox/src/restore_handler.rs:2256-2259, :2368`        | OPEN — DB CHECK regex at `migrations/0001_*.sql:67-68` provides the structural guard; defense-in-depth gap unchanged. |
| R21-S2   | IMPORTANT | (driver-side, cross-worktree)                                   | OPEN — driver validator-call symmetry not audited; r24 confirmed FIELD-list symmetry contract landed (`b6c55d93`) but not the driver-side validator-call symmetry test. |
| R20-S3   | IMPORTANT | `crates/sandbox/scripts/gcp-worker-startup.sh:146-176`          | OPEN — v11→v12 bump landed (`429f2a47`) without adding SHA256 verify. **Pre-cutover blocker.** |
| R20-S2   | IMPORTANT | (driver-side, cross-worktree)                                   | OPEN — driver `cfg.SandboxId` validator not audited. |
| R19-S1   | IMPORTANT | (driver-side, cross-worktree)                                   | OPEN — driver `filepath.Clean` only; PathFieldDisk allow-list broader than `EvalSymlinks` would catch. r24-M2 references this. |
| R18-S1   | IMPORTANT | `crates/sandbox/src/wake_machine.rs:740-748` (doc); `:797-843` (code) | PARTIAL — code matches 5 ranges (exhaustive per focal-list); doc names 3. r24-M1 logs the doc drift. typed_id / path coverage NOT added — R22-S1 controller-side recommendation. |
| R13-S1   | IMPORTANT | `crates/sandbox/scripts/provision-gcp-cluster.sh:286`           | OPEN — `--scopes=storage-rw` + default GCE SA unchanged. ≥10 rounds open. |
| R9-S3    | IMPORTANT | `crates/sandbox/src/snapshot_handler.rs:417`                    | OPEN — `Some("v1")` stamp regardless of AEAD posture; not re-examined this round. |
| R20-S1   | MINOR     | (config)                                                        | OPEN — `ContentAddressedRootfsRoots` slot empty.|
| R15-S3   | MINOR     | (config / docs)                                                 | OPEN — 30 s fence cap undocumented.        |
| R17-S2   | MINOR     | (KEK provisioning)                                              | OPEN — no explicit chown root:root.        |
| R18-S2   | MINOR     | (logging)                                                       | OPEN — fence-error IP leak into scoped log.|
| **R24-M1** | MINOR   | `crates/sandbox/src/wake_machine.rs:740-748`                    | NEW this round — doc-comment understates `match_rfc1918_at` coverage. |
| **R24-M2** | MINOR   | `crates/sandbox/src/restore_handler.rs:2352, :2371`             | NEW this round — operator-trust footprint widened by one driver-Config path field. |

## Lens hand-off

- **nomad-driver-ch maintainers (R22-S1 + R20-S2 + R21-S2 +
  R19-S1)**: cross-worktree backlog unchanged. Bundle the
  `ch_stderr_tail=%q` drop, `isTypedID(cfg.SandboxId/UserId)`,
  and `EvalSymlinks` upgrade into the next driver pin bump.
  Driver v12 landed without these; the next bump is the right
  cut.
- **Sandbox controller (Rust)**: R21-S1 (5-line `parse_with_
  prefix("usr")` at top of `submit_restore_job` /
  `do_restore_inner`) and the sanitizer widening (R22-S1
  controller-side, typed_id + filesystem-path strip) remain the
  cheapest defense-in-depth fixes. R24-M1 is a 3-line doc edit.
- **api-surface r23/r24**: field-list parity contract landed
  (`b6c55d93`) closing R22-T1 / R21-API2. Error-shape symmetry
  contract (r22 cross-lens recommendation) still unimplemented;
  pair with R22-S1's eventual landing.
- **Ops / cluster bring-up (R13-S1 + R20-S3)**: WORM-propagation
  loop and `gs_pull` SHA256 gap unchanged across the v11→v12
  bump. Both pre-cutover blockers; T-8b-stress RED is unrelated
  but cutover gating still applies.

## Counts

- CRITICAL: 0 new; carry: 0.
- IMPORTANT: 0 new; carry: R22-S1, R21-S1, R21-S2, R20-S3,
  R20-S2, R19-S1, R18-S1, R13-S1, R9-S3.
- MINOR: 2 new (R24-M1 doc drift on sanitizer; R24-M2 operator-
  trust footprint via `rootfs_source`); carry: R20-S1, R15-S3,
  R17-S2, R18-S2.
- Total NEW this round: 2 MINOR.
- **Cutover gate**: R20-S3 (driver binary integrity) + R13-S1
  (storage-rw scope) remain pre-cutover blockers; landings since
  r22 did not address either.
