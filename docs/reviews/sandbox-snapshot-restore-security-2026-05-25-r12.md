# Sandbox/snapshot-restore — security r12 review

Date: 2026-05-25 (UTC)
HEAD at audit: `42212c5c`
Round 12 of N (security lens). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/scripts/**`.

## Summary

1 new IMPORTANT (R12-S1, the 7-sibling Rust-2024 env-mutation race);
1 new MINOR (R12-S2, R10-S1 follow-up posture observation on the
partial close path under ChPlugin); no CRITICAL elevation this round.

3 r11 findings now CLOSED at HEAD: R11-S1 (admin token uid-check) at
`b4c3ef27` (= R9-S4d), R11-S2 (host_id mode+uid) at `85e4f2f9`, and
the partial closure of R9-S5 under ChPlugin via `b3bf741c` (the typed
`sandbox_id` Config field on the ChPlugin restore wire wasn't there
before; raw_exec arm still open). R11-S3 (chunk_aad missing
sandbox_id+taken_at) is unchanged — posture-class deferred.

R9-S1 / R9-S2 / R9-S3 / R10-S1 / R10-S2 / R10-S3 / R10-S6 / R9-S6 / S7
/ S8 all carry forward at HEAD unchanged. R12 added no new wire-shape
or attacker-reachable surface to the controller's Nomad/driver
boundary — `SANDBOX_TASK_DRIVER`, `SANDBOX_NOMAD_CH_RUNTIME_DIR`, and
the GCE instance-metadata `INSTALL_CH_PLUGIN_DRIVER` are all
operator-only inputs, not request-derived. The R12-I1 wake-path
TaskDriverMode branch is wire-clean; the T-8b-prereqs-config plugin
stanza is statically literal.

## Hunt-list disposition (security lens)

### 1. R10-S1 symlink residual — re-check 5 secret-file loaders

Re-read all 5 sites at HEAD `42212c5c`. All still use
`std::fs::metadata` (follows symlinks), not `symlink_metadata`. No
loader has migrated to `O_NOFOLLOW + fstat` between r11 and r12. The
3-step TOCTOU (stat → re-open + read) is unchanged everywhere — the
mode/uid checks fire on the link target, then a SEPARATE `std::fs::
read` / `File::open` call re-resolves the path and could land on a
different target if the link was swapped in between.

Updated loader-by-loader table (compare r11 table for delta tracking):

| Loader | File:Line | Mode check | UID check | Symlink-safe |
|---|---|---|---|---|
| `RootKek::from_path` (KEK) | `snapshot_aead.rs:185-217` | 0o400 (190) | uid 0 (198) | **NO** — `metadata` at 190, separate `read` at 206 |
| `AeadKey::from_path` (sealed-record key) | `persist.rs:333-369` | 0o400 (348) | uid 0 (355) | **NO** — `metadata` at 335, separate `File::open` at 364 |
| `enforce_password_file_mode` (pg pw) | `db.rs:826-853` | 0o400 (837) | uid 0 (843) | **NO** — `metadata` at 831, caller's later `read_to_string` re-resolves |
| `load_admin_token` (admin bearer) | `lib.rs:936-977` | 0o400 (951) | uid 0 (956) | **NO** — `metadata` at 947, separate `read_to_string` at 965 |
| `enforce_host_id_file_mode` (host_id) | `db.rs:1161-1188` | 0o600 (1172) | uid 0 (1178) | **NO** — `metadata` at 1166, caller at `:1099` does a separate `read_to_string` |

Carry-forward at HEAD. R10-S1 unchanged.

### 2. R12-I1 wake-path — fresh wire-shape attack-surface audit

`restore_handler.rs::build_restore_nomad_job_json` at HEAD (after
`b3bf741c`) now matches on `TaskDriverMode`. Under `ChPlugin` it
emits a typed Config block (see `:1397-1416`):

```
{
  "vm_index":          u16   (controller alloc, 1..=ceil)
  "kernel":            String — cfg.runtime_dir.join("vmlinuz")
  "cpus":              u32   (controller-computed cpus_boot)
  "memory_mb":         u32
  "restore_from":      String — alloc_dir = host_state_dir/<sandbox_id.simple>/restore
  "sandbox_id":        String — sandbox_id.simple() (32-hex no hyphens)
  "workspace_img":     String — host_state_dir/<sandbox_id.simple>/workspace.img
  "user_home_img":     String — user_home_dir_root/<user_id>/home.img
  "pubkey_hex":        ""    (CH ignores --cmdline on restore)
  "subnet_base_octet": u16
  "disks","fs","net":  []
}
```

Each field has a controller-side derivation, not a request-derived
one:
- `vm_index` — `i16` from `Database::allocate_vm_index` + the
  in-memory `VmIndexAllocator`. Type-checked.
- `kernel` — joined from `cfg.runtime_dir` (operator-set env
  `SANDBOX_NOMAD_CH_RUNTIME_DIR`, default `/var/lib/zeroship/ch`).
- `cpus`, `memory_mb` — `SandboxConfig` controller-owned bounds.
- `restore_from`, `workspace_img`, `user_home_img` — all derived
  from typed `Uuid::simple()` + operator-set path prefixes; the
  `user_id` segment hits the wrapper's `[!0-9a-zA-Z_]` validator at
  handler entry (`handlers.rs:138-154`), then is joined here
  verbatim. R10-S6 carry-forward applies.
- `sandbox_id` — typed `Uuid`, lowercased hex via `.simple()`.
- `pubkey_hex` — empty literal.
- `subnet_base_octet` — `cfg`-bounded u16.

No new attacker-controllable field on the wire. The Go driver
(`nomad-driver-ch`) is the type/range fence per the prompt's T-7
flag-up; that's out of scope for this review (Rust crates only). The
wake-path Config block does NOT regress R9-S5: under ChPlugin mode
`sandbox_id` IS present in the typed Config, which is a partial close
of the cold-boot/restore asymmetry r11 flagged. Under raw_exec
restore mode `ZSBX_SANDBOX_ID` is still missing from the env block
(`restore_handler.rs:1334-1349` lists 9 ZSBX_* keys; sandbox_id is
not among them) — see "R9-S5 partial close" note below.

### 3. T-8b-prereqs-config Nomad plugin stanza (b4500576)

`gcp-worker-startup.sh:194-200` writes a static HCL fragment via
heredoc:

```
cat > /etc/nomad.d/plugin-dir.hcl <<EOF
plugin_dir = "/etc/zeroship/nomad-plugins"

plugin "nomad-driver-ch" {
  config {}
}
EOF
```

- `plugin "nomad-driver-ch"` — driver name is a literal in the
  source. No variable interpolation, no metadata-server data fed
  into the HCL body.
- `config {}` — empty block. If operators later populate this with
  driver-specific options, those would come from operator-edited
  scripts, not from GCE metadata or any wire input.
- `plugin_dir` is the canonical `/etc/zeroship/nomad-plugins`
  literal; the driver binary itself is pulled by `gs_pull
  nomad-driver-ch.v4 /etc/zeroship/nomad-plugins/nomad-driver-ch` at
  line 176 (operator-controlled GCS bucket `ARTIFACT_BUCKET`, set at
  instance-provisioning time via Terraform).

Sanity check passes — no arbitrary config injection vector via this
stanza. The only ways to subvert it require GCE project compromise
(operator-trusted boundary) or `chmod a+w /etc/nomad.d/` on the
running worker (also operator-trusted boundary). Posture: clean.

### 4. R9-S4d admin-token uid==0 reachability in production

Production path traced end-to-end:

- `crates/sandbox/scripts/gcp-worker-startup.sh:86,99,379-380` —
  fetches `sandbox-admin-token` from GCE instance metadata,
  `printf '%s' "$SANDBOX_ADMIN_TOKEN" > "$ART/sandbox-admin-token";
  chmod 0400 "$ART/sandbox-admin-token"`. The startup script runs as
  root via cloud-init, so the file is owned by uid 0.
- `gcp-worker-startup.sh:483` — `Environment=SANDBOX_ADMIN_TOKEN_PATH
  =$ART/sandbox-admin-token` is injected into the controller's
  systemd unit.
- `crates/sandbox/src/lib.rs:580-587` — `AppState::from_config`
  reads `SANDBOX_ADMIN_TOKEN_PATH` from env and passes it to
  `load_admin_token`.
- `lib.rs:936-977` — `load_admin_token` does the mode `0o400` check
  at line 951 AND the `uid != 0` check at line 956.

Closed sibling test pattern (`lib.rs:1592-1654`) ships matching
positive-and-negative arms:
- `load_admin_token_rejects_non_root_owned_file` (gated to non-root
  runner via `nix::unistd::geteuid().is_root()`).
- `load_admin_token_accepts_root_owned_file_when_running_as_root`
  (gated `#[ignore]` unless root).

Production reachability: **CONFIRMED**. The file written by the
startup script will be owned by root and the loader will accept it;
an attacker-pre-created file at the same path before cloud-init
would fail the mode/uid check and crash the controller (fail-loud
as designed).

### 5. C-2 driver fix (f521eb21) — ZSBX_ARTIFACT_DIR path-traversal

`f521eb21` is a Go driver commit (out of Rust scope per the prompt's
read-only scope on `crates/sandbox/**`). The commit-stat shows only
`nomad-driver-ch/**` touched.

Controller-side audit: `ZSBX_ARTIFACT_DIR` is set in two places —
`backend/nomad_ch.rs:2339` (cold-boot env block) and
`restore_handler.rs:1336` (restore env block). Both pass
`cfg.runtime_dir.display().to_string()`, where `cfg.runtime_dir`
comes from `config.rs:749-752`:

```
runtime_dir: PathBuf::from(
    std::env::var("SANDBOX_NOMAD_CH_RUNTIME_DIR")
        .unwrap_or_else(|_| "/var/lib/zeroship/ch".to_string()),
),
```

`SANDBOX_NOMAD_CH_RUNTIME_DIR` is read once at controller boot from
process env (the systemd unit at `gcp-worker-startup.sh:486-495`
doesn't set it explicitly, so the default applies). Attacker has no
request-path influence on this value; the driver's `cp` from
`$ZSBX_ARTIFACT_DIR/rootfs-slim.img` is fed an operator-controlled
prefix.

Posture: **no controller-side path-traversal vector** into the
driver via `ZSBX_ARTIFACT_DIR`. The Go driver's per-file path
validation is its own scope; controller-side audit is clean.

### 6. R13-C1 ENV_LOCK race — production scope

`std::env::set_var` / `remove_var` became `unsafe` in Rust 2024 (UB
when called concurrent with ANY env read, even on disjoint keys —
the global env table itself is not thread-safe). Audit at HEAD:

- `crates/sandbox/src/db.rs:2820` — `set_env(k, v)` inside
  `#[cfg(test)] mod tests`. Serialised by `ENV_LOCK` (`:2790`).
- `crates/sandbox/src/restore_handler.rs:2490-2494` — same shape,
  `#[cfg(test)]`, serialised by `R12_I1_ENV_LOCK`.
- `crates/sandbox/src/backend/nomad_ch.rs:4085-4091` — same shape,
  `#[cfg(test)]`, serialised by `T7_ENV_LOCK`.

**Production scope: none.** The controller never mutates env mid-run
(operator-set at systemd-service start, then read-only). Tests are
the sole consumer.

However there IS a residual test-only IMPORTANT (see R12-S1 below):
the three test-local mutexes do NOT serialise against each other.
If a `nomad_ch::tests` test holding `T7_ENV_LOCK` mutates
`SANDBOX_TASK_DRIVER` while a `db::tests` test holding `ENV_LOCK`
mutates `SANDBOX_HOST_ID` (disjoint keys), the per-mutex locks
think they're each safe — but Rust-2024 stdlib's `set_var`
contract requires NO other thread be reading ANY env var, even on a
disjoint key. This is test-internal UB; production-scope unaffected.

### 7. `unsafe { std::env::set_var/remove_var }` audit

`Grep -n unsafe \{ std::env::set_var|unsafe \{ std::env::remove_var`
on `crates/`:

```
crates/sandbox/src/restore_handler.rs:2490 — test-only
crates/sandbox/src/restore_handler.rs:2491 — test-only
crates/sandbox/src/restore_handler.rs:2494 — test-only
crates/sandbox/src/db.rs:2820            — test-only
crates/sandbox/src/backend/nomad_ch.rs:4085 — test-only
crates/sandbox/src/backend/nomad_ch.rs:4086 — test-only
crates/sandbox/src/backend/nomad_ch.rs:4091 — test-only
crates/runtime/tests/wpt_fetch_redirect.rs:163 — test-only (out of scope)
crates/runtime/tests/fetch_native.rs:225       — test-only (out of scope)
crates/runtime/tests/fetch_native_install.rs:52 — test-only (out of scope)
```

In `crates/sandbox/**` and `crates/sandbox-agent/**`: zero production
env mutations. All 7 sites are `#[cfg(test)]`-gated. (The
`lib.rs:879` hit in the original grep is a documentation comment
mentioning the pattern, not an actual call.)

## Findings (NEW since r11)

### [R12-S1] Test-only env-mutation locks do not serialise across modules — Rust-2024 stdlib `set_var` UB on disjoint keys (IMPORTANT, security-r12)

- **Files**: `crates/sandbox/src/db.rs:2790` (`ENV_LOCK`);
  `crates/sandbox/src/backend/nomad_ch.rs:4073` (`T7_ENV_LOCK`);
  `crates/sandbox/src/restore_handler.rs:2478`
  (`R12_I1_ENV_LOCK`).
- **Symptom**: Three separate module-local `Mutex<()>` static
  locks each protect their own subset of env-touching tests. The
  comments in `nomad_ch.rs:4076-4078` and
  `restore_handler.rs:2473-2477` explicitly say "we don't share the
  same mutex symbol across crates" — but they should share across
  *modules* within the same crate, because Rust 2024's
  `std::env::set_var` is documented unsafe due to the env table
  itself not being thread-safe (NOT because of per-key data races).
  Concurrent `set_var(A)` + `set_var(B)` across modules is UB even
  when A ≠ B.
- **Threat model**: This is test-only — `cargo test -p
  zeroship-sandbox` runs lib tests in parallel by default. A
  `nomad_ch::tests::ch_plugin_restore_jobspec_omits_command` test
  holding `T7_ENV_LOCK` can race with a `db::tests::
  from_env_validates_lease_ttl_minimum` holding `ENV_LOCK`,
  triggering UB inside the stdlib's env-table mutex. The
  consequence is a flaky test run / SIGSEGV / silently wrong env
  reads in adjacent tests — NOT a remote attacker concern; pure
  test-suite hygiene.
- **Why this is "security" lens not "test-cov" lens**: The Rust
  2024 unsafe-env contract is a memory-safety boundary. Three
  separate locks gated by three `#[allow(unsafe_code)]` modules
  with comments asserting safety based on a misreading of the
  stdlib contract is a confused-deputy of the unsafe-code
  invariants. A future test adding a fourth env var would likely
  add a fourth module-local lock following the same pattern. The
  comment in `nomad_ch.rs:4077` ("No other crate touches
  SANDBOX_TASK_DRIVER at test time") is correct but irrelevant —
  the UB risk is on the env table itself, not the specific key.
- **Action**:
  (a) Promote ONE of the existing locks to a crate-wide static
      (e.g. `crate::tests::ENV_LOCK` in a small new
      `tests/support.rs` module visible to all three callers under
      `#[cfg(test)]`).
  (b) Replace `T7_ENV_LOCK`, `R12_I1_ENV_LOCK`, and any future
      mod-local mirror with a `static_or!` re-export of the single
      crate-wide lock.
  (c) Update the safety comments at `nomad_ch.rs:4075-4080`,
      `restore_handler.rs:2480-2483`, and `db.rs:2807-2810` to
      say "no other test in *this crate* touches env" — and have
      the shared lock be the proof of that invariant.
  (d) Belt-and-suspenders: gate `set_var` calls behind a helper
      `with_env_locked(|env| env.set("A", "1"))` that takes the
      crate-wide lock by argument-shape, so the lock-acquire is
      the only entry to the unsafe call. This makes the new-test-
      adds-fourth-lock pattern impossible.

### [R12-S2] R9-S5 partial close under ChPlugin — raw_exec restore env still missing `ZSBX_SANDBOX_ID` (MINOR posture, security-r12)

- **Files**: `crates/sandbox/src/restore_handler.rs:1334-1349`
  (restore env block, both modes); cold-boot symmetric at
  `crates/sandbox/src/backend/nomad_ch.rs:2334-2374`.
- **Symptom**: r11 flagged that the restore env block has NO
  `ZSBX_SANDBOX_ID`, asymmetric with cold-boot which emits it at
  `nomad_ch.rs:2373`. After `b3bf741c` (R12-I1 wake-path
  TaskDriverMode), the ChPlugin Config block at
  `restore_handler.rs:1403` DOES carry `"sandbox_id":
  sandbox_id.simple().to_string()` — partially closing the
  asymmetry. But the env block under raw_exec mode (`:1334-1349`)
  STILL has no `ZSBX_SANDBOX_ID`; the bash wrapper at
  `nomad-vm-wrapper.sh:127` only checks `${ZSBX_ARTIFACT_DIR:?...}`
  in the early validator, and the wrapper's restore branch (line
  364 onward) never re-reads `ZSBX_SANDBOX_ID`. So the asymmetry
  on the env-only channel is "vestigial" — the wrapper doesn't
  consume it on restore.
- **Threat model**: Same as r11 R9-S5: an attacker who can read
  the alloc's env (e.g. `/proc/<pid>/environ` after a wrapper
  process spawn) sees cold-boot allocs carrying ZSBX_SANDBOX_ID
  but restore allocs not. Information posture, not data
  exfiltration. R9-S5 closes IF the controller also emits
  `ZSBX_SANDBOX_ID` to the restore env (mirror cold-boot — 1
  line added).
- **Why this is now MINOR posture and not IMPORTANT**: the
  ChPlugin partial-close at `:1403` means the controller's
  intent to bind sandbox_id into the dispatched Nomad task IS
  on the wire. The raw_exec gap is now legacy-shaped (the
  wrapper doesn't read it during restore). Operator-debuggability
  is the remaining miss — `nomad logs zsbx-restore-<uuid>` shows
  no `ZSBX_SANDBOX_ID` env, breaking the same correlation an
  operator gets on cold-boot allocs. Cheap to close (1 line in
  the restore env block), low risk.
- **Action**:
  ```rust
  let env = serde_json::json!({
      ...
      "ZSBX_RESTORE_FROM": alloc_dir.display().to_string(),
      "ZSBX_SUBNET_BASE_OCTET": cfg.subnet_second_octet.to_string(),
      // R9-S5 close: mirror cold-boot env-symmetry. The wrapper
      // does not consume this on restore but the operator-debug
      // story benefits.
      "ZSBX_SANDBOX_ID": sandbox_id.simple().to_string(),
  });
  ```

## Verified open carry-forward (unchanged at HEAD)

- **R9-S1** (CRITICAL) — `nomad-vm-wrapper.sh:476-498` anchored
  prefix regex; non-matching paths pass through `rewrite()` via
  `return value` at line 490. Verified by re-read at HEAD.
- **R9-S2** (IMPORTANT) — `snapshot_aead.rs::derive_dek` /
  `derive_nonce_prefix` keyed on 1-second timestamp. Re-snapshot
  within same wall-clock-second → nonce reuse. Re-confirmed.
- **R9-S3** (IMPORTANT) — `snapshot_aead_dek_id="v1"` hard-coded
  in `snapshot_handler.rs:417` regardless of root presence.
  Re-confirmed.
- **R9-S5** (IMPORTANT → partially closed) — restore env block
  lacks `ZSBX_SANDBOX_ID` under raw_exec mode
  (`restore_handler.rs:1334-1349`); ChPlugin partial-close via the
  typed `sandbox_id` Config field at `:1403`. See R12-S2 above
  for the remaining raw_exec arm.
- **R10-S1** (IMPORTANT) — symlink-follow on all 5 secret-file
  loaders (KEK, AEAD key, pg-password, admin token, host_id).
  `metadata` follows links; the subsequent `read`/`open` is a
  separate syscall that re-resolves. R10-S1 unchanged.
- **R10-S2** (IMPORTANT) — `restore_handler.rs:294-298`,
  `spawn_blocking(...).await` discards JoinError via `let _`.
  Re-confirmed at HEAD; the surrounding R10-C1/R10-C2 work
  (`be246395`) shipped the spawn_blocking wrap, but the JoinError
  swallow is still there.
- **R10-S3** (MINOR) — `restore_handler.rs:1168-1211`
  `teardown_restore` step (3) `release_vm_index` runs regardless
  of step (1) `nomad_delete_blocking` outcome. Unchanged.
- **R10-S6** (MINOR) — `handlers.rs:138-154`
  `read_sandbox_id_from_sources` accepts any non-empty string;
  wrapper-side `[!0-9a-zA-Z_]` validator at
  `nomad-vm-wrapper.sh:222` is the primary fence. Posture
  unchanged.
- **R11-S3** (MINOR posture) — `snapshot_aead.rs::chunk_aad`
  binds only `"zsbx-snap" || chunk_index`, not sandbox_id /
  taken_at. AAD-amplification class of R9-S2. Unchanged. The
  closure plan (FILE_VERSION bump from 0x01 to 0x02) hasn't
  shipped.
- **R9-S6 / S7 / S8** (MINOR) — `/_clock_resync` agent-body
  journald leak, `/livez|/readyz|/metrics` unauthenticated, admin
  endpoints lack per-bearer rate-limit. Unchanged at HEAD.

## Closed by recent commits

- **R11-S1** at `b4c3ef27` — `load_admin_token` uid==0 check
  shipped with parallel positive-and-negative tests at
  `lib.rs:1601-1654`. Production reachability confirmed via the
  startup-script trace in §4 above. R11-S1 == R9-S4d (the same
  finding, elevated in r11 then closed in the same window).
- **R11-S2** at `85e4f2f9` — `enforce_host_id_file_mode` shipped
  with mode `0o600` + uid 0 check at `db.rs:1161-1188`. Called
  from `db.rs:1098` ahead of the file read. Verified at HEAD.
- **R7-API2** at `c8000537` — closed in r11, re-confirmed by
  re-read of `version.rs::PROTOCOL_VERSION` (still `u32 = 1`) and
  the regression-guard test `mandatory_clock_resync_v1_present`.

## Threat-model audit of secret-file loaders (delta vs. r11)

The five-row table in r11 was complete for `crates/sandbox/src/**`.
At HEAD `42212c5c`:

| Loader | Mode check | UID check | Symlink-safe | Status |
|---|---|---|---|---|
| `RootKek::from_path` | 0o400 | uid 0 | **NO** | R9-S4 closed; R10-S1 open |
| `AeadKey::from_path` | 0o400 | uid 0 | **NO** | R9-S4b closed; R10-S1 open |
| `enforce_password_file_mode` | 0o400 | uid 0 | **NO** | R9-S4c closed; R10-S1 open |
| `load_admin_token` | 0o400 | uid 0 | **NO** | R9-S4d/R11-S1 closed at `b4c3ef27`; R10-S1 open |
| `enforce_host_id_file_mode` | 0o600 | uid 0 | **NO** | R11-S2 closed at `85e4f2f9`; R10-S1 open |

All 5 mode/uid checks are now closed. All 5 are still symlink-
follow (R10-S1). The structural cure (`OpenOptions::custom_flags
(O_NOFOLLOW)` + `fstat`) would close R10-S1 across all 5
simultaneously; the per-loader changes amount to ~4 lines each.
No fifth secret-file loader has been added since r11.

## T-7 / T-8b controller→Go-driver boundary

T-7 (controller-side `TaskDriverMode` flag) has landed at `5fe36805`
(committed, not stashed as r11 noted). The cold-boot ChPlugin Config
block at `nomad_ch.rs:2444-2460` and the wake-path ChPlugin Config
block at `restore_handler.rs:1397-1416` are the controller's only
typed surfaces to the Go driver. Audit per r11's pre-flagged list:

- `vm_index` (u16) — controller-allocator-bounded 1..=ceil; the
  Go driver's HCL decoder is the secondary fence (out of Rust
  scope per the read-only prompt).
- `sandbox_id` (32-hex) — `Uuid::simple()`-formatted; no hyphens;
  the wrapper-side `[!0-9a-zA-Z_]` validator at
  `nomad-vm-wrapper.sh:222` is BYPASSED under ChPlugin (wrapper
  doesn't run). Go driver is the sole validator. NOT a controller-
  side miss; flagged for cross-stack review.
- `pubkey_hex` — cold-boot path emits a controller-derived
  string; restore emits `""`. No attacker influence (signing key
  bytes come from `Persistence`'s sealed records — uid-0-owned
  AEAD key file).
- `kernel` — controller derives `cfg.runtime_dir.join("vmlinuz")`.
  Controller-trusted prefix. No attacker influence.
- `restore_from` — `cfg.host_state_dir.join(sandbox_id.simple()).
  join("restore")`. Controller-derived; no path-traversal vector.
- `workspace_img`, `user_home_img` — derived from typed inputs +
  operator-set path prefixes. The `user_id` segment hits the
  handler's input validator (`handlers.rs:138-154`). R10-S6
  carry-forward.
- `disks`, `fs`, `net` — empty arrays; trigger driver-side
  auto-synthesis from the typed fields above. No payload.

All controller-emitted fields are typed and bounded; no
attacker-reachable wire shape. The Go driver's HCL decoder remains
the type/range fence on the driver side.

## Counts

- CRITICAL: 0 new (R9-S1 carry)
- IMPORTANT: 1 new (R12-S1); carry: R9-S2, R9-S3, R9-S5 (raw_exec
  arm only), R10-S1, R10-S2
- MINOR: 1 new (R12-S2); carry: R10-S3, R10-S6, R11-S3,
  R9-S6/S7/S8
- Total NEW this round: 2

r11-closed at HEAD: 2 (R11-S1 at `b4c3ef27`, R11-S2 at `85e4f2f9`).
r9-carry: 8 (R9-S1, R9-S2, R9-S3, R9-S5, R9-S6/S7/S8, R10-S1/2/3/6,
R11-S3).
