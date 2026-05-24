# Sandbox/snapshot-restore — security r21 review

Date: 2026-05-25 (UTC)
HEAD at audit: `8bc11768`. Catchup r21 — r21-A1 user_id emission
on restore, r17-Q3 wake-row error mapping, R10-API4 / R12-API1
readyz envelope closure. Read-only.

## Summary

**r21-A1 (`fcac5355`) verified — but introduces a new defense-in-
depth gap**: the restore-path now plumbs `user_id` into the
ChPlugin Config's `user_id` field, and that string flows into
`cfg.user_home_dir_root.join(user_id)` at
`restore_handler.rs:2256-2259`. **No `validate_typed_id` /
`is_typed_id` guard fires on the restore branch** before the path
is constructed or before the field is emitted to the driver.

The cold-boot equivalent at `nomad_ch.rs::create:472` DOES gate on
`validate_typed_id(user_id, "usr", "user_id")` at entry. The
restore path's `user_id` is sourced from the persisted
`sandbox.sandboxes.user_id` column (`db.rs:639-655`), and
`insert_sandbox` belt-and-suspenders parses with prefix `"usr"` at
write time (`db.rs:1988-1991`). So at HEAD the trust boundary
holds **iff** every writer of that column goes through
`insert_sandbox`. The restore-side never re-checks, breaking
defense-in-depth symmetry with cold-boot. New IMPORTANT finding
[R21-S1].

**R20-S2 (driverConfig.SandboxId on restore branch — still
open)**: the symmetric Go-side gap is unchanged. r21-A1 closed the
*emission* side but did NOT add `isTypedID(driverConfig.SandboxId)`
at `restore_task.go`. The same shape applies now to the brand-new
`user_id` field: the Go driver will trust whatever the controller
emits, and the controller emits without re-validation. **Two
fields, one missing validator on each side.** See [R21-S2] for
the combined recommendation.

**R20-S3 (driver SHA256 — still open)**: smoke-r17, r18, r19 all
exercised v8 driver pulls via `gs_pull` without integrity check.
THEORY A's confirmation (smoke-r19) does not change the supply-
chain posture — the verified driver came from a trusted build,
but `gs_pull` would happily fetch a tampered v8 binary at next
worker boot. **Pre-cutover gate.** Carry forward.

**R19-S1 (driver EvalSymlinks — still open)**: smoke-r19 confirmed
v8 validator class is functionally correct on the happy path
(all 3 disks accepted) but exercised NO symlink resolution.
Driver still does `filepath.Clean` only (`config_rewrite.go:99-
107`'s seam unchanged). The newly-introduced
`/var/zeroship/ch/users/<user_id>/` prefix is just one more root
where a tenant-writable symlink could redirect block-device
opens. **Pre-cutover gate.** Carry forward.

**readyz envelope (`528c3c44`)**: §10.0-compliant. No security
regression — the new body shapes leak nothing beyond the
machine-readable code `backend_unhealthy` and the static prose.
No tenant identifier, no probe diagnostic, no internal state.
Sign-off.

**wake-row error mapping (`17d65f83`)**: now `Err` on unknown
state. No new tenant-facing surface (admin-only endpoint per r20
review). Sign-off.

**Carry-forwards**: R13-S1, R9-S3, R15-S3, R18-S1, R17-S2, R18-S2,
R20-S1 ALL UNCHANGED.

**New findings**: 2 (R21-S1, R21-S2).

## CRITICAL

None.

## IMPORTANT

### [R21-S1] Restore-path `user_id` flows unvalidated into `user_home_dir_root.join(user_id)` and the driver Config field (NEW)

- **Files**: `crates/sandbox/src/restore_handler.rs:2256-2259`
  (`cfg.user_home_dir_root.join(user_id).join("home.img")`),
  `:2358` (`"user_id": user_id` in ChPlugin Config),
  `:2034-2055` (`submit_restore_job(... user_id, ...)` — no
  validation step), `:856-863` (`do_restore_inner` clones
  `snap.user_id` straight from `SnapshotRowMeta` without
  re-validation), `:623-664` (`read_snapshot_row` reads the DB
  column into the struct unconditionally).
- **Symptom**: a `user_id` of `"../foo"` results in
  `user_home_img = /var/zeroship/ch/users/../foo/home.img` →
  `PathBuf::join` does NOT clean lazily but the on-disk resolver
  will. On the driver side, the emitted `user_id` flows into the
  Go validator's per-user-home prefix
  `/var/zeroship/ch/users/<user_id>/`; without `..` rejection in
  the Go side either, the allow-list root widens silently. Symmetric
  with R20-S2's `SandboxId` finding.
- **Trust model today**: the DB column is gated at write time by
  `db.rs::insert_sandbox:1988-1991` (`parse_with_prefix(user_id,
  "usr")`). At HEAD this means `user_id` is in `usr_<base62>` form
  for every row that came through `insert_sandbox`. Severity is
  **defense-in-depth**, NOT an exploitable bypass — the same
  reasoning that made R20-S2 IMPORTANT-not-CRITICAL. Note that
  the cold-boot path validates EVEN THOUGH the same DB invariant
  holds (`nomad_ch.rs::create:472`); the restore path now
  asymmetrically trusts the persisted column.
- **Action**: at the top of `submit_restore_job` (or `do_restore_
  inner` before line 856), call
  `validate_typed_id(user_id, "usr", "user_id")` and bail with
  `RestoreHandlerError::Internal` on mismatch — this is a
  controller-internal trust-boundary check, not a tenant 400. The
  one-line check costs nothing and closes the asymmetry vs cold-
  boot. Add a unit test exercising `submit_restore_job` with
  `user_id = "../etc"` — it should NOT reach `build_restore_
  nomad_job_json`.
- Severity IMPORTANT (defense-in-depth on a trust-boundary input
  that the cold-boot path already validates).

### [R21-S2] Two-field unvalidated controller→driver hop (combined R20-S2 + R21-S1) (NEW, joint)

- **Files**: `restore_handler.rs:2345-2360` (ChPlugin Config
  builder for restore), `nomad-driver-ch/ch/restore_task.go:273-
  281` (the `isTypedID(cfg.SandboxId)` waiver carries forward AND
  the new `cfg.UserId` field will land in the same waiver
  bracket).
- **Symptom**: when the driver lands v9 (or whichever round picks
  up `UserId` in TaskConfig), it will likely mirror the cold-boot
  path's `isTypedID` check. But the restore branch's existing
  comment — "snapshot already carries that material" — invites a
  second waiver for `UserId` on the same false premise. The
  rewriter consumes the *driver-config* fields, not the
  snapshot's saved config.
- **Action**: pair the controller-side validators (R21-S1) with
  driver-side validators on **both** `cfg.SandboxId` and the
  forthcoming `cfg.UserId`, at the top of `startTaskRestoreBranch`
  before any path construction. The two-emitter / two-consumer
  contract drift R21-A1's commit message flagged ("a field-list
  contract test could catch future divergences") would also
  catch this; security recommends the contract test asserts
  presence-AND-validation-call symmetry, not just presence.
- Severity IMPORTANT (closes both sides of the trust hop; needed
  before cutover regardless of R19-S1's resolution).

## MINOR

None new this round.

## Cross-lens consensus

- **architecture r21** (`8bc11768` reviewer artefacts): r21-A1
  classified as CRITICAL closure on the architecture lens (broken
  driver→allow-list contract). Security agrees the *functional*
  break is closed; the residual finding (R21-S1) is on the
  defense-in-depth axis, not the functional axis. No conflict.
- **api-surface r20 (R20-API1 three-rewriter / no-ownership)**:
  r21-A1's commit message explicitly flags "two-emitter problem
  (cold-boot + restore). A field-list contract test could catch
  future divergences." Security strongly endorses — same shape as
  R21-S2's recommendation. **A contract test that diffs the two
  ChPlugin Config builders' field sets AND asserts each field has
  a validator call on both sides would close R21-S1, R21-S2, and
  pre-empt the next divergence in one stroke.**
- **smoke-r19 (`c2e4a6e0`)**: confirmed THEORY A (validator class
  closed). New layer = CH CreateConsoleDevice on serial.file —
  this is `PathFieldRuntimeFile` per R19-S1-PostLT6's taxonomy.
  Smoke is now exercising the path the symlink gap (R19-S1)
  would attack. Symlink test cases remain absent from the smoke
  matrix; security recommends adding one before cutover.

## Lens hand-off

- **Sandbox controller (Rust)**: R21-S1 is a 5-line fix in
  `restore_handler.rs::submit_restore_job` or `do_restore_inner`.
  Land alongside a test that asserts `user_id = "../etc"` is
  rejected before reaching `build_restore_nomad_job_json`.
- **nomad-driver-ch maintainers**: R20-S2 + R21-S2 — pair
  `isTypedID(cfg.SandboxId)` and `isTypedID(cfg.UserId)` at
  `startTaskRestoreBranch` entry. R19-S1 remains the highest-
  impact open driver-side item (add `EvalSymlinks` to
  `validatePathByKind`). R20-S3 — add SHA256 verification to
  `gs_pull` before next driver pin bump.
- **api-surface r21**: contract-test recommendation in cross-lens
  above. Pin the per-field validator-call symmetry in the wire
  schema spec so it doesn't drift on the next field addition.
- **Ops / cluster bring-up**: R13-S1 + R20-S3 worm-propagation
  loop unchanged. Both block cutover.

## Carry-forward open at HEAD `8bc11768`

- R20-S3 (IMPORTANT) — `gs_pull` no SHA256 verification;
  worm-propagation vector via R13-S1 IAM.
- R20-S2 (IMPORTANT) — driver-side `cfg.SandboxId` unvalidated on
  restore branch; will compound with `cfg.UserId` (see R21-S2).
- R19-S1 (IMPORTANT) — driver `filepath.Clean` only, no
  `EvalSymlinks`; affects all three `PathField*` kinds, broadest
  attack surface on `PathFieldDisk`.
- R13-S1 (IMPORTANT, ≥8 rounds) — `provision-gcp-cluster.sh:286`
  `--scopes=storage-rw` + default GCE SA.
- R9-S3 (IMPORTANT, ≥12 rounds) — `snapshot_handler.rs:417`
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
- IMPORTANT: 2 new (R21-S1, R21-S2); carry: R20-S3, R20-S2,
  R19-S1, R13-S1, R9-S3, R18-S1.
- MINOR: 0 new; carry: R20-S1, R15-S3, R17-S2, R18-S2.
- Total NEW this round: 2.
