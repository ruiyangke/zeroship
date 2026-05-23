# DESIGN — nomad-driver-ch

Companion to [`docs/proposals/nomad-driver-ch.md`](../docs/proposals/nomad-driver-ch.md)
and [`docs/reviews/nomad-driver-ch-volantvm-spike.md`](../docs/reviews/nomad-driver-ch-volantvm-spike.md).
This file is the in-tree implementation record: status of each capability,
sprint owners, and the open questions carried from the proposal.

## Sprint tracker

| Sprint | Scope | Files touched |
|---|---|---|
| **T-0** | Scaffold + plugin handshake + capabilities | `cmd/`, `ch/driver.go`, `ch/task_config.go`, `ch/task_state.go`, `tests/` |
| **T-1** | Cold-boot CH spawn (no restore yet) | `ch/start_task.go`, `ch/ch_client.go`, plus new files for tap/virtiofsd/vm_index |
| **T-2** | Graceful stop ladder + WaitTask + DestroyTask | `ch/stop_task.go`, `ch/wait_task.go`, `ch/ch_client.go` |
| **T-3** | Per-VM /30 tap network plumbing | new file `ch/net.go` (or split into `ch/net/`) |
| **T-4** | RecoverTask via persisted TaskState + API socket | `ch/recover_task.go`, `tests/recover_task_test.go` |
| **T-5** | TaskStats stream (nice-to-have) | `ch/task_stats.go`, `ch/ch_client.go` |
| **T-6** | `--restore` cold-path + `ch-remote resume` | `ch/start_task.go`, `ch/ch_client.go` |

T-0 is the contents of this commit. T-1..T-6 are the follow-up sprints.

## Lifecycle method status

The table below mirrors the proposal § 3.3 RPC inventory. "Status" reads as:

- **Scaffolded** — function exists, returns an error tagged `T-N`.
- **Implemented** — function does what the proposal says; unit-tested.
- **Tested** — covered by `tests/` + a Nomad-client-driven smoke test.
- **Cluster-verified** — soaked on a real worker for at least 24h with no
  regression versus the bash wrapper.

| RPC | Status | Sprint | Notes |
|---|---|---|---|
| `PluginInfo` | Implemented | T-0 | Static literal; covered by `TestPluginInfo`. |
| `ConfigSchema` | Implemented | T-0 | HCL schema for driver-level config. |
| `SetConfig` | Implemented | T-0 | Msgpack decode + stash; no validation yet. |
| `TaskConfigSchema` | Implemented | T-0 | HCL schema for per-task config. |
| `Capabilities` | Implemented | T-0 | Frozen tuple; covered by `TestCapabilities`. |
| `Fingerprint` | Scaffolded | T-1 | Emits `Undetected`; real probe needs CH on PATH. |
| `StartTask` | Scaffolded | T-1 | Decodes config; rest is `T-1: StartTask not implemented`. |
| `StopTask` | Scaffolded | T-2 | `T-2: StopTask not implemented`. |
| `DestroyTask` | Scaffolded | T-2 | `T-2: DestroyTask not implemented`. |
| `WaitTask` | Scaffolded | T-2 | Returns a channel that immediately fires an error result. |
| `InspectTask` | Implemented | T-0 | Reads `taskHandle.TaskStatus()`; trivially correct. |
| `RecoverTask` | Scaffolded | T-4 | `T-4: RecoverTask not implemented`. |
| `TaskStats` | Scaffolded | T-5 | Channel closes on ctx; nothing emitted. |
| `TaskEvents` | Implemented | T-0 | Forwarded from the upstream `eventer`. |
| `SignalTask` | Scaffolded | T-2 | Errors `T-2: SignalTask not implemented`. |
| `ExecTask` | n/a | n/a | Capability claims `Exec=false`; method returns an error. |

## Gap analysis vs `hashicorp/nomad-driver-virt`

This is the table the prompt requested. Reads "what we needed" → "what
upstream gave us" → "what's in this scaffold right now" → "which sprint
finishes the job".

| Required capability | nomad-driver-virt has it | Status here | Sprint |
|---|---|---|---|
| Cold-boot CH spawn (our cmdline shape) | Partial (libvirt XML, not CH JSON) | Stubbed | **T-1** |
| `--restore` + `ch-remote resume` | No | Stubbed | **T-6** |
| Per-VM /30 tap | Partial (libvirt networks) | Stubbed | **T-3** |
| Multi-disk virtio-blk | Partial (libvirt disks) | Stubbed | **T-1** |
| RecoverTask via API socket | Partial (libvirt connection re-attach) | Stubbed | **T-4** |
| Graceful stop chain | Yes | Adapted, stubbed | **T-2** |
| TaskStats | Yes (libvirt domStats) | Stubbed | **T-5** (nice-to-have) |
| Clock-resync hook | n/a (driver-agnostic) | n/a | n/a |
| Plugin handshake | Yes (uses go-plugin) | **Adopted as-is** | T-0 done |
| HCL task config | Yes | Adapted (libvirt fields → CH fields) | **T-1** |
| Capabilities flags | Yes | Adopted (FSIsolation=Image, SendSignals=true, Exec=false) | T-0 done |

### Summary

- **3 of 11** rows are done (Plugin handshake, Capabilities, HCL schema shape).
- **8 of 11** rows are stubbed with `T-N` markers ready for the implementer.
- **1 of 11** rows is n/a (clock-resync is a controller concern; the driver
  is intentionally unaware of the sandbox lifecycle, cf. proposal § 4).

## Architectural notes carried from the proposal

These choices affect every sprint and should not be re-debated without a
fresh review pass.

1. **Native Go (not Rust).** Proposal § 6 recommends a Rust driver (option
   B1). The scaffold is Go because the user explicitly directed it — the
   plan is to revisit the Rust option once the lifecycle is proven. The Go
   path lets us reuse Nomad's official SDK and the upstream skeleton
   verbatim, at the cost of carrying a Go toolchain in our build.
2. **Driver is unaware of the sandbox lifecycle.** No knowledge of "snapshot
   restore" beyond `restore_from: string` on TaskConfig. All clock-resync /
   `register_restored` / sealed-record-unwrap orchestration stays controller-
   side (proposal § 4). Don't leak controller concerns into the driver.
3. **No FFI to the Rust crates.** The driver communicates with the
   controller via Nomad's HTTP API only. Adding a Rust↔Go bridge would
   compromise the "single-language deploy artifact" property; if it ever
   becomes necessary, that's a re-scope.
4. **`NetIsolationModes=[None]`** because we own the tap directly. CNI
   integration is explicitly out of scope (proposal § 12).
5. **`Exec=false`** because user code runs inside the VM, not in the Nomad
   task. The existing sandbox-agent RPC channel handles in-VM commands.

## Open questions

Carried from the proposal § 14 (some answers shift now that we're shipping
Go, not Rust):

1. **AutoMTLS for v1?** Go SDK + go-plugin handles AutoMTLS natively; no
   work for us. Leave on by default.
2. **Proto pin policy.** Following upstream — go-plugin's Nomad SDK pin
   (`github.com/hashicorp/nomad v1.11.3`) carries the protos; bump together.
3. **Shared types crate.** Less relevant for the Go scaffold (the Rust
   controller still hand-rolls the jobspec). If we ever want a typed
   bridge: see `docs/proposals/nomad-driver-ch.md` § 5.2.
4. **Driver binary distribution channel.** Likely `gsutil cp` on boot (matches
   how we provision `cloud-hypervisor` and `virtiofsd`). Decide before T-1
   lands so the wiring matches.
5. **CI for tests that need `/dev/kvm`.** GHA runners don't have kvm. Either
   self-hosted runner or skip-in-CI. The current scaffold tests don't need
   kvm; T-1 onwards will.
6. **Naming.** This module is `github.com/zeroship/nomad-driver-ch`. The
   abandoned volantvm driver is also called `nomad-driver-ch` — disambiguate
   by always referencing the GitHub-organisation prefix in docs.
7. **Concurrency model.** Pure goroutines (this is Go) — no compio question.
   The compio carve-out the proposal worried about is moot.

## Tracking tags reference

Every stub error message includes a `T-N` tag so a `grep -rn 'T-[0-9]'` over
this module enumerates the work-in-progress surface. The mapping:

| Tag | Description | Files |
|---|---|---|
| `T-0` | Scaffold (done in this commit) | n/a |
| `T-1` | StartTask cold-boot + Fingerprint | `start_task.go`, `ch_client.go`, `driver.go` |
| `T-2` | Stop/Wait/Destroy/Signal | `stop_task.go`, `wait_task.go`, `driver.go` |
| `T-3` | Per-VM tap network | (new file in T-3 sprint) |
| `T-4` | RecoverTask | `recover_task.go`, `tests/recover_task_test.go` |
| `T-5` | TaskStats | `task_stats.go`, `ch_client.go` |
| `T-6` | --restore + ch-remote resume | `start_task.go`, `ch_client.go` |
