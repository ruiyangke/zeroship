# Sandbox snapshot-restore architecture review — 2026-05-25 r33

**Reviewer**: architecture-r33 (post-R32-P1 mkfs parallelization, post-r32-A3 closure)
**HEAD**: `05eced23` (worktree `.worktrees/sandbox-snapshot-restore`, READ-ONLY).
**Predecessor**: r32 at `b172cee0` — `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r32.md`.
**Scope**: `crates/sandbox/**` only.

---

## Summary

Four commits since r32. Two are source changes:

- `2faaf39b` R32-P1 — parallelize `workspace.img` + `home.img` mkfs.ext4 via `std::thread::scope` inside the existing `spawn_blocking`. +42 / −4 LOC at `nomad_ch.rs:1141-1180`. Zero new types, zero new function symbols, zero module-boundary motion.
- `d5d4d532` r32-A3 + R33-M1 — thread `sandbox_id: &str` as new 3rd parameter through `wait_for_alloc_running` (signature change, one in-crate caller, one in-crate test). Three `tracing::warn!` sites in the function gain `sandbox_id` + `job` fields; the `alloc_first_seen` info emit gains `sandbox_id`. One rustdoc default-value typo in `config.rs:357-358` (`155` → `20`). +11 / −1 LOC source, +1 / −1 LOC config.

Source-LOC at HEAD:

- `crates/sandbox/src/backend/nomad_ch.rs` — 8364 LOC (+42 vs r32).
- `crates/sandbox/src/restore_handler.rs` — 5333 LOC (unchanged).
- `crates/sandbox/src/lib.rs` — 3184 LOC (unchanged).
- `crates/sandbox/src/backend/mod.rs` — 678 LOC (unchanged).
- `crates/sandbox/src/config.rs` — 1774 LOC (±0; one doc edit).

Both r30 IMPORTANT carries (r32-A1 ZSBX_* env block, r32-A2 NomadStopPermits on AppState) are bit-identical at HEAD. r32-A3 closed cleanly.

Net: 5 findings (0 CRITICAL, 3 IMPORTANT — 2 carries + 1 NEW elevated from concurrency-r33, 2 MINOR — 1 NEW signature-shape, 1 closure trail). No new module-boundary motion.

---

## CRITICAL

None.

---

## IMPORTANT

### [r33-A1 CARRY of r30-A1 / r31-A1 / r32-A1] ZSBX_* env block still emitted in both jobspec builders

Bit-identical at `05eced23`. Both sites unchanged: `nomad_ch.rs:2814-2842`, `restore_handler.rs:2549-2563`. The self-warning rustdoc at `nomad_ch.rs:2782-2784` again survives a touch-the-edges commit (R32-P1 and d5d4d532 both edit `nomad_ch.rs` without modifying it).

The d5d4d532 commit now puts *four* sites in `wait_for_alloc_running` emitting structured `sandbox_id` + `job` fields — third independent piece of evidence that structured tracing is the working pattern at this layer. The env block's debugging justification continues to be undercut by what is actually being written.

**r33 decision**: r32's "land the deletion as a forcing function at r33" recommendation stands. ~70 LOC delete + ~10 LOC `tracing::info!` substitutions. Severity unchanged.

### [r33-A2 CARRY of r30-A2 / r31-A2 / r32-A2] `nomad_stop_permits` on `AppState` still unconditional

Bit-identical. `lib.rs:258-259` and accessor at `:306-310` unchanged. **Now four rounds running**, concurrency-r33 + api-surface-r33 both re-flag. Quadruple-confirmed; the ~30-LOC backend-ownership move is overdue. Severity unchanged.

### [r33-A3 NEW IMPORTANT — elevated from concurrency-r33 R33-I1] Parallel mkfs of per-USER `home.img` widens a pre-existing TOCTOU race

R32-P1 is structurally clean *at the Rust-memory-safety layer* — concurrency-r33's "thread-scope borrow shape — VERIFIED CLEAN" analysis at `nomad_ch.rs:1155-1174` is correct: `workspace_img` and `user_home_img_owned` are disjoint owned `PathBuf`s, `workspace_img_size_gb` is `Copy`, both `ScopedJoinHandle`s are joined inside the scope, and the per-thread panic payload is mapped to `Err` to match the outer `spawn_blocking` panic semantics. The `std::thread::scope` abstraction choice is **correct here**: nesting `compio::spawn_blocking × 2 + futures::join!` would require `Send` bounds on the captured `PathBuf`s and split the error path into two future-poll sites, for the same observable throughput (both calls bottom out on the blocking-thread pool either way).

But the commit message's safety claim — "the two images touch disjoint paths and disjoint directories … zero cross-thread coordination surface" — is **TRUE per-sandbox, FALSE per-user**. `workspace.img` is per-sandbox under `host_dir/<sandbox_id>/workspace.img`; `home.img` is per-USER under `<user_home_dir_root>/<user_id>/home.img`. Two cold-boot CREATEs from the same user, in flight concurrently, both spawn a `home_h` thread targeting the **same path** through `create_ext4_image_if_missing`, which has classic check-then-act TOCTOU:

```
if path.exists() { return Ok(); }    // CHECK
truncate -s NG <path>                 // ACT 1
mkfs.ext4 -q -F <path>                // ACT 2 — no exclusive lock
fsync_dir + assert_disk_image_present
```

Pre-R32-P1 the race existed but workspace.img's serial wall halved the per-user-in-flight rate; R32-P1 removes that throttle. At c=4 same-user cold-boot, up to 4 concurrent `mkfs.ext4 -q -F` against the same file. Concurrency-r33 walked the failure mode (silent partial-superblock overwrite, observable only at next guest mount).

**Architectural verdict**: this is **NOT a missing per-user fence in R32-P1's design** — R32-P1 was *intra-CREATE* parallelism, not *inter-CREATE*. The race lives one layer up and predates R32-P1. The right fix is a per-user mutex at the controller, not inside the `thread::scope`.

**On the proposed `DashMap<UserId, Mutex<()>>` on AppState**: this is **NOT** another instance of r30-A2's antipattern. The home-image staging is *backend-agnostic* — Docker and K8s backends also stage per-user home images in concept. A per-user staging-mutex map is a process-global resource with backend-agnostic semantics. It belongs on AppState.

Contrast with r33-A2 NomadStopPermits: that name is *literally* Nomad-specific, bounds Nomad's `POST /shutdown` only, and Docker/K8s deployments allocate sized capacity they never use. *That* is the antipattern. A per-user staging mutex is not.

**Recommended fix** (~20 LOC): `DashMap<UserId, Arc<Mutex<()>>>` on `AppState`; `NomadCHBackend::create` holds it across the `home.img` half of staging — leave workspace.img unfenced to preserve R32-P1's perf win (only home.img mkfs serialises per user). When `driver_stages_disk_images=true` flips default (Phase 3), the entire map deletes with it.

Alternative: `O_CREAT | O_EXCL` lockfile. ~15 LOC, no AppState surface change, but the loser polls — operationally inferior.

**Stance**: take the Mutex+DashMap. Clean new abstraction with backend-agnostic semantics — the r30-A2 antipattern only fires when the field is *backend-specific* infrastructure shoehorned through process-global state. This isn't.

**Severity**: IMPORTANT. Fix before c≥4 cluster default.

---

## MINOR

### [r33-A4 NEW] `wait_for_alloc_running(..., sandbox_id: &str, ...)` — should take typed `&SandboxId`

The d5d4d532 fix correctly threads `sandbox_id` through the function. But the signature accepts `&str` and the production call site builds a temporary string at `nomad_ch.rs:1241`:

```rust
wait_for_alloc_running(
    &self.cfg.nomad_ch.nomad_addr,
    job_id,
    &sandbox_id.simple().to_string(),   // <- transient String
    Duration::from_secs(self.cfg.nomad_ch.alloc_running_timeout_secs),
)
```

`sandbox_id` upstream is a `Uuid` (see `NomadCHBackend::create` at `nomad_ch.rs:859`). The crate's own convention everywhere else in `nomad_ch.rs` is `sandbox_id: Uuid` + `.simple()` at the *emit* site (e.g., `tracing::info!(sandbox_id = %sandbox_id.simple(), ...)`). Threading a `&str` *and* serialising at the call site, then re-serialising inside the function via `tracing::warn!(sandbox_id = %sandbox_id, ...)`, double-buffers the same value and locks the function's signature to a specific textual form.

Two options:

1. **Take `Uuid` by value** (`Copy`, 16 bytes): `wait_for_alloc_running(nomad_addr, job_id, sandbox_id: Uuid, timeout)`. Emit-site formatting via `%sandbox_id.simple()`. Symmetric with `NomadCHBackend::create`'s own signature. ~6 LOC.
2. **Plumb a thin typed wrapper** if a typed_id phase rolls through this crate (see AGENTS.md "typed_id everywhere"). The sandbox crate is currently on raw `Uuid` throughout — that's a wider refactor and out of scope for this round.

Option 1 is the cheap, in-scope fix. It also obsoletes the in-test sentinel `"00000000000000000000000000000000"` at `nomad_ch.rs:5512` (uses `Uuid::nil()` directly).

**Severity**: MINOR. ~6 LOC. Sweep with the next nomad_ch touch.

### [r33-A5 closure trail] r32-A3 / r32-Mcc / R33-M1

- **r32-A3** closed at `d5d4d532`. `alloc_first_seen` now emits `sandbox_id`. Three rate-limited warn sites in `wait_for_alloc_running` also gained `sandbox_id` + `job` — emit-shape consistency complete for the placement-window milestone.
- **R33-M1** (config rustdoc default-value drift) closed at `d5d4d532`. One-line edit, no surface.

Restated here only for the closure trail.

---

## Module-boundary stability

`backend/nomad_ch.rs` ↔ `restore_handler.rs` ↔ `lib.rs` at `05eced23`:

- `wait_for_alloc_running` signature widened by one param. Sole production caller (`nomad_ch.rs:1238`) and sole test (`nomad_ch.rs:5509`) updated in the same commit. The blocking sibling `wait_for_alloc_running_blocking` in `restore_handler.rs:2743` is **not** widened — restore-path traces still elide `sandbox_id`. That is a separate (potential) finding for next round; the function is called from one site (`restore_handler.rs:2353`) that has `sandbox_id: Uuid` in scope. Leaving for an operator-debugging-driven prompt, not blocking arch.
- `AppState` surface unchanged.
- Two builder symmetry preserved: `build_nomad_job_json` (nomad_ch.rs) and `build_restore_nomad_job_json` (restore_handler.rs), one wire shape.
- `restore_handler.rs` at 5333 LOC unchanged. r30-A4 module-split question (~6 k threshold) is no closer.

No structural drift since r32.

---

## Net assessment

**R32-P1 is the right abstraction at the right layer.** `std::thread::scope` over nested `compio::spawn_blocking + futures::join!` is the correct call — borrow-clean, one panic-catch layer, no Send bounds, same blocking-pool throughput. **The race surface concurrency-r33 found is one layer above R32-P1**, in the controller's lack of per-user serialization for backend-agnostic per-user resources. R32-P1's commit message overclaims safety ("disjoint paths and disjoint dirents") in a way that obscures the per-user collision; the underlying decision is correct.

**The R33-I1 fix is a clean new abstraction, not r30-A2 redux.** Per-user staging mutex is a backend-agnostic process-global resource, semantically distinct from NomadStopPermits (which is Nomad-specific shoehorned through AppState). Take the `DashMap<UserId, Mutex<()>>` on AppState; don't conflate the two antipatterns.

**r32-A3 closure introduced typed-pressure (r33-A4).** Threading `&str` through `wait_for_alloc_running` when both the caller and the test already have `Uuid` in scope is a 6-LOC tidy-up. Worth doing when this function gets touched again.

**Three decisions for r34**:

1. **r33-A3 fix lands the DashMap-on-AppState shape**. Do not treat as r30-A2 antipattern; the semantics differ. The Mutex map deletes when `driver_stages_disk_images=true` becomes default.
2. **r33-A1 (env block) deletion is now four rounds overdue**. The trace-discipline evidence from d5d4d532 makes the case stronger, not weaker.
3. **r33-A2 (NomadStopPermits ownership move) is quadruple-confirmed**. Schedule the ~30-LOC backend-ownership move; gate accessor on `Backend::NomadCh`.
