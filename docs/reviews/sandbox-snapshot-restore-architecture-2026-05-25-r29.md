# Sandbox snapshot-restore architecture review — 2026-05-25 r29

**Reviewer**: architecture-r29 (post-R28-C1 + post-STARTUP-HEREDOC-LEAK + post-BackendBuilder shipment)
**HEAD**: `a3cfca10` (worktree at `.worktrees/sandbox-snapshot-restore`, READ-ONLY).
**Predecessor**: r28 at `0b8cf6c2` — `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r28.md`.
**Lens**: architecture (full this round; r28 was light).
**Scope reminders**: Do NOT touch `docs/proposals/sandbox-snapshot-restore.md`. No edits anywhere; analysis only.

---

## Summary

Three structural events landed between r28 (cycle 38) and r29 (cycle 41):

1. **R28-C1** (`9e1f6276`): inline `compio::time::sleep(release_delay).await` + `release(i)` in `CreateGuard::drop`'s detached future, replacing `VmIndexAllocator::spawn_delayed_release(...)`. The bug was a **runtime-lifetime mismatch**: the helper plants `compio::runtime::spawn(...).detach()` against the **current** runtime, and `CreateGuard::drop` now runs that current-runtime under `detach_isolated("create-rollbk", …)` which terminates as soon as `block_on(fut)` returns Ready — the 5 s timer task is dropped pending. The sibling `stop_inner` site (which runs on the long-lived ntex-worker runtime) keeps the helper. **One bug fixed, but the underlying class of footgun remains** — see r29-A2.

2. **R27-I1** (`df06d172`): `BackendBuilder` shipped, replacing the 3-level `from_config* → from_config_with_persist → from_config_full` telescoping cascade. Two `.with_*()` setters today (`with_persist`, `with_local_nomad_node_id`); shape absorbs future r27-A1 / VFIO / tap-leak fields. **The shape is sound for *uniform* fields; backend-specific fields ("nomad_node_id only meaningful for nomad-ch") leak the backend taxonomy into the public builder surface.** See r29-A3.

3. **STARTUP-HEREDOC-LEAK** (`a3cfca10`): three backticks inside an unquoted bash heredoc fired command-substitution at boot, aborting the systemd unit emission; workers booted without the ch driver; stress-r9 went 0/400 on CREATE. The fix escapes the three backticks. **`lint.sh` runs shellcheck at `--severity=error`; this class is only flagged at `--severity=style` (SC2006).** The fix closes the instance but not the class. See r29-A1.

r28's CRITICAL finding (r28-A1 host_dir lifecycle split-brain) carries **OPEN**, partially resolved by the migration of `host_dir_created` to log-gating-only — but two of the three ambiguities r28-A1 named are still live, and a NEW related ambiguity surfaced: the **disk-image-size contract** (`SANDBOX_WORKSPACE_IMAGE_SIZE_GB`) is not on the wire, so under flag-on the driver and controller each independently chose a size.

Six findings (2 CRITICAL, 2 IMPORTANT, 2 MINOR). The two CRITICALs are both about *class-level* fixes for structural footguns the r28 cycle uncovered as instances.

---

## CRITICAL

### [r29-A1] STARTUP-HEREDOC-LEAK is one instance of a class; the structural fix is not in `lint.sh`

The fix at `crates/sandbox/scripts/gcp-worker-startup.sh:595-599` escapes three backticks (`` `...` `` → `` \`...\` ``) so the unquoted-EOF heredoc no longer fires command substitution at boot. Verified shellcheck-clean per `lint.sh --severity=error`.

The class is **shell-injection-via-comment-text in unquoted bash heredocs**. The codebase has **11 heredocs across 4 scripts**, every one of them unquoted because each needs at least one shell expansion (`$ART`, `$DATACENTER`, `$VM_INDEX_CEIL`, …):

```
crates/sandbox/scripts/gcp-server-startup.sh:129:cat > /etc/nomad.d/nomad.hcl <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:216:cat > /etc/nomad.d/plugin-dir.hcl <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:287:cat > /etc/sysctl.d/99-zsbx.conf <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:305:cat > /usr/local/sbin/zsbx-taps-up.sh <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:321:cat > /etc/systemd/system/zsbx-taps.service <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:352:cat > /etc/nomad.d/nomad.hcl <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:439:cat > "$ART/sandbox-token.env" <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:445:cat > "$ART/sandbox-db.env" <<EOF
crates/sandbox/scripts/gcp-worker-startup.sh:511:cat > /etc/systemd/system/zsbx-ctl.service <<EOF   ← this round
crates/sandbox/scripts/provision-gcp-cluster.sh:338:cat <<EOF
crates/sandbox/scripts/bake-rootfs.sh:46:cat <<EOF
```

`lint.sh` at `--severity=error` does NOT catch this. Empirical verification (against a minimal repro `cat > foo <<EOF\n# Ref: \`crates/foo.rs\`\nEOF`):

| `--severity` | Detects? |
| --- | --- |
| `error` (lint.sh default) | NO |
| `warning` | NO |
| `info` | NO |
| `style` | YES (SC2006) |

The fix at `a3cfca10` is structurally fragile in two ways:

1. **Comment-prefix lull.** A future maintainer adding a comment line above a `Environment=` block — referencing `nomad_ch.rs:797` for diagnostic-trace reasons, just as the original author did — will hit the same bug. The comment "look harmless" because backticks are conventional in rustdoc / markdown; the operator does not think "this is bash code." The fix escaped this round's three lines; it did not eliminate the next set of additions from reintroducing it.
2. **`lint.sh` severity gate.** Raising lint.sh to `--severity=style` is the surface-level fix, but SC2006 also fires on legitimate backticks elsewhere in the scripts (e.g., the existing `\`cd "\$ZSBX_ARTIFACT_DIR"\`` precedent at `gcp-worker-startup.sh:560`). Promoting the gate without grandfathering means a global churn pass; not doing it means the class stays open.

**Severity**: CRITICAL — repeats are likely. Stress-r9 cost ~$0.50 to find this class for the first time; the second occurrence will cost the same and burn another 12 minutes of cluster wall + cycle worth of pilot attention.

**Structural fixes, ranked by leverage**:

1. **(Best) Move env-var injection to systemd drop-ins, keep unit bodies in tracked files.** `/etc/systemd/system/zsbx-ctl.service` becomes a checked-in fixture (no heredoc); `/etc/systemd/system/zsbx-ctl.service.d/10-env.conf` is generated by an `echo > ... <<'EOF'` (quoted-terminator) heredoc that only emits `Environment=` lines and substitutes via `printf '%s\n'`. Eliminates shell expansion from the unit body entirely. ~40 LOC refactor in `gcp-worker-startup.sh`.
2. **(Next best) Promote `lint.sh` to `--severity=style` AND grandfather existing offenders to the SC2006 ignore list.** Plus add an `# shellcheck disable=SC2006` annotation policy for legitimate uses. Catches the class going forward; grandfathered offenders are paid down opportunistically.
3. **(Minimum)** Add a `pre-flight gate to `provision-gcp-cluster.sh`: worker post-boot `nomad node status -self -json | jq '.Drivers.ch.Healthy' == true || ABORT`. Catches *this specific class downstream consequence* (workers without ch driver) but does NOT prevent other unquoted-heredoc bugs (a misexpansion that drops a different `Environment=` line will not abort).

Per `feedback_no_backward_compat.md` (pre-launch), option 1 is the right level — the heredoc-with-env-vars pattern is dead weight from before the systemd drop-in convention got picked up; rename + delete in one PR.

**Fix shape (option 1)**: ~40 LOC. Add `crates/sandbox/scripts/units/zsbx-ctl.service` (literal unit body, no `$VAR`s — uses `EnvironmentFile=/etc/systemd/system/zsbx-ctl.service.d/env.conf` instead). `gcp-worker-startup.sh:511` becomes `install -m 644 "$ART/zsbx-ctl.service" /etc/systemd/system/` + a separate `cat > /etc/systemd/system/zsbx-ctl.service.d/env.conf <<'EOF'` (note quoted terminator). The unit body never sees a heredoc expansion again.

**Recommendation**: file as CRITICAL backlog. Block stress-r10 on the structural fix landing, not just the instance fix. Cluster cycle cost is the load-bearing reason — repeat instances cost real dollars.

### [r29-A2] R28-C1 fixed the instance; the **inline-vs-spawn-by-runtime-lifetime** class is now a latent footgun across every `detach_isolated` body

The R28-C1 commit message identifies the root cause precisely: "the bug is the runtime lifetime mismatch, not the helper." The fix (inline `sleep + release` instead of `spawn_delayed_release`) closes the one call site. The inline-rustdoc comment block at `nomad_ch.rs:2275-2301` documents the trap for future readers.

But the class — **"any `compio::runtime::spawn(...).detach()` call inside a `detach_isolated` body will be dropped at runtime teardown"** — is structural across the entire sandbox crate. Inventory of `detach_isolated` callsites:

```
crates/sandbox/src/sweep.rs:256       detach_isolated("snap-transient", …)
crates/sandbox/src/sweep.rs:335       detach_isolated("wake-gc", …)
crates/sandbox/src/sweep.rs:441       detach_isolated("wake-takeover", …)
crates/sandbox/src/sweep.rs:780       detach_isolated("snap-idle-evict", …)
crates/sandbox/src/sweep.rs:1246      detach_isolated("host-dir-gc", …)
crates/sandbox/src/registry.rs:836    detach_isolated("snap-idle-gc", …)
crates/sandbox/src/admin_handlers.rs:1491  detach_isolated(…)
crates/sandbox/src/admin_handlers.rs:1851  detach_isolated(thread_name, machine.drive())
crates/sandbox/src/backend/nomad_ch.rs:2215 detach_isolated("create-rollbk", …)
crates/sandbox/src/wake_machine.rs (host module — drives via detach_isolated)
```

**Verified today**: only the (now-fixed) CreateGuard::drop call site called `compio::runtime::spawn(...).detach()` inside a `detach_isolated` body. The other call sites either don't spawn detached tasks at all, or use `compio::runtime::spawn_blocking(...)` (which runs to completion before returning, so the runtime stays alive long enough).

**The class is latent because** the helper `VmIndexAllocator::spawn_delayed_release` (at `nomad_ch.rs:363-387`) is still `pub` and still uses `compio::runtime::spawn(...).detach()` internally. Any future code path that:

- adds a new `detach_isolated` site, AND
- needs a delayed `release` (or any similar fire-and-forget timer), AND
- reaches for the existing helper rather than inlining

…will silently re-introduce R28-C1's failure mode. The inline rustdoc at the call site warns against the unify-the-two-sites move, but a green-field implementer of the third site will not read that comment because they're not editing CreateGuard::drop — they're writing fresh code in a different module.

**Three structural fix options**, ranked:

1. **(Best) Add a runtime-lifetime-safe helper to `crate::detach`.** Name: `detach::spawn_delayed_in_current` (returns a future the caller `.await`s instead of detaching a task). The contract becomes: "if you're inside a `detach_isolated`, `.await` this so the runtime stays alive until the sleep+release completes." `spawn_delayed_release` keeps existing for the ntex-worker path. The two are typed differently (the safe one returns `impl Future<Output=()>`; the fire-and-forget one returns `()`).
2. **(Next) Delete `spawn_delayed_release` entirely and inline at every caller** (currently 1 caller: `stop_inner`). The inline is 5 lines; the helper is 25. Per `feedback_no_backward_compat.md`, removing the helper is the higher-leverage move — *if you can't get the helper wrong by accident, the trap can't fire*. Future delayed-release callsites either inline (same as the two existing ones do today) or use the new `detach::spawn_delayed_in_current` helper from option 1.
3. **(Minimum) Document the trap on `detach_isolated`'s rustdoc.** Add a "Footgun" section: "Do not call `compio::runtime::spawn(...).detach()` inside the future returned by `make_fut`; the spawned task lives on the local short-lived runtime and is dropped when `block_on(fut)` returns. If you need fire-and-forget timer work, `.await` it inline." Cheapest fix; doesn't actually prevent the bug — just makes the documentation surface where a future engineer might encounter it.

**Severity**: CRITICAL — the class is a recurrence away. Today it's a single inline + a long warning comment. A new fire-and-forget timer in any of the 9 listed callsites will hit the same R28-C1 wedge silently (no compile error, no test failure — the timer's effect is the lost release of a slot, observed at next-boot orphan-prune via the leak counter).

**Recommendation**: option 1 + option 3 combined. New helper `detach::spawn_delayed_in_current(delay, fut) -> impl Future<Output=()>`; `spawn_delayed_release` deleted from `VmIndexAllocator`; CreateGuard::drop and stop_inner both call the new helper (CreateGuard::drop `.await`s it inline as today; stop_inner stays on the ntex-worker runtime so it spawns + detaches via a sibling helper `detach::spawn_delayed_in_worker`). Two cleanly-typed helpers, each documented for its lifetime; no caller can pick the wrong one because the return type forces the await pattern. Per the **R28-DISCIPLINE** convention, both helpers get a single test that confirms the timer fires under the respective runtime lifetime.

**~80 LOC**: 30 in `detach.rs` (two helpers + tests), -25 in `nomad_ch.rs` (delete `spawn_delayed_release`, replace two call sites), +25 in test coverage.

---

## IMPORTANT

### [r29-A3] BackendBuilder absorbs uniform fields well; backend-specific fields still telescope through `.build()`

The `BackendBuilder` shape at `crates/sandbox/src/backend/mod.rs:182-258` is correct for **uniform** fields — fields that apply to every Backend variant. `with_persist` is uniform: all three backends accept an `Option<Arc<Persistence>>` and store it identically. The setter takes `Arc<Persistence>` (not `Option<…>`), so callers self-document by invoking the setter only when they have a value.

The second setter, `with_local_nomad_node_id(node_id: String)`, **is not uniform**. The rustdoc at `:222-226` explicitly says "Other backends (`docker`, `k8s`) silently ignore the value — the constraint is meaningful only for the Nomad-driven path." This makes the builder surface a typed lie at the API level: an operator can configure `Docker + node_id="us-west-2"` and the second arg silently disappears.

This is a leak of the **backend taxonomy** into a builder that purports to be **field-orthogonal**. The R27-I1 commit message names the design choice ("orthogonal extension fields absorb as new `.with_*()` setters without reshaping any existing call site") — but each future *orthogonal* field is also each future *backend-specific* field. Already-named candidates:

- **r27-A1 staging-locality** (`driver_stages_disk_images`) — lives on `SandboxConfig` directly (not the builder), but the *if-not-nomad-ch-it-does-nothing* shape applies. The flag is currently top-level on `SandboxConfig`, not in any backend's nested config. If it joins the builder, it's the third silently-ignored-by-non-nomad-ch setter.
- **VFIO handoff** (r26 mention) — nomad-ch specific (Cloud Hypervisor passes VFIO devs to guests; Docker has no equivalent; K8s would use device plugins instead, a different API entirely).
- **tap-leak edges** — nomad-ch specific (tap devices don't exist in Docker/K8s).

**Three fix options**:

1. **(Best) Per-backend sub-builders.** `Backend::nomad_ch_builder(&cfg)` returns a `NomadChBuilder<'_>` that exposes only nomad-ch-relevant `.with_*()` setters and a `.build()` that returns `Backend`. `Backend::docker_builder(&cfg)`, `Backend::k8s_builder(&cfg)` similarly. The top-level `Backend::builder(&cfg)` dispatches on `cfg.backend.as_str()` and delegates to the right sub-builder for the uniform-only fields. Callsite ergonomics: tests/lifecycle examples (no backend-specific setters) use the top-level `.builder(&cfg).build()`; production callers in `lib.rs::AppState::from_config` know the backend variant statically (or branch and use the matching sub-builder). LOC: +50 in mod.rs (3 sub-builders × 15 LOC), -20 in mod.rs (delete `with_local_nomad_node_id` from top-level), +0 net at call sites (most are uniform-fields-only).
2. **(Next) Keep BackendBuilder, but document "no-op for other backends" explicitly in each setter's signature.** Rename `with_local_nomad_node_id` → `with_local_nomad_node_id_if_nomad_ch` or `with_nomad_ch_node_id`. Self-documenting; no API surface change.
3. **(Minimum) Document the class in `BackendBuilder`'s top-level rustdoc.** Add: "Setters fall into two classes: **uniform** (`.with_persist`, etc. — apply to every backend) and **backend-specific** (`.with_local_nomad_node_id` — meaningful only for `nomad-ch`; other backends silently ignore). New backend-specific setters MUST be named with the backend prefix to surface this." Cheapest, but doesn't prevent the next reviewer from reproducing the same shape.

**Severity**: IMPORTANT — the current shape has 1 backend-specific setter (`with_local_nomad_node_id`); 3 more are predictable per the R27-I1 commit message ("r27-A1, VFIO, tap-leak"). At 4 backend-specific setters, the builder's `.build()` has 4 silently-ignored values × 2 non-nomad-ch backends = 8 silent-discard arms in the implementation. The pattern is the **`Configurable` antipattern** (`Builder::default().a(...).b(...).c(...).build()` where `a/b/c` only apply to *some* underlying configurations).

**Recommendation**: option 1, pre-emptive, **before** the third nomad-ch setter lands. The shipped shape (2 setters, 1 uniform) is small enough that the refactor is ~50 LOC; at 4-5 setters it becomes load-bearing and the refactor is invasive. The pre-launch / no-back-compat invariant from `AGENTS.md` makes option 1 the right move: rename + delete, one PR.

**Cross-link**: this finding is partially in the **code-quality** lens (API ergonomics) and partially in **architecture** (typed-vs-stringly-typed taxonomy at the public surface). Code-quality r29's verdict, if it runs, should cross-reference this.

### [r29-A4] r28-A1 host_dir lifecycle split-brain: 2 of 3 ambiguities still OPEN; a NEW ambiguity (disk-image-size contract) surfaced

r28-A1 (CRITICAL, P0) named three ambiguities Phase 2 introduced:

1. `home.img` creator under flag-on is undefined at the typed boundary.
2. `CreateGuard::drop` policy under flag-on: "no-op for host_dir" but sweeper rustdoc says "controller-created."
3. `host_dir_created` is a free-form bool used as a state flag.

**Status verification against current `a3cfca10`:**

**Ambiguity 1 (home.img creator under flag-on)**: still OPEN. The wire schema at `build_nomad_job_json_with` (`crates/sandbox/src/backend/nomad_ch.rs:2693-2697`) emits both `workspace_img: <path>` and `user_home_img: <path>` plus the single bool `stage_disk_images`. Whether the driver materializes BOTH images on the staging op is **not verifiable from the controller-side code alone** — requires reading the Go driver's `nomad-driver-ch/ch/stage_disks.go`, which lives in a separate worktree and is not in this review's READ scope. The wire-emission tests at `nomad_ch.rs:7779-7849` pin the bool and the meta, but **NOT** that the driver materializes both files. **The architectural surface r28-A1 #1 named (typed image-list vs. single bool) is unchanged.**

**Ambiguity 2 (sweeper rustdoc vs. reality)**: still OPEN. The doc-comment block at `crates/sandbox/src/sweep.rs:831-866` reads:

```
// **T-8b-stress-r2 controller v34: host_dir cleanup is sweeper-owned,
// not per-alloc.** Stress-r2 (...) showed that the per-alloc
// `rm -rf host_dir` in CreateGuard::drop and stop_inner's step 5 was
// racing with concurrent retry-`create` for the same sandbox_id: the
// failing alloc's DestroyTask removed `workspace.img` while the retry's
// StartTask was running, causing 48/60 CREATEs to fail with
// "workspace.img does not exist (controller must stage before spawn)".
```

And at line 839: `the controller's `create_ext4_image_if_missing` had successfully created`. Under flag-on, the controller does NOT call `create_ext4_image_if_missing` — the driver does. The rustdoc is **incorrect under flag-on Phase 2**. The r28-A1 #2 recommendation (update the rustdoc to read "controller OR driver, depending on flag") was not actioned.

**Ambiguity 3 (`host_dir_created` as state flag)**: PARTIALLY resolved by reduction in scope. At `crates/sandbox/src/backend/nomad_ch.rs:2361-2368` (CreateGuard::drop), `host_dir_created` now ONLY gates whether to log "leaking host_dir" with the structured field set — the actual `rm -rf` has been removed (per the d638b10f sweeper-owns-cleanup policy). So the bool's *behavioral consequence* is no longer divergent across flag states; only its *diagnostic-log content* is. **The type-system gap (free-form bool, not enum) remains** but the blast radius is now "wrong log line on failed-create under flag-on" instead of "wrong rm -rf decision." The r28-A1 #3 typed-enum recommendation is still applicable, but the priority drops from P0 to P2.

**NEW ambiguity (r29-A4-NEW): disk-image-size contract**

`SandboxConfig.workspace_image_size_gb: u32` (config.rs:186) drives both `workspace.img` and `home.img` size in the controller's `spawn_blocking` block under flag-off. The same value is passed to both `create_ext4_image_if_missing` calls at `nomad_ch.rs:873` and `:875`.

**Under flag-on, the wire schema does NOT carry this size.** The driver-side `stage_disk_images` op must either hard-code a size, read its own env var, or read a TaskConfig field that's not in the schema today. Inspection of `build_nomad_job_json_with` confirms: the Config block (`:2679-2705`) emits `vm_index, kernel, cpus, memory_mb, restore_from, sandbox_id, user_id, workspace_img, user_home_img, pubkey_hex, subnet_base_octet, stage_disk_images, disks: [], fs: [], net: []`. **No `*_size_gb` field.**

This is a contract leak. An operator who runs the controller with `SANDBOX_WORKSPACE_IMAGE_SIZE_GB=40` (cluster bumped up workspace.img size for a heavyweight tenant template) and the driver hard-coded to 20 G will get a 20 G workspace under flag-on and a 40 G workspace under flag-off **for the same job spec** — silently, with no error. Symptom would emerge as "writes past 20 G fail with ENOSPC inside the guest" on a flag-on cluster, but fine on a flag-off cluster.

Worse, the **migration path** (Phase 4 default-flip → Phase 3 delete legacy branch) hides this: a flag-on cluster migrated from flag-off MAY have existing workspaces at 40 G that the driver-staged path tries to mkfs.ext4 over at 20 G. The `create_ext4_image_if_missing` skip-if-exists logic protects against re-mkfs (so existing 40 G workspaces stay at 40 G), but a fresh-sandbox-per-user creation produces a 20 G workspace on day-N+1 while the user's pre-flag-flip sandboxes are at 40 G. **Different workspace sizes for the same user, depending on when each sandbox was created.**

**Severity**: IMPORTANT — the migration path is undefined; the contract is not at the wire level; the symptom is silent. The same shape r28-A1 #1 named (typed image-list-with-size in the wire schema) closes this too.

**Recommendation**: bundle r28-A1 #1 + r29-A4-NEW into a single typed Config field:

```rust
"staged_images": [
    {"name": "workspace", "path": "<workspace_img>", "size_gb": <size>},
    {"name": "home",      "path": "<user_home_img>", "size_gb": <size>}
]
```

The driver iterates the list under `stage_disk_images=true`; the bool stays for the "stage at all?" gate. Wire emission centralizes both the image-set AND the size at one site. This is the cleanest place to carve out the "what does the driver stage?" contract because it's typed and exhaustive.

**~30 LOC across `build_nomad_job_json_with` + driver-side `task_config.go`** (cross-worktree). Land before stress-r10 to close r28-A1 #1 in lockstep with r29-A4-NEW.

---

## MINOR

### [r29-A5] sweep.rs:1132 "snapshotted/snapshotted_suspect → PRESERVE" comment is correct but the corresponding code path is *missing* the typed match

At `crates/sandbox/src/sweep.rs:1128-1132`:

```rust
//   - Row exists + status snapshotted/snapshotted_suspect →
//     PRESERVE. The workspace.img is durable state needed by
//     the next wake; reaping would silently break wake.
```

The comment names `Snapshotted` / `SnapshottedSuspect` as PRESERVE. The implementation delegates to `host_dir_eligible_by_db(...)` at `:986-1000`:

```rust
match row {
    None => true, // orphan
    Some(r) => matches!(
        r.status,
        SandboxStatus::Stopped | SandboxStatus::Lost | SandboxStatus::Orphan
    ),
}
```

The implementation lists ONLY the terminal-and-reapable states. `Snapshotted` / `SnapshottedSuspect` are correctly NOT in the match — but the comment's "PRESERVE" framing implies a *positive* preservation rule when the actual logic is *implicit* (everything-not-in-the-three-terminal-arms preserves).

If a future maintainer adds `SandboxStatus::Archived` (terminal, reapable) to the eligibility match without thinking through whether `Snapshotted` should be added too, the match grows by accretion and the "PRESERVE snapshotted" intent is no longer pinned anywhere — just an emergent property of the negative space.

**Fix shape**: convert the match to an exhaustive enum match (no wildcard), so future enum variants force a compile error until the maintainer pins the disposition explicitly:

```rust
match row.map(|r| r.status) {
    None => true,
    Some(SandboxStatus::Stopped) | Some(SandboxStatus::Lost) | Some(SandboxStatus::Orphan) => true,
    Some(SandboxStatus::Snapshotted) | Some(SandboxStatus::SnapshottedSuspect) => false, // PRESERVE
    Some(SandboxStatus::Creating) | Some(SandboxStatus::Running) | Some(SandboxStatus::Restoring) | … => false,
    // No wildcard — new variants force exhaustiveness
}
```

**Severity**: MINOR — defensive shape. The current implementation is correct today; the risk is future drift.

**~15 LOC**: replace one `matches!` with an exhaustive match arm; add a regression test `host_dir_eligible_by_db_snapshotted_is_preserve`.

### [r29-A6] `cfg.clone()` at every variant arm in `BackendBuilder::build` — negligible at boot, but the **pattern** is `Arc<SandboxConfig>` waiting to happen

At `crates/sandbox/src/backend/mod.rs:241-252`:

```rust
match cfg.backend.as_str() {
    "docker" => Ok(Backend::Docker(docker::DockerBackend::new(
        cfg.clone(), persist,
    ))),
    "k8s" => Ok(Backend::K8s(k8s::K8sBackend::new(
        cfg.clone(), persist,
    )?)),
    "nomad-ch" => Ok(Backend::NomadCh(std::sync::Arc::new(
        nomad_ch::NomadCHBackend::new(cfg.clone(), persist)?
            .with_local_nomad_node_id(local_nomad_node_id),
    ))),
    …
}
```

`SandboxConfig` has 50+ public fields including PathBuf, String, ApiToken (Zeroizing<String>), nested `NomadCHConfig` (~20 fields), nested `K8sConfig`, etc. The clone is **once at boot** (`AppState::from_config` is single-shot), so absolute cost is irrelevant — measured in microseconds at most, dominated by allocator overhead.

The architectural pattern matters more than the LOC. `SandboxConfig` is currently `Clone` because it's threaded by-value into Backend constructors. As more code paths reach for it (sweepers, restore_handler, admin handlers — each takes an `&SandboxConfig` or clones one), the **owned-by-many** semantic emerges. If a hot path is ever introduced (e.g., per-request reads of `cfg.driver_stages_disk_images` from a high-throughput endpoint), the clone-on-construct shape doesn't help; everyone already holds their own copy.

**The right move pre-launch**: switch `SandboxConfig` to `Arc<SandboxConfig>` at the AppState level. `Backend::*::new(cfg: Arc<SandboxConfig>, …)`. Builder takes `&'a Arc<SandboxConfig>` (no inner clone in `.build()`; just bumps Arc ref count). This shape composes with `BackendBuilder<'a>`'s lifetime: instead of `cfg: &'a SandboxConfig` (which forces clone at `.build()`), it becomes `cfg: &'a Arc<SandboxConfig>` (single Arc bump at `.build()`).

**Severity**: MINOR — no observable cost today. The pattern guidance matters for the next pre-launch sprint's architecture moves.

**~80 LOC**: thread `Arc<SandboxConfig>` through ~10 constructors. Per `feedback_no_backward_compat.md`, do this as a single PR with no migration shim.

---

## Cross-lens consensus

- **r28-A1 (CRITICAL, P0)**: PARTIALLY ADDRESSED. Ambiguity #3 collapsed to log-line-only divergence (host_dir is leaked unconditionally now). Ambiguities #1 and #2 still OPEN. r29-A4 carries forward + extends with the disk-image-size contract gap (r29-A4-NEW). **Recommendation: bundle r28-A1 + r29-A4-NEW into one structural fix landing the typed staged-images contract.**
- **r28-A2 (pg-saturation surface audit)**: no change at r29. The 500-cap + housekeeper combo absorbed stress-r9's load (which was 0/400 on CREATE for the heredoc-leak reason, not a pg-saturation reason). The audit's recommendation (per-purpose conn count metric) still applies; defer to post-stress-r10 GREEN.
- **r28-A3 (R1-DISC-3 pg_stat_activity oracle)**: not actioned. Round-39 / round-40 test-coverage cycles continued backlog drain on other items per the deferred.md log; R1-DISC-3 still has zero predicate test coverage at r29 HEAD. The architectural shape r28-A3 named is unchanged.
- **r28-A4 (Phase 5 KEEP/REMOVE dispositions)**: no change. The reap-wait + v13/v14/v15 driver patches remain KEEP-as-DiD; r5-A `F_OFD_SETLK` probe remains NOT-LANDED. Analytical conclusions stand.
- **r28-A5 (kernel-state surface sprint plan)**: deferred to post-stress-r10 GREEN. Stress-r9 was inconclusive; the gating cycle for sprint 1 audits has not occurred.
- **r27-A5 (kernel-state inventory rootfs.img row)**: STILL NOT UPDATED.
- **r27-A6 (playbook ADR retros)**: STILL NOT WRITTEN.
- **R28-DISCIPLINE (test-coverage audit)**: r29 architectural lens RECEIVES the audit's observation that R28-C1 has a regression test (`create_guard_drop_releases_vm_index_under_isolated_runtime`) — *but the test pins the instance, not the class.* A test that adds a NEW `detach_isolated` body which `spawn(...).detach()`s and asserts the spawned task's effect-on-the-world fires within N seconds would catch the class. r29-A2 names that gap; bundle with the fix.
- **STARTUP-HEREDOC-LEAK closure** (a3cfca10): instance fix, class still open per r29-A1.
- **stress-r9-retry-2** (in flight): does NOT validate any of r29's findings — it re-runs the r24-A2 binding-wedge cycle. Cluster review for stress-r9-retry-2 should NOT be interpreted as evidence on r29-A1 or r29-A2.
- **cluster review for next stress cycle**: gate on (a) r29-A1 structural fix landing (drop-in units), (b) heredoc-class lint promotion or grandfather. Without either, the next class-instance recurrence is one PR away.

---

## Lens hand-off (priority-ordered)

1. **r29-A1 (P0)**: STARTUP-HEREDOC-LEAK structural fix. ~40 LOC (drop-in units). Blocks the **class** of bugs that costs $0.50 per cluster cycle to discover. Land before stress-r10.
2. **r29-A2 (P0)**: `compio::runtime::spawn(...).detach()` inside `detach_isolated` body footgun. ~80 LOC (two typed helpers + delete `spawn_delayed_release`). Land in tandem with r29-A1 (same PR scope: structural-fixes-for-discovered-classes).
3. **r29-A4 (P1)**: typed staged-images contract closes r28-A1 #1 + r29-A4-NEW disk-size leak. ~30 LOC cross-worktree. Lands at the wire schema; touches both controller and driver. Land before Phase 3 (legacy-branch-deletion) — without it, the migration path is undefined.
4. **r29-A3 (P1)**: per-backend sub-builders. ~50 LOC. Land **before** the third nomad-ch setter (VFIO or tap-leak). At 2 setters today, the refactor is cheap; at 4+ it's invasive.
5. **r28-A3 carry (P1)**: R1-DISC-3 pg_stat_activity oracle. ~80 LOC + 5 LOC upstream. Not addressed by r29 work; defer to the test-coverage lens's next round.
6. **r29-A6 (P2)**: `Arc<SandboxConfig>` migration. ~80 LOC. Pre-launch shape fix; not urgent.
7. **r29-A5 (P3)**: exhaustive `SandboxStatus` match in `host_dir_eligible_by_db`. ~15 LOC. Defense-in-depth.
8. **r27-A5 + r27-A6 carries (P3)**: post-stress-r10 GREEN, document the rootfs.img file-lock surface in the inventory ADR; write r3-A + r4-A + Option C playbook retros.

---

## Carry status

| Finding | r29 status |
| --- | --- |
| r28-A1 Phase 2 split-brain (3 ambiguities) | **PARTIALLY OPEN** — #3 collapsed to log-line; #1 + #2 still open; supersedes / bundles with r29-A4 |
| r28-A2 pg-saturation surface audit | OPEN — no new pressure at r29; defer to post-stress-r10 GREEN |
| r28-A3 R1-DISC-3 pg_stat_activity oracle | OPEN — not actioned; carries to next test-coverage cycle |
| r28-A4 Phase 5 KEEP/REMOVE dispositions | CLOSED — analytical only, no action needed |
| r28-A5 kernel-state surface sprint plan | OPEN — gated on stress-r10 GREEN |
| r27-A5 inventory rootfs.img row | STILL OPEN |
| r27-A6 playbook ADR retros | STILL NOT WRITTEN |
| r26-A6 fsync_dir doc-comment lie | OPEN |
| R28-C1 inline-vs-spawn instance fix | CLOSED at `9e1f6276`; class-level fix tracked in r29-A2 |
| R27-I1 BackendBuilder | CLOSED at `df06d172`; backend-specific-setter taxonomy tracked in r29-A3 |
| STARTUP-HEREDOC-LEAK instance fix | CLOSED at `a3cfca10`; class-level fix tracked in r29-A1 |
| R27-API2 `_test_inject_sandbox` cfg gate | CLOSED at `c2e07b2f` |
| r24-A2-S3 vm_index release delay | LANDED at `c969b94d`; cluster-validated UNKNOWN per stress-r9 RED-inconclusive |
| **r29-A1** STARTUP-HEREDOC-LEAK class | **NEW (P0)** — structural fix beyond the instance |
| **r29-A2** detach_isolated + compio-spawn footgun class | **NEW (P0)** — typed helpers, delete `spawn_delayed_release` |
| **r29-A3** BackendBuilder backend-specific-setter taxonomy | **NEW (P1)** — per-backend sub-builders |
| **r29-A4** r28-A1 carries + NEW disk-image-size contract leak | **NEW (P1)** — typed staged-images wire field |
| **r29-A5** `SandboxStatus` exhaustive match in sweeper eligibility | **NEW (P3)** — defense-in-depth |
| **r29-A6** `Arc<SandboxConfig>` pre-launch shape fix | **NEW (P2)** — pattern guidance |

---

## Net assessment

Three classes of structural bug surfaced in the r28→r29 window — and each was *closed at the instance level only*. R28-C1 fixed one call site of the spawn-inside-detach trap; the helper that enables the trap is still `pub`. STARTUP-HEREDOC-LEAK fixed three backtick comments; the heredoc-with-unquoted-EOF pattern is in 11 callsites. R27-I1 / BackendBuilder is sound shape for uniform fields and a typed-lie for backend-specific fields; one of those has already shipped (`with_local_nomad_node_id`) with three more named in the commit message.

The pattern is **the codebase keeps generating instances of a class faster than the class gets named**. That's how stress-r9 cost $0.50 to find heredoc-leak; that's how R28-C1 cost a full concurrency review to find a 5-second-timer drop. r29's two CRITICAL recommendations both promote instance-fixes to class-fixes:

- r29-A1 → move unit bodies to checked-in files + drop-ins (structural), don't just escape this round's three backticks.
- r29-A2 → typed `detach::spawn_delayed_in_current` (structural) + delete `spawn_delayed_release` (eliminate the trap), don't just inline this round's one call site.

r29's two IMPORTANT recommendations close the architectural gap r28-A1 named, and pre-empt the BackendBuilder telescoping problem before it reaches 4-5 silently-ignored setters.

The pre-launch / no-back-compat invariant from `AGENTS.md` is the load-bearing context for r29: rename, delete, and restructure freely. None of the recommendations land migration shims; each is a single PR that breaks shape and updates every caller. That's the right discipline for this window — and it's the discipline that turns instance-fixes into class-fixes.

Three decisions on the next reviewer's desk:

1. **Land r29-A1 + r29-A2 in one structural-fix PR before stress-r10.** ~120 LOC combined. Closes the two CRITICAL classes; both have already cost ≥1 cluster cycle each to discover.
2. **Bundle r28-A1 ambiguities + r29-A4-NEW into a typed staged-images wire field.** ~30 LOC. Closes the Phase 2 split-brain at the wire-contract level; Phase 3 (legacy-branch deletion) is interpretable only with this in place.
3. **Restructure `BackendBuilder` into per-backend sub-builders BEFORE the third nomad-ch-specific setter lands.** ~50 LOC. Avoids paying the refactor cost when the surface has accreted to 4-5 setters.

r29's net stance: **the architectural pivot (Option C) is sound**; the **per-pivot ambiguities** r28 named are still partially open and have gained one new dimension (size contract); the **class-level discipline** for structural bugs that the r28→r29 window surfaced (R28-C1 trap, HEREDOC-LEAK class) needs to be promoted from "fix the instance" to "delete the trap." Six findings, all small, all closeable in two PRs.
