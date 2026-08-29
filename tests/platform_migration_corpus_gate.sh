#!/usr/bin/env bash
# ============================================================================
# THE PLATFORM CANNOT MIGRATE ITS OWN DATABASE - can it, though?
#
# WHAT WENT WRONG. On 2026-08-28, `deploy/ops/db-migrate.sh` could not apply
# `db/migrations-ts/` AT ALL. It died on file #1, and it had been dead since
# ccda4bb42 swapped the applier from the Rust `zeroship-platform-migrate` binary
# to the Node `zero-migrate` CLI. Two independent blockers, stacked:
#
#   1. RESOLUTION. All 35 corpus files spelled `from "@zeroship/migrate"`. That
#      name was never a package Node could find from `db/migrations-ts/` - it was
#      a V8 MODULE-MAP ALIAS the retired Rust binary installed, pointing at
#      `packages/zero-migrate/dist/embedded-recorder.js`. The deleted code says so
#      in its own comment: "This is the ENGINE's recorder ..., NOT the monorepo's
#      `sdks/migrate` copy" (crates/zeroship-migrate-adapter/src/platform/author.rs
#      at ccda4bb42^). Node has no module map, so the alias became
#      `Cannot find package`.
#   2. RECORDER IDENTITY. Made resolvable against `sdks/migrate`, it failed one
#      layer deeper with `op authoring called outside an active migration
#      recorder`: the corpus recorded into `@zeroship/migrate`'s ambient recorder
#      while the CLI drained `zero-migrate`'s. Two recorder singletons, one op
#      list, and it is empty.
#
# AND THE PART THAT MATTERS MORE THAN EITHER BUG. Three comments in this tree
# asserted "Measured 2026-08-28 against a fresh PostgreSQL 17.11: 35 files
# applied, then an immediate second run applied none" - a measurement that could
# not have been taken, because the path it describes died on file #1. A false
# claim about what was verified is why nobody looked for four months of commits.
# This gate exists so that sentence is produced by a machine or not at all.
#
# WHAT THIS GATE RULES ON, in order of how much it is worth:
#
#   apply / idempotence   Runs the SANCTIONED applier (deploy/ops/db-migrate.sh)
#                         over the COMMITTED corpus against a real PostgreSQL,
#                         twice. This is the arm that would have caught it. The
#                         other three are cheap early warnings.
#   specifier             Every corpus file imports the DSL the applier actually
#                         activates, and nothing else.
#   resolution            That specifier RESOLVES, by Node's own resolver, from
#                         each corpus file's own directory - the upward walk the
#                         CLI's plain `import()` performs.
#   container_paths       The container puts the corpus where its resolution can
#                         still work: under the directory that receives the pnpm
#                         store. This is a PROPERTY check on three declarations,
#                         not a spelling check - see the arm.
#
# THE TWO RECORDERS, AND WHAT ACTUALLY STOPS THIS RECURRING. Said plainly,
# because leaving it implied is how the last claim got believed.
#
# NOTHING IN THIS TREE PREVENTS `sdks/migrate` AND `packages/zero-migrate` FROM
# DRIFTING APART. They are two independent implementations of the same DSL and
# recorder; no test compares them, no build step derives one from the other, and
# after this fix none does either. That is not what was fixed.
#
# What was fixed is the platform's DEPENDENCE on their agreeing. Before, the
# corpus was authored against one name and executed by the other, so the two
# staying in step was a load-bearing assumption held up by nothing. Now the
# corpus names the package whose recorder drains it, and the two may diverge
# freely without the platform noticing or caring. SEVERING a coupling is a
# stronger guarantee than bridging one - a bridge propagates drift, an absent
# edge cannot. This gate then detects, every run, whether that single binding
# still works end to end.
#
# So: prevention, no. Detection on every CI run, yes. If someone re-points the
# corpus at a package the applier does not drain, the `specifier` arm fails
# statically and the `apply` arm fails against a live database.
#
# THE END STATE IS STILL A COLLAPSE, and this gate does not deliver it.
# `@zeroship/migrate` should become a thin layer over the engine's recorder
# rather than a parallel implementation. The evidence that this is affordable:
# `sdks/migrate`'s ambient recorder is not, today, the recorder that records
# anything on a shipped path - the creator build esbuild-aliases the specifier
# onto `zero-migrate` before recording
# (sdks/vite-plugin/src/gen-types/recorder.ts:114-117), and the retired platform
# binary mapped it to the engine bundle too. That collapse is a redesign of a
# published package surface (it carries the `@zeroship/db` lexicon bridge and a
# `host/` addon facade that `zero-migrate` deliberately does not), so it is its
# own change with its own verification - not a thing to do in the same patch as
# restoring the platform's ability to migrate its own database.
#
# WHAT IT DOES NOT CATCH, stated so nobody reads it as complete:
#   - Whether the SCHEMA the corpus produces is correct. It rules on "the applier
#     ran it and the journal is idempotent", never on what the tables mean. A
#     migration that applies cleanly and creates the wrong column passes here.
#   - Drift between `@zeroship/migrate` and `zero-migrate`, per the section above.
#     It cannot see it and does not try.
#   - The creator path. `sdks/vite-plugin/src/gen-types/recorder.ts:114-117`
#     records creator migrations by esbuild-aliasing `@zeroship/migrate` onto
#     `zero-migrate`. That alias is untouched here and unexercised by this gate.
#   - Whether the compose `migrate` service, as opposed to the image it builds,
#     is wired correctly end to end. `container_paths` rules on the three path
#     declarations agreeing; it does not start compose. `--with-container` builds
#     and runs the image itself, and is NOT on by default because the `sdks`
#     stage installs a Rust toolchain to compile the napi addon.
#   - Postgres versions other than the one it runs against.
#
# USAGE
#   tests/platform_migration_corpus_gate.sh                 # own container
#   tests/platform_migration_corpus_gate.sh --dsn <url>     # your database
#   tests/platform_migration_corpus_gate.sh --static-only   # no database
#   tests/platform_migration_corpus_gate.sh --with-container  # + build the image
#
# IT DOES NOT SKIP. Without a usable database the apply arms cannot run, so
# `--static-only` must be asked for BY NAME and says out loud which arms it drops.
# A missing prerequisite is a loud failure naming what is missing, never a quiet
# pass - a gate that skips its only real arm prints what a clean tree prints.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

CORPUS_DIR="$ROOT/db/migrations-ts"
CLI="$ROOT/packages/zero-migrate-cli/dist/cli-bin.js"
# The specifier the applier's own recorder is reachable under. The CLI drains
# `zero-migrate/internal/recorder` (packages/zero-migrate-cli/src/cli.ts:57), so
# a corpus file must record into THAT package or its ops land in a different
# singleton and drain empty. It is also what the CLI scaffolds new migrations
# with (packages/zero-migrate-cli/src/cli.ts:931).
DSL_SPECIFIER="zero-migrate"
# The spelling that caused the outage: a name no applier binds outside a V8
# module map. Kept as a named refusal rather than a general "anything else"
# check so the failure message can say what happened last time.
ALIAS_SPECIFIER="@zeroship/migrate"

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

echo "== platform migration corpus gate =="
echo "corpus: $CORPUS_DIR"

# ---------------------------------------------------------------------------
# Arm 1 - specifier. Every corpus file must import the DSL the applier
# activates, and none may import the alias that broke it.
#
# `examined` is the number of files whose import specifier this arm read and
# ruled on - not the number of files present. A file with no import at all is
# counted as ruled-on-and-failing rather than skipped, because a corpus file
# that imports nothing is a file that records nothing.
# ---------------------------------------------------------------------------
spec_examined=0
spec_bad=0
for f in "$CORPUS_DIR"/*.ts; do
  [ -e "$f" ] || continue
  spec_examined=$((spec_examined + 1))
  if grep -q "from \"$ALIAS_SPECIFIER\"" "$f"; then
    echo "FAIL[specifier]: $(basename "$f") imports $ALIAS_SPECIFIER" >&2
    echo "  No applier binds that name. It was a V8 module-map alias installed by" >&2
    echo "  a binary deleted in ccda4bb42; Node has no module map. Use $DSL_SPECIFIER." >&2
    spec_bad=$((spec_bad + 1))
  elif ! grep -q "from \"$DSL_SPECIFIER\"" "$f"; then
    echo "FAIL[specifier]: $(basename "$f") does not import $DSL_SPECIFIER" >&2
    spec_bad=$((spec_bad + 1))
  fi
done
gate_arm specifier "$spec_examined" 25

# ---------------------------------------------------------------------------
# Arm 2 - resolution. The specifier must RESOLVE, and it is resolved the way the
# applier resolves it: Node's own resolver, rooted at each migration file, which
# is the upward node_modules walk a plain `import()` under tsx performs.
#
# NOT a check that `db/migrations-ts/package.json` exists or names a dependency.
# That would answer spelling. This performs the resolution.
#
# IT MUST BE AN ESM `import()`, AND THAT IS NOT A DETAIL. The first version of
# this arm used `createRequire(<file>).resolve(...)`, and it failed on all 35
# files while the live apply beside it passed on all 35 - because CommonJS
# resolution reads the `require` condition of the target's `exports` map, and
# `packages/zero-migrate/package.json` publishes only `import`. The arm was
# answering a question no applier asks. `--input-type=module` with the cwd set to
# the corpus directory reproduces the ACTUAL walk: Node treats the eval module's
# URL as `file://<cwd>/[eval1]`, so bare-specifier resolution starts in the corpus
# directory exactly as it does for a migration file, under ESM conditions, and the
# `await import` proves the module also LOADS rather than merely resolving.
#
# The lesson is the general one: verify against the parser that will read it.
# ---------------------------------------------------------------------------
res_examined=0
res_bad=0
for f in "$CORPUS_DIR"/*.ts; do
  [ -e "$f" ] || continue
  res_examined=$((res_examined + 1))
  if ! (cd "$CORPUS_DIR" && node --input-type=module \
        -e "await import(process.argv[1]);" "$DSL_SPECIFIER") >/dev/null 2>&1; then
    echo "FAIL[resolution]: $DSL_SPECIFIER does not import from $(basename "$f")" >&2
    echo "  Node walks up from $CORPUS_DIR looking for node_modules/$DSL_SPECIFIER." >&2
    echo "  Run pnpm install; db/migrations-ts is a workspace member that declares it." >&2
    res_bad=$((res_bad + 1))
  fi
done
gate_arm resolution "$res_examined" 25

# ---------------------------------------------------------------------------
# Arm 3 - container_paths. The image must put the corpus somewhere its
# resolution still works.
#
# The bug this rules on: the corpus was copied to `/db/migrations-ts` while the
# pnpm store landed at `/app/node_modules`. Node's upward walk from `/db/...`
# reaches `/db/node_modules` and `/node_modules`, neither of which exists, so the
# apply could not have worked in the container even after the specifier was
# fixed. There was also no `package.json` above `/db`, so tsx read the files as
# CJS first.
#
# THIS IS A PROPERTY CHECK, NOT A GREP FOR A KNOWN-GOOD STRING. It extracts the
# three paths the three files actually declare and rules on the RELATIONSHIP
# between them: the corpus must be a descendant of the directory that receives
# node_modules, and the runtime default and the compose flag must both name the
# path the Dockerfile creates. Hardcoding "/app/db/migrations-ts" here would pass
# any future layout that merely kept the spelling.
# ---------------------------------------------------------------------------
DOCKERFILE="$ROOT/deploy/Dockerfile"
ENTRYPOINT="$ROOT/deploy/ops/migrate-entrypoint.sh"
COMPOSE="$ROOT/deploy/compose/docker-compose.yml"

# Where node_modules lands in the migrate stage, and where the corpus lands.
nm_dest="$(grep -oP '^COPY --from=sdks /build/node_modules \K\S+' "$DOCKERFILE" | tail -1)"
corpus_dest="$(grep -oP '^COPY --from=sdks /build/db/migrations-ts \K\S+' "$DOCKERFILE" | tail -1)"
entry_default="$(grep -oP '^MIGRATIONS_DIR="\K[^"]+' "$ENTRYPOINT" | head -1)"
compose_dir="$(grep -A1 -- '- --migrations-dir' "$COMPOSE" | grep -oP '^\s+- \K/\S+' | head -1)"

cp_examined=0
cp_bad=0

# (a) corpus under the node_modules root.
cp_examined=$((cp_examined + 1))
nm_root="${nm_dest%/node_modules}"
if [ -z "$nm_dest" ] || [ -z "$corpus_dest" ]; then
  echo "FAIL[container_paths]: could not read the migrate stage's COPY destinations" >&2
  echo "  node_modules='$nm_dest' corpus='$corpus_dest' in $DOCKERFILE" >&2
  echo "  The COPY lines were reshaped; this arm reads them and must be updated with them." >&2
  cp_bad=$((cp_bad + 1))
elif [ "${corpus_dest#"$nm_root"/}" = "$corpus_dest" ]; then
  echo "FAIL[container_paths]: corpus '$corpus_dest' is not under '$nm_root'" >&2
  echo "  Node resolves the DSL by walking UP from the corpus, so a corpus outside" >&2
  echo "  the tree that holds node_modules cannot resolve it. This is the exact" >&2
  echo "  layout that made the container unable to migrate before 2026-08-28." >&2
  cp_bad=$((cp_bad + 1))
fi

# (b) the entrypoint default names the path the image creates.
cp_examined=$((cp_examined + 1))
if [ "$entry_default" != "$corpus_dest" ]; then
  echo "FAIL[container_paths]: entrypoint default '$entry_default' != image path '$corpus_dest'" >&2
  cp_bad=$((cp_bad + 1))
fi

# (c) the compose --migrations-dir names it too.
cp_examined=$((cp_examined + 1))
if [ "$compose_dir" != "$corpus_dest" ]; then
  echo "FAIL[container_paths]: compose --migrations-dir '$compose_dir' != image path '$corpus_dest'" >&2
  cp_bad=$((cp_bad + 1))
fi

gate_arm container_paths "$cp_examined" 3

# ---------------------------------------------------------------------------
# Arms 4 and 5 - the live apply. Everything above is an early warning; this is
# the arm that rules on the claim in the title.
# ---------------------------------------------------------------------------
corpus_files="$(find "$CORPUS_DIR" -maxdepth 1 -name '*.ts' | wc -l)"

if [ "$STATIC_ONLY" -eq 1 ]; then
  echo
  echo "--static-only: the apply and idempotence arms did NOT run." >&2
  echo "  Those are the arms that rule on whether the platform can migrate its own" >&2
  echo "  database. What ran was three static early warnings over $corpus_files files." >&2
  echo "  This mode is for a machine with no Docker and no PostgreSQL; it is not a" >&2
  echo "  pass of this gate." >&2
  gate_arms_finish || exit 1
  exit 1
fi

OWN_CONTAINER=""
cleanup() {
  if [ -n "$OWN_CONTAINER" ]; then
    docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT HUP INT TERM

if [ ! -f "$CLI" ]; then
  echo "FAIL: the sanctioned applier's CLI is missing: $CLI" >&2
  echo "  run: pnpm install && pnpm build" >&2
  exit 2
fi

if [ -z "$DSN" ]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "FAIL: no --dsn given and docker is not on PATH." >&2
    echo "  This gate needs a PostgreSQL it may create schemas, roles and extensions" >&2
    echo "  in. Pass --dsn <url> for one you control, or --static-only to run the" >&2
    echo "  three static arms and fail." >&2
    exit 2
  fi
  # A port nobody else is on. This tree runs many PostgreSQL containers at once
  # (compio-postgres fixtures, e2e stacks), so a fixed port is a collision.
  port=""
  for p in $(seq 5560 5599); do
    if ! ss -ltn 2>/dev/null | grep -q ":$p "; then port="$p"; break; fi
  done
  if [ -z "$port" ]; then
    echo "FAIL: no free TCP port in 5560-5599 for the gate's own PostgreSQL." >&2
    exit 2
  fi
  OWN_CONTAINER="zs-migcorpus-gate-$port"
  docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  echo "starting a fresh PostgreSQL on 127.0.0.1:$port ($OWN_CONTAINER)"
  if ! docker run -d --name "$OWN_CONTAINER" \
      -p "127.0.0.1:$port:5432" \
      -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
      postgres:17 >/dev/null 2>&1; then
    echo "FAIL: could not start the gate's PostgreSQL container." >&2
    exit 2
  fi
  if ! docker exec "$OWN_CONTAINER" sh -c \
      'for i in $(seq 1 60); do pg_isready -U postgres -q && exit 0; sleep 1; done; exit 1'; then
    echo "FAIL: the gate's PostgreSQL never became ready." >&2
    exit 2
  fi
  DSN="postgres://postgres:zeroship@localhost:$port/zeroship"
fi

run1="$(mktemp)"; run2="$(mktemp)"
trap 'rm -f "$run1" "$run2"; cleanup' EXIT HUP INT TERM

# The SANCTIONED applier, by the path an operator would type. Calling the CLI
# directly here would exercise a different command line from the one that ships,
# which is how the last measurement came to describe a path that did not run.
echo "applying the committed corpus with deploy/ops/db-migrate.sh"
ZEROSHIP_MIGRATE_DSN="$DSN" bash "$ROOT/deploy/ops/db-migrate.sh" >"$run1" 2>&1
run1_status=$?
applied_files="$(grep -c '^apply ' "$run1")"

if [ "$run1_status" -ne 0 ]; then
  echo "FAIL[apply]: the applier exited $run1_status." >&2
  tail -20 "$run1" >&2
fi

# THE RECONCILIATION. A silent skip looks exactly like health: the applier would
# exit 0 having applied a subset. So the arm's count is compared to the number of
# files on disk, and a mismatch is a failure even when the exit code is 0.
if [ "$applied_files" -ne "$corpus_files" ]; then
  echo "FAIL[apply]: $applied_files files reported, $corpus_files on disk." >&2
  echo "  A file the applier never mentioned was skipped, not applied. Compare:" >&2
  echo "    ls $CORPUS_DIR/*.ts" >&2
  echo "    grep '^apply ' <the run log>" >&2
  run1_status=1
fi
gate_arm apply "$applied_files" 25

echo "re-running to rule on idempotence"
ZEROSHIP_MIGRATE_DSN="$DSN" bash "$ROOT/deploy/ops/db-migrate.sh" >"$run2" 2>&1
run2_status=$?
# Ruled on: files the second run reported at all. Of those, the ones that applied
# nothing must be all of them.
second_seen="$(grep -c '^apply ' "$run2")"
second_empty="$(grep -c '"applied":\[\]' "$run2")"

if [ "$run2_status" -ne 0 ]; then
  echo "FAIL[idempotence]: the second run exited $run2_status." >&2
  tail -20 "$run2" >&2
fi
if [ "$second_empty" -ne "$second_seen" ] || [ "$second_seen" -ne "$corpus_files" ]; then
  echo "FAIL[idempotence]: $second_empty of $second_seen files applied nothing" \
       "(expected $corpus_files of $corpus_files)." >&2
  echo "  A second apply that changes the database means the journal version a" >&2
  echo "  migration derives is not stable across runs." >&2
  run2_status=1
fi
gate_arm idempotence "$second_seen" 25

# ---------------------------------------------------------------------------
# Arm 6 (opt-in) - the image. Builds the `migrate` target and applies the corpus
# from INSIDE it, which is the only way to rule on the COPY set rather than on
# the host's node_modules. Off by default: the `sdks` stage installs a Rust
# toolchain to compile the napi addon.
# ---------------------------------------------------------------------------
container_status=0
if [ "$WITH_CONTAINER" -eq 1 ]; then
  echo "building the migrate image"
  image_build_log="$(mktemp)"
  if ! docker build -f "$ROOT/deploy/Dockerfile" --target migrate \
       -t zeroship-migrate:gate "$ROOT" >"$image_build_log" 2>&1; then
    echo "FAIL[image]: the migrate target did not build." >&2
    echo "  Last 80 lines of docker build output:" >&2
    tail -80 "$image_build_log" >&2
    container_status=1
    img_applied=0
  else
    # A SECOND, EMPTY DATABASE, so the in-image apply is a real first apply
    # rather than a re-run over what the host arms already built.
    #
    # Created through a throwaway `postgres` image rather than `docker exec` on
    # the gate's own container: with --dsn the gate never started one, so the
    # exec form ran `docker exec ""`, failed, and was swallowed by the `2>&1` -
    # leaving this arm to report `database "gate_img" does not exist` and 0 of 35.
    # Going through the DSN works for a caller-supplied database too.
    admin_dsn="${DSN%/*}/postgres"
    docker run --rm --network host postgres:17 \
      psql "$admin_dsn" -v ON_ERROR_STOP=1 \
      -c "DROP DATABASE IF EXISTS gate_img" >/dev/null 2>&1
    if ! docker run --rm --network host postgres:17 \
        psql "$admin_dsn" -v ON_ERROR_STOP=1 \
        -c "CREATE DATABASE gate_img" >/dev/null 2>&1; then
      echo "FAIL[image]: could not create the gate_img database via $admin_dsn" >&2
      container_status=1
    fi
    # The SAME DSN with the database swapped, so this arm reaches whatever the
    # rest of the gate reached. 0600 because the CLI's config reader REFUSES a
    # config carrying a literal url with any bit set in 0o077, and the entrypoint
    # copies this DSN into one.
    secret="$(mktemp -d)"
    chmod 700 "$secret"
    printf '%s' "${DSN%/*}/gate_img" > "$secret/dsn"
    chmod 600 "$secret/dsn"
    img_log="$(mktemp)"
    # `--network host`, NOT `--add-host=host.docker.internal:host-gateway`. The
    # gate's own PostgreSQL publishes on 127.0.0.1 only, and the host-gateway
    # address is the bridge IP, which a loopback-bound listener does not answer -
    # that spelling fails to connect rather than proving anything. Host networking
    # also lets a caller-supplied --dsn point anywhere they can already reach.
    docker run --rm --network host \
      -v "$secret/dsn:/tmp/dsn:ro" \
      zeroship-migrate:gate --database-url-file /tmp/dsn > "$img_log" 2>&1
    img_status=$?
    img_applied="$(grep -c '^apply ' "$img_log")"
    if [ "$img_status" -ne 0 ] || [ "$img_applied" -ne "$corpus_files" ]; then
      echo "FAIL[image]: in-image apply exited $img_status with $img_applied of" \
           "$corpus_files files." >&2
      tail -20 "$img_log" >&2
      container_status=1
    fi
    rm -rf "$secret" "$img_log"
  fi
  rm -f "$image_build_log"
  gate_arm image "$img_applied" 25
fi

echo
echo "corpus files on disk:      $corpus_files"
echo "files applied (first run): $applied_files"
echo "files applying nothing (second run): $second_empty of $second_seen"

overall=0
[ "$spec_bad" -eq 0 ] || overall=1
[ "$res_bad" -eq 0 ] || overall=1
[ "$cp_bad" -eq 0 ] || overall=1
[ "$run1_status" -eq 0 ] || overall=1
[ "$run2_status" -eq 0 ] || overall=1
[ "$container_status" -eq 0 ] || overall=1

gate_arms_finish || overall=1

if [ "$overall" -ne 0 ]; then
  echo "PLATFORM MIGRATION CORPUS GATE: FAILED" >&2
  exit 1
fi
echo "PLATFORM MIGRATION CORPUS GATE: PASSED"
