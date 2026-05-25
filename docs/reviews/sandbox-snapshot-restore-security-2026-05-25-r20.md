# Sandbox/snapshot-restore — security r20 review

Date: 2026-05-25 (UTC)
HEAD at audit: `8718120b`. Catchup r20 — driver-side C-7-LT-6
(per-field path allow-list) + R19-API1 closure + smoke-r16 review +
driver v6/v7 pin bumps. Read-only.

## Summary

**R19-S1 status POST-C-7-LT-6: STILL OPEN, scope narrowed** (see
[R19-S1-PostLT6] below). The Go driver's symlink gap (filepath.Clean
without EvalSymlinks) is now per-field-aware but still does no
realpath resolution. Severity stays IMPORTANT for `PathFieldDisk`
(broader allow-list = more symlink-pre-placement opportunity) and
unchanged IMPORTANT for `PathFieldRuntimeFile` / `PathFieldFsSocket`.

**R19-API1 (`fde4f51c`) closure verified clean**. The new wire-visible
`error_message` literal at `db.rs:3335-3337` —
> *"wake worker aborted: controller did not complete the wake within
> the timeout (see operator runbook)"*
contains **zero internal detail**: no sandbox_id, no wake_id, no
lessee identifier, no timeout numeric, no review ID (`R19-C1` token
removed; lineage moved to structured tracing `closure_ref` field at
`sweep.rs:401`, log-pipeline only). The route surface
(`/admin/sandboxes/{id}/wake/{wake_id}`) gates on `admin_check` at
`admin_handlers.rs:1748` — admin-bearer-only, single trust level. No
tenant exposure.

**C-7-LT-6 (`f73f2b49`) per-field allow-list — new attack surface**.
Two new findings:
- [R20-S1] `ContentAddressedRootfsRoots` is admin-set via hclspec
  `false`/optional with empty default; **but the slot is currently
  empty** in the production stanza (`gcp-worker-startup.sh:197-199`
  ships `plugin "nomad-driver-ch" { config {} }`). No attacker
  influence today. Tomorrow: any operator who adds entries should
  treat them as trust-anchor roots — see finding.
- [R20-S2] `driverConfig.SandboxId` flows UNVALIDATED into
  `sandboxPrefix()` on the restore branch (the `isTypedID` guard
  applies on cold-boot only — see `restore_task.go:273-278`'s
  explicit waiver). `filepath.Join("/var/zeroship/ch", sandboxID)`
  will Clean a `..` component and silently widen the allow-list root.

**Driver v6/v7 supply chain (`gs_pull nomad-driver-ch.v{6,7}`)**:
[R20-S3] no SHA256 verification of the GCS object bytes — `gs_pull`
at `gcp-worker-startup.sh:146-159` is `gsutil cp` + `chmod`, no
checksum compare against a pinned hash. Object naming gives version
pinning only, not integrity. Any actor with bucket-write IAM can
overwrite `nomad-driver-ch.v7` in place. The smoke-r16 commit
message claims "SHA256 reproducibility verified" — that's
build-side, not pull-side. **NEW finding (IMPORTANT)**.

**Three-rewriter coexistence (R20-API1 from api-surface)**: from a
security angle, the three rewriters share an artefact with NO version
marker. Each layer's allow-list is rebuilt from local context. If
wrapper retires post-cutover, the Go driver's per-field allow-list
covers the *path-bearing* fields the wrapper covered — verified by
field-by-field comparison below. The wrapper has `os.path.realpath`,
the driver does NOT (R19-S1). Wrapper-retirement IS a security
posture regression on the symlink axis unless R19-S1 closes first.
See cross-lens consensus.

**Carry-forwards**: R13-S1, R9-S3, R15-S3, R18-S1, R17-S2, R18-S2
ALL UNCHANGED at HEAD `8718120b`.

**New findings**: 4 (R19-S1 post-LT6 status update; R20-S1, R20-S2,
R20-S3).

## CRITICAL

None.

## IMPORTANT

### [R19-S1-PostLT6] Driver-side per-field allow-list ports the lexical-only check verbatim; symlink gap unchanged across all three field kinds

- **Files**: `nomad-driver-ch/ch/config_rewrite.go:158-176`
  (`hasPathPrefix`) explicitly notes *"Callers MUST ensure `..`
  components have been rejected upstream — hasPathPrefix is a
  string-prefix check, not a realpath check."* `validatePathByKind`
  (l.201-272) uses `filepath.Clean` only.
- **Per-field severity update**:
  - `PathFieldRuntimeFile` (serial, console): unchanged from r19.
    Attacker pre-places `<taskDir>/local/serial.log` →
    `/etc/shadow`; CH opens for write — local-write escalation.
    IMPORTANT.
  - `PathFieldFsSocket` (fs[].socket): same shape. IMPORTANT.
  - `PathFieldDisk` (disks[].path): **broader allow-list = broader
    pre-placement opportunity**. Three valid roots — task_dir, the
    per-sandbox `/var/zeroship/ch/<sbx>/`, AND any operator-supplied
    content-addressed root. A symlink at
    `/var/zeroship/ch/<this-sbx>/workspace.img` → `/var/zeroship/ch/
    <other-sbx>/workspace.img` would (lexically) pass the allow-list
    and have CH attach another tenant's workspace as a block device.
    Pre-condition: write access to this sandbox's persistent dir,
    which the runtime tenant already has — **this materially raises
    the severity ceiling vs. R19-S1's r19 framing**, where the only
    attacker write-target was the alloc dir.
- **Action**: extend `validatePathByKind` with
  `filepath.EvalSymlinks` on the cleaned path before the
  `hasPathPrefix` step. The existing comment at `:99-107` (carried
  from C-7-LT-4) already names the design seam. Two unit-test
  shapes per kind suffice (allow-symlink-inside, reject-symlink-
  outside). NB: `EvalSymlinks` fails on non-existent paths; the
  driver should treat the symlink-not-yet-materialised case as
  benign (rewriter runs before CH attaches disks) — confirm by
  running against a fresh alloc.

### [R20-S2] `driverConfig.SandboxId` flows unvalidated into the per-sandbox allow-list root on the restore branch (NEW)

- **Files**: `nomad-driver-ch/ch/restore_task.go:273-281` waives
  `isTypedID(cfg.SandboxId)` ("snapshot already carries that
  material"); l.350 passes the same `driverConfig.SandboxId` into
  `rewriteConfigJSON(..., driverConfig.SandboxId, ...)`.
  `config_rewrite.go:131-136`'s `sandboxPrefix` does
  `filepath.Join("/var/zeroship/ch", sandboxID)`.
- **Symptom**: `filepath.Join` Cleans the argument. A SandboxId of
  `"../foo"` yields prefix `/var/zeroship/foo/`. A SandboxId of `""`
  yields `""` (handled — falls through to task_dir + content roots
  only). Any non-empty value with `..` or absolute-path-injection
  widens the allow-list silently.
- **Trust model today**: SandboxId is sourced from the controller's
  job-submit (Nomad task config `sandbox_id`), which is built from
  the controller's `AppRecord` — tenants do not write this field.
  Severity is therefore defense-in-depth, **not** an exploitable
  bypass at HEAD. But the cold-boot validator at
  `start_task.go:476-478` exists for the analogous reason (kernel-
  cmdline injection) and the restore branch's "snapshot already
  carries that material" comment is **incorrect for the rewriter's
  consumption path** — the rewriter consumes the *driver-config*
  SandboxId, not the snapshot's.
- **Action**: call `isTypedID(driverConfig.SandboxId)` (or a
  rewriter-local equivalent that ALSO rejects `..` even in the
  presence of other allowed chars — `isTypedID`'s `[0-9a-zA-Z_]`
  whitelist already excludes `/` and `.`, so a single call suffices)
  at the top of `startTaskRestoreBranch` before passing into
  `rewriteConfigJSON`. Severity IMPORTANT (defense-in-depth on a
  trust-boundary input that the cold-boot path already validates).

### [R20-S3] Driver v6/v7 binary pulled from GCS with no SHA256 verification (NEW)

- **Files**: `crates/sandbox/scripts/gcp-worker-startup.sh:146-159`
  (`gs_pull` helper) and `:176` (`gs_pull nomad-driver-ch.v7 …`).
- **Symptom**: `gs_pull` is `gsutil cp` + 5-retry + `chmod`. **No
  SHA256/SHA512 verification** against a pinned digest. Object
  naming (`.v7`) gives version pinning at the namespace level but
  not integrity: a `gsutil rewrite` or fresh upload to the same
  object name produces a different blob with the same fetch URL.
- **Threat model**: the artifact bucket (`$ARTIFACT_BUCKET`) is
  presumably IAM-locked; any compromise of bucket-write IAM (or
  the build pipeline that uploads to it) substitutes the driver
  binary on every worker boot. The driver runs as root inside the
  Nomad agent, on the host the workers run on — full sandbox-host
  compromise. R13-S1's `--scopes=storage-rw` (still open ≥7
  rounds) **directly compounds this**: a compromised worker has
  storage-rw on the same bucket the binary is pulled from, closing
  the loop to a worm-style propagation across the fleet on next
  reboot.
- **Action**: add a per-object SHA256 manifest to the bucket (or
  ship the pinned hashes in the controller config) and have
  `gs_pull` compute + compare before `chmod`. Fail-loud on
  mismatch. The Go driver's `--version` surfaces the embedded
  gitSHA (`gcp-worker-startup.sh:179`) — that's nice-to-have for
  ops but does not authenticate the binary it was extracted from.
  Severity IMPORTANT given the closed loop with R13-S1.

## MINOR

### [R20-S1] `ContentAddressedRootfsRoots` config slot widens the allow-list root set; currently unset (NEW)

- **Files**: `nomad-driver-ch/ch/driver.go:144-160` (Config field +
  hclspec). `gcp-worker-startup.sh:194-200` ships an EMPTY config
  block today — slot is unused.
- **Symptom**: when an operator populates the list, every entry
  becomes a trust-anchor root for `PathFieldDisk`. The HCL spec
  has `false` (optional) but no validation that entries are
  absolute, no rejection of overlapping prefixes with `/etc`,
  `/`, `/var/lib/nomad/alloc`, etc. A misconfigured entry like
  `/` would accept any disk path; `/var/zeroship/ch` would re-
  enable cross-sandbox cross-tenant attach (collapsing the
  per-sandbox prefix isolation).
- **Why MINOR-not-IMPORTANT**: pre-condition is admin-side
  misconfiguration of the Nomad client stanza; the runtime
  trust-boundary is intact (no tenant influences the list).
- **Action**: when this slot starts being populated, add admin-side
  validation: entries must be absolute paths, must NOT overlap
  with the per-sandbox prefix, and must NOT be `/` or `/var`. A
  startup-time `validateContentAddressedRoots(p.config)` would
  fail-loud before the driver registers, preventing silent
  misconfiguration.

## Cross-lens consensus

- **api-surface r20 (R20-API1)**: security confirms the three-
  rewriter co-existence is a versioning gap on the artefact, NOT
  an immediate authz bug. The per-rewriter allow-lists are
  consistent in spirit (all reject `..`, all require absolute, all
  enforce containment) but DIFFER on one axis: the bash wrapper
  has `os.path.realpath`; the Go driver does not (R19-S1-PostLT6
  above). **If wrapper retires post-cutover, the security posture
  on the symlink axis regresses unless R19-S1 closes first.**
  api-surface's recommendation to emit `_zsbx_path_schema_version`
  on snapshot capture is the right vehicle; security supports
  but adds: pin the realpath-or-not decision in the schema too.
- **concurrency / architecture (driver-side)**: C-7-LT-6 closes
  the smoke-r16 functional regression cleanly. Security has no
  objection to the per-field shape; objections are at
  R19-S1-PostLT6 (realpath) and R20-S2 (SandboxId validation).
- **R19-API1 closure (api-surface r20)**: cross-confirmed. Body
  literal is operator-facing prose with no internal identifiers.
  Security signs off on the closure.

## Lens hand-off

- **Architecture / driver maintainers**: R19-S1-PostLT6 — adding
  `filepath.EvalSymlinks` to `validatePathByKind` is now the
  highest-impact single fix on this branch. Pair with R20-S2
  (`isTypedID` on restore branch's SandboxId before rewriter
  consumption) — both land in the same file pair.
- **Ops / cluster bring-up**: R20-S3 is the highest-impact open
  item not yet on anyone's plate. Pair with R13-S1 closure (≥7
  rounds) — together they close the worm-propagation surface.
  Add an `--sha256-manifest` source for `gs_pull` and ship the
  expected hashes in the worker config.
- **api-surface r21**: R20-API1's `_zsbx_path_schema_version`
  marker is the natural vehicle for pinning realpath-policy
  alongside field-coverage. Coordinate with security before
  landing so the version increment captures both axes.

## Carry-forward open at HEAD `8718120b`

- R13-S1 (IMPORTANT, ≥7 rounds) — `provision-gcp-cluster.sh:286`
  `--scopes=storage-rw` + default GCE SA. Compounds R20-S3.
- R9-S3 (IMPORTANT, ≥11 rounds) — `snapshot_handler.rs:417`
  stamps `Some("v1")` regardless of AEAD posture.
- R18-S1 (IMPORTANT) — `match_rfc1918_at` covers 5 of ~10 IPv4
  ranges in the SSRF blocklist.
- R15-S3 (MINOR) — fence cap 30 s undocumented.
- R17-S2 (MINOR) — KEK file implicit-root owner; no explicit
  `chown root:root`.
- R18-S2 (MINOR) — fence-error IP leak into scoped log target.

## Counts

- CRITICAL: 0 new; carry: 0.
- IMPORTANT: 3 new (R19-S1-PostLT6 update, R20-S2, R20-S3);
  carry: R13-S1, R9-S3, R18-S1.
- MINOR: 1 new (R20-S1); carry: R15-S3, R17-S2, R18-S2.
- Total NEW this round: 4.
