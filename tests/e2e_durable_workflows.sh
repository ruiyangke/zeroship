#!/usr/bin/env bash
# DW-07 durable-workflows M1 keystone faithful e2e.
#
# Boots disposable Postgres on :5440, applies the real platform migrations,
# boots real control/gateway/worker processes, deploys a real workflow .zship,
# then drives the real control workflow engine from the Rust integration test.
#
# NO ANTI-HOLLOW-GATE FLOOR, AND IT DOES NOT NEED ONE. The four
# tests/e2e_dev_vs_deployed_*.sh harnesses carry a MIN_PASSED floor because they
# iterate over CAPTURED rows: an empty capture runs no loop body, fires no
# assertion, fails nothing, and would otherwise exit 0 having proved nothing.
# This script has no such shape, verified 2026-08-11 by reading every site:
#   - every one of its 13 `fail` sites is immediately followed by `exit 1`
#     (or `exit 2` for the preflight ones), so no failure is merely printed;
#   - `set -euo pipefail` above aborts on any unchecked command failure;
#   - all six loops iterate a LITERAL or `seq` list (binaries, ports, retry
#     counts) - none iterates data captured at run time, so no loop body can
#     silently execute zero times.
# `pass`/`fail` here are plain `echo` with no counters, which is why grepping
# for a floor finds nothing. That is the structure providing the guarantee, not
# an omission. If a loop over captured rows is ever added, this stops being true
# and a floor becomes load-bearing.

set -euo pipefail

# `--bench` runs the DW-23 load bench instead of the DW-07 keystone arm. It is
# an ARGUMENT, not an environment variable. It used to be
# ZEROSHIP_DW23_BENCH_ONLY, which nothing set, and its three tuning knobs
# (RUNS / CONCURRENCY / MAX_SECS) were likewise env reads with no setter. The
# bench parameters are now literals in the Rust test, so the archived run in
# docs/archive/benchmarks/2026-07-07-durable-workflows-load.md reproduces from
# the command alone.
BENCH_ONLY=0
case "${1:-}" in
  --bench) BENCH_ONLY=1 ;;
  "") ;;
  *) echo "usage: $0 [--bench]" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"

PG_PORT="${PG_PORT:-5440}"
PG_CONTAINER="${PG_CONTAINER:-zs-dw07-pg}"
PG_USER="${PG_USER:-postgres}"
PG_PASS="${PG_PASS:-zeroship}"
PG_DB="${PG_DB:-zeroship_dw07_$(date +%s)_$$}"
ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9130}"
ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-9131}"
ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-9132}"
APP_NAME="dw07-$(date +%s)-$$"
DBURL="postgres://$PG_USER:$PG_PASS@localhost:$PG_PORT/$PG_DB"

WORK="$(mktemp -d -t zs-dw07-XXXXXX)"
PIDFILE="$WORK/pids"
: > "$PIDFILE"
PG_ADMIN_CONTAINER=""
OWNED_PG_CONTAINER=0
CREATED_PG_DB=0

pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1"; }

cleanup() {
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do
      [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  if [ "$CREATED_PG_DB" = "1" ] && [ -n "$PG_ADMIN_CONTAINER" ]; then
    docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
      -c "DROP DATABASE IF EXISTS \"$PG_DB\" WITH (FORCE);" >/dev/null 2>&1 || true
  fi
  # The per-app runtime role `env.db` speaks as is CLUSTER-scoped, so dropping
  # the disposable database above does not take it with it, and :5440 is
  # routinely a container shared with other harnesses. `prepare_side_effect_table`
  # (crates/zeroship-control/tests/durable_workflows_keystone_e2e.rs) creates one
  # per run - the app id is fresh each time, so they accumulate rather than
  # collide. Drop it here; after the database is gone it owns nothing, so this
  # succeeds. Best-effort: a leaked NOLOGIN role is untidy, not a failure.
  if [ -n "${APP_ID:-}" ] && [ -n "$PG_ADMIN_CONTAINER" ]; then
    docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
      -c "DROP ROLE IF EXISTS \"app_${APP_ID}_role\";" >/dev/null 2>&1 || true
  fi
  if [ "$OWNED_PG_CONTAINER" = "1" ]; then
    docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

stop_services() {
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do
      [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done < "$PIDFILE"
    : > "$PIDFILE"
  fi
  wait 2>/dev/null || true
}

terminate_pg_db_connections() {
  docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
    -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '$PG_DB' AND pid <> pg_backend_pid();" \
    >/dev/null
}

require_cmd() {
  command -v "$1" >/dev/null || { fail "$1 required"; exit 2; }
}

build_zship() {
  local js_file="$1"
  local out_path="$2"
  local stage hash now
  stage="$(mktemp -d -t zs-dw07-zship-XXXXXX)"
  mkdir -p "$stage/blobs"
  hash="$(sha256sum "$js_file" | awk '{print $1}')"
  cp "$js_file" "$stage/blobs/$hash"
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  cat > "$stage/manifest.json" <<EOF
{"version":1,"resources":{"/[...rest]":{"auth":"anonymous","publicly_accessible":true}},"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"worker":{"entry":"index.js","modules":{"index.js":"$hash"}},"workflows":["KeystoneWorkflow","SignalWorkflow","TopicSignalWorkflow","ConcurrentWorkflow","ConcurrentCommitWorkflow","SingleCommitWorkflow","SideEffectWorkflow","BareAwaitWorkflow","NameDivergenceWorkflow","CompensationWorkflow","CompensationNameDivergenceWorkflow","ScheduledWorkflow","BenchWorkflow","BlobOutputWorkflow","StreamLimitWorkflow","ChildEchoWorkflow","ChildFailWorkflow","ChildBlockWorkflow","ChildTrackedBlockWorkflow","ParentCallWorkflow","ParentStartManyWorkflow","ParentCatchChildFailureWorkflow","ParentCascadeWorkflow","ParentManyCascadeWorkflow","ContinueAsNewWorkflow","CompensableCarryWorkflow"],"schedules":[{"name":"dw14-scheduled","workflowName":"ScheduledWorkflow","input":{"case":"schedule"},"overlap":"allow","catchUp":{"mode":"skip"},"schedule":{"kind":"cron","cron_expr":"* * * * *","tz":"UTC","overlap":"allow","catchUp":{"mode":"skip"}}}],"metadata":{"compiler":"dw07-e2e","built_at":"$now"}}
EOF
  (cd "$stage" && tar --format=ustar -cf - manifest.json "blobs/$hash") \
    | zstd -q -f -o "$out_path"
  rm -rf "$stage"
}

wait_health() {
  local name="$1" url="$2" log="$3"
  local i
  for i in $(seq 1 45); do
    curl -sf "$url" >/dev/null 2>&1 && { pass "$name healthy"; return 0; }
    sleep 1
  done
  fail "$name unhealthy"
  tail -80 "$log" || true
  exit 1
}

require_cmd docker
require_cmd curl
require_cmd sha256sum
require_cmd tar
require_cmd zstd
require_cmd pnpm
require_cmd lsof

echo "=== DW-07 build ==="
pnpm build
cargo build --release -p zeroship-control -p zeroship-gateway -p zeroship-worker
pnpm --filter zero-migrate-cli build
for b in zeroship-control zeroship-gate zeroship-worker dev-provision; do
  [ -x "$BIN/$b" ] || { fail "missing $BIN/$b"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { fail "missing the zero-migrate CLI - run: pnpm install && pnpm build"; exit 2; }
pass "release binaries built"

echo "=== DW-07 test warmup ==="
# `durable_workflows_keystone_e2e` is a MODULE of the `main` test target, not a
# target of its own: control's 48 integration files collapsed into four
# executables on 2026-08-20, so `--test durable_workflows_keystone_e2e` no longer
# resolves. Every selection below names the target and filters on the module
# path, which is a prefix of every test name in it.
cargo test -p zeroship-control --test main --no-run
# workflow_engine_test needs a live PostgreSQL and therefore carries
# `required-features = ["live-db-tests"]`; without the feature cargo reports "no
# test target named workflow_engine_test" rather than building it.
cargo test -p zeroship-control --features live-db-tests --test workflow_engine_test --no-run
pass "test binaries warmed"

echo "=== DW-07 database :$PG_PORT ==="
case "$PG_DB" in
  *[!A-Za-z0-9_]*)
    fail "PG_DB must contain only letters, digits, and underscores: $PG_DB"
    exit 2
    ;;
esac

PG_ADMIN_CONTAINER="$(docker ps --format '{{.Names}} {{.Ports}}' \
  | awk -v port="$PG_PORT" 'index($0, ":" port "->5432/tcp") { print $1; exit }')"
if [ -n "$PG_ADMIN_CONTAINER" ]; then
  pass "using existing Postgres container $PG_ADMIN_CONTAINER on :$PG_PORT"
else
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
    -e POSTGRES_PASSWORD="$PG_PASS" -e POSTGRES_USER="$PG_USER" -e POSTGRES_DB=postgres \
    postgres:16 -c max_connections=300 >/dev/null
  PG_ADMIN_CONTAINER="$PG_CONTAINER"
  OWNED_PG_CONTAINER=1
fi

for _ in $(seq 1 45); do
  docker exec "$PG_ADMIN_CONTAINER" pg_isready -U "$PG_USER" >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$PG_ADMIN_CONTAINER" pg_isready -U "$PG_USER" >/dev/null 2>&1 || {
  fail "postgres never became ready"
  exit 1
}
pass "PG ready on :$PG_PORT"

docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS \"$PG_DB\" WITH (FORCE);" \
  -c "CREATE DATABASE \"$PG_DB\";" >/dev/null
CREATED_PG_DB=1
pass "created disposable database $PG_DB on :$PG_PORT"

if [ -f "$ROOT/ops/postgres-init.sql" ]; then
  docker exec -i "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 \
    < "$ROOT/ops/postgres-init.sql" >/dev/null
  pass "applied ops/postgres-init.sql"
fi

zs_platform_migrate "$DBURL" \
  --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship \
  --project-id zeroship > "$WORK/migrate.log" 2>&1 || {
    fail "platform migrations failed"
    tail -80 "$WORK/migrate.log"
    exit 1
  }
pass "platform migrations applied with zeroship-platform-migrate"

mkdir -p "$WORK/blobs" "$WORK/blob-cache"

echo "=== DW-07 workflow .zship ==="
cat > "$WORK/workflow.js" <<EOF
import { env } from "zeroship";

// WHAT THESE TWO HELPERS ARE FOR, and why they do not use \`fetch\`.
//
// A durable workflow's exactly-once property is not visible in the journal. A
// step body that ran TWICE and was then memoized leaves a journal byte-for-byte
// identical to one that ran once, so the count of BODY EXECUTIONS has to be
// recorded somewhere the engine does not write. That out-of-band record is what
// \`bump\` and \`commit\` produce, and every side_counts / effect_attempt_counts /
// effect_commit_counts assertion in
// crates/zeroship-control/tests/durable_workflows_keystone_e2e.rs reads it.
// The mechanism is scaffolding; the property is the test.
//
// UNTIL 2026-08-27 THE SCAFFOLDING WAS AN HTTP CALL to a harness server on
// http://127.0.0.1:<side port>, and it only connected because this script
// launched \`zeroship-worker\` with \`ZEROSHIP_DEV=1\`, which the SSRF gate read
// out of the process environment and treated as "skip all host/IP validation".
// That read is gone (crates/zeroship-runtime/src/transport/ssrf.rs:56-68):
// dev-ness is a stated input written only by \`set_dev_mode\`, whose one caller
// is \`cmd_serve\` (crates/zeroship-cli/src/main.rs:96), and what it now grants
// is loopback ONLY. \`zeroship-worker\` states nothing, so it runs the full
// floor and refuses 127.0.0.1 - which is correct, and must not be given a way
// around. There is no worker flag, feature or variable to re-open it here.
//
// env.db is the replacement, and it is not a workaround: it is a real platform
// primitive performing a real durable write, into the same Postgres the
// assertions already query, which is what a creator's workflow step doing a
// side effect would actually do. It needs no egress at all.
//
// The three tables live in this app's OWN schema and are created by
// \`prepare_side_effect_table\` in that Rust file.
const SIDE_EFFECTS = "workflow_e2e_side_effects";
const EFFECT_ATTEMPTS = "workflow_e2e_effect_attempts";
const EFFECT_COMMITS = "workflow_e2e_effect_commits";

async function bump(runId, stepName) {
  const table = env.db.collection(SIDE_EFFECTS);
  // PRE-insert count, preserving verbatim what the old side-effect server
  // returned: its data-modifying CTE could not see its own INSERT, so the first
  // bump for a (run, step) answered 0. assert_side_effect_steps pins that 0, and
  // it is the journaled value replayed into the run output.
  const count = await table.count({ run_id: runId, step_name: stepName });
  await table.insert({ run_id: runId, step_name: stepName });
  return { step: stepName, count };
}

async function commit(runId, stepName, key) {
  // One attempt row per entry into the effect boundary...
  await env.db.collection(EFFECT_ATTEMPTS).insert({
    run_id: runId,
    step_name: stepName,
    idempotency_key: key,
  });
  // ...and at most one commit row per idempotency key, however many times the
  // boundary is entered.
  //
  // This catch IS the old \`ON CONFLICT (idempotency_key) DO NOTHING\`, and it is
  // not a substitute for one: the UNIQUE index does the deciding, in the
  // database, so two concurrent entries carrying one key still leave one row -
  // which is the exact property effect_commit_counts measures. The loser just
  // learns it lost. \`upsert\` would be wrong here: it is DO UPDATE, so a
  // re-entry would rewrite the surviving row's run_id and step_name, and
  // ordered_commit_steps reads both.
  //
  // Only \`unique_violation\` is swallowed (crates/zeroship-data-orm/src/error.rs:287,
  // :337-339); every other db error still fails the step, which is what a
  // silently-broken side effect must do to a test that counts side effects.
  try {
    await env.db.collection(EFFECT_COMMITS).insert({
      run_id: runId,
      step_name: stepName,
      idempotency_key: key,
    });
  } catch (error) {
    if (!error || error.code !== "unique_violation") {
      throw error;
    }
  }
  const committed = await env.db
    .collection(EFFECT_COMMITS)
    .count({ idempotency_key: key });
  return { step: stepName, committed };
}

export class KeystoneWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    await step.sleep("sleep", "PT1S");
    const b = await step.run("b", () => bump(trigger.runId, "b"));
    return { a, b };
  }
}

export class SignalWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    try {
      const signal = await step.waitForSignal("go", {
        type: "go",
        timeout: trigger.input.timeout,
        maxSignalAge: trigger.input.maxSignalAge,
      });
      const b = await step.run("b", () => bump(trigger.runId, "b"));
      return { state: "signaled", a, signal, b };
    } catch (error) {
      if (!error || error.name !== "WorkflowTimeoutError") {
        throw error;
      }
      const timeout = await step.run("timeout", () => bump(trigger.runId, "timeout"));
      return { state: "timeout", errorName: error.name, a, timeout };
    }
  }
}

export class TopicSignalWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    const signal = await step.waitForSignal("topic-go", {
      type: "go",
      topic: trigger.input.topic,
      timeout: "PT30S",
    });
    const b = await step.run("b", () => bump(trigger.runId, "b"));
    return { state: "topic-signaled", a, signal, b };
  }
}

export class ConcurrentWorkflow {
  async run(trigger, step) {
    const [a, b, c] = await Promise.all([
      step.run("a", () => bump(trigger.runId, "a")),
      step.run("b", () => bump(trigger.runId, "b")),
      step.run("c", () => bump(trigger.runId, "c")),
    ]);
    const final = await step.run("final", () => bump(trigger.runId, "final"));
    return { a, b, c, final };
  }
}

export class ConcurrentCommitWorkflow {
  async run(trigger, step) {
    const [a, b, c] = await Promise.all([
      step.run("a", () => commit(trigger.runId, "frontier:a", \`\${trigger.runId}:frontier:a\`)),
      step.run("b", () => commit(trigger.runId, "frontier:b", \`\${trigger.runId}:frontier:b\`)),
      step.run("c", () => commit(trigger.runId, "frontier:c", \`\${trigger.runId}:frontier:c\`)),
    ]);
    const final = await step.run("final", () =>
      commit(trigger.runId, "frontier:final", \`\${trigger.runId}:frontier:final\`),
    );
    return { a, b, c, final };
  }
}

export class SingleCommitWorkflow {
  async run(trigger, step) {
    const once = await step.run("once", () =>
      commit(trigger.runId, "once", \`\${trigger.runId}:once\`),
    );
    return { once };
  }
}

export class SideEffectWorkflow {
  async run(trigger, step) {
    const v = await step.sideEffect("v", () => bump(trigger.runId, "v"));
    const after = await step.run("after", () => bump(trigger.runId, "after"));
    return { v, after };
  }
}

export class BareAwaitWorkflow {
  async run(trigger, step) {
    const pending = step.run("first", () => bump(trigger.runId, "first"));
    // Body-level I/O must fail before this workflow can observe the pending step.
    //
    // THIS ONE STAYS A \`fetch\`, and the URL is deliberately unreachable. The
    // guard under test is \`installWorkflowIoGuards\`, which replaces
    // \`globalThis.fetch\` and throws NondeterministicError SYNCHRONOUSLY when
    // the dispatch is in body mode (sdks/workflows/src/journal.ts:250-254,
    // :263-273) - before the real fetch, and therefore before the SSRF floor.
    // It guards \`fetch\` / \`setTimeout\` / \`setInterval\` and nothing else, so
    // moving this call to env.db like \`bump\` would stop exercising it. Nothing
    // ever connects, so the host does not need to resolve or listen.
    await fetch("http://127.0.0.1:1/never-connects", { method: "POST" });
    await pending;
    return { unreachable: true };
  }
}

export class NameDivergenceWorkflow {
  async run(trigger, step) {
    return await step.run("actual", () => bump(trigger.runId, "actual"));
  }
}

export class CompensationWorkflow {
  async run(trigger, step) {
    const a = await step.run(
      "a",
      { compensate: (output, ctx) => commit(trigger.runId, \`undo:\${output.step}\`, ctx.idempotencyKey) },
      () => bump(trigger.runId, "a"),
    );
    const b = await step.run(
      "b",
      { compensate: (output, ctx) => commit(trigger.runId, \`undo:\${output.step}\`, ctx.idempotencyKey) },
      () => bump(trigger.runId, "b"),
    );
    if (trigger.input.case === "cancel-compensate") {
      await step.sleep("rollback-wait", "PT30S");
      return { unreachable: true, a, b };
    }
    await step.run("c", () => {
      throw new Error("c failed");
    });
    return { unreachable: true };
  }
}

export class CompensationNameDivergenceWorkflow {
  async run(trigger, step) {
    await step.run(
      "actual",
      { compensate: (output, ctx) => commit(trigger.runId, \`undo:\${output.step ?? "actual"}\`, ctx.idempotencyKey) },
      () => bump(trigger.runId, "actual"),
    );
    return { ok: true };
  }
}

export class ScheduledWorkflow {
  async run(trigger) {
    return {
      input: trigger.input,
      runId: trigger.runId,
      workflowName: trigger.workflowName,
      startedAt: trigger.startedAt.toISOString(),
    };
  }
}

export class BenchWorkflow {
  async run(trigger, step) {
    const checkpoint = await step.run("checkpoint", () => ({
      runId: trigger.runId,
      marker: trigger.input.marker,
    }));
    return { checkpoint };
  }
}

export class BlobOutputWorkflow {
  async run(trigger, step) {
    const payload = "x".repeat(trigger.input.size);
    const digest = payload.length + ":" + payload.charCodeAt(0);
    const output = await step.run("big", { output: "blob" }, () => ({ payload, digest }));
    const value = output && typeof output.json === "function" ? await output.json() : output;
    return { digest: value.digest, len: value.payload.length };
  }
}

export class StreamLimitWorkflow {
  async run(trigger, step) {
    await step.run("too-big", { output: "stream" }, () => "s".repeat(trigger.input.size));
    return { unreachable: true };
  }
}

export class ChildEchoWorkflow {
  async run(trigger, step) {
    const seen = await step.run("child-seen", () => ({
      value: trigger.input.value,
      runId: trigger.runId,
    }));
    return seen;
  }
}

export class ChildFailWorkflow {
  async run() {
    throw new Error("child failed as requested");
  }
}

export class ChildBlockWorkflow {
  async run(trigger, step) {
    await step.sleep("child-block", trigger.input.sleep ?? "PT30S");
    return { unblocked: true };
  }
}

export class ChildTrackedBlockWorkflow {
  async run(trigger, step) {
    const started = await step.run("child-start", () => bump(trigger.runId, "child-start"));
    await step.sleep("child-block", trigger.input.sleep ?? "PT30S");
    return { started, unblocked: true };
  }
}

export class ParentCallWorkflow {
  async run(trigger, step) {
    const child = await step.call(
      ChildEchoWorkflow,
      { value: trigger.input.value },
      { cascade: trigger.input.cascade === true },
    );
    const after = await step.run("after-child", () => ({
      value: child.value,
      childRunId: child.runId,
    }));
    return { child, after };
  }
}

export class ParentStartManyWorkflow {
  async run(trigger, step) {
    const outputs = await step.startMany(
      ChildEchoWorkflow,
      trigger.input.values.map((value) => ({
        input: { value },
        key: \`child-\${value}\`,
      })),
      { cascade: true },
    );
    return { outputs };
  }
}

export class ParentCatchChildFailureWorkflow {
  async run(trigger, step) {
    try {
      await step.call(ChildFailWorkflow, { value: trigger.input.value });
      return { caught: false };
    } catch (error) {
      const marker = await step.run("caught-child-failure", () => ({
        name: error?.name,
        message: error?.message,
      }));
      return { caught: true, marker };
    }
  }
}

export class ParentCascadeWorkflow {
  async run(trigger, step) {
    await step.call(
      ChildBlockWorkflow,
      { sleep: trigger.input.sleep ?? "PT30S" },
      { cascade: true },
    );
    return { unreachable: true };
  }
}

export class ParentManyCascadeWorkflow {
  async run(trigger, step) {
    await step.startMany(
      ChildTrackedBlockWorkflow,
      trigger.input.values.map((value) => ({
        input: { value, sleep: trigger.input.sleep ?? "PT30S" },
        key: \`tracked-\${value}\`,
      })),
      { cascade: true },
    );
    return { unreachable: true };
  }
}

export class ContinueAsNewWorkflow {
  async run(trigger, step) {
    if (!trigger.input.generation) {
      const before = await step.run("before-can", () => bump(trigger.runId, "before-can"));
      await step.continueAsNew({
        generation: 1,
        case: trigger.input.case,
        previousRunId: trigger.runId,
        marker: before,
      });
    }
    const after = await step.run("after-can", () => bump(trigger.runId, "after-can"));
    return {
      generation: trigger.input.generation,
      case: trigger.input.case,
      previousRunId: trigger.input.previousRunId,
      marker: trigger.input.marker,
      after,
    };
  }
}

export class CompensableCarryWorkflow {
  async run(trigger, step) {
    const done = await step.run(
      "compensable",
      {
        compensate: async (output) => {
          await commit(trigger.runId, "undo:compensable", output.step);
        },
      },
      () => bump(trigger.runId, "compensable"),
    );
    await step.continueAsNew({
      generation: 1,
      case: trigger.input.case,
      done,
    });
  }
}

export default {
  async fetch() {
    return new Response("dw07-ok");
  },
  workflows: {
    KeystoneWorkflow,
    SignalWorkflow,
    TopicSignalWorkflow,
    ConcurrentWorkflow,
    ConcurrentCommitWorkflow,
    SingleCommitWorkflow,
    SideEffectWorkflow,
    BareAwaitWorkflow,
    NameDivergenceWorkflow,
    CompensationWorkflow,
    CompensationNameDivergenceWorkflow,
    ScheduledWorkflow,
    BenchWorkflow,
    BlobOutputWorkflow,
    StreamLimitWorkflow,
    ChildEchoWorkflow,
    ChildFailWorkflow,
    ChildBlockWorkflow,
    ChildTrackedBlockWorkflow,
    ParentCallWorkflow,
    ParentStartManyWorkflow,
    ParentCatchChildFailureWorkflow,
    ParentCascadeWorkflow,
    ParentManyCascadeWorkflow,
    ContinueAsNewWorkflow,
    CompensableCarryWorkflow,
  },
};
EOF
build_zship "$WORK/workflow.js" "$WORK/workflow.zship"
pass "built real workflow .zship"

echo "=== DW-07 services ==="
for port in "$ZEROSHIP_CONTROL_PORT" "$ZEROSHIP_WORKER_PORT" "$ZEROSHIP_GATEWAY_PORT"; do
  lsof -ti :"$port" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gateway-broker-secret"
printf '%s' "dw07-gateway-broker-secret-32-bytes-minimum-ok" > "$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE"
chmod 0600 "$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE"

e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
"$BIN/zeroship-control" \
  --port "$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/blobs" \
  --gateway-url "http://localhost:$ZEROSHIP_GATEWAY_PORT" \
  --disable-workflow-engine > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health control "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" "$WORK/control.log"

# NO `ZEROSHIP_DEV=1`, AND THERE IS NO REPLACEMENT FOR IT. This line carried one
# until 2026-08-27, purely so the workflow's step bodies could `fetch` a harness
# HTTP server on 127.0.0.1: the SSRF gate read `ZEROSHIP_DEV` out of the process
# environment and, when it was `1`, skipped host and IP validation entirely - for
# every `fetch` the process made, in a worker as much as in `zeroship serve`.
# That was a production hole, and closing it made this variable INERT here
# (crates/zeroship-runtime/src/transport/ssrf.rs:45-72, :103-121). The step
# bodies now record their side effects through `env.db` instead; see the comment
# above `bump` in the fixture. Do not add a worker flag, feature or variable to
# re-open loopback egress - `docs/reference/env-vars.md:124-128` records that
# `--dev-insecure` / `ZEROSHIP_DEV_INSECURE` were deliberately deleted, and a
# flag a deployment file can set is a default, not an escape hatch.
"$BIN/zeroship-worker" \
  --port "$ZEROSHIP_WORKER_PORT" \
  --threads 1 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/blobs" \
  --poll-interval 1 \
  --max-step-blob-bytes 2097152 \
  --workflow-advance-unsigned > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health worker "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" "$WORK/worker.log"

"$BIN/zeroship-gate" \
  --port "$ZEROSHIP_GATEWAY_PORT" \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" \
  --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" \
  --broker-secret-file "$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE" \
  --poll-interval 1 \
 > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health gateway "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" "$WORK/gate.log"

echo "=== DW-07 deploy ==="
"$BIN/dev-provision" \
  --db "$DBURL" \
  --blob-store "$WORK/blobs" \
  --name "$APP_NAME" \
  --zship "$WORK/workflow.zship" > "$WORK/provision.out"
APP_ID="$(awk -F= '/^app_id=/{print $2}' "$WORK/provision.out")"
[ -n "$APP_ID" ] || { fail "dev-provision did not return app_id"; cat "$WORK/provision.out"; exit 1; }

docker exec -i "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 >/dev/null <<SQL
UPDATE zeroship.plans
   SET workflows_allowed = true, updated_at = now()
 WHERE id = (SELECT plan_id FROM zeroship.apps WHERE id = '$APP_ID');
UPDATE zeroship.apps
   SET workflows_enabled = true, updated_at = now()
 WHERE id = '$APP_ID';
INSERT INTO zeroship.workflow_rollout_config (id, dispatch_paused, ingress_disabled, updated_by)
VALUES ('global', false, false, 'dw-e2e')
ON CONFLICT (id) DO UPDATE SET
  dispatch_paused = false,
  ingress_disabled = false,
  updated_at = now(),
  updated_by = EXCLUDED.updated_by;
SQL
# NO `zeroship.app_egress_rules` ROW HERE ANY MORE, and deleting it lost no
# coverage, because it never granted anything. That table is the `node:net` /
# `node:tls` / outbound-WebSocket rule set; `fetch` is explicitly NOT gated by it
# (crates/zeroship-core/src/net_policy.rs:25-28), so it never touched the step
# bodies' calls. And no rule in it can widen the platform SSRF floor either -
# INVARIANT GRANTS-NARROW, net_policy.rs:14-16 - so it could not have rescued the
# loopback fetch after the floor tightened. The one thing that ever made those
# calls connect was `ZEROSHIP_DEV=1` on the worker, which this harness no longer
# sets and which no longer means anything to a worker.
DEPLOY_ID="$(docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -At -v ON_ERROR_STOP=1 \
  -c "SELECT id FROM zeroship.app_deploys WHERE app_id = '$APP_ID' ORDER BY activated_at DESC, created_at DESC, id DESC LIMIT 1;")"
[ -n "$DEPLOY_ID" ] || { fail "deploy registration did not create app_deploys row"; exit 1; }
pass "deployed app $APP_ID and pinned deploy $DEPLOY_ID"

sleep 3
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/" >/dev/null || {
  fail "warmup request failed"
  tail -80 "$WORK/worker.log" || true
  tail -80 "$WORK/gate.log" || true
  exit 1
}
pass "gateway/worker warmed real deployed app"

if [ "$BENCH_ONLY" = "1" ]; then
  echo "=== DW-23 workflow engine load bench ==="
  ZEROSHIP_DW_E2E=1 \
  PG_TEST_URL="$DBURL" \
  ZEROSHIP_DW_E2E_CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT" \
  ZEROSHIP_DW_E2E_GATEWAY_URL="http://localhost:$ZEROSHIP_GATEWAY_PORT" \
  ZEROSHIP_DW_E2E_APP_ID="$APP_ID" \
  ZEROSHIP_DW_E2E_DEPLOY_ID="$DEPLOY_ID" \
  ZEROSHIP_DW_E2E_BLOB_ROOT="$WORK/blobs" \
    cargo test -p zeroship-control --test main durable_workflows_keystone_e2e::dw23_workflow_engine_load_bench -- --ignored --nocapture --test-threads=1 || {
      fail "DW-23 workflow engine load bench failed"
      echo "--- control.log ---"
      tail -120 "$WORK/control.log" || true
      echo "--- worker.log ---"
      tail -160 "$WORK/worker.log" || true
      echo "--- gate.log ---"
      tail -120 "$WORK/gate.log" || true
      exit 1
    }
  pass "DW-23 workflow engine load bench passed"
  exit 0
fi

echo "=== DW-07 keystone assertions ==="
ZEROSHIP_DW_E2E=1 \
PG_TEST_URL="$DBURL" \
ZEROSHIP_DW_E2E_CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT" \
ZEROSHIP_DW_E2E_GATEWAY_URL="http://localhost:$ZEROSHIP_GATEWAY_PORT" \
ZEROSHIP_DW_E2E_APP_ID="$APP_ID" \
ZEROSHIP_DW_E2E_DEPLOY_ID="$DEPLOY_ID" \
ZEROSHIP_DW_E2E_BLOB_ROOT="$WORK/blobs" \
  cargo test -p zeroship-control --test main durable_workflows_keystone_e2e::durable_workflows_m1_keystone_real_spine -- --nocapture --test-threads=1 || {
    fail "DW-07 keystone assertions failed"
    echo "--- control.log ---"
    tail -120 "$WORK/control.log" || true
    echo "--- worker.log ---"
    tail -160 "$WORK/worker.log" || true
    echo "--- gate.log ---"
    tail -120 "$WORK/gate.log" || true
    exit 1
  }
ZEROSHIP_DW_E2E=1 \
PG_TEST_URL="$DBURL" \
ZEROSHIP_DW_E2E_CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT" \
ZEROSHIP_DW_E2E_GATEWAY_URL="http://localhost:$ZEROSHIP_GATEWAY_PORT" \
ZEROSHIP_DW_E2E_APP_ID="$APP_ID" \
ZEROSHIP_DW_E2E_DEPLOY_ID="$DEPLOY_ID" \
ZEROSHIP_DW_E2E_BLOB_ROOT="$WORK/blobs" \
  cargo test -p zeroship-control --test main durable_workflows_keystone_e2e:: -- --nocapture --test-threads=1 --skip durable_workflows_m1_keystone_real_spine || {
    fail "DW-07 keystone assertions failed"
    echo "--- control.log ---"
    tail -120 "$WORK/control.log" || true
    echo "--- worker.log ---"
    tail -160 "$WORK/worker.log" || true
    echo "--- gate.log ---"
    tail -120 "$WORK/gate.log" || true
    exit 1
  }
pass "DW-07 keystone e2e passed"

echo "=== DW-07 stop services before regression ==="
stop_services
terminate_pg_db_connections
pass "stopped real services; workflow engine regression runs alone"

echo "=== DW-07 workflow engine regression ==="
PG_TEST_URL="$DBURL" \
  cargo test -p zeroship-control --features live-db-tests --test workflow_engine_test -- --nocapture --test-threads=1 || {
    fail "DW-07 workflow engine regression failed"
    echo "--- control.log ---"
    tail -120 "$WORK/control.log" || true
    echo "--- worker.log ---"
    tail -160 "$WORK/worker.log" || true
    echo "--- gate.log ---"
    tail -120 "$WORK/gate.log" || true
    exit 1
  }
pass "DW-07 workflow engine regression passed"
