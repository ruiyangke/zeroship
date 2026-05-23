# Architecture review — 2026-05-25 round 9

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `f1bed99a` + working-tree diff (A1 AEAD-wrap, R8-A3-5
spawn_blocking, A2b verify-metadata; uncommitted)
**Lens**: architecture — fix-cycle drain; sidecar vs. bash; r8 C1-triage check
**Prior**: `…-architecture-2026-05-24-r8.md` (HEAD `c07cbb62`)

## Summary

8 findings (3 critical, 3 important, 2 minor). r8's three flagship
triage items (proposal 1 LeasedVmSlot, proposal 2 SnapshotCapableBackend,
proposal 3 AppStateBuilder) all **stalled another cycle**: working-tree
shows A1+A3-5+A2b only; zero LOC against any of the three. **r8 C1
"11 items subsumable on execution" gap holds at 11.** Below: a structured
Rust-sidecar recommendation (R3-A3 from deferred); recommend **execute**,
~2-3 focused days.

## CRITICAL

**C1. r8-C1 triage stall — 9th cycle since R4-A2 opened.**
`lib.rs:217,276,316,324,350,361,372,393` still carry the 7 `with_*` +
`pub fn new_fixture`; `restore_handler.rs:162-170` still ships the
silent `Ok(())` default `register_restored`; `backend/mod.rs:176-180`
still carries the asymmetric `NomadCh(Arc<…>)` wrap + `nomad_ch_handle()`
escape hatch. Every fixer cycle since R4-A2 has chosen surface fixes
(`pub`→`pub(crate)`, `&dyn`→`Arc<dyn>`, spawn_blocking wraps) over the
three structural items that close 11 deferred entries on landing. r8's
binding question "can proposal 1 ship as a single conservative refactor
without adding features?" got answered by absence — no.

**C2. R8-DEPLOY1 still UNCLOSED in wrapper.**
`sandbox-agent/src/main.rs:90-100` hard-errors at boot when
`SANDBOX_AGENT_SANDBOX_ID` is unset; `handlers.rs:105` reads only that
env or `/run/keys/sandbox-id`. Grep across `crates/sandbox/scripts/`
returns **zero hits** at `f1bed99a`. The wrapper at lines 437-449
(cold-boot) and 374-377 (restore) does not append the env to
`--cmdline` nor write `/run/keys/sandbox-id`. **Every cluster wake is
broken until landed.** Deferred line 442-446 calls this "BROKEN until
landed (CRITICAL)" and this cycle's "fix all critical bugs first"
directive still skipped it. Two-line wrapper edit + rootfs re-bake.

**C3. A1 AEAD wrap (working-tree, `lib.rs:611-684`) lacks fail-CLOSED
on GCS path.** The in-flight diff's tiered branch logs
`tracing::error!("plaintext on disk + GCS")` then **still constructs
the store with `kek = None`** (passthrough). Same control-flow shape as
R5-Q1's default `Ok(())`: loud signal beside unchanged behaviour. A
boot assertion mirroring `assert_persist_required_when_snapshot_enabled`
(`lib.rs:809`) — call it `assert_kek_required_when_gcs_enabled` —
pushes tiered+no-KEK into fail-CLOSED at boot, not 24/7 error-log spam.

## IMPORTANT

**I1. R8-A3-5 propagated `Arc<dyn RestoreBackend>` flip.**
`restore_handler.rs:199-202,344-347` now take `Arc<dyn RestoreBackend>`
(was `&dyn`) so the closure moves into `spawn_blocking`. Third
`&dyn → Arc<dyn>` flip in 3 cycles (R5-P1b, R7-P1, R8-A3-5); the
still-`&dyn ChRemoteClient` shape signals an unmade architectural
choice. Decide: `&dyn` everywhere with `Arc<dyn>` only at compio
handoff, OR `Arc<dyn>` as crate invariant. "Neither, locally optimal"
isn't a choice.

**I2. AppStateBuilder count at 8 with A1.** r8-I1 called inflection
passed at 10; A1's `let aead_root_kek = RootKek::from_env()?` at
`lib.rs:625` plus the 73-line inline composition at `lib.rs:611-684`
opens an 8th `with_*` candidate. Each follow-on field (`with_aead_kek`,
future `with_l2_store`) accretes onto the same constructor instead of
being type-required by a `SnapshotStoreBuilder`.

**I3. R6-P1 detached snapshot still lacks shutdown-quiescence.**
`admin_handlers.rs:1310-1324` still `compio::runtime::spawn(...).detach()`.
Cumulative open: r6-P1, r8-I3, r9-I3. Every cycle that adds a
detach-style task is one that doesn't add `DetachedTaskTracker`.

## MINOR

**M1. Working-tree posture (positive).** A1, R8-A3-5, A2b are
self-contained; no new architectural surface from this fixer pass.
Cycle is **triage-stagnant, not regression-amplifying** — distinct
from r7-r8.

**M2. r8 C1 table re-counted.** Proposal 1 still 0/6; proposal 2 3/5
(no movement); proposal 3 0/3. Three cycles since r8's
proposal-execution recommendation, zero structural commits landed.

---

## Rust sidecar vs. bash wrapper (R3-A3 — recommend EXECUTE, ~2-3 days)

**Bash bug history demands this.** B12 (env-name mismatch), B13
(restore path missing), B17 (CH paused post-restore), B19's
wrapper-side prep, B22's clock_resync env injection, R8-DEPLOY1
(C2 above — env still missing), W1 (`sed -i -E` code-exec sink). Seven
CRITICAL bugs in two rounds, all in one 465-LOC bash file.
Cluster-smoke discovery cost is ~$0.50/round; bash logic that should
be unit-testable surfaces at the end of a 30-min provision loop.

**Sidecar boundary.** A `zsbx-vm-wrapper` binary (or
`zeroship-sandbox vm-wrapper` subcommand) consumes the same env
contract. Bash collapses to 5 LOC: `exec zsbx-vm-wrapper "$@"`. Main
loop: parse env → validate (taps, images, hex pubkey) → if `ZSBX_RESTORE_FROM`
set: `serde_json` rewrite of `config.json` (replaces sed at line 367) +
spawn CH `--restore` + poll resume via `ureq` to CH HTTP API + supervise;
else: build typed `Command::new("cloud-hypervisor")` argv + supervise.
SIGTERM/SIGKILL escalation = `Drop` on `ChChild` (replaces trap at
268-298). One trait `VmRunner` with `validate_env / prepare_disks /
rewrite_config / spawn_ch(SpawnMode) / poll_resume / supervise` makes
each step unit-testable.

**Structural closures (beyond W1).** R3-A3 closes (in deferred, not
duplicated here): **W1** (sed code-exec → typed JSON rewrite);
**R3-T3 + R5-T2** (`#[test]` replaces `bats`/`shellcheck`-only);
**R8-DEPLOY1** (env becomes a typed field on `ChCmdline`; missing it
fails compile, not cluster smoke); **R6-C1** (`Drop` on `ResumePoller`
replaces `RESUME_PID` capture); and the B12/B13/B17-class regressions
(env validation, path stat, paused-vCPU resume all become `Result`
arms with unit tests).

**Effort estimate verification.** Deferred says 2-3 days; my LOC count
supports the low end for the binary itself. **Long pole = cluster
smoke validation** — the bash is the wire contract between controller,
Nomad raw_exec, CH, and agent. Ship behind a `ZSBX_VM_WRAPPER_USE_RUST=1`
toggle, bash-default first, flip after one cluster cycle confirms
parity. Net: 2-3 dev days + 1 cluster cycle.

LOC classification (465 bash → ~240 Rust + ~150 LOC tests):

| Bash region (lines) | LOC | Rust shape |
|---|---:|---|
| Env validation (91-179) | 88 | `clap`/`serde-env`, ~30 |
| Path/IP/MAC (181-190) | 10 | trivial, ~10 |
| Tap up + image stat (192-229) | 38 | `Command::new("ip")`+`Path::exists`, ~25 |
| rootfs copy (231-263) | 33 | `std::fs::copy` + `FICLONE`, ~15 |
| Traps + cleanup (97-102, 268-298) | 30 | `Drop` + `signal-hook`, ~40 |
| Restore branch (316-428) | 115 | `serde_json` rewrite + `ureq` poll, ~80 |
| Cold-boot CH spawn (430-449) | 19 | typed `Command` argv, ~25 |
| Final wait + propagate (451-465) | 15 | `child.wait()`, ~15 |

The sed at line 367 — the W1 sink — collapses to a 12-line
`serde_json` rewrite that's correct by type, not by regex anchoring.
That alone justifies the work.
