# sandbox-snapshot-restore — Security Review r4

**Branch HEAD**: `29afea72` · **Scope**: `crates/sandbox/**` + `crates/sandbox-agent/**`
**Prior**: `…security-2026-05-24-r{1,2,3}.md`. This pass re-verifies open CRITICALs (A1, F2, F4, W1), interrogates A4 (74-site error-envelope migration) and B18 (shared `Arc<Mutex<VmIndexAllocator>>`) for new attack surface, and audits the new `error_envelope.rs::test_helpers`.

---

## CLOSED since r3

- **B18 (cross-tenant slot takeover via private `VmIndexReservations`)** — closed at `b4ddb98b` + `469e22c8`. The chain that allowed sandbox-A's reservation to be invisible to sandbox-B's create-side allocator is gone (`backend/nomad_ch.rs:155-157,388-389` + `lib.rs:617-639`). Lock is `std::sync::Mutex`, only held across sub-microsecond `BTreeSet` ops — no async fence held → DoS via lock-bombing is not practical.
- **A4 wire-shape consistency (envelope drift across 74 sites)** — every error now funnels through `error_envelope::ErrorEnvelope` / `error_response()`. Reserved-key guard at `error_envelope.rs:96-101` prevents `extra` clobbering `error`/`message`. `test_helpers::body_json` is correctly `#[cfg(test)]`-gated at `error_envelope.rs:124` — no production exposure.

---

## STILL OPEN (re-verified)

### CRITICAL

**S1. (A1 re-verified) Production snapshot store still bare; AEAD layer never wrapped** — `crates/sandbox/src/lib.rs:573-649`. The `if config.snapshot_enabled` branch composes `LocalDiskSnapshotStore` or `TieredSnapshotStore<LocalDisk, GcsSnapshotStore>` and stops there. `AeadSnapshotStore` is **not in the chain**, yet `snapshot_handler.rs:358` continues to stamp `snapshot_aead_dek_id="v1"` into pg. Audit trail says "encrypted"; bytes on GCS are guest RAM in plaintext. Materially worse than r1/r2/r3 because A4's now-uniform envelope makes "AEAD enabled" a more credible-looking field in operator dashboards.

**S2. (W1 re-verified) Wrapper `sed -i -E` rewrites attacker-controllable `config.json`** — `crates/sandbox/scripts/nomad-vm-wrapper.sh:359`:
```bash
sed -i -E "s#/opt/nomad/data/alloc/[^/]+/[^/]+/local#${NOMAD_TASK_DIR}#g" \
     "$ZSBX_RESTORE_FROM/config.json"
```
With S1 open, an attacker with GCS-write can craft a `config.json` whose unrelated string fields contain `#`-delimiter chars or back-references that turn the replacement value into a sed expression executed against the guest's own config (e.g., embedding a path that, after substitution, points `disks[].path` at a host-side device node). The B17 fix added more sed pipelines at lines 402, 416-417 — none anchored, none escaping. R3-A3's "move wrapper to Rust sidecar" remains the structural fix.

**S3. (F2 re-verified) `wait_for_livez` is unsigned; pre-resume race window** — `crates/sandbox/src/restore_handler.rs:1166-1185`. The probe is plain HTTP GET against `/livez` with no per-sandbox signing-key challenge. Doc comment at `restore_handler.rs:1159-1165` justifies "no stale tenant because the prior alloc terminated as part of the snapshot's destructive teardown" — but B17's post-resume window now opens a ~9.5s gap during which CH `--restore` has attached the tap and any host that can reach `tap…/30` can answer `/livez` with `200`. Neither A4 nor B18 closes this gap.

**S4. (NEW) `err()` propagates raw driver error strings into the `message` field at 30+ admin sites** — `crates/sandbox/src/admin_handlers.rs:296,322,402,474,496,552,574,618,635,693,711,724,758,770,781,784,858,862,866,881,892,905,919,929,967,971,1078-1081,1115-1119,1199`. Examples:
```rust
Err(e) => return err(500, "pg_query_failed", format!("query: {e}")),
Err(e) => return err(500, "gdpr_delete_events_failed", format!("delete events: {e}")),
SnapshotHandlerError::ChRemote(s) => err(500, "ch_remote_failed", format!("ch_remote: {s}")),
```
`compio_postgres` errors include host:port, schema names, sometimes SQL fragments / row values. The admin endpoints **are** admin-token-gated (so this is not a public leak), but the `code` field is the stable contract — the variable `message` should be elided to a static prose or routed through a sanitizer. Pattern repeats in `admin_handlers.rs:1199` where `lookup_source_vm_ops` error `e` (which may carry Nomad addr / sandbox internal id) flows to a 503 body.

### IMPORTANT

**S5. (NEW) `snapshot_corrupt` 500 reveals AEAD failure separately from checksum failure** — `crates/sandbox/src/restore_handler.rs:347-368`. Both `ChecksumMismatch` and `InvalidArtifact` (the AEAD-auth-failure branch) collapse to `RestoreHandlerError::SnapshotCorrupt`, which the admin handler at `admin_handlers.rs:1113` renders as `"snapshot_corrupt"` with message `"snapshot_corrupt: row marked snapshotted_suspect"`. Good. **But** the tracing line at `restore_handler.rs:360-364` logs `error = %s` where `s` is the AEAD impl's raw error string (potentially algorithm internals). Operators with log access see more than they should — if AEAD ever lands (S1), tighten that log to a fixed string.

**S6. (NEW) F4 — A6b `pub fn database()` and `pub fn config()` accessors re-leak credentials** — `crates/sandbox/src/lib.rs:335-345`. Doc says "out-of-crate integration tests need to read sandbox rows back". `Arc<Database>` carries the DSN; `SandboxConfig` carries `token: ApiToken` (the A7 field still `pub` per `config.rs:19`, in deferred backlog). The setters were locked down; the getters open the same door. Either move tests in-crate (`#[cfg(test)]`-only `pub(crate)`) or strip secrets from the borrowed view.

**S7. (NEW) `error_response` accepts `impl Into<String>` — no length cap, no NUL-byte filter** — `crates/sandbox/src/error_envelope.rs:116-122`. Half the in-crate callers feed `format!("…: {e}")` strings. A pathologically large `e` (e.g., a pg server-error with embedded query+row) renders into the JSON body and ships verbatim. Combined with S4, a single bad query can ship many KB of JSON. Add a cap (e.g., `truncate at 512 chars`) and a `chars().filter(|c| c.is_control()).count() == 0` sanity check in the envelope's `into_response`.

### MINOR

**S8. (NEW) Wake-in-progress is observable via `/livez` from the tap subnet** — see S3. Mostly the same defect, but worth calling out separately for the "what does an attacker who can reach the host's `tap…/30` see?" lens. An unauthenticated probe of the agent_url during the ~9.5s-15s wake window gets a clean `200 OK` from `/livez` — i.e., **wake completion is precisely timeable** by an attacker who knows the per-VM IP. Mitigation: B17's resume happens before the controller's first `/livez` poll succeeds (line 388-419 of `nomad-vm-wrapper.sh`); a side-channel observer sees the same 200 the controller sees. F2's signed-fingerprint check would close this once plumbed.

**S9. (Confirmation) `error_envelope::test_helpers::body_json` is correctly gated** — `crates/sandbox/src/error_envelope.rs:124` `#[cfg(test)]`. No production leak; the only consumers are the unit tests in the same file and the per-site test modules added by A4 commits `64db0d30` / `c0296c76` / `5330acd9` / `2928d5ae`.

---

## Summary

- 4 CRITICAL still open (S1=A1, S2=W1, S3=F2, S4=NEW err-msg leak).
- 3 IMPORTANT new/refined (S5 AEAD log leak, S6=F4 reopened via getters, S7 envelope-message DoS).
- 2 MINOR (S8 wake-timing side channel, S9 confirmation).

## CLOSED from r1-r3

- **B18 (cross-tenant slot takeover)** — `b4ddb98b` + `469e22c8` share the allocator. Mutex usage audited as safe (no async fence held).
- **A4 (envelope drift)** — 74 sites migrated; reserved-key guard in place; `test_helpers` correctly `#[cfg(test)]`-gated.

## Two most critical citations

- `crates/sandbox/src/lib.rs:573-649` (S1: AEAD layer never composed despite `snapshot_enabled=true` and pg row stamping `snapshot_aead_dek_id="v1"`).
- `crates/sandbox/scripts/nomad-vm-wrapper.sh:359` (S2: unanchored `sed -i -E` on attacker-influenceable `config.json`, paired with S1's plaintext GCS surface = remote-code-execution-as-raw-exec-root vector when both land in production).
