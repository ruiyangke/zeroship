#!/usr/bin/env bash
# Prove that the sanctioned platform applier records and durably applies the
# complete committed migration corpus through the single @zeroship/migrate DSL.
#
# The important failure mode is a successful empty drain: an imported migration
# can record into one ambient singleton while the CLI drains another. Counting
# `apply ...` lines cannot distinguish that from health. This gate therefore:
#
#   1. lints through the CLI and reconciles every report's migration name and
#      opCount against a committed per-file ledger;
#   2. applies to fresh PostgreSQL and requires every file to return at least one
#      newly applied journal identity;
#   3. rereads history and status and reconciles those durable journal identities
#      with the first apply;
#   4. applies again and requires exact per-file skips plus unchanged history.
#
# Usage:
#   tests/platform_migration_corpus_gate.sh
#   tests/platform_migration_corpus_gate.sh --dsn <url>
#   tests/platform_migration_corpus_gate.sh --static-only
#   tests/platform_migration_corpus_gate.sh --with-container
#
# `--static-only` runs the import, resolution, container-layout and recorder
# proofs, but deliberately exits non-zero because it cannot prove application.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

CORPUS_DIR="$ROOT/db/migrations-ts"
OP_LEDGER="$CORPUS_DIR/op-counts.json"
CLI="$ROOT/packages/zero-migrate-cli/dist/cli-bin.js"
DSL_INDEX="$ROOT/packages/zero-migrate/dist/index.js"
DSL_RECORDER="$ROOT/packages/zero-migrate/dist/internal/recorder.js"
DSL_SPECIFIER="@zeroship/migrate"
# An exact import of this former package name would recreate the split-recorder
# seam. It is a refusal, not an alias.
OLD_DSL_SPECIFIER="zero-migrate"

DSN=""
STATIC_ONLY=0
WITH_CONTAINER=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --dsn) DSN="${2:-}"; shift 2 ;;
    --static-only) STATIC_ONLY=1; shift ;;
    --with-container) WITH_CONTAINER=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

gate_arms_init platform_migration_corpus

OWN_CONTAINER=""
WORK_DIR="$(mktemp -d -t zeroship-migration-corpus.XXXXXX)"
cleanup() {
  if [ -n "$OWN_CONTAINER" ]; then
    docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  fi
  if [ -n "$WORK_DIR" ] && [ -d "$WORK_DIR" ]; then
    rm -rf -- "$WORK_DIR"
  fi
}
trap cleanup EXIT HUP INT TERM

echo "== platform migration corpus gate =="
echo "corpus: $CORPUS_DIR"

# Refuse an absent or stale build. Both the CLI and the DSL dist matter: a fresh
# CLI draining an old recorder is still a verdict about code nobody is reading.
assert_fresh_artifact() {
  local artifact="$1" label="$2"
  shift 2

  if [ ! -f "$artifact" ]; then
    echo "FAIL: $label is missing: $artifact" >&2
    echo "  run: pnpm build" >&2
    exit 2
  fi

  local source newer stale=""
  for source in "$@"; do
    if [ -d "$source" ]; then
      newer="$(find "$source" -type f -newer "$artifact" -print 2>/dev/null | head -3)"
      if [ -n "$newer" ]; then
        stale="${stale}${stale:+$'\n'}${newer}"
      fi
    elif [ -f "$source" ]; then
      if [ "$source" -nt "$artifact" ]; then
        stale="${stale}${stale:+$'\n'}${source}"
      fi
    else
      echo "FAIL: freshness input is missing for $label: $source" >&2
      exit 2
    fi
  done

  if [ -n "$stale" ]; then
    echo "FAIL: $label is stale: $artifact" >&2
    echo "  These build inputs are newer:" >&2
    printf '%s\n' "$stale" | sed 's|^|    |' >&2
    echo "  run: pnpm build" >&2
    exit 2
  fi
}

# Arm 1: every TypeScript corpus file imports the canonical DSL exactly once and
# none imports the former exact specifier.
spec_examined=0
spec_bad=0
for f in "$CORPUS_DIR"/*.ts; do
  [ -e "$f" ] || continue
  spec_examined=$((spec_examined + 1))
  canonical_count="$(grep -Ec "from[[:space:]]+['\"]${DSL_SPECIFIER}['\"]" "$f")"
  old_count="$(grep -Ec "from[[:space:]]+['\"]${OLD_DSL_SPECIFIER}['\"]" "$f")"
  if [ "$old_count" -ne 0 ]; then
    echo "FAIL[specifier]: $(basename "$f") imports removed package $OLD_DSL_SPECIFIER" >&2
    spec_bad=$((spec_bad + 1))
  fi
  if [ "$canonical_count" -ne 1 ]; then
    echo "FAIL[specifier]: $(basename "$f") has $canonical_count canonical DSL imports; expected 1" >&2
    spec_bad=$((spec_bad + 1))
  fi
done
gate_arm specifier "$spec_examined" 25

# Arm 2: load the canonical package using Node's ESM resolver from the corpus
# directory, which is where the CLI's migration import begins its upward walk.
#
# WHAT MAKES THAT WALK SUCCEED, recorded here because the link it finds is
# gitignored and so reads as debris to anyone who notices it. `pnpm install`
# creates a scoped package link in the corpus directory's own node_modules,
# because db/migrations-ts is a declared workspace member whose manifest depends
# on the DSL by `workspace:*`. It is a required development prerequisite, not
# residue: delete it and this arm fails on every corpus file with
# `Cannot find package`, since the walk then reaches the repo root, whose
# node_modules holds no @zeroship scope at all. That is written down in
# pnpm-workspace.yaml, whose member entry states both reasons (resolution and
# module kind) and names this gate as what rules on them; `pnpm install`
# restores it.
res_examined=0
res_bad=0
for f in "$CORPUS_DIR"/*.ts; do
  [ -e "$f" ] || continue
  res_examined=$((res_examined + 1))
  if ! (cd "$CORPUS_DIR" && node --input-type=module \
        -e 'await import(process.argv[1]);' "$DSL_SPECIFIER") >/dev/null 2>&1; then
    echo "FAIL[resolution]: $DSL_SPECIFIER does not load from $(basename "$f")" >&2
    res_bad=$((res_bad + 1))
  fi
done
gate_arm resolution "$res_examined" 25

# Arm 3: extract the image declarations and prove that the corpus remains below
# the node_modules root, with entrypoint and compose pointing at the copied path.
DOCKERFILE="$ROOT/deploy/Dockerfile"
ENTRYPOINT="$ROOT/deploy/ops/migrate-entrypoint.sh"
COMPOSE="$ROOT/deploy/compose/docker-compose.yml"

nm_dest="$(grep -oP '^COPY --from=sdks /build/node_modules \K\S+' "$DOCKERFILE" | tail -1)"
corpus_dest="$(grep -oP '^COPY --from=sdks /build/db/migrations-ts \K\S+' "$DOCKERFILE" | tail -1)"
entry_default="$(grep -oP '^MIGRATIONS_DIR="\K[^"]+' "$ENTRYPOINT" | head -1)"
compose_dir="$(grep -A1 -- '- --migrations-dir' "$COMPOSE" | grep -oP '^\s+- \K/\S+' | head -1)"

cp_examined=0
cp_bad=0
cp_examined=$((cp_examined + 1))
nm_root="${nm_dest%/node_modules}"
if [ -z "$nm_dest" ] || [ -z "$corpus_dest" ]; then
  echo "FAIL[container_paths]: could not extract COPY destinations from $DOCKERFILE" >&2
  cp_bad=$((cp_bad + 1))
elif [ "${corpus_dest#"$nm_root"/}" = "$corpus_dest" ]; then
  echo "FAIL[container_paths]: corpus '$corpus_dest' is not below '$nm_root'" >&2
  cp_bad=$((cp_bad + 1))
fi

cp_examined=$((cp_examined + 1))
if [ "$entry_default" != "$corpus_dest" ]; then
  echo "FAIL[container_paths]: entrypoint '$entry_default' != image '$corpus_dest'" >&2
  cp_bad=$((cp_bad + 1))
fi

cp_examined=$((cp_examined + 1))
if [ "$compose_dir" != "$corpus_dest" ]; then
  echo "FAIL[container_paths]: compose '$compose_dir' != image '$corpus_dest'" >&2
  cp_bad=$((cp_bad + 1))
fi
gate_arm container_paths "$cp_examined" 3

assert_fresh_artifact "$CLI" "sanctioned migration CLI" \
  "$ROOT/packages/zero-migrate-cli/src" \
  "$ROOT/packages/zero-migrate-cli/package.json" \
  "$ROOT/packages/zero-migrate-cli/tsup.config.ts"
assert_fresh_artifact "$DSL_INDEX" "migration DSL public bundle" \
  "$ROOT/packages/zero-migrate/src" \
  "$ROOT/packages/zero-migrate/package.json" \
  "$ROOT/packages/zero-migrate/tsup.config.ts"
assert_fresh_artifact "$DSL_RECORDER" "migration DSL recorder bundle" \
  "$ROOT/packages/zero-migrate/src" \
  "$ROOT/packages/zero-migrate/package.json" \
  "$ROOT/packages/zero-migrate/tsup.config.ts"

if [ ! -f "$OP_LEDGER" ]; then
  echo "FAIL[recorded_ops]: operation ledger is missing: $OP_LEDGER" >&2
  exit 2
fi

# Arm 4: ask the same CLI that applies the corpus to record it without a DB, then
# reconcile every sorted file, authored name and operation count with the ledger.
# Generic lint deliberately refuses platform-only capabilities such as roles,
# grants and raw SQL before the platform charter is composed, so its validation
# verdict is not this arm's verdict. Its complete JSON report is the recorder
# instrument; the live apply below is the policy-aware execution verdict.
lint_json="$WORK_DIR/lint.json"
lint_err="$WORK_DIR/lint.err"
ZEROSHIP_MIGRATE_VERB=lint \
  bash "$ROOT/deploy/ops/db-migrate.sh" --dialect postgres --json \
  >"$lint_json" 2>"$lint_err"
lint_status=$?
record_status=1
recorded_files=0
recorded_ops=0
recorded_refusals=0
record_summary="$(node --input-type=module - \
  "$lint_json" "$OP_LEDGER" "$CORPUS_DIR" "$lint_status" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

const [lintPath, ledgerPath, corpusDir, lintStatusRaw] = process.argv.slice(2);
const lintStatus = Number(lintStatusRaw);
const reports = JSON.parse(readFileSync(lintPath, "utf8"));
const ledger = JSON.parse(readFileSync(ledgerPath, "utf8"));

assert.ok(Array.isArray(ledger) && ledger.length > 0, "operation ledger must be nonempty");
assert.equal(new Set(ledger.map((entry) => entry.file)).size, ledger.length, "duplicate ledger file");
assert.equal(new Set(ledger.map((entry) => entry.name)).size, ledger.length, "duplicate ledger name");

const files = readdirSync(corpusDir)
  .filter((name) => name.endsWith(".ts") && !name.endsWith(".d.ts"))
  .sort();
assert.deepEqual(files, ledger.map((entry) => entry.file), "ledger files do not equal corpus files");
assert.ok(Array.isArray(reports), "lint --json did not return a report array");
assert.equal(reports.length, ledger.length, "lint report count does not equal ledger count");

let opTotal = 0;
for (let i = 0; i < ledger.length; i += 1) {
  const expected = ledger[i];
  const report = reports[i];
  assert.ok(Number.isInteger(expected.opCount) && expected.opCount > 0,
    `ledger ${expected.file} has a non-positive opCount`);
  assert.equal(report.label, expected.name, `${expected.file}: authored name drift`);
  assert.equal(report.opCount, expected.opCount, `${expected.file}: recorded opCount drift`);
  assert.ok(Array.isArray(report.dialects) && report.dialects.length === 1,
    `${expected.file}: expected exactly one dialect result`);
  assert.equal(report.dialects[0].dialect, "postgres", `${expected.file}: wrong lint dialect`);
  if (report.dialects[0].opCount !== undefined) {
    assert.equal(report.dialects[0].opCount, expected.opCount,
      `${expected.file}: verifier opCount drift`);
  }
  opTotal += expected.opCount;
}
assert.ok(opTotal > 0, "corpus recorded zero operations");
const refusalCount = reports.filter((report) => !report.ok).length;
assert.equal(lintStatus, refusalCount === 0 ? 0 : 1,
  "lint exit status does not agree with its complete JSON report");
process.stdout.write(`${ledger.length}\t${opTotal}\t${refusalCount}`);
NODE
)"
record_status=$?
if [ "$record_status" -eq 0 ]; then
  IFS=$'\t' read -r recorded_files recorded_ops recorded_refusals <<<"$record_summary"
else
  echo "FAIL[recorded_ops]: could not reconcile lint's JSON report (exit $lint_status)" >&2
  tail -40 "$lint_err" >&2
fi
gate_arm recorded_ops "$recorded_files" 25

if [ "$STATIC_ONLY" -eq 1 ]; then
  echo >&2
  echo "--static-only: recorder proof saw $recorded_files files and $recorded_ops ops." >&2
  echo "  Apply, durable history/status, and idempotence did NOT run; this is not a pass." >&2
  gate_arms_finish || true
  exit 1
fi

if [ -z "$DSN" ]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "FAIL: no --dsn given and docker is not on PATH" >&2
    exit 2
  fi
  port=""
  for p in $(seq 5560 5599); do
    if ! ss -ltn 2>/dev/null | grep -q ":$p "; then
      port="$p"
      break
    fi
  done
  if [ -z "$port" ]; then
    echo "FAIL: no free TCP port in 5560-5599 for PostgreSQL" >&2
    exit 2
  fi
  OWN_CONTAINER="zs-migcorpus-gate-$port"
  docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  echo "starting fresh PostgreSQL on 127.0.0.1:$port ($OWN_CONTAINER)"
  if ! docker run -d --name "$OWN_CONTAINER" \
      -p "127.0.0.1:$port:5432" \
      -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
      postgres:17 >/dev/null 2>&1; then
    echo "FAIL: could not start PostgreSQL container" >&2
    exit 2
  fi
  if ! docker exec "$OWN_CONTAINER" sh -c \
      'for i in $(seq 1 60); do pg_isready -U postgres -q && exit 0; sleep 1; done; exit 1'; then
    echo "FAIL: PostgreSQL did not become ready" >&2
    exit 2
  fi
  DSN="postgres://postgres:zeroship@localhost:$port/zeroship"
fi

run1="$WORK_DIR/apply-first.out"
run1_err="$WORK_DIR/apply-first.err"
history1="$WORK_DIR/history-first.json"
history1_err="$WORK_DIR/history-first.err"
run2="$WORK_DIR/apply-second.out"
run2_err="$WORK_DIR/apply-second.err"
history2="$WORK_DIR/history-second.json"
history2_err="$WORK_DIR/history-second.err"
status_json="$WORK_DIR/status.json"
status_err="$WORK_DIR/status.err"

echo "applying committed corpus with deploy/ops/db-migrate.sh"
ZEROSHIP_MIGRATE_DSN="$DSN" bash "$ROOT/deploy/ops/db-migrate.sh" \
  >"$run1" 2>"$run1_err"
run1_status=$?

ZEROSHIP_MIGRATE_DSN="$DSN" ZEROSHIP_MIGRATE_VERB=history \
  bash "$ROOT/deploy/ops/db-migrate.sh" --json >"$history1" 2>"$history1_err"
history1_status=$?

ZEROSHIP_MIGRATE_DSN="$DSN" ZEROSHIP_MIGRATE_VERB=status \
  bash "$ROOT/deploy/ops/db-migrate.sh" --json --strict >"$status_json" 2>"$status_err"
status_cmd_status=$?

echo "re-running to prove exact no-op idempotence"
ZEROSHIP_MIGRATE_DSN="$DSN" bash "$ROOT/deploy/ops/db-migrate.sh" \
  >"$run2" 2>"$run2_err"
run2_status=$?

ZEROSHIP_MIGRATE_DSN="$DSN" ZEROSHIP_MIGRATE_VERB=history \
  bash "$ROOT/deploy/ops/db-migrate.sh" --json >"$history2" 2>"$history2_err"
history2_status=$?

for result in \
  "first apply:$run1_status:$run1:$run1_err" \
  "first history:$history1_status:$history1:$history1_err" \
  "strict status:$status_cmd_status:$status_json:$status_err" \
  "second apply:$run2_status:$run2:$run2_err" \
  "second history:$history2_status:$history2:$history2_err"; do
  IFS=: read -r label code stdout_path stderr_path <<<"$result"
  if [ "$code" -ne 0 ]; then
    echo "FAIL[live]: $label exited $code" >&2
    tail -30 "$stderr_path" >&2
    tail -30 "$stdout_path" >&2
  fi
done

reconcile_status=1
applied_files=0
applied_versions=0
history_events=0
status_plans=0
status_steps=0
second_seen=0
if [ "$run1_status" -eq 0 ] && [ "$history1_status" -eq 0 ] && \
   [ "$run2_status" -eq 0 ] && [ "$history2_status" -eq 0 ] && \
   [ "$status_cmd_status" -eq 0 ]; then
  live_summary="$(node --input-type=module - \
    "$OP_LEDGER" "$run1" "$history1" "$run2" "$history2" "$status_json" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const [ledgerPath, firstPath, history1Path, secondPath, history2Path, statusPath] =
  process.argv.slice(2);
const ledger = JSON.parse(readFileSync(ledgerPath, "utf8"));

function parseApply(path) {
  const records = [];
  for (const line of readFileSync(path, "utf8").split(/\r?\n/)) {
    if (!line.startsWith("apply ")) continue;
    const match = /^apply ([^:]+): (\{.*\})$/.exec(line);
    assert.ok(match, `${path}: malformed apply result: ${line}`);
    records.push({ label: match[1], outcome: JSON.parse(match[2]) });
  }
  return records;
}

function requireOutcomeArrays(record, phase) {
  for (const key of ["applied", "skipped", "recovered", "pendingContracts"]) {
    assert.ok(Array.isArray(record.outcome[key]), `${phase} ${record.label}: ${key} is not an array`);
  }
}

function assertUnique(values, label) {
  assert.equal(new Set(values).size, values.length, `${label} contains duplicate identities`);
}

function assertSameSet(actual, expected, label) {
  assert.equal(actual.length, expected.length, `${label}: cardinality differs`);
  assert.deepEqual([...actual].sort(), [...expected].sort(), `${label}: identities differ`);
}

const expectedLabels = ledger.map((entry) => entry.file.replace(/\.[^.]+$/, ""));
const first = parseApply(firstPath);
assert.deepEqual(first.map((record) => record.label), expectedLabels,
  "first apply labels do not exactly equal corpus files");

const firstIds = [];
for (const record of first) {
  requireOutcomeArrays(record, "first apply");
  assert.ok(record.outcome.applied.length > 0,
    `first apply ${record.label}: recorder drained empty`);
  assert.deepEqual(record.outcome.skipped, [], `first apply ${record.label}: not a fresh apply`);
  assert.deepEqual(record.outcome.recovered, [], `first apply ${record.label}: recovered work`);
  assert.deepEqual(record.outcome.pendingContracts, [],
    `first apply ${record.label}: left pending contracts`);
  firstIds.push(...record.outcome.applied);
}
assert.ok(firstIds.length > 0, "first apply produced zero journal identities");
assertUnique(firstIds, "first apply");

const h1 = JSON.parse(readFileSync(history1Path, "utf8"));
assert.ok(Array.isArray(h1.events) && h1.events.length > 0, "history after apply is empty");
assert.ok(h1.events.every((event) => event.kind === "applied"),
  "fresh history contains a non-applied event");
const historyIds = h1.events.map((event) => event.version);
assertUnique(historyIds, "first history");
assertSameSet(historyIds, firstIds, "history versus first apply");

const second = parseApply(secondPath);
assert.deepEqual(second.map((record) => record.label), expectedLabels,
  "second apply labels do not exactly equal corpus files");
for (let i = 0; i < second.length; i += 1) {
  const record = second[i];
  requireOutcomeArrays(record, "second apply");
  assert.deepEqual(record.outcome.applied, [], `second apply ${record.label}: applied again`);
  assert.deepEqual(record.outcome.recovered, [], `second apply ${record.label}: recovered work`);
  assert.deepEqual(record.outcome.pendingContracts, [],
    `second apply ${record.label}: left pending contracts`);
  assertUnique(record.outcome.skipped, `second apply ${record.label} skips`);
  assertSameSet(record.outcome.skipped, first[i].outcome.applied,
    `second apply ${record.label}: skips versus first applied identities`);
}

const h2 = JSON.parse(readFileSync(history2Path, "utf8"));
assert.deepEqual(h2, h1, "history changed during the second no-op apply");

const status = JSON.parse(readFileSync(statusPath, "utf8"));
assert.equal(status.busy, false, "status was busy and reconciled no state");
assert.deepEqual(status.lockHolders, [], "clean status has lock holders");
for (const key of [
  "pending", "aborted", "rolledBack", "pendingContracts", "blocked",
  "unexpectedJournal", "interruptedUnwinds",
]) {
  assert.deepEqual(status[key], [], `status ${key} is not empty`);
}
assert.ok(Array.isArray(status.plans), "status omitted plan-aware detail");
assert.equal(status.plans.length, ledger.length, "status plan count differs from corpus");
assert.deepEqual(status.plans.map((plan) => plan.name), ledger.map((entry) => entry.name),
  "status plan names differ from authored ledger names");
assert.deepEqual(status.applied, status.plans.map((plan) => plan.version),
  "status logical applied ids differ from its plans");
assert.equal(status.currentVersion, status.applied.at(-1), "status currentVersion is not final plan");

const statusStepIds = [];
for (const plan of status.plans) {
  assert.equal(plan.state, "applied", `status plan ${plan.name} is ${plan.state}`);
  assert.deepEqual(plan.missingDependencies, [], `status plan ${plan.name} misses dependencies`);
  assert.ok(Array.isArray(plan.steps) && plan.steps.length > 0,
    `status plan ${plan.name} has no journal-visible steps`);
  assert.ok(plan.steps.every((step) => step.state === "applied"),
    `status plan ${plan.name} has a non-applied step`);
  statusStepIds.push(...plan.steps.map((step) => step.version));
}
assertUnique(statusStepIds, "status steps");
assertSameSet(statusStepIds, firstIds, "status steps versus first apply");

process.stdout.write([
  first.length,
  firstIds.length,
  h1.events.length,
  status.plans.length,
  statusStepIds.length,
  second.length,
].join("\t"));
NODE
)"
  reconcile_status=$?
  if [ "$reconcile_status" -eq 0 ]; then
    IFS=$'\t' read -r applied_files applied_versions history_events \
      status_plans status_steps second_seen <<<"$live_summary"
  fi
fi

# Each arm reports the items it actually reconciled, with floors deliberately
# below today's corpus size so normal deletion is not a census update.
gate_arm apply_outcomes "$applied_files" 25
gate_arm journal_events "$history_events" 25
gate_arm status_plans "$status_plans" 25
gate_arm idempotence_outcomes "$second_seen" 25

# Optional image proof. It uses a second empty database and subjects every image
# result to the same exact-label/nonempty-outcome parser instead of line counts.
container_status=0
if [ "$WITH_CONTAINER" -eq 1 ]; then
  image_build_log="$WORK_DIR/image-build.log"
  image_stdout="$WORK_DIR/image-apply.out"
  image_stderr="$WORK_DIR/image-apply.err"
  img_files=0
  img_versions=0

  echo "building and exercising the migrate image"
  if ! docker build -f "$ROOT/deploy/Dockerfile" --target migrate \
      -t zeroship-migrate:gate "$ROOT" >"$image_build_log" 2>&1; then
    echo "FAIL[image]: migrate image build failed" >&2
    tail -80 "$image_build_log" >&2
    container_status=1
  else
    admin_dsn="${DSN%/*}/postgres"
    docker run --rm --network host postgres:17 \
      psql "$admin_dsn" -v ON_ERROR_STOP=1 \
      -c "DROP DATABASE IF EXISTS gate_img" >/dev/null 2>&1
    if ! docker run --rm --network host postgres:17 \
        psql "$admin_dsn" -v ON_ERROR_STOP=1 \
        -c "CREATE DATABASE gate_img" >/dev/null 2>&1; then
      echo "FAIL[image]: could not create gate_img via the supplied server" >&2
      container_status=1
    else
      secret="$WORK_DIR/image-secret"
      mkdir "$secret"
      chmod 700 "$secret"
      printf '%s' "${DSN%/*}/gate_img" >"$secret/dsn"
      chmod 600 "$secret/dsn"

      docker run --rm --network host \
        -v "$secret/dsn:/tmp/dsn:ro" \
        zeroship-migrate:gate --database-url-file /tmp/dsn \
        >"$image_stdout" 2>"$image_stderr"
      img_status=$?
      if [ "$img_status" -ne 0 ]; then
        echo "FAIL[image]: in-image apply exited $img_status" >&2
        tail -30 "$image_stderr" >&2
        tail -30 "$image_stdout" >&2
        container_status=1
      else
        image_summary="$(node --input-type=module - "$OP_LEDGER" "$image_stdout" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const [ledgerPath, outputPath] = process.argv.slice(2);
const ledger = JSON.parse(readFileSync(ledgerPath, "utf8"));
const records = [];
for (const line of readFileSync(outputPath, "utf8").split(/\r?\n/)) {
  if (!line.startsWith("apply ")) continue;
  const match = /^apply ([^:]+): (\{.*\})$/.exec(line);
  assert.ok(match, `malformed image apply result: ${line}`);
  records.push({ label: match[1], outcome: JSON.parse(match[2]) });
}
const labels = ledger.map((entry) => entry.file.replace(/\.[^.]+$/, ""));
assert.deepEqual(records.map((record) => record.label), labels,
  "image apply labels do not exactly equal corpus files");
const ids = [];
for (const record of records) {
  for (const key of ["applied", "skipped", "recovered", "pendingContracts"]) {
    assert.ok(Array.isArray(record.outcome[key]), `image ${record.label}: ${key} is not an array`);
  }
  assert.ok(record.outcome.applied.length > 0, `image ${record.label}: recorder drained empty`);
  assert.deepEqual(record.outcome.skipped, [], `image ${record.label}: database was not empty`);
  assert.deepEqual(record.outcome.recovered, [], `image ${record.label}: recovered work`);
  assert.deepEqual(record.outcome.pendingContracts, [],
    `image ${record.label}: left pending contracts`);
  ids.push(...record.outcome.applied);
}
assert.ok(ids.length > 0, "image apply produced zero journal identities");
assert.equal(new Set(ids).size, ids.length, "image apply returned duplicate identities");
process.stdout.write(`${records.length}\t${ids.length}`);
NODE
)"
        image_parse_status=$?
        if [ "$image_parse_status" -eq 0 ]; then
          IFS=$'\t' read -r img_files img_versions <<<"$image_summary"
        else
          container_status=1
        fi
      fi
    fi
  fi
  gate_arm image_apply_outcomes "$img_files" 25
fi

echo
echo "recorded corpus:          $recorded_files files, $recorded_ops operations"
echo "generic lint refusals:    $recorded_refusals envelopes"
echo "first apply:              $applied_files files, $applied_versions journal identities"
echo "durable history:          $history_events applied events"
echo "clean status:             $status_plans plans, $status_steps applied steps"
echo "second exact no-op apply: $second_seen files"
if [ "$WITH_CONTAINER" -eq 1 ]; then
  echo "image first apply:        $img_files files, $img_versions journal identities"
fi

overall=0
[ "$spec_bad" -eq 0 ] || overall=1
[ "$res_bad" -eq 0 ] || overall=1
[ "$cp_bad" -eq 0 ] || overall=1
[ "$record_status" -eq 0 ] || overall=1
[ "$run1_status" -eq 0 ] || overall=1
[ "$history1_status" -eq 0 ] || overall=1
[ "$run2_status" -eq 0 ] || overall=1
[ "$history2_status" -eq 0 ] || overall=1
[ "$status_cmd_status" -eq 0 ] || overall=1
[ "$reconcile_status" -eq 0 ] || overall=1
[ "$container_status" -eq 0 ] || overall=1

gate_arms_finish || overall=1
if [ "$overall" -ne 0 ]; then
  echo "PLATFORM MIGRATION CORPUS GATE: FAILED" >&2
  exit 1
fi
echo "PLATFORM MIGRATION CORPUS GATE: PASSED"
