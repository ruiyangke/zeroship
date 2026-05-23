# Security Review — sandbox-snapshot-restore (2026-05-24 r2)

Scope: `crates/sandbox/**` + `crates/sandbox-agent/**` on
`feat/sandbox-snapshot-restore` @ `3b888a2a`. Read-only.

## r1 closure status

- **A2 (CRITICAL) — CLOSED at `f32507ce`.** `GcsSnapshotStore::verify`
  (`crates/sandbox/src/snapshot_store_gcs.rs:586-611`) now streams
  all three artifact files through `verify_canonical_sha256_from_streams`
  (lines 621-664), updating SHA-256 with `name || len_be || bytes`
  and returning `ChecksumMismatch` on disagreement. The stream loop
  reads in 64 KiB chunks and explicitly bails when `read_total != len`
  (line 645) — no partial-read bug; truncation, padding, and lying
  content-length headers all fail with `InvalidArtifact`. r1 #2 is
  genuinely fixed, not papered over.
- **A5 — closed at `2e0d17f7`** (admin_token `pub(crate)` + builder).
- **A1, A3, A4, A6 still open** per `sandbox-snapshot-restore-deferred.md`.

## Findings

### 1. CRITICAL — `pub persist: Option<Arc<Persistence>>` still carries sealed-record AEAD keys (A6 confirmed open).
`crates/sandbox/src/lib.rs:71` remains `pub`, while A5's sibling
field one line later (admin_token, line 106) was tightened to
`pub(crate)`. The A5-pattern `with_persistence` builder exists
(line 240) but does not enforce anything that a direct field
assignment can't bypass — any out-of-crate consumer can write
`state.persist = None` after boot and silently disarm sealed-record
re-hydration, or swap in a foreign `Persistence` whose `AeadKey`
unwraps prior sealed records. Same exposure shape A5 was raised to
close. The deferred backlog has this — but it's worth re-flagging
because the A5 commit's review apparatus walked right past it.

### 2. HIGH — Other `pub` fields on `AppState` carry security-relevant handles.
`crates/sandbox/src/lib.rs:65,114-116` keep
`pub database`, `pub snapshot_store`, `pub ch_remote`,
`pub restore_backend` mutable from out-of-crate. A test or
downstream consumer can swap `snapshot_store` for an attacker-
controlled `Arc<dyn SnapshotStore>` whose `get()` returns
fabricated `memory-ranges` — `restore_handler.rs` then boots that
into the tenant's VM index. The `dyn`-typed fields are worse than
A6's concrete `Persistence` because anyone with `&AppState` already
sees the trait object; swapping costs zero new privilege. Recommend
the same `pub(crate)` + `with_*` builder treatment as A5.

### 3. HIGH — Snapshot integrity still does not bind `sandbox_id` or `snapshot_taken_at` (r1 #7 unresolved).
`crates/sandbox/src/snapshot_store.rs:150-185`'s
`compute_artifact_sha256` hashes only `name || len || bytes` over
the three artifact files. With A1 still open, the GCS bucket holds
plaintext artifacts under `snapshots/v1/<sbx_id>/...`
(`snapshot_store_gcs.rs:138-144`) and the only restore-time
identity binding is `expected_sha256` read from pg. An attacker
with bucket-write substitutes tenant-A's intact artifact set into
tenant-B's prefix, updates pg via any other vector (e.g. SQL
injection elsewhere), and `restore_handler` accepts it. The AEAD
layer would bind `(sandbox_id, snapshot_taken_at)` into the DEK
(`snapshot_aead.rs:270-282`), but A1 keeps that off the call graph.
Pair with finding #2 — a swapped `snapshot_store` makes the same
exploit local.

### 4. MEDIUM — `chunk_aad` omits `sandbox_id` + `snapshot_taken_at` (defense-in-depth gap; non-exploitable today).
`crates/sandbox/src/snapshot_aead.rs:310-316` builds AAD as
`b"zsbx-snap" || chunk_index_be_u32`. The cross-snapshot binding
rides entirely on the DEK derivation, which is fine as long as
HKDF stays collision-free. If a future refactor caches a DEK or
reuses a key across snapshots (a plausible perf optimization), the
AAD won't catch the cross-snapshot swap. Cheap fix: append the
sandbox_id-bytes and `snapshot_taken_at` BE-u64 to the AAD; cost
is 16 bytes per chunk, ~16 KiB extra hashed per 1 GB snapshot.

### 5. MEDIUM — `nomad-vm-wrapper.sh` unanchored sed rewrite of restored `config.json` (r1 #5 unresolved).
`crates/sandbox/scripts/nomad-vm-wrapper.sh:359` still runs
`sed -i -E "s#/opt/nomad/data/alloc/[^/]+/[^/]+/local#${NOMAD_TASK_DIR}#g"`
against attacker-influenceable JSON content. With A1 open, the
snapshot's `config.json` is plaintext on GCS and an attacker with
bucket-write inserts crafted strings. `NOMAD_TASK_DIR` is still
unquoted with respect to sed metacharacters. r1 flagged this; no
fix landed. Combined with raw_exec running as root and `set -e`
disabled by the `||` guards above it, this remains a code-exec
sink. Defense: a Rust-side path-rewriter that ingests
`serde_json::Value`, rewrites only the two known fields, and
refuses anything outside the schema.

### 6. MEDIUM — `ControllerIdleSnapshotter` tenant isolation rides solely on `sbx_<base62>` derivation.
`crates/sandbox/src/sweep.rs:341-345` calls
`snapshot_handler::snapshot_sandbox(... sandbox_id ...)`, which at
`snapshot_handler.rs:341-347` formats `sbx_<base62>` and writes to
`store.put(sandbox_id_typed, ...)`. No creator-id prefix; all
tenants share the bucket prefix `snapshots/v1/`. A buggy migration
or a sandbox-id collision (UUIDv7 monotonic — collision
probability is ~negligible, but operator-injected fixtures could
violate this) puts creator-A's snapshot under creator-B's id. Not
exploitable through `ControllerIdleSnapshotter` alone today, but
the layer offers no isolation property of its own — every defense
is upstream in the typed-id generator. T9 (orchestrator dedup)
landing without a creator-prefix invariant cements this.

### 7. MEDIUM — Restore `user_id` still flows unsanitised into FS path + Nomad env (r1 #4 unresolved).
`crates/sandbox/src/restore_handler.rs:874-877` does
`cfg.user_home_dir_root.join(user_id).join("home.img")` with no
typed-id check, and `restore_handler.rs:894,934` propagate the
same string into Nomad `Meta` + `Env.ZSBX_USER_HOME_IMG`. The
wrapper only checks `[ -f <path> ]`, not that the path stays under
the configured root. Mitigation requires `typed_id::parse_usr(...)`
at the restore-submit site; pg `user_id` is a string column, not a
typed wrapper.

### 8. LOW — Admin-token loader checks mode but not ownership.
`crates/sandbox/src/lib.rs:564-576` enforces mode `0o400` on the
`SANDBOX_ADMIN_TOKEN_PATH` file but does not verify the file is
owned by `sandbox-ctl` (or the calling uid). On a host where
`/etc/zsbx/sandbox-admin-token` is owned by an unprivileged user
with mode 0o400, the loader still reads it — that user can pre-
seed the bearer, then read it back via `/proc/<pid>/environ` or a
debug endpoint. Add a `meta.uid() == geteuid()` check, or refuse
unless uid is root (matches the wrapper's raw_exec posture).

### 9. LOW — GCS metadata-token cache still unprotected against post-mortem residue.
`crates/sandbox/src/snapshot_store_gcs.rs:79-85` keeps the bearer
in a plain `String` field of `CachedToken` (no `Zeroizing`).
r1 #3 flagged this; no remediation landed. Independent of the SA-
key compromise vector, a core dump or coredump-on-panic recovers
the live bearer. Wrap in `zeroize::Zeroizing<String>` to mirror
the admin-token treatment at `lib.rs:106`.

### 10. LOW — TOCTOU window on `workspace.img` between snapshot CAS and `teardown_source_for_snapshot`.
`crates/sandbox/src/snapshot_handler.rs:335-370`: after
`ch.snapshot()` returns and the row CASes to `snapshotted`
(line 350), there is a window before
`vm_ops.teardown_source(sandbox_id)` runs (line 364) during which
the source VM is paused and its `workspace.img` is open by CH but
**also** readable on disk. A concurrent admin op (or attacker with
host-fs access via a co-tenant compromise) can read or modify
`workspace.img` between snapshot-success and source teardown — the
restored VM will then boot a tampered workspace. The `stop_
preserving_state` path (`backend/nomad_ch.rs:905-910`) widens this
window indefinitely on success-with-teardown-failure. Mitigation:
chattr +i / chmod 0400 the workspace.img during the snapshot
window, or move the image into the snapshot-store root.

---

Severity: CRITICAL = exploitable now / open r1; HIGH = exploitable
with one adjacent compromise; MEDIUM = trust-boundary erosion or
race; LOW = operational / defense-in-depth.

Counts: 1 CRITICAL · 2 HIGH · 4 MEDIUM · 3 LOW · 10 total.
