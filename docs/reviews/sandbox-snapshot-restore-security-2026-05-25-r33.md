# Sandbox/snapshot-restore — security r33 review

Date: 2026-05-25 (UTC). HEAD: `05eced23` (branch `feat/sandbox-snapshot-restore`).
Predecessor: r32 at `b172cee0` (`docs/reviews/sandbox-snapshot-restore-security-2026-05-25-r32.md`).
Scope: `crates/sandbox/**` and `crates/sandbox-agent/**`. READ-ONLY.

In-scope landings since r32:

- `2faaf39b` — `sandbox/nomad-ch`: parallelise the cold-boot `mkfs.ext4` pair for
  `workspace.img` and `home.img` inside a `std::thread::scope` (R32-P1, perf).
- `fe8c9216` — reviewer paperwork (out-of-scope `docs/reviews/`).
- `d5d4d532` — thread `sandbox_id` into `wait_for_alloc_running` and its
  three retry-path `tracing::warn!` sites; rustdoc drift fix (R33-M1).
- `05eced23` — cycle 54 reviewer paperwork (out-of-scope `docs/reviews/`).

## Summary

**r33 produces ZERO new CRITICAL, ZERO new IMPORTANT, ZERO new MINOR
security findings.** R13-S1 (worker SA `storage-rw`) remains the SOLE
pre-cutover Ops blocker. The two in-scope landings touch a perf code
path (R32-P1) and observability fields (R33-A3); neither alters AEAD,
admin auth, pubkey trust chain, or wire-format trust boundaries.

## CRITICAL

None.

## IMPORTANT

None new.

## MINOR

None new.

---

## Per-focal-area checks (per r33 brief)

### Focal #1 — R33-I1 (per-user `mkfs.ext4` race): is this a security issue?

**NOT a security finding. Concurrency-lens issue only.** Reasoning:

1. **Same-user-only collision surface.** The disputed path is
   `<user_home_dir_root>/<user_id>/home.img`. Two CREATEs racing on the
   same dirent presuppose **identical `user_id`**. They can't observe
   each other's `home.img` because they're the same user's `home.img`
   by construction.

2. **The per-user serialization gate already covers the mkfs window
   within a single controller.** `nomad_ch.rs:896-908`:

   ```rust
   let mut creating = self.creating_users.lock()…;
   if !creating.insert(user_id.to_string()) {
       return Err("concurrent sandbox create in progress for user …");
   }
   …
   let release_creating = ReleaseCreating::new(self.creating_users.clone(),
                                                user_id.to_string());
   ```

   `release_creating` is a `let`-bound RAII guard whose `Drop` fires
   at the close of `create()` (line ~1450 region). The `spawn_blocking`
   that runs both `mkfs.ext4` invocations sits between those two
   anchors. **A single controller cannot run two concurrent
   `home.img` mkfs for the same user.** Concurrency r33 confirms this
   (concurrency-r33 §[N/A] mkfs scope-borrow audit) and treats R33-I1
   as a **cross-controller**/multi-process surface only.

3. **Cross-controller race shape is shared NFS, NOT cross-tenant.** Two
   controllers writing the same `home.img` requires `host_state_dir` /
   `user_home_dir_root` to be shared across controller hosts. Today,
   the GCE deploy keeps these on each worker node's local NVMe
   (gcp-worker-startup.sh stages disks under `/var/lib/zeroship/sbx/`),
   and per-user home image lives on the same node-local mount as the
   ch-driver-managed VM that will mount it. **No cross-controller
   shared mount exists in the validated cluster topology.** If a
   future ADR introduces shared storage for `home.img`, the security
   posture would change because the race could then drop the
   user's filesystem in a corrupted state across controllers — but
   that's still single-user, not cross-tenant.

4. **What R33-I1 cannot cause:** cross-user content disclosure. The
   path component `<user_id>` is in the cwd of the mkfs invocation;
   `mkfs.ext4 -q -F` cannot reach a sibling `<other_user_id>/home.img`
   no matter what its inputs look like (no path traversal — `user_id`
   is `validate_typed_id(user_id, "usr", …)?`-gated at line 863, which
   calls `parse_with_prefix` to enforce `usr_<22-base62>` strictly, so
   `..` / `/` / slug chars are rejected upstream of any path
   `.join()`). The races cannot pivot to a different user's home
   image. The blast radius is bounded to the racing user's own
   data — a self-DoS / self-corruption surface, not a confidentiality
   surface.

**Net: defer R33-I1 to concurrency lens. Not a security finding.**

### Focal #2 — `sandbox_id` in new trace emits: sensitive when logged?

**CLEAN.** The d5d4d532 change adds `sandbox_id = %sandbox_id` to:

- `wait_for_alloc_running` JSON-parse-error warn (`nomad_ch.rs:3118-3123`)
- `wait_for_alloc_running` alloc_first_seen info (`:3136-3141`, was
  present at r32 review at `:3094`; the r33 churn is the `sandbox_id`
  field addition)
- HTTP-non-200 retry warn (`:3201-3205`)
- HTTP-transport-error retry warn (`:3222-3226`)

`sandbox_id` is a UUIDv7 (typed-id base62 random-tail; no entropy
linkage to user/project derivable without DB join). It is already in
the HTTP response body of every CREATE (`SandboxInfo.sandbox_id`),
already emitted by ≥30 `tracing::info!` sites in this same file
(confirmed in r32 audit), and is the standard observability key for
this subsystem. Pushing it into the previously sandbox-id-less retry
paths is **strictly an improvement** for incident-response correlation
(prior code logged `error = %msg` with no anchor to which sandbox the
retry storm belonged to).

Fields **not** present in the new emits: `key_fp`, `ZSBX_PUBKEY_HEX`,
`user_id`, `project_id`, sealed-record bytes, root KEK material,
agent_url, IP, MAC. None of the new fields cross any
identifier-disclosure threshold this lens has not previously accepted.

The test stub at `nomad_ch.rs:5509-5513` passes a fixed
`"00000000000000000000000000000000"` placeholder — test-only, not
production.

### Focal #3 — AEAD trust chain unchanged?

**CLEAN.**

```
$ git diff b172cee0..05eced23 -- crates/sandbox/src/snapshot_aead.rs
(empty)
$ git diff b172cee0..05eced23 -- crates/sandbox/src/persist.rs
(empty)
```

HKDF-derive → ChaCha20-Poly1305 → cipher-tag-`0x01` → per-chunk
monotonic nonce → root KEK from `SANDBOX_SNAPSHOT_ROOT_KEK_PATH`
chain is byte-identical to r31/r32. Sealed-record trust path (worker
→ control → restore) unchanged.

### Focal #4 — Admin auth unchanged?

**CLEAN.** `git diff b172cee0..05eced23 -- crates/sandbox/src/admin_handlers.rs`
is empty. All 12 admin handlers retain `admin_check_required(...)` as
the first non-arg statement (r32 table holds verbatim). T1
symmetric-timing logic in `admin_check_required` itself unchanged.

### Focal #5 — TOCTOU implications of R32-P1: malicious user-supplied input racing the mkfs to expose filesystem content?

**CLEAN. No user-supplied input flows into the mkfs path.**

Audit of every argument to `create_ext4_image_if_missing` after R32-P1:

| Arg | Source | User-controllable? |
|---|---|---|
| `workspace_img` | `workspace_image_path(host_dir)` = `host_dir.join("workspace.img")` | `host_dir = self.cfg.nomad_ch.host_state_dir.join(sandbox_id.simple().to_string())`. `sandbox_id` is generated by the controller (typed-id new, line ~852). Not user-controllable. |
| `user_home_img_owned` | `user_home_image_path(&cfg.user_home_dir_root, user_id)` = `<root>/<user_id>/home.img`. | `user_id` is `validate_typed_id(user_id, "usr", "user_id")?`-gated (line 863). `parse_with_prefix` requires `usr_<22-base62>` strict shape; `..`, `/`, `\0`, and any non-base62 char rejected upstream. |
| `workspace_img_size_gb` | `self.cfg.workspace_image_size_gb` (`u32`) | Operator config, not request-time. |

The thread-scope refactor at `:1155-1174` did NOT widen the input
trust boundary. The same `user_home_img_owned: PathBuf` that was
passed to the **sequential** `create_ext4_image_if_missing` call
pre-R32-P1 is now passed to the **parallel** spawn. The
typed-id-validated path remains the only input.

The R33-I1 race surface is **between two valid same-user CREATE
flows**, both of which have already passed `validate_typed_id` at the
boundary. A malicious request can't inject a different `user_id` into
one of the racing threads — `user_id` is a borrow into the parent
closure, not a fresh user-controlled input.

Net: R32-P1 introduces a per-user concurrency hazard (deferred to
concurrency r33) but **does not introduce a TOCTOU exploitable from
the HTTP boundary**.

### Focal #6 — Anything else r33-relevant?

The R33-M1 rustdoc-drift fix in d5d4d532 is doc-only (zero
behavioural change; the security lens verified the underlying
behaviour matches the new doc text).

---

## Carry table at r33

| Finding | r32 | r33 |
|---|---|---|
| **r30-carry-HEREDOC** unquoted heredocs | CARRY | CARRY |
| **r30-carry-r28-M1** T5 body control-char | CARRY | CARRY |
| **r30-carry-S1** Guard B node-id | CARRY | CARRY |
| **r30-carry-M3** wake-poll RFC1918 | CARRY | CARRY |
| **r30-carry-M4** artifact_path Full-bearer | CARRY | CARRY |
| **r30-carry-M5** CI policy | CARRY | CARRY |
| **R20-S3** driver SHA pin | v25 carry | v25 SHA at `gcp-worker-startup.sh:172`; pin enforcement intact |
| **R13-S1** worker storage-rw scope | OPEN IMPORTANT | OPEN — **SOLE pre-cutover Ops blocker** |
| **R21-S1 / R21-S2 / R20-S2 / R19-S1 / R9-S3** | OPEN IMPORTANT | OPEN — unchanged |
| **R33-I1** per-user mkfs race | n/a | **NOT a security finding** (concurrency r33 owns) |

---

## Counts

- CRITICAL: 0 new; 0 carry.
- IMPORTANT: 0 new. Carries: R21-S1, R21-S2, R20-S2, R19-S1, **R13-S1**, R9-S3.
- MINOR: 0 new. Carries: r30-carry-HEREDOC, r30-carry-r28-M1, r30-carry-S1, r30-carry-M3, r30-carry-M4, r30-carry-M5.
- **Closed this round: 0** (r33 has no security-lens closures or openings).
- Total NEW this round: **0 CRITICAL, 0 IMPORTANT, 0 MINOR.**
- **Cutover gate: R13-S1 remains the SOLE pre-cutover Ops blocker.**

## Lens hand-off

- **Sandbox controller (Rust)**: pre-cutover CLEAR of IMPORTANTs; r33 net
  is zero-delta on security posture.
- **Ops**: R13-S1 (worker SA `storage-rw` scope tighten) carries.
- **Concurrency**: **R33-I1 (per-user mkfs race) is theirs** — security
  has audited and confirms no cross-tenant or input-injection surface;
  defer scope/severity call to concurrency lens.
- **Code-quality**: r30-carry residuals unchanged.
