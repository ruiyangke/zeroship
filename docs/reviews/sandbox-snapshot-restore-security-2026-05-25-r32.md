# Sandbox/snapshot-restore — security r32 review

Date: 2026-05-25 (UTC). HEAD: `b172cee0` (branch `feat/sandbox-snapshot-restore`).
Predecessor: r31 at `729f22dd` (`docs/reviews/sandbox-snapshot-restore-security-2026-05-25-r31.md`).
Scope: `crates/sandbox/**` and `crates/sandbox-agent/**`. READ-ONLY.

In-scope landings since r31:

- `c56893b2` — drop `driver.raw_exec.enable = "1"` from Nomad client config (r31-S1 closure).
- `4ac1e526` — `wait_for_alloc_running` parse-error sleep 250→100 ms (cadence drift fix).
- `1d58ab53` — emit two new CREATE-path `tracing::info!` points (`submit_done`, `alloc_first_seen`).
- `3bc689ca` + `0fec9bc3` — R32-M1 wrapper-attribution scrub (comment/docstring/error-string only, text-only).
- `a6e517b2` — wake-path cadence tightening (sleeps only, no code-shape change).
- `b172cee0` — cycle 51+52 reviewer paperwork (out-of-scope `docs/reviews/`).
- `8d82ecde` — r32-T1 cluster trace report (out-of-scope `docs/reviews/`).

## Summary

**r32 produces ZERO new CRITICAL, ZERO new IMPORTANT, ZERO new MINOR.**

[r31-S1] is **CLOSED** at `c56893b2` (line dropped exactly as the r31 fix prescribed).
R13-S1 (worker SA `storage-rw`) remains the SOLE pre-cutover Ops blocker.

## CRITICAL

None.

## IMPORTANT

None.

## MINOR

None new this round.

---

## "Verified closed since r31" section

### [r31-S1] Nomad client `driver.raw_exec.enable = "1"` — **CLOSED** at `c56893b2`

Diff against r31 prescription:

```hcl
   options = {
-    "driver.raw_exec.enable" = "1"
-    "user.blacklist"         = ""
+    "user.blacklist" = ""
   }
```

Exactly the one-line edit r31 prescribed. The ch driver path is unaffected:
`plugin "nomad-driver-ch" { config {} }` (lines 202-204) still loads explicitly.
Nomad's default for raw_exec is disabled; with the option removed, the agent
no longer registers the raw_exec driver at all, eliminating the dead-capability
escalation surface r31 flagged.

Worker rollout: ungated on next provision-script run (the controller pin v39
that pulled in this script-side change landed at r31 in `729f22dd`).

---

## Focal-list checks (per r32 brief)

### Focal #1 — New `tracing::info!` emits: anything sensitive in the keys?

**CLEAN.** Two new emits added in `nomad_ch.rs` (1d58ab53):

```rust
// nomad_ch.rs:1194-1199 (submit_done)
tracing::info!(
    sandbox_id = %sandbox_id,
    job = %job_id,
    elapsed_ms = %create_started.elapsed().as_millis(),
    "sandbox/nomad-ch create submit_done"
);

// nomad_ch.rs:3094-3098 (alloc_first_seen)
tracing::info!(
    job = %job_id,
    elapsed_ms = %fn_started.elapsed().as_millis(),
    "sandbox/nomad-ch alloc_first_seen"
);
```

Fields audited:

- `sandbox_id` — `Uuid` (sandbox-typed-id; UUIDv7 random tail). Already
  emitted by ≥30 sites across `nomad_ch.rs` (e.g., L418, L934, L1034 pre-r32).
  Public identifier; no PII coupling.
- `job` (`job_id`) — `"zsbx-<sandbox_id.simple()>"`. Deterministically derived
  from `sandbox_id`. No new entropy disclosed.
- `elapsed_ms` — non-secret latency scalar.

**Not present** in the new emits: pubkey fingerprint (`key_fp`), pubkey hex
(`ZSBX_PUBKEY_HEX`), `user_id`, `project_id`, path with `<user_id>`,
`agent_url`, IP, MAC, tap name, sealed-record bytes, root KEK material,
artifact-dir paths, env-var contents. None of the new fields cross any
identifier-disclosure threshold the lens hasn't already accepted for the
existing CREATE-path emits at L1033 (`sandbox_id`+`user_id`+`project_id`+`key_fp`)
and L1274 (`sandbox_id`+`vm_index`+`key_fp`+`elapsed_ms`).

The cadence-fix sites (`4ac1e526`, `a6e517b2`) do not add or alter any
`tracing::info!`/`warn!`/`error!`/`debug!` macro — only sleep durations
and matching doc-comments.

### Focal #2 — `gcp-worker-startup.sh:359` raw_exec removal: any other latent privilege escalation?

**CLEAN.** Searched the final post-c56893b2 script for `raw_exec`, `driver.`,
`privileged`, `root`, `sudo`, `setcap`, `cap_add`:

- The only remaining `client { options = { … } }` entry is `"user.blacklist" = ""` — empty
  blacklist is the Nomad default and not a privilege grant in itself; what
  matters is that no driver capable of arbitrary host execution is enabled.
- `chown root:root /etc/zeroship/nomad-plugins/nomad-driver-ch` (L172) is
  the driver-binary install owner-set, unchanged from r31. The driver binary
  is SHA-pinned (R20-S3 still enforced; current pin SHA `7bb90576…` at L172).
- `# Runs as root on first boot. Idempotent.` (L5) — the startup script itself
  runs as root via GCE metadata; no change.
- The explicit `plugin "nomad-driver-ch" { config {} }` stanza (L202-204) is
  the only driver-load path, and it's path- and SHA-pinned at L168-172.

No other escalation arm exists. The Nomad agent post-c56893b2 surfaces
exactly one driver — ch — and that driver's only privileged operation is
spawning the pinned cloud-hypervisor binary against a typed `TaskConfig`
the controller serializes (see Focal #3 in r31 — `serde_json::json!`
throughout; no string interpolation).

The L251-253 inline comment (`Ensure the rootfs image is the virtio-blk
variant the wrapper expects…`) is a stale R32-M1 leftover that the scrub
missed. **Not a security finding** — it's a code-quality residual in a
comment; the actual artifact name pin (`rootfs-slim.img.virtio-blk-v5`)
on L157 is unchanged and correct. Defer to code-quality lens; not raising.

### Focal #3 — R32-M1 scrub: did any error-message change leak something prior didn't?

**CLEAN.** Audited the 28 text-only edits in `3bc689ca` + `0fec9bc3` plus
the `backend/mod.rs` rustdoc fix in c56893b2's chain. All edits replace
the literal string `wrapper` with `ch driver` (or rewrite a deleted-wrapper
description into a TaskConfig-shape description). Two are user-facing error
strings; the rest are rustdoc / inline comments / test assertion messages
(only printed on test failure).

User-facing error strings audited:

1. `nomad_ch.rs:3981-3985` — `stale agent at {base_url}: expected pubkey_fingerprint={expected_fp}, got {actual_fp}; previous tenant's ch driver task still owns the IP`. Pre-r32: `…previous tenant's wrapper still owns the IP`. **Net leak delta: zero.** Both phrasings already disclose `base_url`, both fingerprints, and the diagnostic. The new phrasing replaces an attribution noun (operational accuracy) without adding any field.
2. `nomad_ch.rs:7340-7344` — test-only assertion message; only printed on
   `cargo test` failure. No production-log path.

Test-rename audit (R7 `derive_mac_matches_wrapper_pattern` →
`derive_mac_matches_pinned_format`, same for `derive_tap_*`): names are
internal test identifiers, unobservable outside `cargo test` output. Zero
production-log exposure.

### Focal #4 — AEAD trust chain: still tight?

**CLEAN.** `git diff 729f22dd..b172cee0 -- crates/sandbox/src/snapshot_aead.rs`
is empty (verified). The HKDF-derive → ChaCha20-Poly1305 → cipher-tag-`0x01`
→ per-chunk monotonic nonce → root KEK from `SANDBOX_SNAPSHOT_ROOT_KEK_PATH`
(mode 0o400, root-owned, set up at `gcp-worker-startup.sh:454-470`) chain is
byte-identical to r31. Sealed-record trust path (worker → control →
restore) unchanged.

The R32-M1 scrub did not touch any seal/unseal call site or the
`SANDBOX_AGENT_SANDBOX_ID` / `ZSBX_PUBKEY_HEX` injection-path code (only
rustdoc around it). The pubkey-hex cmdline-injection trust chain is
controller → ch driver (Go) → guest kernel cmdline → /sbin/init →
`/run/keys/controller-pubkey` → agent — the controller side is unchanged
this round, and the ch driver side is out-of-scope (separate worktree).

### Focal #5 — Admin auth: 12 handlers all gated?

**CLEAN.** `admin_handlers.rs` is byte-identical to r31. All 12 handlers
still call `admin_check_required` as the first non-arg statement:

| Handler | Line | Role |
|---|---|---|
| `list_all_sandboxes` | 411 | ReadOnly |
| `get_sandbox_detail` | 534 | ReadOnly |
| `list_user_sandboxes` | 593 | ReadOnly |
| `list_user_shares` | 687 | ReadOnly |
| `list_hosts` | 758 | ReadOnly |
| `export_user` | 835 | Full |
| `delete_user` | 1004 | Full |
| `snapshot_sandbox` | 1361 | Full |
| `wake_sandbox` | 1570 | Full |
| `poll_wake` | 1888 | ReadOnly |
| `cold_boot_sandbox` | 2024 | Full |
| `metrics_endpoint` | 2056 | ReadOnly |

T1 symmetric-timing logic in `admin_check_required` itself unchanged. No
new admin handlers added this round.

---

## Carry table at r32

| Finding | r31 | r32 |
|---|---|---|
| **r30-carry-HEREDOC** unquoted heredocs | CARRY | CARRY |
| **r30-carry-r28-M1** T5 body control-char | CARRY | CARRY |
| **r30-carry-S1** Guard B node-id | CARRY | CARRY |
| **r30-carry-M3** wake-poll RFC1918 | CARRY | CARRY |
| **r30-carry-M4** artifact_path Full-bearer | CARRY | CARRY |
| **r30-carry-M5** CI policy | CARRY | CARRY |
| **R20-S3** driver SHA pin | v24 carry | v25 SHA at `gcp-worker-startup.sh:172`; pin enforcement intact |
| **R13-S1** worker storage-rw scope | OPEN IMPORTANT | OPEN — **SOLE pre-cutover Ops blocker** |
| **R21-S1 / R21-S2 / R20-S2 / R19-S1 / R9-S3** | OPEN IMPORTANT | OPEN — unchanged |
| **[r31-S1]** raw_exec enabled in Nomad client | NEW MINOR | **CLOSED at c56893b2** |

---

## Counts

- CRITICAL: 0 new; 0 carry.
- IMPORTANT: 0 new. Carries: R21-S1, R21-S2, R20-S2, R19-S1, **R13-S1**, R9-S3.
- MINOR: 0 new. Carries: r30-carry-HEREDOC, r30-carry-r28-M1, r30-carry-S1, r30-carry-M3, r30-carry-M4, r30-carry-M5.
- **Closed this round: 1 ([r31-S1] raw_exec).**
- Total NEW this round: **0 CRITICAL, 0 IMPORTANT, 0 MINOR.**
- **Cutover gate: R13-S1 remains the SOLE pre-cutover Ops blocker.**

## Lens hand-off

- **Sandbox controller (Rust)**: pre-cutover CLEAR of IMPORTANTs; r32 net
  is a reduction (one MINOR closed, zero new).
- **Ops**: R13-S1 (worker SA `storage-rw` scope tighten) carries.
- **Concurrency**: R30-I1 (`catch_unwind` on gc_stopper) remains open;
  security lens defers.
- **Code-quality**: one cosmetic residual at `gcp-worker-startup.sh:251`
  ("wrapper expects" in a comment about the rootfs artifact pin) — defer.
