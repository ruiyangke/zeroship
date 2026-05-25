# Sandbox/snapshot-restore — security r22 review

Date: 2026-05-25 (UTC)
HEAD at audit: `8b366b6d`. Catchup r22 — R20-C1 SQL guard
(`ccb2abc8`), R10-API4 actual fix at sandbox-agent (`00a00d01`),
smoke-r20/r21 + driver v9/v10/v11. Read-only.

## Summary

**r22 produces one new IMPORTANT** [R22-S1] tied to the
driver-side C-7-LT-11 stderr-tail capture: the 4096-byte CH stderr
tail is embedded verbatim in the driver's RPC error string
(`ch/restore_task.go:551-554`), reaches the controller via
`ClientDescription` on alloc-terminal poll
(`backend/nomad_ch.rs:2618`), and from there can land in
`wake_jobs.error_message` after `sanitize_error_message`. That
sanitizer only redacts RFC1918 IPs + IPv6-LL prefixes — it does
**NOT** redact filesystem paths or typed_ids, both of which CH
stderr routinely emits (`/var/zeroship/ch/users/<user_id>/home.img`,
sandbox_id, alloc UUIDs, kernel dmesg if present). Severity is
IMPORTANT, not CRITICAL: at HEAD the Nomad poller surfaces only
`"Failed tasks"` from `ClientDescription` (smoke-r21 evidence,
line 49-53), so the verbose driver error currently stays inside
nomad journald. But once any downstream consumer lifts the driver
error string into operator-facing output (the API-3 / R12-API1
ladder explicitly trends in that direction), tenant user_ids leak.
**Sanitize at the driver** (drop the `%q`-quoted tail; emit a
fixed-length hex-encoded checksum + path-only reference) OR widen
`sanitize_error_message` to redact `usr_<base62>` / `sbx_<base62>`
/ filesystem paths.

**r22-A1 (driver symlinks RestoreFrom artifacts into runDir,
`1d5aa2f9`)** — security-clean. Trace: `RestoreFrom =
host_state_dir/<sandbox_id.simple()>/restore/`
(`restore_handler.rs:2020-2027`); `sandbox_id` is server-side
UUIDv7, no attacker path control. Snapshot store hard-links
0o444 regular files from content-addressed L1
(`snapshot_store.rs:286-326`), sha256-verified BEFORE staging
(`:277-283`). The driver's two symlinks (state.json,
memory-ranges) use compile-time-constant filenames and src/dst
under driver-owned dirs (`ch/restore_task.go:399-405`). No
cross-tenant attack surface via RestoreFrom unless an attacker
already has root write to host_state_dir, which collapses the
threat model.

**R20-C1 SQL guard (`ccb2abc8`)** — clean. Parameterized SQL with
`WHERE wake_id = $5::TEXT AND state NOT IN ('ok', 'failed')`
(`db.rs:3231-3232`). No injection surface; terminal-overwrite race
silently no-ops, which is the intended invariant. Sign-off.

**R10-API4 actual fix at sandbox-agent (`00a00d01`)** — clean.
§10.0 envelope shape; agent doesn't leak tenant data on readyz
failure path (`sandbox-agent/src/handlers.rs:498-510`). Sign-off.

**Carry-forwards unchanged at HEAD `8b366b6d`**: R20-S3 (driver
SHA256 — `gs_pull` at `gcp-worker-startup.sh:146-159` still
unverified, line 176 pulls `nomad-driver-ch.v11` with no integrity
check), R20-S2 + R21-S2 (driver-side `cfg.SandboxId` / `cfg.UserId`
validators on restore branch — driver v11 source not audited this
round but commit log shows no `isTypedID` addition since
`50da6ae2` C-7-LT-7), R19-S1 (driver `filepath.Clean` only; the
C-7-LT-10 symlinks are filename-constant src/dst so they don't
exploit the gap, but the broader validator surface unchanged),
R21-S1 (restore-path `user_id` still flows unvalidated into
`user_home_dir_root.join(user_id)` at `restore_handler.rs:2256-
2259` and into the ChPlugin Config `user_id` field at `:2358`;
zero `validate_typed_id` calls anywhere in `restore_handler.rs`).

**Carries**: R13-S1, R9-S3, R15-S3, R18-S1, R17-S2, R18-S2 ALL
UNCHANGED.

**New findings**: 1 (R22-S1).

## CRITICAL

None.

## IMPORTANT

### [R22-S1] CH stderr-tail embedded verbatim in driver error string — `usr_<base62>` / filesystem paths leak into `wake_jobs.error_message` once any downstream lift surfaces it (NEW)

- **Files**:
  `nomad-driver-ch/ch/restore_task.go:529-536` (socket-timeout
  branch, original C-7-LT-3-PR2 site),
  `:541-554` (NEW resume-failure branch, C-7-LT-11 `6efdc42e`);
  `crates/sandbox/src/backend/nomad_ch.rs:2614-2620` (controller
  lifts `ClientDescription` into the wake-machine error path);
  `crates/sandbox/src/wake_machine.rs:757-775` (sanitizer scope —
  IPs only, not paths/IDs).
- **Symptom**: driver embeds up to 4096 bytes of CH stderr via
  `fmt.Errorf("...: %w; ch_stderr_tail=%q (path=%s)", ...)`. CH
  stderr on restore typically contains tenant-identifying material:
  - `/var/zeroship/ch/users/usr_033MGIp35ja7ENmJ7fJ44g/home.img`
    (smoke-r21 line 128 — disk-path enumeration is one of CH's
    most common verbose-error shapes).
  - sandbox_id (`sbx_…`), alloc UUIDs, runDir path.
  - kernel ring-buffer fragments on a CH crash (if CH was started
    with `--serial tty` or similar — restore mode has serial.file
    = File mode, but the dmesg leaks pre-snapshot have been
    observed on the snapshot side at `ch_version` field display).
  - Tap-device IPs (`10.99.x.y`) — these the sanitizer **does**
    catch; the rest it does not.
- **Trust model today (why not CRITICAL)**: smoke-r21's wake_jobs
  body shows only `"backend: nomad alloc terminal status=failed:
  Failed tasks"` — Nomad's `ClientDescription` is the short
  top-line, NOT the full driver error. The verbose driver error
  with stderr-tail lands in `journalctl -u nomad` (line 73-94 of
  smoke-r21), which is operator-only. The leak is latent until
  any change lifts the driver error into the wake-machine error
  message (the smoke-r21 Recommendation Option 2 explicitly
  forecasts exactly this for r22 triage productivity). At that
  point — TODAY's stub plus tomorrow's lift — the tail will pass
  through `sanitize_error_message`, which **redacts RFC1918 IPs
  and IPv6-link-local addresses ONLY**. Filesystem paths and
  typed_ids pass through verbatim. The column is SELECT-able by
  `sandbox_app` role (`wake_machine.rs:732-735`) and retained
  `T_KEEP` post-completion.
- **What CH stderr will routinely contain**: the smoke-r21
  observed error chain (`HttpApiClient(ServerResponse(
  InternalServerError, Some("[\"Error from API\",\"The VM could
  not resume\",\"VM is not running\"]")))`) carries no
  tenant data — that's the lucky shape of THIS defect class. But
  the prior 3 cycles (r15/r19/r20) all hit
  `CreateConsoleDevice(NotFound)` chains whose verbose form
  included the full retargeted `serial.file` path
  `/opt/nomad/data/alloc/<uuid>/ch/local/serial.log` — alloc UUID
  is not high-sensitivity but the rewritten home.img path that
  carries `usr_<base62>` literally is. CH itself logs paths on
  any I/O error during restore, and the resume-failure branch's
  whole point is observability into post-restore CH state — paths
  are the dominant signal in those messages.
- **Action (driver side, preferred)**: in
  `ch/restore_task.go:551-554`, replace the `%q`-quoted stderr
  tail with a controlled summary: `ch_stderr_sha256=<8-hex>
  ch_stderr_bytes=<len> (full tail in operator log at <path>)`.
  Log the verbatim tail at the driver's stderr level (`hclog`)
  where it's already captured for journald, but DO NOT include
  it in the error returned to nomad-controller. Mirror the same
  pattern at the socket-timeout branch `:529-536` for symmetry.
  Action: ~10 lines + a unit test
  (`TestStartTaskRestoreBranch_ResumeFailure_DoesNotLeakStderrTail`).
- **Action (controller side, defense-in-depth)**: widen
  `sanitize_error_message` to redact `usr_[A-Za-z0-9]{18,22}`,
  `sbx_[A-Za-z0-9]{18,22}`, `wak_[A-Za-z0-9]{18,22}`, and
  absolute paths starting with `/var/zeroship/ch/` or
  `/opt/nomad/data/alloc/`. Even with the driver fix landed, this
  catches the next leak class without requiring a driver bump.
- **Severity**: IMPORTANT — latent leak hardens into operational
  exposure once the smoke-r21 Recommendation Option 2 follow-on
  ("lift CH-side stderr into controller-visible state for
  productive triage") lands. Cheapest fix is in the driver before
  the next pin bump.

## MINOR

None new this round.

## Cross-lens consensus

- **architecture r22** (`docs/reviews/sandbox-snapshot-restore-
  architecture-2026-05-25-r22.md`): r22-A1 elevated to CRITICAL on
  the architecture lens (driver/wrapper restore-input contract
  divergence). Security agrees the architectural fix in
  `1d5aa2f9` is correct; the residual security note (R22-S1) is
  on a SEPARATE axis (driver→controller error shape, not
  driver→CH input contract). No conflict; the symlinking
  introduced in C-7-LT-10 is itself security-clean per the trace
  above.
- **smoke-r21** (`cluster-2026-05-25-T8b-smoke-r21.md`): the doc
  explicitly identifies "no CH-side log of what the VMM did
  during `--restore`" as C-7-LT-11 (line 100, 272-273). The fix
  `6efdc42e` solves the observability gap but introduces R22-S1.
  Pair-fix: keep driver-side stderr capture for journald, drop it
  from the RPC error envelope.
- **R21-S2 contract test (carry)**: r21 security recommended a
  field-list contract test that asserts validator-call symmetry
  between cold-boot and restore ChPlugin Config builders. Still
  unimplemented at HEAD `8b366b6d`. Same recommendation now
  extends to **error-shape symmetry**: both branches' driver-
  return error should pass the same controller-side sanitization.
  A contract test that asserts the driver error from BOTH branches
  contains zero `usr_/sbx_/wak_` substrings would catch R22-S1
  and pre-empt the next divergence.

## Lens hand-off

- **nomad-driver-ch maintainers (R22-S1 owner)**: drop the
  `ch_stderr_tail=%q` portion from the RPC-visible error in
  BOTH `ch/restore_task.go:534` and `:552`. Keep
  `hclog.Error(...)` of the tail for journald. ~10 LOC + a
  zero-leak assertion test. Pair with R20-S2 / R21-S2 / R19-S1
  before the next driver pin bump; v11 is the right cut.
- **Sandbox controller (Rust)**: R21-S1 (5-line
  `validate_typed_id` at top of `submit_restore_job` /
  `do_restore_inner`) remains the cheapest defense-in-depth fix.
  Also widen `sanitize_error_message` per R22-S1 controller-side
  recommendation — three regex-equivalent byte-scan additions to
  the existing `strip_*` pass chain.
- **api-surface r22**: contract-test recommendation in cross-lens
  above. Same wire-schema spec needs an "error-shape symmetry"
  clause alongside "field-list symmetry".
- **Ops / cluster bring-up**: R13-S1 + R20-S3 worm-propagation
  loop unchanged; v11 driver landed without SHA256 verification.
  Both still block cutover. Add SHA256 verify to `gs_pull` before
  the next worker-image bake.

## Carry-forward open at HEAD `8b366b6d`

- R20-S3 (IMPORTANT) — `gs_pull` no SHA256 verification;
  worm-propagation vector via R13-S1 IAM.
- R20-S2 (IMPORTANT) — driver-side `cfg.SandboxId` unvalidated on
  restore branch; compounds with `cfg.UserId` (R21-S2).
- R19-S1 (IMPORTANT) — driver `filepath.Clean` only, no
  `EvalSymlinks`; broadest attack surface on `PathFieldDisk`.
- R21-S1 (IMPORTANT) — restore-path `user_id` flows unvalidated
  through `user_home_dir_root.join(user_id)` and into the driver
  Config; defense-in-depth gap vs cold-boot symmetry.
- R21-S2 (IMPORTANT) — validator-call symmetry contract test
  proposed, unimplemented.
- R13-S1 (IMPORTANT, ≥9 rounds) — `provision-gcp-cluster.sh:286`
  `--scopes=storage-rw` + default GCE SA.
- R9-S3 (IMPORTANT, ≥13 rounds) — `snapshot_handler.rs:417`
  stamps `Some("v1")` regardless of AEAD posture.
- R18-S1 (IMPORTANT) — `match_rfc1918_at` covers 5 of ~10 IPv4
  ranges in the SSRF blocklist.
- R20-S1 (MINOR) — `ContentAddressedRootfsRoots` config slot
  widens allow-list when populated; currently empty.
- R15-S3 (MINOR) — fence cap 30 s undocumented.
- R17-S2 (MINOR) — KEK file implicit-root owner; no explicit
  `chown root:root`.
- R18-S2 (MINOR) — fence-error IP leak into scoped log target.

## Counts

- CRITICAL: 0 new; carry: 0.
- IMPORTANT: 1 new (R22-S1); carry: R20-S3, R20-S2, R19-S1,
  R21-S1, R21-S2, R13-S1, R9-S3, R18-S1.
- MINOR: 0 new; carry: R20-S1, R15-S3, R17-S2, R18-S2.
- Total NEW this round: 1.
