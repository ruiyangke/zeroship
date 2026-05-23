# Security Review — sandbox-snapshot-restore (2026-05-24 r1)

Scope: `crates/sandbox/**` + `crates/sandbox-agent/**` on
`feat/sandbox-snapshot-restore` @ `b048b491`. Read-only.

## Findings

### 1. CRITICAL — AEAD wrap layer never composed; snapshot artifacts are stored in plaintext despite `snapshot_aead_dek_id="v1"` in pg.
`crates/sandbox/src/lib.rs:316–345` builds the production snapshot
store as either bare `LocalDiskSnapshotStore` or
`TieredSnapshotStore<LocalDisk, Gcs>` and never wraps with
`AeadSnapshotStore`. `snapshot_aead.rs` is a dead module by call-graph.
Meanwhile `snapshot_handler.rs:358` records `Some("v1")` for the
`snapshot_aead_dek_id` column and `snapshot_aead.rs:613` would stamp
`+aead-cc20p1305` onto `ch_version` (it never runs). Confidentiality of
memory-ranges (≈1 GB of guest RAM — secrets, tokens, kernel keyring)
relies on filesystem ACLs only; on GCS, plaintext lands in the bucket.
Pg attestation is **misleading**: an auditor reading the column would
believe artifacts are AEAD-wrapped when they are not.

### 2. CRITICAL — GCS snapshot integrity check is bypassable on `verify()`.
`snapshot_store_gcs.rs:660–682`'s `verify` HEADs each file and
**discards `expected_sha256`** (`let _ = expected_sha256;`). The
function returns `Ok(())` whenever the three objects exist; it does
**not** compare GCS's reported `x-goog-hash` sha256 against the
caller's expected hash. An attacker with bucket-write (compromised SA
key, mis-scoped IAM) can substitute any same-named object set with
their own `x-goog-hash` and `verify` will pass. `get()` (line 619)
*does* re-hash post-download — so a restore catches this — but any
operator/automation that gates on `verify` is fooled.

### 3. HIGH — GCS download trusts `x-goog-hash` from a TLS-terminated proxy and races a metadata-server token over plaintext HTTP.
`snapshot_store_gcs.rs:54` fetches OAuth2 tokens over
`http://metadata.google.internal/...` (plaintext, per GCE convention),
which is fine on a hardened VM but means **any local process** on the
controller host with the `Metadata-Flavor: Google` header can pull the
controller's service-account bearer (link-local 169.254.169.254 ARP
poison or shared sidecar). Combined with finding #1, anyone who
compromises a co-tenant on the controller host can read/write every
tenant's plaintext snapshot. The token is cached in `Mutex<Option<…>>`
without `Zeroizing` (line 100) — heap residue.

### 4. HIGH — `user_id` flows unvalidated into filesystem paths used by the restore submitter.
`restore.rs:283` reads `user_id: Option<String>` from pg
(`row.try_get(3)`), then `restore_handler.rs:874` does
`cfg.user_home_dir_root.join(user_id).join("home.img")`. There is no
typed-id check at this site — the caller relies on the writer having
validated. A compromised write path (SQL injection in another handler,
gdpr role abuse, or a buggy migration backfill) that placed
`user_id="../../etc"` would coerce `home.img` to traverse out of the
user-home root. The same `user_id` lands in Nomad job `Meta`
(restore.rs:894) and the Nomad job `Env.ZSBX_USER_HOME_IMG`
(restore.rs:934) unsanitised — the wrapper script only checks `[ -f
<path> ]`, not that the path stays under the configured root.

### 5. HIGH — `nomad-vm-wrapper.sh` rewrites the snapshot's `config.json` in place via unanchored sed substitution.
`scripts/nomad-vm-wrapper.sh:359` runs
`sed -i -E "s#/opt/nomad/data/alloc/[^/]+/[^/]+/local#${NOMAD_TASK_DIR}#g"`
against the **restored** `config.json` (which lives inside
`$ZSBX_RESTORE_FROM`, controller-staged but originally CH-authored).
The regex matches **any** occurrence of that prefix — a snapshot whose
attacker-controlled string fields (CH allows arbitrary serial-log
paths, fs tags, etc.) embed the prefix will see those rewritten too.
More importantly, `NOMAD_TASK_DIR` is unquoted with respect to sed
metacharacters; a `#` in the env var (improbable, but Nomad allocates
the dir name) terminates the substitution and injects sed commands.
Combined with `set -e` + raw_exec running as root, this is a code-exec
sink on the host.

### 6. HIGH — `init.sh` hex decoder is bash-only and silently fails to error on truncated input.
`scripts/init.sh:93–95`:
`printf '%b' "$(printf '%s' "$PUBKEY_HEX" | sed 's/\(..\)/\\x\1/g')" >
/run/keys/controller-pubkey`. The hex/length checks above are correct,
but the decoder relies on bash `%b` honouring `\xNN`. If the rootfs
bake ever drops bash (`#!/bin/bash` on line 1 is documented but not
enforced by CI), the decoder silently produces the literal escape
string. The agent rejects every signed request, the controller marks
the row `unreachable`, and the restore path **gracefully flips it back
to `running`** at the next probe (`restore.rs:339-367`) — a working-as-
intended path that masks a controller-key MITM if an attacker can
swap the rootfs init script. No defence-in-depth check that the
decoded blob is exactly 32 bytes is performed by init.

### 7. MEDIUM — Snapshot SHA-256 chain does not bind `snapshot_taken_at`, `vm_index`, or sandbox identity.
`snapshot_store.rs:151–185` computes `H = sha256(name || len ||
bytes)` over the **three artifact files only**. An attacker who can
write to GCS (finding #3) can swap one tenant's snapshot for another's
intact-but-foreign artifact: the SHA-256 still matches the foreign
row's pg-recorded hash, and `restore_handler.rs:323` accepts it
because the only identity binding is `expected_sha256` read from pg.
Restore then boots tenant A's memory-ranges into tenant B's
vm_index/IP/tap — full cross-tenant takeover. The AEAD layer would
bind `(sandbox_id, snapshot_taken_at)` into the DEK and refuse the
swap; with finding #1 it doesn't run.

### 8. MEDIUM — Restore handler returns `RestoreOutcome::Restored` for `Unreachable→Running` flips without re-verifying agent fingerprint.
`restore.rs:339–367` CASes `unreachable→running` purely on a
single signed `/version` probe match (probe ran above at line 309),
without re-checking that the row's `key_fp` still equals the probe
result on the **flip** itself. The race window is small but real
(probe + CAS are not atomic). If an attacker can briefly hijack the
agent IP between probe and flip, the controller resurrects a row
pointed at the wrong agent.

### 9. MEDIUM — `admin_check` parses `Authorization` with `unwrap_or("")` after `to_str()`, which silently treats non-UTF-8 bearers as missing-but-not-suspicious.
`admin_handlers.rs:160–166`. A header with arbitrary bytes returns
`presented = b""`, which then fails the constant-time compare — fine
for auth. But it bypasses any future per-request audit that wants to
log "auth attempt with malformed header". Treat as MINOR if no audit
is planned; MEDIUM if Phase-5 JWT lands and the same shape is reused.

### 10. MEDIUM — `RealRestoreBackend::reservations` is process-local, not pool-shared with `NomadCHBackend`'s allocator.
`restore.rs:713` constructs `Arc<Mutex<VmIndexReservations>>` standalone
(comment at line 657–662 explicitly flags this). A `wake_sandbox` can
reserve `vm_index=7` while a concurrent `create_sandbox` allocates the
same slot from `NomadCHBackend::vm_index_allocator` — the second
caller binds tenant A's tap/MAC/IP on top of tenant B's. v1 acks the
gap in the docstring; until shared, this is a cross-tenant collision
vector on every host running both paths.

### 11. LOW — Sealed-record cleanup in `delete_user` operates outside the GDPR transaction.
`admin_handlers.rs:969`. A crash between `tx.commit()` and
`unlink_sealed_for_user` leaves sealed signing keys on disk for a
user whose pg rows are gone — the keys are unreachable (no row to
join against) but the *ciphertext lives indefinitely*. GDPR-deletion
contract suggests crypto-shredding the AEAD key or zero-overwriting
the sealed file; current implementation just `remove_file`.

### 12. LOW — GCS `delete()` ignores `errs` ordering and aggregates into `InvalidArtifact`.
`snapshot_store_gcs.rs:640–658`. A partial-delete (config.json gone,
memory-ranges 403 from a IAM-revoked path) leaves the artifact
half-present and reports `InvalidArtifact`. The tiered delete (line
867) then logs warn and returns Ok — operator sees a successful
delete in metrics while half the snapshot persists in the bucket.

---

Severity tags: CRITICAL = exploitable now; HIGH = exploitable with one
adjacent compromise; MEDIUM = race / trust-boundary erosion; LOW =
operational / compliance.
