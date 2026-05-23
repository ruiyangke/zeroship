# Sandbox/snapshot-restore — security r11 review

Date: 2026-05-25 (UTC)
HEAD at audit: `4f441a20`
Round 11 of N (security lens). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/scripts/**`.

## Summary

3 findings (1 CRITICAL elevation, 1 IMPORTANT new, 1 MINOR new). 1
r9 finding CLOSED at HEAD (R7-API2 documented at `c8000537`). The
fourth sibling of the R9-S4 uid-check family (`lib.rs::load_admin_token`)
is CONFIRMED OPEN and elevated to CRITICAL here (R11-S1): same
mode-only / no-uid-check shape as the three closed siblings, but the
blast radius is full operator API (cross-tenant list, GDPR export,
GDPR delete) rather than a single key file. R11-S2 (new MINOR)
pins the parallel host-id file (`/var/lib/zeloship/sandbox/state/host_id`)
which is read without mode or uid validation — operator-hardened in
practice but a defense-in-depth gap that matches the pattern this
cycle has been closing. R11-S3 (new MINOR) flags an AAD-binding gap
in the snapshot-AEAD scheme that AMPLIFIES R9-S2: the per-chunk AAD
binds only `"zsbx-snap" || chunk_index`, NOT `sandbox_id` or
`snapshot_taken_at`, so a R9-S2-class nonce reuse turns from
key-stream leak (already catastrophic) into ALSO an authenticator
forgery — without sandbox_id/taken_at in AAD, an attacker who recovers
the key stream can cut-and-paste authenticated chunks across snapshots
of the same sandbox (and, in the worst case, ACROSS sandboxes if the
DEK ever collides). R9-S1/S2/S3/S5/S4d, R10-S1/S2/S3 all carry forward
unchanged at HEAD. T-7 (nomad-driver-ch ChPlugin jobspec mode) is
PRESENT IN `stash@{0}`, NOT in HEAD — flagged for r12 as the future
controller→Go-driver boundary that will need its own validation pass.

## Findings (NEW since r10)

### [R11-S1] `lib.rs::load_admin_token` MISSING owner-uid check — fourth sibling of the R9-S4 family, blast radius = full operator API (CRITICAL, security-r11)
- **Files**: `crates/sandbox/src/lib.rs:924-957`
  (`load_admin_token`); production wiring at `:580-587`.
- **Symptom**: The loader checks
  `meta.permissions().mode() & 0o777 != 0o400` at line 938 but
  does NOT check `meta.uid() != 0` (compare the closed siblings —
  `snapshot_aead.rs:198-204` (R9-S4 at `cca1e74d`),
  `persist.rs:355-361` (R9-S4b at `e4e5db60`),
  `db.rs:841-847` (R9-S4c at `2c10f63a`). The closed siblings'
  test pattern (positive arm requires root, negative arm verifies
  non-root refusal) is absent — `lib.rs:1465-1510` exercises None,
  good-mode, bad-mode, empty, and trailing-newline trim only.
- **Threat model**: A non-root local attacker who can write to a
  parent directory in `SANDBOX_ADMIN_TOKEN_PATH`'s prefix BEFORE the
  controller boots can:
  (1) `umask 0277; echo "attacker-known-token" > /that/path`
  (2) `chmod 0400 /that/path` (still owned by the attacker uid)
  (3) Wait for the controller to (re)boot.
  (4) Issue `curl -H "Authorization: Bearer attacker-known-token"
      https://controller/admin/sandboxes` and successfully list ALL
      sandboxes across ALL tenants. Same path works for
      `GET /admin/users/{id}/export` (GDPR export → full PII dump)
      and `DELETE /admin/users/{id}` (GDPR cascade delete).
- **Why it matters**: The three closed siblings (R9-S4/4b/4c) each
  protect ONE key: snapshot KEK (single-tenant impact via decrypt of
  artifacts), AEAD-sealed-record key (single-tenant persist auth
  fixtures), pg password (controller's db credential — bigger). This
  fourth one is bigger still: a single accepted attacker token grants
  the full operator surface (`admin_handlers.rs:44-61`) — list,
  detail, per-user sandboxes, per-user shares, export, DELETE, hosts.
  The closed-sibling commits' bodies say "matches R9-S4 (snapshot KEK)
  and R9-S4b (AEAD key) sibling invariants" — this fourth one was
  conspicuously absent from that list (the test file even lacks the
  uid-arm tests that the three closed loaders all carry now).
- **Where this differs from R9-S4 et al**: blast radius. The KEK
  loads bytes for a SINGLE cryptographic primitive (snapshot AEAD
  rotation can recover the previous state); the admin token IS the
  trust anchor for `/admin/*` itself — there is no second factor
  (`admin_handlers.rs:23-28` explicitly defers JWT to "Phase 5").
  Recovering from a poisoned admin token = key rotation + audit
  every `/admin/*` request log line + GDPR re-notify if the export
  endpoint was hit.
- **Action**:
  (a) Mirror the closed-sibling shape exactly. After the mode check
      at line 938, add:
      ```rust
      use std::os::unix::fs::MetadataExt as _;
      let uid = meta.uid();
      if uid != 0 {
          return Err(format!(
              "SANDBOX_ADMIN_TOKEN_PATH={path:?}: owner uid {uid} \
               != 0 (refusing to load; chown root:root the file)"
          ));
      }
      ```
  (b) Add the two parallel tests the closed siblings ship:
      `loader_refuses_non_root_owned_file_at_mode_0o400` (negative,
      runs without root) + `loader_accepts_root_owned_file_at_mode_0o400`
      (positive, gated `#[ignore]` unless `nix::unistd::geteuid().is_root()`).
      The existing `loader_reads_token_when_mode_0o400` test masks
      this gap because every CI runner is running as the
      attacker-equivalent uid (non-zero) and the test still passes.
  (c) Verify the same loader shape exists nowhere else in the
      workspace (one more sweep of `0o400` checks confirms there is
      no fifth sibling). See "Threat-model audit of secret-file
      loaders" below — confirmed only these four in `crates/sandbox/**`.

### [R11-S2] `host_id` file read without mode/uid validation — defense-in-depth gap on the HA peer-identity anchor (MINOR, security-r11)
- **Files**: `crates/sandbox/src/db.rs:1094-1105`
  (read at `host_id_file_path()`); writer at `:1129-1138` (writes
  `0o600` mode, no chown). Path resolution at `:1122-1127` (default
  `/var/lib/zeroship/sandbox/state/host_id`).
- **Symptom**: When `SANDBOX_HOST_ID` env var is unset, the
  controller reads `<SANDBOX_PERSIST_DIR>/state/host_id` with no
  mode check, no uid check, and no shape check beyond
  `Uuid::parse_str`. A non-root attacker who can write to that
  directory (e.g., during fresh-controller bootstrap on a machine
  the attacker shared / staged) can:
  (1) `mkdir -p /var/lib/zeroship/sandbox/state`
  (2) `echo <a-peer's-uuid> > /var/lib/zeroship/sandbox/state/host_id`
  (3) Wait for controller boot. The controller now self-identifies
      as the spoofed peer.
- **Threat model**: HA peer-takeover poisoning. With the controller
  carrying a peer's host_id, the lease/heartbeat protocol in
  `lib.rs` (start_health_loop + heartbeat) starts writing as if it
  were that peer. The CAS-fenced `claim_orphan_transient_for_recovery`
  at `db.rs:2552-2578` defends against self-recovery
  (`expected_host_id != self.host_id`) — but the spoofed identity
  IS the live peer, so the defense is bypassed: the attacker-aliased
  controller can take over the live peer's transients. Combined with
  R11-S1 (admin token, separate failure class), this is a
  cluster-wide DoS / state-confusion path.
- **Why it matters**: In production
  `/var/lib/zeloship/sandbox/state/` is owned by root with mode 0700
  (operator hardening — see `docs/runbooks/local-dev.md`), so the
  attacker can't write there. This is squarely defense-in-depth: if
  the operator's mount mode drifts to 0755 or if the path is
  redirected via a typo'd `SANDBOX_PERSIST_DIR`, the controller
  silently accepts whatever lives at `state/host_id`.
- **Action**:
  (a) On read, enforce `mode == 0o600` + `uid == 0` (or `uid ==
      effective_uid_of_controller`, whichever the operator-script
      sets). The writer already emits 0o600 (line 1135), so the
      check is symmetric.
  (b) Reject non-`hst_*`-prefixed UUIDs in the file (the env-var
      path at `:1080-1090` already does this — the file path
      should match).
  (c) Document the threat: "if `SANDBOX_PERSIST_DIR` is set to an
      attacker-writable location, host identity is forgeable" in
      the runbook.

### [R11-S3] Snapshot AEAD per-chunk AAD does NOT bind `sandbox_id` or `snapshot_taken_at` — R9-S2 nonce-reuse promotes to forgery, cross-snapshot chunk-substitution feasible if DEK ever collides (MINOR, security-r11)
- **Files**: `crates/sandbox/src/snapshot_aead.rs:324-330`
  (`chunk_aad`); `:143-145` (`AAD_PREFIX = b"zsbx-snap"`); used at
  `:428-434` (encrypt) and `:552-558` (decrypt).
- **Symptom**: The AAD bound into each chunk's Poly1305 tag is
  `"zsbx-snap" || chunk_index BE` — a fixed 13-byte prefix plus the
  4-byte counter. Neither `sandbox_id` nor `snapshot_taken_at`
  appears. The DEK derivation DOES bind both
  (`derive_dek` at `:284-296` includes
  `salt = sandbox_id || snapshot_taken_at_be_u64`), so under normal
  operation the chunk tag binds them transitively via the key. The
  problem: AAD is the LAST line of defense when the key/nonce binds
  fail.
- **Threat model**: Two scenarios where AAD-only binding to
  `(sandbox_id, taken_at)` would have saved the day:
  (1) **DEK collision via R9-S2 path** (1-second timestamp):
      A retry-after-failure scenario (snapshot rolled back from
      `snapshotting → running`, then re-snapshotted within the same
      wall-clock second) produces the SAME `derive_dek` output AND
      the same `derive_nonce_prefix` output for two distinct
      ciphertexts. ChaCha20-Poly1305 forfeits all confidentiality
      and integrity under nonce reuse. With sandbox_id+taken_at in
      AAD, the second put would at least fail-closed on decrypt
      (taken_at mismatch in the on-disk header would not match the
      AAD bound at encrypt time) — turning a silent data-corruption
      class into a loud reject-and-rollback.
  (2) **Cross-snapshot chunk replay** under any future bug class
      where the DEK is reused (HKDF info-string typo, KEK rotation
      with stale-cached DEK, test seam leakage to prod): an attacker
      who can plant a single tampered chunk in the L2 GCS bucket
      could swap chunk N from snapshot-2 into snapshot-1 (same
      sandbox, different taken_at). Today the only check is the
      file-wide SHA-256 in pg (`SnapshotMetadata.sha256`); the
      per-chunk authenticator would catch it ONLY if the AAD bound
      taken_at. (The SHA-256 catches the file-level tamper, but
      that's a 2nd defense; per-chunk should be self-contained.)
- **Why it matters**: This is a posture observation, not a directly
  exploitable defect TODAY (the SHA-256 in pg catches L2 tampering
  at the file granularity, and the DEK reuse class is itself R9-S2
  which is open IMPORTANT). The find is: R9-S2's "1-second timestamp
  granularity" finding has a TWIN — even if R9-S2 is closed via
  millisecond/nanosecond resolution, the AAD-only-prefix shape
  leaves the per-chunk integrity weaker than it should be. Both fixes
  are cheap; pairing them closes the class.
- **Action**: Extend AAD to bind the snapshot identity:
  ```rust
  fn chunk_aad(
      sandbox_id_bytes: &[u8],
      snapshot_taken_at_unix_secs: u64,
      chunk_index: u32,
  ) -> Vec<u8> {
      let mut a = Vec::with_capacity(
          AAD_PREFIX.len() + sandbox_id_bytes.len() + 8 + 4
      );
      a.extend_from_slice(AAD_PREFIX);
      a.extend_from_slice(sandbox_id_bytes);
      a.extend_from_slice(&snapshot_taken_at_unix_secs.to_be_bytes());
      a.extend_from_slice(&chunk_index.to_be_bytes());
      a
  }
  ```
  Wire-incompatible with existing on-disk artifacts — the encrypt
  AND decrypt sides must change together. Compatibility plan:
  (a) Bump `FILE_VERSION` from `0x01` to `0x02` in the header at
      `:62-68`; (b) decrypt accepts both versions, encrypts only
      `0x02`; (c) deprecate `0x01` after the L1/L2 churn rotates
      every artifact (typical sandbox sweep window ≤ 1 week).
  Lower-risk alternative if a wire bump is judged too heavy:
  derive the AAD prefix from the DEK (one extra HKDF-expand call
  with `info = "zsbx-snap-aad-v2"`); on-disk format unchanged
  because the AAD is computed, not stored. But this gives weaker
  binding (DEK already encodes sandbox_id+taken_at, so the AAD is
  redundant with DEK — defeats the point). The wire bump is the
  right shape.

## Verified open carry-forward (unchanged at HEAD)

- **R9-S1** (CRITICAL) — `nomad-vm-wrapper.sh:476-498` anchored
  prefix regex `^/opt/nomad/data/alloc/[^/]+/[^/]+/local(/|$)` —
  non-matching paths pass through (e.g.
  `disks: [{"path":"/etc/shadow"}]` reaches CH `--restore`).
  `snapshot_aead.rs:28-35` scope comment still pins config.json
  AEAD-exempt.
- **R9-S2** (IMPORTANT) — `snapshot_aead.rs:609-612`
  `d.as_secs()`. Same-second re-snapshot of the same sandbox →
  identical DEK + nonce_prefix → ChaCha20-Poly1305 nonce reuse →
  key-stream leak. See R11-S3 above for AAD-amplification class.
- **R9-S3** (IMPORTANT) — `snapshot_handler.rs:417`
  `Some("v1")` hard-coded passed into `update_snapshot_metadata`
  regardless of `self.root.is_some()`. Posture leak via the
  `ch_version` suffix discrepancy still as r10 described.
- **R9-S4d** (CRITICAL) — superseded by R11-S1 above
  (same finding, elevated and pinned with the closed-sibling commit
  evidence + concrete action). The verification re-read of
  `lib.rs:924-957` confirmed mode-only / no-uid-check.
- **R9-S5** (IMPORTANT) — restore branch:
  `restore_handler.rs:1327-1356` env block has NO `ZSBX_SANDBOX_ID`;
  `nomad-vm-wrapper.sh:394-396` only echoes (informational) when the
  env IS present in the alloc, but the restore-path Nomad job doesn't
  set it. Asymmetric with cold-boot.
- **R10-S1** (IMPORTANT) — symlink-follow on R9-S4 KEK loader at
  `snapshot_aead.rs:185-217` + same shape at `persist.rs:333-369`
  (R9-S4b loader, now uid-checked but still symlink-traversing).
  `std::fs::metadata` + `std::fs::read` re-resolve the path
  independently. `O_NOFOLLOW` + `fstat` is the structural cure.
- **R10-S2** (IMPORTANT) — `restore_handler.rs:294-298`:
  ```rust
  let _ = compio::runtime::spawn_blocking(move || {
      backend_for_teardown.teardown_restore(...);
  }).await;
  ```
  Still discards the `JoinError` — no log on panic / cancellation.
  Re-confirmed unchanged at HEAD `4f441a20`.
- **R10-S3** (MINOR) — `restore_handler.rs:1159-1202`
  `teardown_restore` order is (1) Nomad DELETE → (2)
  `unregister_restored` → (3) `release_vm_index` always. Step (3)
  runs even when step (1)'s `nomad_delete_blocking` errored
  (which is just `tracing::warn`'d at line 1165-1170). Verified
  unchanged at HEAD.
- **R10-S6** (MINOR) — `handlers.rs:138-154`
  `read_sandbox_id_from_sources` accepts any non-empty string. The
  wrapper-side validator at `nomad-vm-wrapper.sh:222` is the
  primary fence today. Posture observation.
- **R9-S6 / S7 / S8** (MINOR) — unchanged at HEAD. `/_clock_resync`
  agent-body embed (256 chars verbatim) at
  `restore_handler.rs:1650-1660` journald leak;
  `/livez|/readyz|/metrics` unauth; admin endpoints lack per-bearer
  rate-limit.

## Closed by recent commits

- **R7-API2** documented at `c8000537` — capability list reframed
  as a versioned diagnostic manifest with per-entry semantic, not
  a uniform negotiation surface. `clock.resync-v1` annotated
  "Mandatory (not feature-detected)" with a regression-guard test
  `mandatory_clock_resync_v1_present` at
  `version.rs:172-188`. No wire string removed; no capability list
  shrunk; the `PROTOCOL_VERSION: u32 = 1` const unchanged. Pure
  documentation closure with a tripwire test — clean.
- **R9-S4** at `cca1e74d` (snapshot_aead.rs KEK uid check) —
  closed in r10, re-confirmed at HEAD.
- **R9-S4b** at `e4e5db60` (persist.rs AeadKey uid check) —
  closed in r10, re-confirmed at HEAD.
- **R9-S4c** at `2c10f63a` (db.rs pg-password uid check) —
  closed since r10, re-confirmed at HEAD via direct re-read of
  `enforce_password_file_mode` at `db.rs:824-851`.

## Threat-model audit of secret-file loaders

Every site in `crates/sandbox/**` where a security-sensitive file
is loaded with a mode/uid check. Results from
`Grep("permissions\(\)\.mode\(\)|0o400|0o600", path=crates/sandbox/src/)`,
de-duplicated to the loader functions:

| Loader | File:Line | Mode check | UID check | Status |
|---|---|---|---|---|
| `RootKek::from_path` (snapshot KEK) | `snapshot_aead.rs:185-217` | mode 0o400 (line 193) | uid 0 (line 198) — R9-S4 cca1e74d | **closed** |
| `AeadKey::from_path` (sealed-record key) | `persist.rs:333-369` | mode 0o400 (line 348) | uid 0 (line 355) — R9-S4b e4e5db60 | **closed** |
| `enforce_password_file_mode` (pg password) | `db.rs:824-851` | mode 0o400 (line 835) | uid 0 (line 841) — R9-S4c 2c10f63a | **closed** |
| `load_admin_token` (admin bearer) | `lib.rs:924-957` | mode 0o400 (line 938) | **MISSING** | **R11-S1 CRITICAL** |
| `host_id_file_path` read | `db.rs:1094-1105` | none | none | **R11-S2 MINOR** |
| `load_pubkey_from_path` (agent-side controller pubkey) | `sandbox-agent/src/auth.rs:116-148` | none (pubkey not secret) | none (pubkey not secret) | **OK** (public-by-design) |

Sweep is complete for `crates/sandbox/src/**` — no fifth secret
loader exists. The agent crate's `load_pubkey_from_path` is correctly
permissive: the pubkey is non-secret and the file is operator-mounted
read-only (ConfigMap shape). All five non-pubkey loaders SHOULD share
the mode + uid invariant; one of five is missing it (R11-S1).

## /\_clock\_resync path regression check

`sandbox-agent/src/sig.rs:420` — `verify_kind_skew_bypass` still
`pub(crate)`. The five test arms at `:1432-1582` exercise: skew
bypass forward/backward, signature reject, body tamper reject,
nonce-replay reject, and malformed-signature reject. The nonce LRU
at `:285` is `Mutex<LruCache<String, u64>>` constructed with
`NONCE_CACHE_CAPACITY` (NonZero) — bounded by design. No regression
since r7's API1 closure; no new attack surface introduced by the
r10 closures.

## T-7 (controller→Go-driver boundary) — pre-emptive flag for r12

`stash@{0}` (NOT committed, NOT in `4f441a20`) introduces a
`TaskDriverMode::{RawExec,ChPlugin}` enum + `build_nomad_job_json_with`
in `nomad_ch.rs` that translates `ZSBX_*` env vars into a TYPED
`TaskConfig` block under the `ChPlugin` arm (driver name `"ch"`,
fields `vm_index, sandbox_id, kernel, cpus, memory_mb, restore_from,
workspace_img, user_home_img, pubkey_hex, subnet_base_octet,
disks/fs/net`). The controller validates none of these fields BEFORE
sending the JSON to Nomad — the Go driver's HCL decoder is the only
type/range fence on the wire today.

This is an UNCOMMITTED proposal, not the canonical posture per the
prompt directive, but worth recording the future shape so r12+
remembers to audit:

- `vm_index` (uint16) — controller-side allocator yields 1..ceil;
  no schema constraint visible in the stash. If the Go driver
  trusts the field as a tap-name suffix (`zsbx-nm-${vm_index}`),
  an off-by-one or controller bug → tap collision.
- `sandbox_id` (string) — passed verbatim from `Uuid::simple()`
  (32-hex, no hyphens); the wrapper's `[!0-9a-zA-Z_]` validator
  (R10-S6 fence) is BYPASSED under `ChPlugin` — the Go driver is
  now the sole validator. Confirm the Go side rejects non-hex.
- `pubkey_hex` (string) — should be exactly 64 hex chars; the
  wrapper's hex check at script-line ~150 is bypassed in `ChPlugin`.
- `kernel` (string) — controller derives `runtime_dir.join("vmlinuz")`
  and ships the host path verbatim. If the controller is compromised
  to set `kernel = "/etc/shadow"`, the Go driver would open
  /etc/shadow as a vmlinux image and CH would fail to boot — not a
  privesc, but a path-traversal-style sanity check belongs at the
  driver decoder.
- `restore_from` (string) — same path-traversal concern as
  `disks[].path` in R9-S1 (Python rewriter only touches the
  Nomad-alloc prefix). Under `ChPlugin` the driver receives
  `restore_from` BEFORE the wrapper's rewriter would run; we need
  to confirm the Go driver does its own anchoring.

Action for r12 when T-7 lands: cross-reference
`nomad-driver-ch/ch/task_config.go::TaskConfig` HCL decoder for
field-level constraints, and add controller-side pre-validation if
the driver-side check is weak. The current stash adds NO
controller-side validator — the assumption is "Go driver is the
fence." Verify that's true before the flag flips on cluster.

## Counts

- CRITICAL: 1 (R11-S1; R9-S4d effectively rolled into R11-S1)
- IMPORTANT: 0 new (carry: R9-S2, R9-S3, R9-S5, R10-S1, R10-S2)
- MINOR: 2 (R11-S2, R11-S3; carry: R10-S3, R10-S6, R9-S6/S7/S8)
- Total NEW this round: 3

r10-closed: 1 (R7-API2 at `c8000537`).
r9-carry (unchanged at HEAD): 8 (R9-S1, R9-S2, R9-S3, R9-S5,
R9-S6, R9-S7, R9-S8 + R10-S1/S2/S3/S6 as r10 carries).
