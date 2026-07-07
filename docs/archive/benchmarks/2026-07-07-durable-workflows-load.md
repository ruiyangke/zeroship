# Durable workflows load bench — 2026-07-07

Status: single-machine dev measurement. This is not certified capacity and does
not satisfy operator G3 sign-off by itself.

## Harness

- Command:
  `ZEROSHIP_DW23_BENCH_ONLY=1 ZEROSHIP_DW23_BENCH_RUNS=128 ZEROSHIP_DW23_BENCH_CONCURRENCY=32 ZEROSHIP_DW23_BENCH_MAX_SECS=90 ./tests/e2e_durable_workflows.sh`
- Stack: real control service, real gateway, real worker, real control DB,
  deployed `.zship`, and `workflow_engine::tick_with_dispatcher` driven from an
  ignored Rust test.
- Workflow: `BenchWorkflow`, one `step.run("checkpoint", ...)` per run.
- Backlog: 128 queued runs on one app.
- Engine concurrency setting: 32 in-flight dispatches.
- Machine: `linux x86_64 logical_cpus=16 mem_gib=62.8`.

## Raw Results

| Metric | Measurement |
| --- | ---: |
| Elapsed wall time | 25.007436 s |
| Total dispatch claims | 256 |
| Dispatch claim throughput | 10.237 claims/sec |
| Completed run throughput | 5.118 runs/sec |
| Checkpoints written | 128 |
| Time until all checkpoints written | 12.730982 s |
| Journal write throughput | 10.054 checkpoints/sec |
| Replay latency samples | 256 |
| Replay latency p50 | 14.587 ms |
| Replay latency p95 | 281.657 ms |
| Replay latency p99 | 282.872 ms |
| Replay latency max | 285.044 ms |
| Sleep-wake accuracy | unknown; not measured in this pass |

## Placeholder Capacity Seed

operator-pending (G3): placeholder from a single-machine dev bench — operator
must re-measure + sign off before GA.

The placeholder is deliberately below the measured result:

- max concurrent runs per app: 4 (1/8 of the measured concurrency setting)
- max dispatches per app per second: 2 (<20% of measured dispatch claim throughput)
- max checkpoint writes per app per second: 2 (<20% of measured journal write throughput)

The code seed lives in
`crates/control/src/workflow_limits.rs` as
`operator_pending_g3_workflow_capacity`. It is documentation-only and is not
wired as a certified GA enforcement limit.
