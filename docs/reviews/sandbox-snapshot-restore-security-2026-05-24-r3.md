# Security Review — sandbox-snapshot-restore (2026-05-24 r3)

Scope: `crates/sandbox/**` + `crates/sandbox-agent/**` on
`feat/sandbox-snapshot-restore` @ `03d15012`. Read-only.

## r1/r2 closure status

- **r2 #1 (A6) — CLOSED at `d9b95c2e`** and **r2 #2 (other pub creds, A6b) — CLOSED at `2380605e`.** `crates/sandbox/src/lib.rs:61,83,104,139,157-159` are now `pub(crate)`. New `with_*` builders (lines 272-377) are the single legal out-of-crate write path. Field swap-after-boot no longer compiles.
- **C2 (persist.delete leak on snapshot-aware stop) — CLOSED at `78320b56`.** `backend/nomad_ch.rs:1192` gates `persist.delete` behind `if remove_host_dir`, so `stop_preserving_state` no longer wipes the sealed signing key that the wake path needs.
- **r1 #6 (init.sh bash `\xNN` decoder) — CLOSED at `fce3e208`.** `scripts/init.sh:1` is `#!/bin/bash`; the decoder is no longer dash-fragile.
- **r1 #2 (GCS verify) — closed in r2.** Open from prior rounds: **A1**, **r1 #4 (user_id path)**, **W1 (wrapper sed)**, **r1 #3 (metadata token plaintext+no zeroize)**, **r1 #7 (SHA-256 not bound to sandbox_id)**, **r1 #10 (split vm_index allocator)**.

## Findings

### 1. CRITICAL — A1 confirmed still open at `lib.rs:586-605`; pg-attested `snapshot_aead_dek_id="v1"` lies.
The production store is constructed as `Arc::new(LocalDiskSnapshotStore::new(...))` or `Arc::new(TieredSnapshotStore::new(l1, l2))` — **no `AeadSnapshotStore::new(...)` wrap anywhere on the production path**. Meanwhile `snapshot_handler.rs:358` continues to stamp `Some("v1")` into the pg `snapshot_aead_dek_id` column. An operator reading the audit trail believes ChaCha20-Poly1305 is in force; 1 GB of guest RAM (kernel keyring, app secrets, OAuth tokens, AEAD keys held by other in-VM code) is written plaintext to GCS. With **finding #3** below, that is end-to-end exploitable on any controller with bucket-write.

### 2. CRITICAL — Cross-tenant slot takeover chain (r1 #7 + r1 #10 + B18) is now demonstrably live.
B18 is the first cluster smoke result that exercises a reused `vm_index`. `crates/sandbox/src/backend/nomad_ch.rs:588-600` mints a fresh `signing_key`/`pubkey_hex`/`key_fp` per sandbox; `wait_for_agent_livez` (line 2767+) polls `/version` and rejects the IP until the new agent reports the new fingerprint. The only thing protecting cross-tenant agent-IP impersonation **during the host_fence window** is this fingerprint loop. But: (a) `restore_handler.rs:286-412` does NOT call a fingerprint-bound wait — `wait_for_livez` on the restore path is unsigned (compare `backend.wait_for_livez(...)` at line 389 with the signed version at `nomad_ch.rs:2767`); (b) `restore.rs:309-367` flips `unreachable → running` after a single signed probe, no re-check on flip — r1 #8 unresolved. Combined: any window where a stale agent answers `/livez=200` while the controller holds the right pubkey lets a wake CAS resurrect the wrong row at the right IP. The B18 reproducer (11/16 c=4 cycles 401) is the benign symptom; the malign version is a stale tenant whose pubkey **accidentally** equals the new one (low) or whose `/livez` flip-races the row CAS (real).

### 3. HIGH — `restore_handler.rs:389` skips fingerprint attestation on wake.
`backend.wait_for_livez(sandbox_id, snap.vm_index)` polls only the unsigned `/livez`. The cold-boot path goes through the signed `wait_for_agent_livez` with `expected_fp = key_fp`. On wake we trust whatever agent is answering on `10.<base>.<100+idx>.2:7777` to be ours. If a worker reboot orphan-pruned an old VM and a stale tenant happens to be on the same IP, restore boots into the wrong VM and records `Running` against pg row B. Deferred backlog [T5] tracks this but is labeled "defer until cross-worker restore"; it's a Phase-B blocker, not a defer.

### 4. HIGH — Pub accessors `config()` / `database()` (lib.rs:334-344) leak `Arc<Database>` whose `Debug` impl spills the DSN.
A6b correctly restricted the **fields**, but exposed `pub fn config(&self) -> &SandboxConfig` and `pub fn database(&self) -> Option<&Arc<Database>>`. `SandboxConfig.token` is still `pub` (A7 open per deferred). A read accessor returning `&SandboxConfig` lets any out-of-crate consumer call `.token` directly — defeating the A5/A6b model. Worse: integration tests that `println!("{:?}", state.config())` or that pass the `Arc<Database>` into a logging macro spill the embedded pg password through the standard derive-Debug. Recommend: return a builder-derived `SandboxConfigView` that omits credential fields, and add `#[derive(Debug)]` overrides on `SandboxConfig` / `Database` that redact `token` and DSN.

### 5. HIGH — W1 sed is still live and now reachable from a malformed snapshot config.
`scripts/nomad-vm-wrapper.sh:359` keeps the unanchored `sed -i -E "s#…#${NOMAD_TASK_DIR}#g"` rewrite. r1 and r2 both flagged it; no fix. With A1 plaintext, an attacker with bucket-write places a `config.json` whose `disks[].path` contains sed metacharacters (`#`, `&`, `\`), and the substitution either truncates or injects under raw_exec root. The new B17 resume block (lines 388-419) doesn't add a new injection vector — `"$API_SOCK"` derives from controller-trusted `$ZSBX_RUNTIME` — but it does add 30 lines of bash that the deferred [R3-A3] refactor to a Rust sidecar would have replaced wholesale. Every cycle that ships more bash makes W1 harder to retire.

### 6. MEDIUM — `restore_handler.rs:870-877` `cfg.user_home_dir_root.join(user_id)` still has zero typed-id validation (r1 #4 + r2 #7 unresolved).
`user_id` is read out of pg as a plain `String` (`restore_handler.rs:283`). The wrapper's `[ -f "$ZSBX_USER_HOME_IMG" ]` check at `scripts/nomad-vm-wrapper.sh:226` checks existence, not containment under `user_home_dir_root`. A buggy upstream write that lands `user_id = "../shared"` mounts the wrong tenant's home.img into the new VM — both as `/dev/vdc` and via the Nomad `Meta`/`Env` propagation at restore_handler.rs:894/934. Mitigation is one line: `typed_id::parse_usr(...)?` at the restore-submit site.

### 7. MEDIUM — `wait_for_agent_livez` legacy-agent fallback (`nomad_ch.rs:2819-2831`) is a soft fail-open.
On a 200 `/version` with no `pubkey_fingerprint` field, the code logs a warning and returns `Ok(())`. The justification ("/version is signed-auth gated so the agent IS ours") is correct *today* — but a future refactor that adds a public `/version` endpoint (e.g. for prometheus scrape) would silently drop attestation. Recommend: gate the fallback behind a config flag (`SANDBOX_ALLOW_LEGACY_AGENT=1`) so the fail-open requires explicit operator opt-in.

### 8. MEDIUM — `init.sh:93` still depends on `sed`+`printf '%b'` and silently produces partial pubkey on truncated cmdline.
Even with `#!/bin/bash`, an odd-length-divisible-by-two but truncated `zsbx_pubkey=<hex>` produces a shorter-than-32-byte file. init.sh validates even-length (line 77) but not exactly-32-bytes-after-decode. The agent's auth loader (`crates/sandbox-agent/src/auth.rs`) checks the length, so the failure mode is a 401 loop, not silent acceptance — but that's the same B18 signature, hiding any real pubkey-mismatch behind the 401. Add `[ "$PUBKEY_HEX_LEN" -eq 64 ] || exit 1` immediately before the decode.

### 9. LOW — GCS metadata-token cache still unprotected against post-mortem residue (r1 #3, r2 #9 unresolved).
`snapshot_store_gcs.rs:79-85` `CachedToken` keeps the bearer in a plain `String`. Two rounds of review have flagged this; no remediation. Wrap in `zeroize::Zeroizing<String>` (one-line change, mirrors `lib.rs:139`).

### 10. LOW — `ch-remote ping`/`resume` shell-out under `bash -e`+`pipefail` swallows resume failure to a warn (line 405).
`scripts/nomad-vm-wrapper.sh:401-406` runs `ch-remote ... resume` inside a `( ... ) &` subshell and on failure echoes a WARN. The cluster smoke calls this "non-fatal because /livez probe will surface it" — but `/livez` then surfaces a generic timeout, not "VM was paused." Add an explicit exit-code propagation so the controller can distinguish "resume failed" from "VM rebooted into a 401 loop" — improves on-call MTTR.

---

Severity: CRITICAL = exploitable now / lying audit trail; HIGH =
exploitable with one adjacent compromise; MEDIUM = trust-boundary or
race; LOW = operational / defense-in-depth.

Counts: 2 CRITICAL · 3 HIGH · 3 MEDIUM · 2 LOW · 10 total.
