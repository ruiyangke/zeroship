# Sandbox snapshot-restore code-quality review — 2026-05-25 r27

**Reviewer**: code-quality r27 (cron-pilot)
**HEAD**: `8def39e2`. **Prior**: r26 (`add6d5ef`).
**Scope since r26**: r27-S1 (`069dd277`), r27-M1 (`4f0f2259`), r27-M2 (`dfccd049`), R26-I1 close (`b5ec01a1`).

## Summary

- **6 findings**: 0 critical, 1 important, 5 minor.
- **R26-I1 CLOSED at `b5ec01a1`** — verified. Single source-of-truth `SnapshotRowMeta`.
- **Option 3 staging-locality rewrite assessment: NOT PAINFUL.** Direct code read confirms.
- **No new production unwrap()/expect() since r26.**

## IMPORTANT

### [R27-I1] `Backend::from_config / from_config_with_persist / from_config_full` telescoping — breakeven crossed

**Files**: `crates/sandbox/src/backend/mod.rs:188-234`.

**r27 movement**: zero. The cascade did not grow but is structurally unchanged.

**Why actionable NOW**: prompt cites three plausible-near-term orthogonal fields:
- r27-A1 staging-locality ADR: needs `driver_materializes_disks: bool`
- r26 VFIO-handoff edge: device-id field
- r26 tap-leak edge: bridge name

Any one forces a 4th constructor. ~17 LOC of pure delegation today; 4th orthogonal field doubles that.

**Recommended shape**: `BackendBuilder` with `.with_persist()` / `.with_local_nomad_node_id()`. ~25-30 LOC NET, absorbs every future orthogonal field. Per AGENTS.md "pre-launch no back-compat" — rename + delete in same PR.

## MINOR

### [R27-M1] r27-S1 hand-rolled host parse — userinfo + IPv6-zone-id error paths undocumented

**File**: `crates/sandbox/src/config.rs:436-513`.

What works: every claimed shape passes via 10 tests. RFC 3986 § 3.2.2 brackets correct.

What's quietly wrong (rejection-path UX):
- `http://user:pass@127.0.0.1:4646` → `host = "user"` → "not a loopback literal" error misleads operator who pasted credentials
- `[fe80::1%eth0]` → bracket extract yields `::1%eth0` → IpAddr::parse rejects → poor error msg

**Recommendation**: 4-6 lines of rustdoc on unsupported shapes + optional `@` pre-detection (4-5 lines).

### [R27-M2] `bytes[i] as char` in 6 sanitize-strip sites — Latin-1 cast corrupts non-ASCII (LATENT)

**Files**: `wake_machine.rs:932 / :938 / :982 / :1039 / :1112 / :1221`.

**Bug**: `b as char` for `b > 0x7F` interprets as Latin-1. A 3-byte UTF-8 codepoint like `€` (`E2 82 AC`) becomes 3 separate chars re-encoded to 6 bytes — corruption.

**Why latent**: Nomad bodies are JSON (UTF-8 passthrough); observed driver msgs are 100% ASCII. So corruption never triggers today.

**Why r27 surfaces it**: r27-M2 explicitly added `is_char_boundary` to the truncation path of `extract_failed_task_event_msgs` with a `€`-straddling regression test. The strip passes in `wake_machine.rs` don't get the same treatment. **Asymmetry**.

**Fix shape** (out of scope): replace `out.push(bytes[i] as char)` with `out.push_str(&msg[i..i+utf8_char_len])`. Or scan via `char_indices()`. ~30 LOC + 6 test cases.

**Recommendation**: not urgent (no observed corruption today), but the asymmetry is visible.

### [R27-M3] `is_char_boundary` decrement loop duplicated verbatim — extract on 3rd use

**Sites**: `wake_machine.rs:809-814` + `nomad_ch.rs:2908-2913`.

Identical 5-line decrement loop. N=2 today; trigger to extract at N=3. r27-M2's commit message already calls out the mirroring — healthy attribution.

**Recommendation**: do nothing this round. Watch-item.

### [R27-M4] `HYPHEN_POSITIONS.contains(&off)` linear scan per byte

**File**: `wake_machine.rs:1242-1252`.

36 iterations × 4-element linear contains = 144 comparisons per UUID-shaped probe. Negligible cost.

**Suggested**: `matches!(off, 8 | 13 | 18 | 23)` — 1 LOC, drops const, lets compiler emit jump-table. Pure clarity + micro-perf.

### [R27-M5] `strip_filesystem_paths` ordering comment over-explains a non-issue

**File**: `wake_machine.rs:1082-1088`. Comment contradicts itself ("order matters" + "actually safe either way"). Intent (preserve discipline for future shared-prefix entries) is correct but buried. Rewrite to lead with truth + discipline rationale.

## r27-A1 staging-locality structural assessment

Prompt asks: *"if stress-r5 RED triggers Option 3 structural rewrite, what's the code-quality assessment of the current `try_create` / `materializeRootfs` / `create_ext4_image_if_missing` shape? Are they coupled in ways that make rewrite painful?"*

**Answer**: NOT PAINFUL. Direct read of `nomad_ch.rs:670-820`:

1. Staging encapsulated in ONE `compio::runtime::spawn_blocking` block (`:777-792`).
2. Produces ONE `PathBuf` (`workspace_img`) and ONE bool side-effect (`guard.host_dir_created`).
3. 4 logically-independent operations inside (2× mkdir + 2× create_ext4_image_if_missing).
4. `workspace_img` consumed only at `:812` (passed to `build_nomad_job_json`).
5. `create_ext4_image_if_missing` (`:3887-3947`) is self-contained.

**Option 3 rewrite cost**: replace `:777-792` with a derive-only step (`let workspace_img = workspace_image_path(host_dir);`), add jobspec flag, remove `guard.host_dir_created`. Driver-side gets ~50 LOC new responsibility. **Controller-side delta: roughly -20 LOC, +5 LOC = net negative.** CreateGuard cleanup paths simplify.

**Verdict**: current shape well-factored AND future Option 3 rewrite is mechanical 1-hour PR. **No structural debt blocking the rewrite.**

## R26 carries — status this round

| Finding | r27 state |
| --- | --- |
| **R26-I1** read_snapshot_row duplicate | **CLOSED at `b5ec01a1`** — verified |
| **R26-I2** from_config* cascade | OPEN — carry as R27-I1, breakeven crossed |
| **R26-A1** BackendFailureDetail trait | DEFERRED per r26 (below ≥6-edge threshold) |
| **R26-M1-M5** | OPEN, no change |

## Bottom line

r27 lands clean on r27-S1, r27-M1, r27-M2, R26-I1. One real code-smell carry:

- **R27-I1** (`from_config*` 3-level cascade): breakeven now crossed. Land builder THIS round before 4th constructor forces 60-LOC cascade.

Four cosmetic items deferred. **No critical findings. No production unwrap()/expect() regression. Option 3 structural rewrite is mechanical when needed.**

Code-quality lens reads HEAD as **production-ready modulo R27-I1 builder lift**.
