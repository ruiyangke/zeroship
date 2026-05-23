# nomad-driver-ch sprint status

Auto-maintained by the 10-minute cron + sprint fixers.

## Completed

- [x] T-0 (scaffold) — `ee4a76c3` (`nomad-driver-ch: scaffold from hashicorp/nomad-driver-virt (T-0)`)
- [x] T-0.5 (flake + main.go) — see commits
  - `de35f49e` — `nomad-driver-ch: flake.nix + flake.lock for dev shell (T-0.5 part 1)`
  - `9f1a7e49` — `nomad-driver-ch: cmd/main.go go-plugin entry point (T-0.5 part 2)`
- [x] T-1 (StartTask cold-boot CH spawn) — `6a8d2fcf` (`nomad-driver-ch/start_task: implement cold-boot CH spawn (T-1)`)
- [x] T-2 (graceful stop ladder + DestroyTask + SignalTask) — `dd017c23` (`nomad-driver-ch/stop_task: graceful-stop ladder (T-2)`)
- [x] T-3 (per-VM /30 tap setup + teardown) — `31e05a3c` (`nomad-driver-ch/start_task,stop_task: per-VM /30 tap setup + teardown (T-3)`)
- [x] T-4 (RecoverTask via ch-remote API socket) — `nomad-driver-ch/recover_task: reattach to running CH via API socket (T-4)`
- [x] T-5 (TaskStats: per-task telemetry via host /proc) — `nomad-driver-ch/task_stats: per-task telemetry via /proc+/sys (T-5)`

## In progress

- (none)

## Up next

- T-6: Restore path (vm.restore + post-restore hooks; controller-side clock-resync wiring is T-7)
