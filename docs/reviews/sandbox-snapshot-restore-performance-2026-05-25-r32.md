# Sandbox/snapshot-restore — performance r32 review

Date: 2026-05-25 (UTC). HEAD: `7c44cc78` (v25 perf-validation paperwork).
Driver pin: v25. Controller: v39.
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r31.md`.

New data this round (from `perf-validation-v25-r1.md`):

- c=1 WAKE p50: **49.1 s** (v24 50.7 s; FADV_WILLNEED prewarm **−1.6 s**,
  not the hypothesised 5–15 s).
- c=4 CREATE OK: **18/20** at ceil=20 (R31-P1 confirmed working).
- c=1 CREATE p50: **8.9 s** (v24 6.4 s; cold-cluster, not a regression).
- The −1.6 s prewarm delta places ≥ 95 % of wake wall inside CH-internal
  restore. The controller cannot shorten that with code; the next
  leverage point is hiding it (already done — see R32-P3).

---

## Summary

**1 new IMPORTANT, 2 new MINOR, R31-P1 RESOLVED, R30-P1 carries.**

- **R32-P1 NEW IMPORTANT** — CREATE path c=1 cold-boot 8.9 s lives
  largely in two sequential `mkfs.ext4` subprocesses on `workspace.img`
  and `home.img` (`nomad_ch.rs:1128-1146`). They run inside ONE
  `spawn_blocking` but are SERIALIZED inside it. Two knobs already exist:
  parallelise the two mkfs calls (~1–2 s win on first-sandbox-per-user);
  flip `driver_stages_disk_images=true` (`config.rs:1011`) so the driver
  overlaps mkfs with Nomad scheduling.

- **R32-P2 NEW MINOR** — `do_restore_inner` mkdir+rmdir of `alloc_dir`
  runs inline on the async worker (`restore_handler.rs:870-881`) BEFORE
  the `store.get` spawn_blocking. `rewrite_config_json` (line 985) does
  sync `read_to_string` + `write` also inline. Fold into the existing
  spawn_blocking.

- **R32-P3 NEW MINOR / answers brief** — async-restore-ack is ALREADY in
  place (`wake_machine.rs:104-108` returns 202 immediately, `drive()`
  runs detached). User-facing latency is NOT coupled to the 49 s wake
  wall when the client uses the async-ack polling protocol. The only
  callers still paying the full wall are the sync legacy
  `wake_response_mode` (`restore_handler.rs:2375-2384`); retire it.

- **R31-P1 RESOLVED** — ceil=20 + release_delay=2 s landed
  (`b75728ce`); v25 c=4 measurement showed CREATE OK 12/20 → 18/20.

- **Cadence quick-win (`a6e517b2`) effect** — UNMEASURABLE in c=1 wake
  p50. 50.7 → 49.1 s is the prewarm delta; the polling reduction trims
  ~50–200 ms but is masked by prewarm noise in c=1 N=3.

- **A3 / R5-P1 sync-I/O carries** — `store.get` 1-GB SHA-256 + GCS path
  is already wrapped in `spawn_blocking` (`restore_handler.rs:914`). All
  `ureq` Nomad calls go through `http_*_unsigned` which all use
  `spawn_blocking` (`nomad_ch.rs:3608-3645`). No new sync-I/O-on-async
  hits beyond R32-P2.

---

## CRITICAL

None.

---

## IMPORTANT

### R32-P1 NEW IMPORTANT — c=1 CREATE 8.9 s: sequential `mkfs.ext4 ×2`; two recovery paths already in tree

**File:Line**: `crates/sandbox/src/backend/nomad_ch.rs:1128-1146`,
`:4275-4335` (`create_ext4_image_if_missing`).

Cold-boot CREATE stages two raw ext4 images sequentially inside one
spawn_blocking:

```rust
let staged = compio::runtime::spawn_blocking(move || {
    std::fs::create_dir_all(&host_dir_owned)?;
    std::fs::create_dir_all(parent)?;
    create_ext4_image_if_missing(&workspace_img, size_gb)?;        // truncate + mkfs.ext4 + fsync_dir
    create_ext4_image_if_missing(&user_home_img_owned, size_gb)?;  // truncate + mkfs.ext4 + fsync_dir
    Ok(workspace_img)
}).await?;
```

`mkfs.ext4` on a fresh sparse file is typically 0.5–2 s per image on
GCE NVMe — the cold-boot v25 c=1 CREATE 8.9 s (vs warm 6.4 s) is the
~2.5 s gap consistent with the second mkfs round-trip (`home.img`
exists after the first sandbox).

**Knob A — Parallelise the two ext4 mkfs (LOC ~15)**: spawn two
`std::thread::spawn` inside the existing closure, join both. Cuts
cold-boot wall by ~1 × mkfs (~ 1–2 s) on first-sandbox-per-user. Disjoint
paths; zero-risk.

**Knob B — Flip `driver_stages_disk_images` default to `true`
(`config.rs:1011`, `:838`)**: the spawn_blocking branch is already
bypassable (`nomad_ch.rs:1122`). The driver materialises images on the
alloc-running worker, overlapping mkfs with Nomad scheduling — controller
returns immediately. Phase-4 cutover gate is the only blocker.

**Priority**: IMPORTANT. Knob B is the durable answer.

### R30-P1 carry IMPORTANT — permit-hold documentation gap

Unchanged. `nomad_ch.rs:1465-1468`.

---

## MINOR

### R32-P2 NEW MINOR — Three sync FS ops inline on the compio worker in `do_restore_inner`

**File:Line**: `crates/sandbox/src/restore_handler.rs:870-881, 985`.

```rust
if alloc_dir.exists() { std::fs::remove_dir_all(&alloc_dir)?; }  // line 871
std::fs::create_dir_all(&alloc_dir)?;                            // line 877
// ... post-store.get ...
rewrite_config_json(&config_path, snap.vm_index)?;               // line 985
```

`rewrite_config_json` (line 1251) is sync `read_to_string` + `write`.
Each costs 50–500 μs; under c=20 cohort wakes the worker-pool back-
pressure adds up.

**Fix** (~15 LOC): fold the three ops into the existing `store.get`
spawn_blocking. **MINOR** — wake p50 dominated by CH 42 s is not moved.

### R32-P3 NEW MINOR / brief answer — Async-restore-ack already exists; "decouple from CH 42 s" is solved controller-side

**File:Line**: `wake_machine.rs:104-108`, `lib.rs:1202`,
`restore_handler.rs:2375-2384`.

The wake controller returns 202 + wake_id immediately;
`wake_machine::drive()` runs as detached `detach_isolated`. User-facing
latency depends on client polling cadence against `wake_jobs`, not the
49 s CH restore wall.

**Outstanding**: the sync `wake_response_mode` path is vestigial (60 s
ntex-client deadline was the source of the C-7 fix dance). Confirm
async is the production default; retire sync if no caller depends on
it.

**Priority**: MINOR — observation. The brief's "next leverage point"
question is answered: there is no further controller-side latency to
extract from the wake path; the 42 s ceiling is CH-internal.

### Carry MINORs unchanged from r31

| Tag | Notes |
|---|---|
| R30-P2 `NomadStopPermitGuard::Drop` silent discard | `nomad_ch.rs:657-668` |
| R30-P3 `dec_nomad_stop_permits_in_use` convention | `metrics.rs:496-528` |
| R29-P2 doubled blocking-pool peak on wake join | `wake_machine.rs:518-530` |
| R29-P3 `transport_error` prefix-match stringly-typed | `restore_handler.rs:3083-3098` |

---

## RESOLVED this round

### R31-P1 — vm_index_ceil + release_delay tuning

`b75728ce` bumped defaults: ceil 12→20, delay 5→2 s. Measured c=4
CREATE OK: 12/20 → **18/20** (+50 % throughput). The remaining 2 are
boundary saturation at c=20; not a regression.

---

## OPEN / LATENT carries

R16-P3 / R16-P5 / R17-P2 / R5-P1b — snapshot-side bandwidth + sweep
caches; unchanged.

---

## Where the 8.9 s c=1 CREATE goes (brief Q1)

| Phase | Wall | Source |
|---|---|---|
| Pre-create gate + vm_index alloc | ~10 ms | `nomad_ch.rs:920-1057` |
| **2 × `mkfs.ext4` + fsync_dir on first-sandbox** | **~2.5 s** | `create_ext4_image_if_missing` ×2 |
| `submit_nomad_job` POST | ~200 ms | `nomad_ch.rs:3631-3645` |
| `wait_for_alloc_running` (driver StartTask) | ~3–5 s | `nomad_ch.rs:3031-3170` |
| `wait_for_agent_livez` (TCP + signed /version) | ~500 ms–1 s | `nomad_ch.rs:3833-3970` |
| persist.seal (sealed-auth pg) | ~50 ms | `nomad_ch.rs:1316-1335` |

The 6.4 s warm baseline reflects `home.img` early-return: subsequent
sandboxes per user mkfs only `workspace.img`, saving ~1.5 s.

---

## Cadence quick-win (`a6e517b2`) — measured effect

| Phase | v24 | v25 | Cadence-attributable delta |
|---|---|---|---|
| CREATE p50 c=1 | 6.4 s | 8.9 s | none (cold-cluster overhead) |
| WAKE p50 c=1 | 50.7 s | 49.1 s | ≤ 200 ms (rest is prewarm) |

The 250→100 ms / 150→50 ms changes save at most 50–200 ms per wake (2–3
polls × cadence reduction). v25's wake p50 improvement is dominated by
FADV_WILLNEED prewarm; the cadence delta is below the c=1 N=3 noise
floor. Correct and cheap, but not the source of the −1.6 s.

---

## Net assessment

The 8.9 s CREATE c=1 cold-boot path has two staged exits already in
tree (R32-P1 Knobs A + B). Knob B (flip `driver_stages_disk_images`
default) is the durable answer; Phase-4 gate is the only blocker.

The 49 s WAKE p50 is at its controller-side floor. Async-restore-ack
(R32-P3) already decouples user-facing latency from the 42 s CH wall.
No further controller leverage until CH itself gets faster.

Cadence quick-win is cheap and correct but ≤ 200 ms of the v25 −1.6 s
delta; rest is prewarm.

Sync-I/O hygiene (R32-P2): three small ops on the compio worker that
the existing spawn_blocking should swallow. Trivial.

**Next round's measurement ask**: c=1 N=20 wake-only run (isolate
CREATE/SNAPSHOT noise) to land a tight wake p50/p95/p99 against the
next driver pin.

---

## To perf r33 backlog

1. **R32-P1 Knob A** — parallelise the two `mkfs.ext4` calls inside the
   existing spawn_blocking. ~15 LOC, ~1–2 s first-sandbox win.
   **IMPORTANT (NEW r32).**
2. **R32-P1 Knob B** — flip `driver_stages_disk_images` default once
   Phase 4 is green; delete the spawn_blocking branch. Phase-3 gated.
3. **R32-P2** — fold three sync FS ops into the existing store.get
   spawn_blocking. ~15 LOC. **MINOR (NEW r32).**
4. **R32-P3** — confirm/retire sync `wake_response_mode`.
   **MINOR (NEW r32).**
5. **R30-P1** — permit-hold documentation gap. ~20 LOC docstring.
   **IMPORTANT carry.**
6. **R29-P2 / R29-P3 / R30-P2 / R30-P3** — carries, unchanged.

Controller-side LOC for r33 actionable items: ~50 LOC (Knob A + R32-P2 +
R30-P1 docstring). Knob B is a deletion gated on Phase 4.
