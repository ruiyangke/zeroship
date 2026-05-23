# sandbox-snapshot-restore — Security Review r6

**Branch HEAD**: `29196e0c` · **Scope**: `crates/sandbox/**` + `crates/sandbox-agent/**`
**Prior**: `…security-2026-05-24-r{1,2,3,4,5}.md`. Verifies r5-S1 closure at `7a094786`, B21 closure at `b25a4ea1`; re-checks W1/A1/S5; opens #22 security lens; re-confirms F2 closable in scope.

---

## CLOSED since r5

- **r5-S1 (B19 fail-OPEN: `register_restored` silently no-ops on `persist=None`)** — closed at `7a094786`. `crates/sandbox/src/lib.rs::AppState::from_config` now invokes `assert_persist_required_when_snapshot_enabled(snapshot_enabled, persist.is_some(), test_override)`; production refuses to boot when `SANDBOX_SNAPSHOT_ENABLED=true && persist.is_none()`. Truth-table covered by 5 unit tests; cluster c=4 confirms the legal config boots and the previous `register_restored skipped — persist=None` warning is gone on all 9 wakes.
- **r5-B21 (controller systemd missing `SANDBOX_PERSIST_AUTH=1` + AEAD key)** — closed at `b25a4ea1`. `gcp-worker-startup.sh:319-328` provisions `$ART/sandbox-aead-key` as a 32-byte file under `umask 077`, force-chmod `0o400`, then exports `SANDBOX_PERSIST_AUTH=1` + `SANDBOX_AEAD_KEY_PATH` + `SANDBOX_PERSIST_DIR` in the unit (`:386-388`). Files created as root (no `User=`/`Group=` on the service), so the controller runs as root and CAN read its own 0400 key — no ownership mismatch. Idempotent re-tighten on reboot preserves sealed-record decryptability.

---

## STILL OPEN

### CRITICAL

**S1. (Bug #22 — NEW) Post-wake agent `/exec` returns 401 — controller-side `register_restored` installs the right key, but agent rejects** — `crates/sandbox/scripts/nomad-vm-wrapper.sh:366-369` + `crates/sandbox/scripts/init.sh:53-95` + `crates/sandbox/src/backend/nomad_ch.rs:626-637,1650-1681`. The restore-branch invokes `cloud-hypervisor --restore source_url=file://$DIR` with **no `--cmdline`** — CH uses the snapshot's embedded cmdline, which carries the **original** `zsbx_pubkey=<hex>` from create-time. `init.sh` already ran inside the snapshotted VM, so `/run/keys/controller-pubkey` (tmpfs, captured in `memory-ranges`) is restored verbatim and contains the original create-time pubkey. Persistence::unseal at `restore_handler.rs:451` recovers `signing_key_bytes` from the sealed record (same bytes used at `nomad_ch.rs:870` create-time seal); `register_restored` at `:1658` rebuilds `SigningKey::from_bytes(&signing_key_bytes)`, which derives the SAME verifying key. So in steady state the keys MUST match. The 401 on every wake (Appendix E) means one of:
- **(a) Persistence layer corruption**: `sealed.signing_key_bytes` ≠ the bytes used at create. Check that `nomad_ch.rs:626 random_key32()` output is what gets sealed at `:870`. They share `sk_bytes` in scope, so this should hold — but verify the AEAD wrap/unwrap by hex-diffing the persisted sealed record's recovered `signing_key_bytes` against the freshly-created in-memory `sk_bytes`. If AEAD nonce reuse / DEK derivation drift corrupted the bytes, every unseal returns plausible-looking-but-wrong 32 bytes.
- **(b) Most likely**: agent `/exec` clock skew. `sig.rs:312-316` rejects `|now - ts| > SKEW_S` (5s). A restored VM resumes with the snapshot's wall clock; if `cloud-hypervisor --restore` doesn't re-init the guest RTC and the controller signs with current wall time, the agent's `unix_now()` returns the snapshot-captured time → every controller signature looks `SkewTooLarge`. **Then the agent returns 401 with `AuthFail::SkewTooLarge.as_str() = "skew-too-large"`, not `bad-signature`**. The reviewer-r6 brief reports the agent body is `{"error":"unauthorized"}` — that's the generic 401 envelope; the actual `AuthFail` variant is in the audit log. **Action**: SSH into a wake-failed VM, `journalctl -u sandbox-agent | grep auth_fail` to read the `AuthFail` discriminator. If `skew-too-large`, fix is `hwclock --hctosys` in init.sh OR add `ch-remote add-vdev rtc` post-resume OR have the agent re-sync clock from controller in `/livez`.
- **(c) Less likely**: nonce LRU pollution surviving snapshot. The `Verifier::nonces` LRU was captured in the snapshot. The first controller request post-wake supplies a fresh nonce, which is NOT in the LRU, so this isn't the cause unless skew check passes a nonce already in the captured LRU.

**Security implication of #22**: the 401 is fail-CLOSED — the agent correctly refuses unsigned/skewed/replayed requests. No confidentiality leak. But operationally it blocks every wake. The fix MUST NOT broaden the verifier (`SKEW_S` is 5s by design); the right fix forces the guest clock forward post-resume, OR re-runs init.sh's key-load path post-resume (more invasive — CH `--restore` is supposed to skip kernel init by design).

**S2. (A1, unchanged) AEAD never wrapped in prod store** — `lib.rs:316-345`. `snapshot_handler.rs:358` still stamps `snapshot_aead_dek_id="v1"` into pg while on-disk/GCS bytes are plaintext guest RAM. R5-S1 closure made `state.persist` mandatory but did NOT make the snapshot-store AEAD wrap mandatory; these are two independent crypto knobs. Operators see "encrypted" in pg audit while bytes are plaintext.

**S3. (W1, unchanged across 3+ rounds) Wrapper `sed -i -E` unanchored on `config.json`** — `crates/sandbox/scripts/nomad-vm-wrapper.sh:359`. `sed -i -E "s#/opt/nomad/data/alloc/[^/]+/[^/]+/local#${NOMAD_TASK_DIR}#g"`. With A1 still open, an attacker with bucket-write could substitute a config.json whose unanchored-matching fields carry sed-metacharacters (`#`, `&`, `\`, newline) → arbitrary substitution → RCE as raw_exec root. The hard_link in r5-S5 was claimed to break the link via `sed -i` rename — true at the inode level, but the *substitution itself* runs against attacker-influenceable bytes. R3-A3 (Rust sidecar) remains the structural fix.

**S4. (r5-S5, partially mitigated; still IMPORTANT) `LocalDiskSnapshotStore::get` hard_link aliasing** — `snapshot_store.rs:266-281`. Verified: `hard_link` is used unconditionally same-FS, no `set_permissions(0o444)` on the alloc-side link after creation. CH MAP_SHARED writeback on `memory-ranges` corrupts canonical L1 in place; raw_exec `chmod` on alloc dir widens canonical L1 inode mode. `sed -i` rewrites `config.json` via rename (breaks that link) but NOT `memory-ranges` (which CH may write back). The unit test `local_disk_get_uses_hard_link_when_same_fs` (`:487`) asserts `nlink >= 2` and `ino` equality — proves the aliasing is intentional. Fix: `std::os::unix::fs::PermissionsExt::set_mode(0o444)` on each link immediately after `hard_link()` returns Ok, OR `reflink_copy::reflink_or_copy`.

### IMPORTANT

**S5. (R5-S3, now closable in 3 lines — re-confirmed)** `wait_for_livez_blocking` unsigned probe — `restore_handler.rs:1322-1341` (unsigned) vs `nomad_ch.rs:2767+` (`wait_for_agent_livez` signed). At `restore_handler.rs:450-455` the post-B19 wake path unseals `signing_key_bytes` BEFORE calling `register_restored`. Swap the unsigned probe at `:431` for the signed variant by passing `sealed.signing_key_bytes` (or `Arc<SigningKey>`) into `wait_for_livez`. Closes F2 with zero new surface. Particularly relevant given #22: a signed `/version` probe would have caught the verifying-key/signing-key mismatch at probe-time instead of the first post-wake `/exec`.

**S6. (F4, unchanged) `pub fn database()` re-leaks DSN** — `lib.rs:329-345` + `db.rs:482-484`. Still permits out-of-crate callers to drain `Database::dsn() -> &str` (password-bearing). Move integration tests in-crate or return a redacted view.

**S7. (R5-Q1 / r5-S7, unchanged) `RestoreBackend::register_restored` default `Ok(())`** — `restore_handler.rs:162-170`. Default impl is silent no-op; stacks with S2's persist=None warn arm. Now that R5-S1 closed the fail-OPEN at the AppState level, this is "belt-and-braces" rather than CRITICAL — but a future prod `RestoreBackend` impl that forgets to override will re-introduce the slot-leak/state-map-desync symptom. Make the trait method required; have `StubRestoreBackend` implement explicitly.

**S8. (B21 file-mode validation gap)** `gcp-worker-startup.sh:319-328` provisions `sandbox-aead-key` at 0o400 owned by root. No post-write `stat` check that the mode actually landed (e.g., NFS or weird filesystem could ignore chmod). Controller-side `AeadKey::from_path` is documented to refuse other modes; verify it actually does by reading `crates/sandbox/src/aead_key.rs` — if it does NOT enforce 0o400 on read, an operator who manually copies the file with 0o644 silently degrades from "tmpfs-only secret" to "world-readable".

### MINOR

**S9. (r4-S7, unchanged) `error_response` accepts unbounded `impl Into<String>`** — `error_envelope.rs:116-122`. Cap `message` at 512 chars + strip control chars in `into_response`.

---

## Summary

- **Findings**: 9 (3 CRITICAL open: #22 / A1 / W1; 4 IMPORTANT: S4 hard_link / S5 F2 / S6 F4 / S7 trait default / S8 AEAD file-mode validation; 1 MINOR).
- **Closed since r5**: r5-S1 (boot-time fail-CLOSED assertion at `7a094786`), B21 (env vars + AEAD key file at `b25a4ea1`).
- **#22 security lens**: the 401 is the agent correctly fail-CLOSED-rejecting the controller's signed RPC. Three hypotheses ranked by likelihood: **(b) clock skew on resumed VM** (most likely — CH `--restore` resumes guest clock at snapshot-time, controller signs at wall time, `|now-ts| > 5s`), (a) sealed-record signing-key corruption (verify hex-diff at unseal), (c) nonce LRU survived snapshot (least likely; first wake's fresh nonce should pass). **Single most informative test**: `journalctl -u sandbox-agent | grep auth_fail` post-wake to read the `AuthFail` discriminator (`skew-too-large` vs `bad-signature` vs `replayed-nonce`) — sig.rs:207-217 maps every failure mode to a distinct audit string.

## Two most critical citations

- `crates/sandbox/scripts/nomad-vm-wrapper.sh:366-369` + `crates/sandbox-agent/src/sig.rs:312-316` (#22: restore branch passes no `--cmdline`, snapshot's embedded init runs at snapshot's wall clock; agent's 5s skew window 401s every signed `/exec` until clock re-sync — fail-CLOSED for confidentiality, fail-blocking for liveness).
- `crates/sandbox/src/snapshot_store.rs:266-281` (S4=r5-S5 still open: `hard_link` unconditionally same-FS with no `set_permissions(0o444)` post-link → CH MAP_SHARED writeback on `memory-ranges` or raw_exec chmod on alloc-dir silently widens canonical L1 in place; unit test at `:487` documents the aliasing as intentional).
