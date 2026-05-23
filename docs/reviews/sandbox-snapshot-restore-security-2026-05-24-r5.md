# sandbox-snapshot-restore — Security Review r5

**Branch HEAD**: `3e8bfad5` · **Scope**: `crates/sandbox/**` + `crates/sandbox-agent/**`
**Prior**: `…security-2026-05-24-r{1,2,3,4}.md`. Verifies S4 closure at `4fd92bef`; re-checks A1/F2/F4/W1; audits B19 register-restored fingerprint plumbing for fail-OPEN; checks inode-sharing of A3-partial hard_link.

---

## CLOSED since r4

- **r4-S4 (raw-driver-error leak in `err()` `message`)** — closed at `4fd92bef`. New `err_safe()` at `admin_handlers.rs:228-245` logs raw `e` via `tracing::error!` and renders fixed-prose `public_msg`. The 30+ leak sites now feed static strings ("database error", "hypervisor error", "internal error"); `format!.*\{e\}` over the file returns zero matches. `map_snapshot_error`/`map_restore_error` at `:1102-1183` route every variant through `err_safe()` except `SnapshotCorrupt` (fixed prose) and `StateMismatch`/`NotFound` (typed-id only). Raw `e` stays in journald; no body or header leak — confirmed.

---

## STILL OPEN

### CRITICAL

**S1. (NEW) Wake `register_restored` silently no-ops when `state.persist=None` — fail-OPEN** — `restore_handler.rs:450-474` + `admin_handlers.rs:1357`. Guard at `:450` is `if let Some(p) = persist { … } else { warn-skip }`. Prod wake passes `state.persist.as_deref()`. **No boot-time gate** ties `snapshot_enabled → persist.is_some()` (`lib.rs:411-460` grep → zero hits). With `SANDBOX_SNAPSHOT_ENABLED=1` + `SANDBOX_PERSIST_AUTH≠1`, every wake returns 200, the VM is reachable on its tap, but the state-map has no entry → `exec`/`stop`/`delete` return `sandbox_not_found` and `vm_index` slot leaks (idempotent-Ok branch in `stop_inner` fires without releasing). Exact pre-B19 symptom under a different cover. Fix: assert in `from_config` refusing startup when `snapshot_enabled && persist.is_none()`.

**S2. (A1) AEAD never wrapped in prod store** — `crates/sandbox/src/lib.rs:573-649`. Unchanged. `snapshot_handler.rs:358` still stamps `snapshot_aead_dek_id="v1"` into pg while on-disk/GCS bytes are plaintext guest RAM.

**S3. (F2) `wait_for_livez_blocking` unsigned** — `restore_handler.rs:1322-1341`. Doc at `:1314-1321` names the missing check. B19 already unseals `signing_key_bytes` at `:451` before `register_restored` at `:460`; the key is in scope. A `verify_version_signature` call between unseal and `register_restored` closes F2 with zero new surface. Currently a tap-subnet attacker that answers `/livez` 200 makes the controller proceed to `register_restored` without challenge.

**S4. (W1) Wrapper `sed -i -E` unanchored on `config.json`** — `crates/sandbox/scripts/nomad-vm-wrapper.sh:359` (+ `:402,416-417` from B17). `sed -i` rename-breaks the hard-link so the canonical L1 isn't directly clobbered. The original RCE-as-raw-exec-root vector — unanchored substitution against attacker-influenceable JSON — is unchanged. R3-A3's Rust sidecar is the structural fix.

### IMPORTANT

**S5. (NEW, A3-partial inode aliasing) Canonical L1 and writable alloc dir share inodes** — `snapshot_store.rs:259-273`. `LocalDiskSnapshotStore::get` hard-links 1 GB `memory-ranges`, `config.json`, `state.json` from `<l1_root>/<sandbox>/` into the alloc dir. (a) CH may mmap `memory-ranges` MAP_SHARED → in-VM dirty-page writeback corrupts the canonical L1; the next wake reads tainted RAM. (b) CH shutdown may rewrite `state.json`. (c) No `set_permissions` post-link — if raw_exec chmods the alloc dir for nomad-task read, the canonical L1 inode is implicitly widened. Mitigation: `reflink_or_copy` (BTRFS/XFS CoW), set link read-only, or copy `memory-ranges` specifically.

**S6. (F4) `pub fn database()` re-leaks DSN** — `lib.rs:329-345` + `db.rs:482-484`. `pub fn database() -> Option<&Arc<Database>>` is out-of-crate callable; `Database::dsn() -> &str` returns the password-bearing DSN. `ApiToken::as_bytes()` is also `pub` so the `pub fn config()` borrow can be drained for the bearer token too. Move integration tests in-crate, or return a redacted view.

**S7. (NEW) `RestoreBackend::register_restored` default `Ok(())`** — `restore_handler.rs:162-170`. Default impl exists for `StubRestoreBackend` but also lets a future prod impl forget to override and silently return Ok. Stacks with S1's persist=None arm — two independent layers each silently swallow the registration. Make required; have stubs implement explicitly.

**S8. (r4-S5) AEAD-failure raw `InvalidArtifact(s)` logged via `error = %s`** — `restore_handler.rs:398-407`. Once A1 lands, leaks crypto-impl internals to journald. Tighten to fixed log message; details at DEBUG only.

### MINOR

**S9. (r4-S7) `error_response` accepts unbounded `impl Into<String>`** — `error_envelope.rs:116-122`. With S4 closed the practical risk dropped, but API still permits multi-KB body on regression. Cap `message` at 512 chars + strip control chars in `into_response`.

---

## Summary

- 4 CRITICAL open (S1 new B19 fail-OPEN, S2=A1, S3=F2, S4=W1).
- 4 IMPORTANT (S5 hard_link aliasing, S6=F4 re-leak, S7 trait default, S8 AEAD log).
- 1 MINOR (S9 envelope cap).
- **r4-S4 closure verified**: `err_safe()` sanitizes 30+ sites; raw stays in journald; no body/header leak.

## Two most critical citations

- `crates/sandbox/src/restore_handler.rs:450-474` + `admin_handlers.rs:1357` (S1: persist=None silently skips `register_restored` and returns 200; no boot-time gate ties `snapshot_enabled` to `persist.is_some()` → fail-OPEN slot-leak + state-map desync).
- `crates/sandbox/src/snapshot_store.rs:259-273` (S5: hard_link aliases canonical L1 `memory-ranges`/`state.json` to writable alloc dir without mode normalization or reflink-CoW; CH dirty-page writeback or alloc-dir chmod silently widens the canonical L1 in place).
