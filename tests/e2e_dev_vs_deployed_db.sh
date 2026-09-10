#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Dev vs deployed: env.db. Run ONE identical operation sequence against
# `pnpm dev` (SQLite) and against the same app deployed behind the gateway
# (PostgreSQL), then (a) diff the RESULTS and (b) assert ABSOLUTE properties
# of the DEPLOYED answers.
#
# This is the db leg of scenario 11 (docs/pilot/e2e-scenarios.md). It is the
# last of the dev-vs-deployed legs to be walked; kv, storage, auth, workflows,
# streaming and the starter's RPC came first. It was blocked until 2026-08-10
# on #176 (dev generated types from the migrations and never APPLIED them, so
# `.zeroship/dev.sqlite` held 0 tables) and #162 (a creator-declared
# `created_at` collided with the injected policy column). Both are fixed; this
# script is what turns "unblocked" into a measured result.
#
# WHY db-todos AND NOT db-hitcounter. `tests/e2e_db_app_end_to_end.sh` already
# proves the deployed data plane down to rows landing in Postgres, but it
# drives `examples/db-hitcounter`, whose only creator column is `path` -- one
# lowercase word, no case boundary, no foreign key, no relation, one row shape.
# It is blind BY CONSTRUCTION to the defect class that was live until today:
# a camelCase column (`todos.userId`) has to survive the migration DDL, the
# generated descriptor, and the data plane's SELECT list, on two different
# backends, spelled the same way each time. db-todos has that column, a
# migration-declared foreign key, a relation eager-load, and cursor
# pagination.
#
# WHAT IS NORMALISED, and what that makes invisible. Only two classes:
#
#   minted ids   `user_…`/`todo_…` are UUIDv7-derived, so they cannot agree
#                across two databases. They are replaced by <IDn> aliases
#                assigned in ORDER OF FIRST APPEARANCE over the whole capture,
#                which preserves REFERENTIAL identity: a todo whose `userId`
#                points at the seeded user still reads `<ID1>` on both tiers,
#                so an FK that pointed at the wrong row would still diverge.
#                What this hides: the id FORMAT beyond its prefix -- alphabet,
#                length, monotonicity. Section 4 asserts those absolutely on
#                the deployed bodies.
#
#   timestamps   `created_at`/`updated_at`/`deleted_at` are wall-clock. The
#                scrub replaces the DIGITS only (`"created_at":<TS>`), so a
#                tier answering an ISO STRING rather than epoch millis does
#                NOT match `<TS>` and still shows up as a divergence. What it
#                hides: the MAGNITUDE (seconds vs millis), and whether
#                `updated_at` moved on insert. The `tsrel` row below is
#                computed BEFORE the scrub and carries the digit count plus
#                `created_at == updated_at`, so both of those stay comparable;
#                section 4 pins the magnitude absolutely.
#
#   Also scrubbed: `request_id` (a per-process counter, so it encodes how many
#   requests the tier had served, not what env.db did) and the opaque
#   `continueCursor` blob (it base64-embeds the minted ids). The cursor is NOT
#   dropped -- it is decoded into its own `p1cur`/`p2cur` rows, which ARE
#   compared, so its structure and its bound orderBy stay in the diff.
#
# WHAT IS DELIBERATELY NOT PROBED:
#   todos.shareToWebhook  action + runQuery + outbound fetch. The db-specific
#                         part (runQuery reading the row) is already covered by
#                         todos.get; the rest is the fetch seam, which
#                         tests/e2e_dev_vs_deployed_env.sh and the stream leg
#                         own. Adding a one-shot HTTP stub here would test
#                         networking, not env.db.
#   todos.subscribe       `db.live` SSE. Real db surface, but its rerun trigger
#                         is backend-specific and time-dependent, and the
#                         streaming TRANSPORT is already walked by
#                         tests/e2e_dev_vs_deployed_stream.sh. Comparing it
#                         here would mix a timing seam into a data seam. Named
#                         as a gap rather than silently omitted.
#   tags (json column)    exercised only as `[]`, because createTodo hardcodes
#                         it. The empty-array round-trip IS compared; a
#                         populated JSON document is NOT, and no procedure in
#                         db-todos accepts one.
#   (CONCURRENT tx used to be listed here as unprobed. It is probed now --
#    sections 3b and 4b. The claim that stood here, "from inside one request
#    there is no way to stage two competing writers", was WRONG: two
#    `db.transaction()` calls under one `Promise.all` stage exactly that, and
#    measuring it found the defect in #244. Left as a correction rather than
#    deleted, because the false claim is what kept the row closed.)
#
# Prereqs (docs/runbooks/local-dev.md):
#   pnpm build
#   cargo build --release -p zeroship-control -p zeroship-worker \
#       -p zeroship-gateway -p zeroship-cli -p zeroship-migrate-server --bins
#   pnpm install && pnpm build
#   docker (this script starts and destroys its own ephemeral Postgres)
#   pnpm install in examples/db-todos
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
# `zeroship.app_members` is deleted; an app reaches the people who answer for it
# through its project's organization. `seat_app_owner` writes that join, reads
# the seat back out of the database and exits when it is not there - an
# INSERT ... SELECT over no rows is a SUCCESSFUL statement that seats nobody,
# and the 403 it later produces surfaces far from here.
source "$ROOT/tests/lib/organization_fixture.sh"
APP="$ROOT/examples/db-todos"
ZSHIP="$APP/dist/app.zship"
WORK="$(mktemp -d -t zs-devdeploy-db-XXXXXX)"

# Ports distinct from golden_path.sh (9390/8390/8300) and the kv leg
# (9392/8392/8302/3011) so the suites can run concurrently.
ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9393}"
ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-8393}"
ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-8303}"
ZEROSHIP_MIGRATE_SERVER_PORT="${ZEROSHIP_MIGRATE_SERVER_PORT:-9493}"
DEV_PORT="${DEV_PORT:-3021}"
# VITE's own port. DEV_PORT above is the RUNTIME port. vite was silently taking
# its :5173 global default, which nothing here declared, tracked or freed, so a
# second harness on this machine fought it for the port and the cleanup trap
# could never reclaim it. --strictPort at the call so a conflict fails loudly
# rather than moving to a port nobody watches. Checked against the runtime's
# bad-ports list before choosing it (see #272 and the stream harness). See #272.
VITE_PORT="${VITE_PORT:-5021}"
PG_PORT="${PG_PORT:-5487}"
PGC="zs-devdeploy-db-pg"
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
ZEROSHIP_CONTROL_KEY="dd-ck"; ZEROSHIP_CONTROL_MASTER_KEY="dd-mk"
export E2E_STALE_WORKER_BEARER="${E2E_STALE_WORKER_BEARER:-devdeploy-worker-key-0123456789abcd}"
APP_NAME="dbtodos"

# One identity per RUN, used VERBATIM on both tiers. `users.email` and
# `users.handle` are UNIQUE, so a fixed literal passes once per database and
# fails forever after; a per-tier random one would make the two captures
# differ by construction and the diff meaningless.
RUN="${RUN_ID:-$(date +%s)}"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  # Kill the CHILD before the subshell. `PIDS` holds SUBSHELL pids, and killing a
  # subshell does not reap the `vite` it launched: the child is reparented to
  # init and survives. Measured 2026-08-10 -- three orphaned `node vite.js`
  # accumulated across consecutive runs (`PPID 1`, each still holding its own
  # run's `DATABASE_URL`), competed for DEV_PORT, and a later run's race probe
  # was OOM-killed mid-measurement:
  #     line 507: 3285383 Killed  RACE_BASE=... node --input-type=module -
  # which the harness then reported as `dev race produced no runs` plus 96 red
  # verdicts -- a resource failure wearing a platform failure's clothes.
  for p in "${PIDS[@]:-}"; do
    pkill -P "$p" 2>/dev/null || true
    kill "$p" 2>/dev/null || true
  done
  # Backstop for a child that re-execs or double-forks past `pkill -P`. Scoped by
  # THIS run's state dir, read out of the process's own environment, so a
  # CONCURRENT run's dev server can never be caught by it -- a pid pattern alone
  # would make two runs of this leg kill each other.
  # `/proc`-based, so it is a silent no-op off Linux -- there it degrades to the
  # DEV_PORT sweep below, which is what this harness had before.
  if [ -n "${DEVSTATE:-}" ]; then
    for p in $(pgrep -f 'vite' 2>/dev/null); do
      [ "$p" = "$$" ] && continue   # pgrep -f matches this shell's own cmdline
      grep -aqs -- "$DEVSTATE" "/proc/$p/environ" 2>/dev/null && kill -9 "$p" 2>/dev/null
    done
  fi
  # BOTH ports, by LISTENER not by recorded PID: `( cd x && vite )&` records the
  # subshell, and vite outlives it as an orphan the PID loop cannot reach.
  for _p in "$DEV_PORT" "$VITE_PORT"; do
    lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
  done
  docker rm -f "$PGC" >/dev/null 2>&1 || true
  [ "${KEEP_WORK:-0}" = "1" ] && { echo "  work dir kept: $WORK"; return; }
  rm -rf "$WORK"
}
trap cleanup EXIT

command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1 || {
  echo "  docker unavailable -- this leg needs a real Postgres for the deployed tier."
  echo "  NOT skipped-as-pass: exiting 2 so a missing prereq cannot read as a green run."
  exit 2
}
for b in zeroship zeroship-control zeroship-gate zeroship-worker \
         zeroship-migrate-server dev-provision; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b -- see the prereqs in this file's header"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI -- see the prereqs in this file's header"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
[ -f "$JOSE" ] || { echo "missing jose at $JOSE"; exit 2; }
RECORDER="$ROOT/sdks/vite-plugin/dist/gen-types/recorder.js"
[ -f "$RECORDER" ] || { echo "missing $RECORDER -- run pnpm build"; exit 2; }

# Same BUILD on both sides? `pnpm dev` runs target/release/zeroship; the
# deployed side runs separate binaries. A partial rebuild reports version skew
# as a backend divergence -- see tests/lib/binary_freshness.sh.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/zeroship-data-v8/src crates/zeroship-data-orm/src crates/zeroship-data-sql/src crates/zeroship-runtime/src crates/zeroship-worker/src crates/zeroship-gateway/src crates/zeroship-control/src crates/zeroship-migrate-server/src sdks/db/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control zeroship-migrate-server dev-provision" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

echo "=== dev vs deployed (db-todos, env.db) ==="
echo "  run identity: $RUN"

jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

# ---------------------------------------------------------------------------
# The probe. TWO outputs off the same calls:
#   $RAWFILE   the body VERBATIM, one line per label. Feeds section 4's
#              absolute verdicts, which must not read scrubbed text.
#   stdout     nothing -- the comparable rendering is produced afterwards by
#              `render`, because the id aliasing needs the WHOLE capture to
#              assign stable numbers.
#
# Every operation below is here because it can DIVERGE between SQLite and
# Postgres, and the comment says how.
# ---------------------------------------------------------------------------
probe() {
  local base="$1" rpc="$1/__zeroship/v1"
  call() {
    curl -sS -m 30 -X POST -H 'content-type: application/json' \
      "$rpc/$1" -d "{\"json\":${2:-{\}}}" 2>&1
  }
  row() { printf '%-10s %s\n' "$1" "$(call "$2" "${3:-}")" >> "$RAWFILE"; }

  # --- insert, and read the row back off the insert response --------------
  # Round-trip of every declared column plus the seven injected system
  # columns. SQLite has no boolean and no timestamptz; Postgres has both, so
  # `done`/`archived` and `created_at` are the two most likely places for the
  # two tiers to answer different JSON for the same write.
  row seedA users.seed "{\"email\":\"alice-$RUN@probe.test\",\"name\":\"Alice\",\"handle\":\"alice_$RUN\"}"
  row seedB users.seed "{\"email\":\"bob-$RUN@probe.test\",\"name\":\"Bob\",\"handle\":\"bob_$RUN\"}"
  local aid bid
  aid="$(grep -m1 '^seedA ' "$RAWFILE" | grep -oE '"id":"user_[^"]+"' | head -1 | cut -d'"' -f4)"
  bid="$(grep -m1 '^seedB ' "$RAWFILE" | grep -oE '"id":"user_[^"]+"' | head -1 | cut -d'"' -f4)"
  if [ -z "$aid" ] || [ -z "$bid" ]; then
    printf '%-10s SEED FAILED, probe aborted\n' abort >> "$RAWFILE"
    return 1
  fi

  # --- the camelCase seam --------------------------------------------------
  # `todos.userId` is the column that was broken until today. The migration
  # creates it quoted, the descriptor carries the name, and the data plane has
  # to ask for the SAME spelling. Postgres folds unquoted identifiers to lower
  # case and SQLite does not, so a single missing quote diverges here and
  # nowhere else in this app.
  # FIVE todos for alice, not three: `todos.listPage` is probed at
  # numItems=2, and three rows make page 2 the last page, so the third hop
  # would be a call past the end rather than a page. The first version of this
  # probe did exactly that and both tiers answered an opaque error -- agreeing,
  # and proving nothing about a multi-page walk. `mkT4` belongs to BOB so the
  # `userId` filter has something to exclude.
  row mkT1 todos.create "{\"userId\":\"$aid\",\"title\":\"buy milk\",\"priority\":\"low\"}"
  row mkT2 todos.create "{\"userId\":\"$aid\",\"title\":\"walk dog\"}"
  row mkT3 todos.create "{\"userId\":\"$aid\",\"title\":\"ship it\",\"priority\":\"high\"}"
  row mkT4 todos.create "{\"userId\":\"$bid\",\"title\":\"bob task\"}"
  row mkT5 todos.create "{\"userId\":\"$aid\",\"title\":\"read book\"}"
  row mkT6 todos.create "{\"userId\":\"$aid\",\"title\":\"pay bills\",\"priority\":\"high\"}"
  local t1
  t1="$(grep -m1 '^mkT1 ' "$RAWFILE" | grep -oE '"id":"todo_[^"]+"' | head -1 | cut -d'"' -f4)"

  # --- the migration-declared foreign key, enforcement half ---------------
  # `t.text().references("users","id")`. SQLite enforces FKs only with
  # `PRAGMA foreign_keys=ON`; Postgres always does. A tier that silently
  # accepted the orphan would answer a row here where the other answers an
  # error, and `orphanN` (a count, not a message) says whether the row landed
  # independently of how the failure is worded.
  row orphan todos.create '{"userId":"user_doesNotExist0000000","title":"orphan"}'
  row orphanN todos.count '{"userId":"user_doesNotExist0000000"}'

  # --- unique constraint, second declared index type ----------------------
  row dupEmail users.seed "{\"email\":\"alice-$RUN@probe.test\",\"name\":\"Dup\",\"handle\":\"dup_$RUN\"}"

  # --- single-row read, and the missing-row shape -------------------------
  row getT1 todos.get "{\"id\":\"$t1\"}"
  row getNone todos.get '{"id":"todo_0000000000000000000000"}'
  row countA todos.count "{\"userId\":\"$aid\"}"

  # --- ORDERED multi-row read ---------------------------------------------
  # `todos.list` is `.sort({id:-1})`, so the order IS a contract here and is
  # compared VERBATIM. Contrast the kv leg, where `kv.keys.list` had no
  # documented order and had to be compared as a set. If this diverges, an
  # explicit sort is being dropped or inverted by one backend.
  row list todos.list "{\"userId\":\"$aid\"}"

  # --- relation eager-load, the foreign key's other half ------------------
  # `find({...}, { with: { userId: true } })` must replace the bare FK with
  # the joined user row, batched. Note this query carries NO sort, so its row
  # ORDER is an unspecified property of each backend -- exactly the shape of
  # the divergence the kv leg found. Compared verbatim anyway: this run is the
  # measurement, and pinning it to a set before measuring would decide the
  # answer in advance.
  row withUser todos.listWithUser "{\"userId\":\"$aid\"}"

  # --- DataLoader batching -------------------------------------------------
  # Two concurrent `db.users.get(id)` coalesce into one WHERE id IN (...).
  # The stitch step (which row goes back to which caller) is what a regression
  # would break, and it is visible in the response.
  row pair users.getPair "{\"aId\":\"$aid\",\"bId\":\"$bid\"}"

  # --- cursor pagination ---------------------------------------------------
  # Same page size, same sort, three hops. The cursor is opaque on the wire,
  # so it is decoded below and its STRUCTURE compared; whether page 2 advances
  # past page 1 is asserted on the rows.
  row p1 todos.listPage "{\"userId\":\"$aid\",\"cursor\":null,\"numItems\":2}"
  local c1
  c1="$(grep -m1 '^p1 ' "$RAWFILE" | grep -oE '"continueCursor":"[^"]*"' | head -1 | cut -d'"' -f4)"
  row p2 todos.listPage "{\"userId\":\"$aid\",\"cursor\":\"$c1\",\"numItems\":2}"
  local c2
  c2="$(grep -m1 '^p2 ' "$RAWFILE" | grep -oE '"continueCursor":"[^"]*"' | head -1 | cut -d'"' -f4)"
  row p3 todos.listPage "{\"userId\":\"$aid\",\"cursor\":\"$c2\",\"numItems\":2}"
  # Decode both cursors. The base64 blob itself embeds minted ids, so the
  # ENCODED form is scrubbed in `render` and these decoded rows are what keeps
  # the cursor in the comparison.
  printf '%-10s %s\n' p1cur "$(printf '%s' "$c1" | base64 -d 2>/dev/null)" >> "$RAWFILE"
  printf '%-10s %s\n' p2cur "$(printf '%s' "$c2" | base64 -d 2>/dev/null)" >> "$RAWFILE"

  # --- system columns under mutation --------------------------------------
  # `version` must increment per write and `updated_at` must move. `version`
  # is not scrubbed at all; `updated_at` keeps its shape but loses its digits,
  # so its MOVEMENT is captured separately by `tsupd` below.
  #
  # The sleep makes that movement deterministic rather than a coin toss. Dev's
  # SQLite stores `created_at`/`updated_at` as TEXT CURRENT_TIMESTAMP, which is
  # WHOLE-SECOND resolution -- measured: every dev row comes back as
  # ...000 millis. Without a full second between the insert and the update, a
  # correct dev tier reports `updated_at == created_at` on some runs and not
  # others, and the harness would flake on the clock rather than measure the
  # backend.
  sleep 1.2
  row setDone todos.setDone "{\"id\":\"$t1\",\"done\":true}"
  row archive todos.archive "{\"id\":\"$t1\"}"

  # --- soft delete ---------------------------------------------------------
  # The descriptor says `softDelete: false`, yet dev answers a row with
  # `deleted_at` set and `version` bumped. Whatever the semantics, both tiers
  # must agree on them, and the row must stop being visible afterwards.
  row del todos.delete "{\"id\":\"$t1\"}"
  row getDel todos.get "{\"id\":\"$t1\"}"
  row listAfter todos.list "{\"userId\":\"$aid\"}"

  # --- timestamp RELATION, computed before the scrub ----------------------
  # The scrub blanks the digits, so magnitude and equality would be invisible.
  # This row carries the digit count of `created_at` (13 == epoch millis, 10
  # == seconds) and whether `created_at == updated_at` on a freshly inserted
  # row. A tier storing seconds, or moving `updated_at` on insert, diverges
  # here even though the scrubbed rows agree.
  local ca ua
  ca="$(grep -m1 '^mkT2 ' "$RAWFILE" | grep -oE '"created_at":[0-9]+' | head -1 | cut -d: -f2)"
  ua="$(grep -m1 '^mkT2 ' "$RAWFILE" | grep -oE '"updated_at":[0-9]+' | head -1 | cut -d: -f2)"
  printf '%-10s digits=%s equal=%s\n' tsrel "${#ca}" \
    "$([ -n "$ca" ] && [ "$ca" = "$ua" ] && echo true || echo false)" >> "$RAWFILE"
  # And whether an UPDATE moved `updated_at` past `created_at`. Same reason:
  # scrubbed digits make a frozen `updated_at` invisible to the diff.
  local uc uu
  uc="$(grep -m1 '^setDone ' "$RAWFILE" | grep -oE '"created_at":[0-9]+' | head -1 | cut -d: -f2)"
  uu="$(grep -m1 '^setDone ' "$RAWFILE" | grep -oE '"updated_at":[0-9]+' | head -1 | cut -d: -f2)"
  printf '%-10s moved=%s\n' tsupd \
    "$([ -n "$uu" ] && [ -n "$uc" ] && [ "$uu" -gt "$uc" ] 2>/dev/null && echo true || echo false)" >> "$RAWFILE"
  # And the timestamp RESOLUTION, which is the one thing the `<TS>` scrub hid
  # completely and which the two backends do NOT agree on. Six todos are
  # inserted back to back inside ~100ms; this counts how many DISTINCT
  # `created_at` values they got. A whole-second clock collapses them to 1; a
  # millisecond clock keeps all 6.
  #
  # Deterministic enough to assert on, and here is the residual risk stated
  # rather than hidden: a second-resolution tier lands 2 instead of 1 if the
  # inserts straddle a second boundary (~10% of runs, since the six span ~100ms
  # of a 1000ms tick). It can never reach 6 -- that would need five boundaries
  # inside 100ms. So `1 or 2` vs `6` separates the two clocks with no overlap,
  # and the row is compared as the COUNT, not as a pass/fail threshold.
  local distinct
  distinct="$(grep -E '^mkT[1-6] ' "$RAWFILE" | grep -oE '"created_at":[0-9]+' \
    | sort -u | wc -l)"
  printf '%-10s distinct_created_at=%s of 6\n' tsres "$distinct" >> "$RAWFILE"

  # --- TRANSACTIONS --------------------------------------------------------
  # Appended at the END on purpose: `countA`, `list`, `listAfter` and the
  # timestamp rows above assert exact counts for alice, and a tx probe that
  # wrote into her list would move them. These use their OWN user so the
  # counts they read are counts of their own rows and nothing else.
  #
  # Until 2026-08-10 this app had ZERO `db.transaction()` calls -- both greps
  # hit comments -- so commit / rollback / savepoint / isolation were walked on
  # NEITHER tier. `examples/db-e2e` has four real calls but cannot serve: no
  # migrations/, no generated/zeroship/, so its manifest carries no
  # runtime_descriptor and boot installs nothing on env.db (#209). The
  # procedures were ported into db-todos instead; rationale in
  # examples/db-todos/src/index.ts.
  # WHAT THE SCRUB HIDES IN THESE ROWS: almost nothing, by construction. The
  # tx procedures return derived scalars (counts, codes, messages, titles), not
  # row objects, so `<TS>` touches only `seedTx` and `<IDn>` touches only
  # `seedTx.id` and `txCommit.aId`. Checked rather than assumed: diffing the
  # ten tx RESULT rows RAW -- no scrub at all -- leaves exactly one difference,
  # the minted `aId`, which two separate databases cannot agree on. So a green
  # comparison here is not an artifact of the normalisation, which is the trap
  # the kv leg fell into.
  row seedTx users.seed "{\"email\":\"tx-$RUN@probe.test\",\"name\":\"Tx\",\"handle\":\"tx_$RUN\"}"
  local txid
  txid="$(grep -m1 '^seedTx ' "$RAWFILE" | grep -oE '"id":"user_[^"]+"' | head -1 | cut -d'"' -f4)"
  if [ -z "$txid" ]; then
    printf '%-10s TX SEED FAILED, tx probes skipped\n' txabort >> "$RAWFILE"
    return 1
  fi

  # Commit, plus read-your-own-writes on the tx connection before COMMIT.
  row txCommit todos.txCommit "{\"userId\":\"$txid\",\"tag\":\"c$RUN\"}"
  # Throw inside the callback -> ROLLBACK. Carries what the CALLER receives
  # (the creator's own error, code and message verbatim) and whether the row
  # is really gone, read back outside the transaction.
  row txRoll todos.txRollback "{\"userId\":\"$txid\",\"tag\":\"r$RUN\"}"
  # Nested transaction = SAVEPOINT. Inner throw must roll back ONLY the inner
  # insert while the outer commits its own row.
  row txNest todos.txNested "{\"userId\":\"$txid\",\"tag\":\"n$RUN\"}"

  # THE DOCUMENTED DIVERGENCE. docs/reference/sqlite-divergences.md claims PG
  # emits `BEGIN ISOLATION LEVEL ...` while SQLite validates the string and
  # runs a plain `BEGIN`. Verified in the source
  # (crates/zeroship-data-orm/src/transaction/mod.rs:356-391), never measured through
  # the creator surface. These four rows are that measurement: if the two tiers
  # answer the same for a valid level, the divergence is REAL IN THE SQL and
  # UNOBSERVABLE through env.db for a single uncontended transaction -- which
  # is a result, not a non-result. What these rows CANNOT see: anything that
  # needs two CONCURRENT transactions (lost update, write skew, a
  # serialization failure), because the tx slot is per-app-per-isolate and a
  # second transaction() call inside one request NESTS instead of running
  # alongside. So a creator who relies on SERIALIZABLE to reject a conflicting
  # writer is still unmeasured here.
  row txIsoNone todos.txIsolation "{\"userId\":\"$txid\",\"tag\":\"i0$RUN\",\"level\":null}"
  row txIsoSer  todos.txIsolation "{\"userId\":\"$txid\",\"tag\":\"i1$RUN\",\"level\":\"serializable\"}"
  row txIsoRR   todos.txIsolation "{\"userId\":\"$txid\",\"tag\":\"i2$RUN\",\"level\":\"repeatableRead\"}"
  # Rejected BEFORE any SQL runs, by normalize_isolation_level() in
  # crates/zeroship-data-v8/src/v8_classes/db.rs:295 -- so it is backend-independent
  # by construction and MUST agree. Probed anyway: "must agree by
  # construction" is the kind of claim that turns out to be wrong.
  row txIsoBad  todos.txIsolation "{\"userId\":\"$txid\",\"tag\":\"i3$RUN\",\"level\":\"snapshot\"}"

  # Savepoint depth: MAX_SAVEPOINT_DEPTH = 8, so 9 levels (BEGIN + 8
  # SAVEPOINTs) is the deepest allowed and 10 must be refused with
  # `savepoint_depth_exceeded`. 9 also proves SQLite really opens eight
  # savepoints rather than silently flattening them.
  row txD9  todos.txDepth "{\"userId\":\"$txid\",\"tag\":\"d9$RUN\",\"levels\":9}"
  row txD10 todos.txDepth "{\"userId\":\"$txid\",\"tag\":\"da$RUN\",\"levels\":10}"

  # Independent tally of everything the tx probes left behind, read through the
  # ordinary (non-tx) path. Each procedure reports its own count; this row is
  # the one the procedures cannot fake.
  row txTotal todos.count "{\"userId\":\"$txid\"}"

  # --- CONCURRENT TRANSACTIONS ---------------------------------------------
  # The regime an isolation level exists for, and the last unmeasured element
  # of scenario 3. Everything above is a single UNCONTENDED transaction.
  #
  # These two use their OWN user so `txTotal` above stays an exact count.
  #
  # Both are staged from inside ONE request via `Promise.all`, deliberately:
  # that removes the confound named in docs/reference/sqlite-divergences.md
  # (deployed can spread requests across isolates, `pnpm dev` cannot). One
  # request is one isolate on BOTH tiers, so what these rows compare is the
  # transaction machinery and not the request scheduler. The cross-REQUEST
  # version, which does depend on the scheduler, is section 3b/4b.
  row cxSeed users.seed "{\"email\":\"cx-$RUN@probe.test\",\"name\":\"Cx\",\"handle\":\"cx_$RUN\"}"
  local cxid
  cxid="$(grep -m1 '^cxSeed ' "$RAWFILE" | grep -oE '"id":"user_[^"]+"' | head -1 | cut -d'"' -f4)"
  if [ -z "$cxid" ]; then
    printf '%-10s CX SEED FAILED, concurrency probes skipped\n' cxabort >> "$RAWFILE"
    return 1
  fi
  # Two transactions opened in the same JS turn. A creator reading
  # docs/reference/db.md expects two independent units of work: two rows, no
  # errors.
  row cxPar todos.txParallel "{\"userId\":\"$cxid\",\"tag\":\"p$RUN\"}"
  # A holds a transaction open across an await and then ABORTS; B opens its own
  # transaction inside that window and COMMITS. `bAfter` is the load-bearing
  # field: B reported success, so B's row must be in the table. If it is not,
  # B was folded into A's transaction and A's ROLLBACK destroyed a stranger's
  # committed write.
  row cxOvl todos.txOverlap "{\"userId\":\"$cxid\",\"tag\":\"o$RUN\",\"holdMs\":400}"
  # The sharper version: leg B here is an ORDINARY `db.todos.insert()` with
  # no transaction anywhere in its call chain. If the tx connection is routed
  # ambiently ("does this app have a tx open?") rather than by call context,
  # then the DEFAULT write path inherits a stranger's transaction, and A's
  # rollback deletes B's row.
  row cxPlain todos.txPlainWrite "{\"userId\":\"$cxid\",\"tag\":\"w$RUN\",\"holdMs\":400}"
  # Independent tally, read outside any transaction. Contract: 2 rows from
  # cxPar (both legs commit) + 0 from cxOvl leg A (aborted) + 1 from cxOvl
  # leg B (committed) + 0 from cxPlain leg A (aborted) + 1 from cxPlain
  # leg B (an ordinary write) = 4.
  row cxTotal todos.count "{\"userId\":\"$cxid\"}"
}

# ---------------------------------------------------------------------------
# THE OTHER DIRECTION: does an op still reach its OWN transaction?
#
# `cxPlain` asks whether an unrelated write is wrongly pulled INTO a
# transaction. These ask the converse, which the same change can break: a
# write that IS inside a transaction must still route to it.
#
# RUN LAST, ON PURPOSE, and appended to the same capture so section 5 still
# diffs them. `txBranch` deliberately leaves an operation in flight when its
# transaction aborts, and on the dev tier that strands SQLite's single
# connection inside an open transaction: every later `db.transaction()` on
# that server then answers `begin_failed: cannot start a transaction within a
# transaction`. That is a real defect (`exec_settle_top_level` treats an
# already-drained slot as settled and issues no ROLLBACK), and it is reported
# rather than hidden -- but running this probe mid-sequence made an unrelated
# control, the dev cross-request race, report "no pair overlapped", which is a
# measurement destroyed rather than a finding. So the destructive probe goes
# after everything it could contaminate.
#
# Its own user, so `cxTotal` stays an exact count.
# ---------------------------------------------------------------------------
scope_probe() {
  local base="$1" rpc="$1/__zeroship/v1"
  call() {
    curl -sS -m 30 -X POST -H 'content-type: application/json' \
      "$rpc/$1" -d "{\"json\":${2:-{\}}}" 2>&1
  }
  row() { printf '%-10s %s\n' "$1" "$(call "$2" "${3:-}")" >> "$RAWFILE"; }

  row bxSeed users.seed "{\"email\":\"bx-$RUN@probe.test\",\"name\":\"Bx\",\"handle\":\"bx_$RUN\"}"
  local bxid
  bxid="$(grep -m1 '^bxSeed ' "$RAWFILE" | grep -oE '"id":"user_[^"]+"' | head -1 | cut -d'"' -f4)"
  if [ -z "$bxid" ]; then
    printf '%-10s BX SEED FAILED, scope probes skipped\n' bxabort >> "$RAWFILE"
    return 1
  fi
  # Writes on PARALLEL branches inside one callback. Every other tx probe
  # awaits in a straight line, so only this one can tell whether a branched
  # continuation inherits the transaction.
  row txBranch todos.txBranchWrites "{\"userId\":\"$bxid\",\"tag\":\"b$RUN\"}"
  # A write from a continuation that outlived its transaction. The answer is
  # REPORTED, not assumed -- see the verdicts in section 4.
  row txOrphan todos.txOrphanedWrite "{\"userId\":\"$bxid\",\"tag\":\"r$RUN\",\"holdMs\":300}"
  # Independent tally: 0 from txBranch (the callback throws) + 1 from txOrphan
  # (its in-tx row commits) + 0 from txOrphan's orphaned write = 1.
  row bxTotal todos.count "{\"userId\":\"$bxid\"}"
}

# ---------------------------------------------------------------------------
# The cross-REQUEST write-write race, run N times and CLASSIFIED.
#
# Separate from `probe()` and deliberately OUTSIDE the byte-diff: the deployed
# worker runs 2 threads with a thread-local isolate cache, so two concurrent
# requests for one app may land on one isolate (cooperative interleaving) or on
# two (genuine parallelism), and `pnpm dev` is pinned to `--workers=1` and has
# no second isolate at all. That is a real structural difference between the
# tiers and forcing it into a byte-diff would report the scheduler as a data
# divergence. What IS compared is the CLASSIFICATION, in section 4b.
#
# One run of a race is worthless -- races are probabilistic. Each call fires
# RACE_N pairs and prints a tally, so a rare interleaving is visible as a count
# rather than as a coin toss.
race() { # <base> <outfile>
  RACE_BASE="$1" RACE_N="${RACE_N:-16}" \
    node --input-type=module - > "$2" 2>&1 <<'NODE'
const base = process.env.RACE_BASE;
const N = Number(process.env.RACE_N), holdMs = 400;
const headers = { "content-type": "application/json" };
const call = async (proc, json) => {
  const r = await fetch(`${base}/__zeroship/v1/${proc}`, {
    method: "POST", headers, body: JSON.stringify({ json }) });
  const t = await r.text();
  try { return JSON.parse(t); } catch { return { unparseable: t.slice(0, 160) }; }
};
// One signature per leg, deliberately COARSE: an error CODE, or `ok` plus the
// count the transaction read before it wrote. Messages and ids are excluded so
// the two tiers are comparable; the raw bodies are in the harness work dir.
const sig = (j) => j.threw ? `threw:${j.threw.code}`
  : j.error ? `err:${j.error.code}` : `ok:before=${j.data?.before}`;
// Its OWN user, so `todos.countTitle` below counts this race's rows and
// nothing else, and so a failed seed is loud rather than silent.
const stamp = `${Date.now()}${Math.floor(Math.random() * 1e6)}`;
const seeded = await call("users.seed", {
  email: `race-${stamp}@probe.test`, name: "Race", handle: `race_${stamp}` });
const userId = seeded?.json?.id;
if (typeof userId !== "string") {
  console.log(`runs=0\n  0x SEED_FAILED ${JSON.stringify(seeded).slice(0, 200)}`);
  process.exit(0);
}
const tally = new Map();
for (let i = 0; i < N; i++) {
  const tag = `rc-${i}-${Math.random().toString(36).slice(2, 8)}`;
  const fire = (delay) => new Promise((res) => setTimeout(
    () => res(call("todos.txRaceStep", { userId, tag, holdMs, level: "serializable" })), delay));
  const [a, b] = await Promise.all([fire(0), fire(holdMs / 4)]);
  const ja = a.json, jb = b.json;
  if (!ja || !jb) { const k = `WIRE_ERROR`; tally.set(k, (tally.get(k) ?? 0) + 1); continue; }
  // Did the two handlers actually overlap in wall-clock time? Without this the
  // whole run could be two serialised requests reporting a clean result that
  // says nothing about concurrency.
  const overlapped = jb.t0 < ja.t1 && ja.t0 < jb.t1;
  const rows = (await call("todos.countTitle", { userId, title: tag })).json;
  // The two integrity questions, independent of which backend is underneath:
  //   lostCommit  a leg reported success and its row is missing.
  //   ghostCommit a leg reported failure and a row landed anyway.
  const okCount = [ja, jb].filter((j) => !j.threw && !j.error).length;
  const verdict = rows === okCount ? "consistent"
    : rows < okCount ? `lostCommit(ok=${okCount},rows=${rows})`
    : `ghostCommit(ok=${okCount},rows=${rows})`;
  const k = `overlap=${overlapped} A=${sig(ja)} B=${sig(jb)} rows=${rows} ${verdict}`;
  tally.set(k, (tally.get(k) ?? 0) + 1);
}
console.log(`runs=${N}`);
for (const [k, v] of [...tally].sort((x, y) => y[1] - x[1] || (x[0] < y[0] ? -1 : 1))) {
  console.log(`${String(v).padStart(3)}x ${k}`);
}
NODE
}

# Render a raw capture into the comparable form. Id aliases are assigned in
# order of first appearance over the whole file; the probe creates every row
# before it reads any, so the map is fixed by the creation rows and a later
# ordering divergence cannot renumber it.
render() { # <rawfile> <outfile>
  local sedf="$WORK/alias.sed.$$" id n=0
  : > "$sedf"
  while read -r id; do
    n=$((n+1))
    printf 's|%s|<ID%d>|g\n' "$id" "$n" >> "$sedf"
  done < <(grep -oE '(user|todo)_[0-9A-Za-z]+' "$1" | awk '!seen[$0]++')
  sed -E \
    -e 's/"(created_at|updated_at|deleted_at)":[0-9]+/"\1":<TS>/g' \
    -e 's/"request_id":"[^"]*"/"request_id":"<RID>"/g' \
    -e 's/"continueCursor":"[^"]*"/"continueCursor":"<B64>"/g' \
    "$1" | sed -f "$sedf" > "$2"
  rm -f "$sedf"
}

# --- 1. real build ---------------------------------------------------------
echo ""
echo "--- 1. build ---"
( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -30 "$WORK/build.log"; exit 1; }

d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
# The descriptor is what installs the typed collections on `env.db` at
# deployed boot. Without it every handler hits `undefined.find` -- the exact
# failure db-todos shipped with before generated/zeroship was committed.
grep -q '"runtime_descriptor"' "$d/manifest.json" \
  && pass "manifest carries runtime_descriptor" \
  || fail "manifest has no runtime_descriptor (run gen-types / commit generated/zeroship)"
# Every RPC resource must declare an auth posture. Without one it resolves to
# `auth: "user"` and the gateway refuses it: green in dev, 401 deployed. That
# is how kv-dashboard, auth-uploads-kv AND db-todos shipped (#163).
total=$(grep -oE '"rpc:[^"]+":' "$d/manifest.json" | wc -l)
authed=$(grep -oE '"rpc:[^"]+":\{[^}]*"auth":' "$d/manifest.json" | wc -l)
[ "$total" -gt 0 ] && [ "$authed" -eq "$total" ] \
  && pass "all $total rpc resources declare an auth posture" \
  || fail "only $authed of $total rpc resources declare auth (missing src/server/config.ts?)"

# --- 2. dev side -----------------------------------------------------------
echo ""
echo "--- 2. dev (pnpm dev, SQLite) ---"
# A PRIVATE state dir. The operator's own dev server holds an exclusive lock
# on .zeroship/kv.redb and is never reaped (#221); sharing the directory makes
# this harness fight it for the lock and fail with "Database already open".
DEVSTATE="$WORK/devstate"; mkdir -p "$DEVSTATE"
DEV_DBURL="sqlite:$DEVSTATE/dev.sqlite"
for _p in "$DEV_PORT" "$VITE_PORT"; do
  lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

# MIGRATE FIRST, as its own step. Since ee2c352aa `pnpm dev` applies nothing --
# it opens the app database READONLY and names the command that fixes it. This
# harness kept the pre-split assertion ("dev applied its migrations ahead of the
# runtime", grepped out of the DEV-SERVER log) and so went red the moment it ran
# against the new contract, on a private state dir that is empty by construction.
# Measured 2026-08-10 at HEAD, before this block existed:
#     FAIL dev did not apply migrations (#176 regression?)
#     FAIL dev server never answered
#     seedA {"message":"internal error","name":"Error","request_id":"30"}
#   with the runtime logging `db: no such table: default.todos` 30 times.
# golden_path.sh took the same repair at line ~677; the reasoning there applies
# verbatim here, including WHY the dist path and not `pnpm migrate`: the
# `zeroship-dev-migrate` bin is only symlinked by an install that post-dates
# ee2c352aa, so an older node_modules answers "command not found".
#
# DATABASE_URL is passed EXPLICITLY and identically to both processes. The
# resolution helper (sdks/vite-plugin/src/dev-database-url.ts) puts the shell
# ahead of `.env` and the dev default, so passing it here is what guarantees the
# apply writes the file the runtime later opens -- applying to some other file is
# a silent failure that looks exactly like success.
MIGRATE_CLI="$ROOT/sdks/vite-plugin/dist/cli/migrate-dev.js"
if [ ! -f "$MIGRATE_CLI" ]; then
  fail "dev-migrate CLI missing at $MIGRATE_CLI (run pnpm build)"
else
  if ( cd "$APP" && DATABASE_URL="$DEV_DBURL" node "$MIGRATE_CLI" ) > "$WORK/dev-migrate.log" 2>&1; then
    pass "dev migrations applied ahead of the runtime ($(grep -oE 'applied=[0-9]+ skipped=[0-9]+' "$WORK/dev-migrate.log" | tail -1))"
  else
    fail "zeroship-dev-migrate failed: $(tail -3 "$WORK/dev-migrate.log" | tr '\n' ' ')"
  fi
fi
# `applied=0 skipped=0` on a fresh database means nothing ran -- a failure
# wearing a success's clothes (#176). The CLI already exits non-zero on exactly
# that, so the verdict above carries it; the count is echoed so a reader can see
# WHICH it was rather than trusting the exit status alone.
(
  cd "$APP" &&
  DB_TODOS_API_PORT="$DEV_PORT" \
  DATABASE_URL="$DEV_DBURL" \
  ZEROSHIP_KV_PATH="$DEVSTATE/kv.redb" \
  ZEROSHIP_WORKFLOW_SQLITE_PATH="$DEVSTATE/workflows.sqlite" \
  ZEROSHIP_WORKER_STORAGE_URL="file://$DEVSTATE/storage" \
  ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1
) & PIDS+=($!)
# Readiness: a deadline plus a log-derived diagnosis, not a fixed 30 x 2s count
# sized on an idle machine (#273). Sourced HERE and not at the top: e2e_stack.sh
# opens with `: "${ZEROSHIP_CONTROL_PORT:=9120}"` and four more of that shape, which only
# assign when unset, so sourcing it above this harness's own port block would
# hand it the library's ports.
#
# The probe is unchanged and is READ-ONLY -- todos.count for a user id that
# cannot exist -- so calling it while waiting cannot disturb the seeded rows the
# assertions below count.
# shellcheck source=/dev/null
source "$ROOT/tests/lib/e2e_stack.sh"
_dev_ping() {
  curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/todos.count" \
    -d '{"json":{"userId":"user_doesNotExist0000000"}}'
}
stack_wait_dev "dev server" "$WORK/dev.log" _dev_ping || true
RAWFILE="$WORK/dev.raw"; : > "$RAWFILE"
probe "http://localhost:$DEV_PORT"
grep -q '^seedA .*"id":"user_' "$WORK/dev.raw" && pass "dev server answered the probe" \
  || { fail "dev server never answered"; tail -25 "$WORK/dev.log"; head -3 "$WORK/dev.raw"; exit 1; }

# --- 2b. dev: the cross-REQUEST race ---------------------------------------
race "http://localhost:$DEV_PORT" "$WORK/dev.race"
grep -q '^runs=[1-9]' "$WORK/dev.race" \
  && pass "dev ran the concurrent-writer race ($(head -1 "$WORK/dev.race"))" \
  || { fail "dev race produced no runs"; head -5 "$WORK/dev.race" | sed 's/^/    /'; }

# --- 2c. dev: the transaction-scope probes (destructive; see scope_probe) ---
RAWFILE="$WORK/dev.raw"
scope_probe "http://localhost:$DEV_PORT"

# Rendered AFTER the scope probes so their rows reach the section-5 diff.
render "$WORK/dev.raw" "$WORK/dev.txt"

# --- 3. deployed side ------------------------------------------------------
echo ""
echo "--- 3. deployed (gateway -> worker -> PostgreSQL) ---"
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT $ZEROSHIP_MIGRATE_SERVER_PORT; do
  lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
docker rm -f "$PGC" >/dev/null 2>&1 || true
# `log_statement=all` is what makes the transaction-isolation divergence
# VISIBLE. `env.db` exposes no way to read `transaction_isolation` (there is no
# raw-SQL escape, by design), so the isolation clause is invisible in every
# response body -- the probes below prove the two tiers ANSWER the same, which
# is a different claim from "they ran the same SQL". The Postgres statement log
# is the only place in this harness where the actual `BEGIN ...` text can be
# read. There is no SQLite counterpart, so the dev half of that comparison
# stays a source-reading claim (transaction/mod.rs:384-391), and section 4 says
# so where it asserts on this log.
docker run --name "$PGC" -d -p "$PG_PORT:5432" -e POSTGRES_PASSWORD=zeroship \
  -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship postgres:16 \
  -c max_connections=200 -c log_statement=all >/dev/null \
  || { fail "docker run postgres"; exit 1; }
# 90, not 40: measured 2026-08-10, a cold `postgres:16` first-boot is ~2s idle
# but blew past 40s on a run that had a vite dev server and a 16-pair
# concurrency race competing for the same disk. A too-short wait here reports
# "Postgres never became ready" -- an infrastructure timeout wearing a platform
# failure's clothes.
PG_T0=$(date +%s)
# The predicate is `psql -d zeroship 'select 1'` THREE TIMES RUNNING, not a
# single pg_isready -- and that is not a preference, it is measured (#274).
#
# The postgres entrypoint starts a TEMPORARY server to run its init, then stops
# it and starts the real one. pg_isready answers yes to the temporary server.
# Sampling both predicates against a fresh `postgres:16` started exactly as
# above, 2026-08-11, printing only transitions:
#
#     sample 1: pg_isready=no   psql=FAIL
#     sample 5: pg_isready=YES  psql=FAIL      <- temporary init server
#     sample 6: pg_isready=no   psql=FAIL      <- the restart window
#     sample 7: pg_isready=YES  psql=1         <- the real server
#
# So pg_isready is NOT monotonic: yes, no, yes. The old loop broke at sample 5
# and the confirming call landed at sample 6, which is how a 90 x 1s wait
# reported "Postgres never became ready after 1s". That is not hypothetical --
# it is what this harness did when run alongside two others on 2026-08-11, and
# the solo re-run passed the same leg with the same 1s timing, so a green here
# was luck rather than waiting.
#
# `psql -d zeroship 'select 1'` is monotonic across that whole timeline: it
# cannot succeed until CREATE DATABASE has completed on the real server. The
# 3-in-a-row requirement is belt-and-braces against a future entrypoint that
# restarts more than once, and it is copied deliberately from the shared
# tests/lib/e2e_stack.sh stack_pg_up, whose own comment records this same bug
# ("A single pg_isready let a run through that window and the migration step
# then died"). This harness starts its own container -- it needs
# max_connections=200 and log_statement=all -- so it could not just call that
# helper, but it has no business using a weaker gate than the one beside it.
PG_OK=0
for _ in $(seq 1 90); do
  if docker exec "$PGC" psql -U postgres -d zeroship -tAc 'select 1' >/dev/null 2>&1; then
    PG_OK=$((PG_OK + 1)); [ "$PG_OK" -ge 3 ] && break
  else
    PG_OK=0
  fi
  sleep 1
done
# Report the elapsed seconds either way. "never became ready" with no number
# cannot be told apart from "the wait was too short", and the first version of
# this leg spent two runs on exactly that ambiguity.
# The confirming call asserts the SAME predicate the loop waited on. Asserting
# pg_isready here while waiting on psql would re-open the gap by the back door:
# the wait would be right and the verdict would come from the weaker check.
docker exec "$PGC" psql -U postgres -d zeroship -tAc 'select 1' >/dev/null 2>&1 \
  && pass "ephemeral Postgres query-able on :$PG_PORT (ready in $(( $(date +%s) - PG_T0 ))s)" || {
    fail "Postgres never became ready after $(( $(date +%s) - PG_T0 ))s (container state: $(docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}}' "$PGC" 2>&1))"
    docker logs --tail 20 "$PGC" 2>&1 | sed 's/^/    /'
    exit 1
  }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }

zs_platform_migrate "$DBURL" \
  --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  && pass "platform migrations applied" \
  || { fail "platform migrations failed"; tail -25 "$WORK/migrate.log"; exit 1; }

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null
chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"

ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/sk.pem"
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/sk.pem" "$WORK" || exit 1
PIDS+=($E2E_PLATFORM_OP_PID)
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/bundles" \
  > "$WORK/control.log" 2>&1 & PIDS+=($!)
"$BIN/zeroship-migrate-server" --port "$ZEROSHIP_MIGRATE_SERVER_PORT" \
  --tmp-dir "$WORK/migrated-tmp" \
 > "$WORK/migrated.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 \
  && pass "control healthy" || { fail "control did not come up"; tail -30 "$WORK/control.log"; exit 1; }
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT/readyz" >/dev/null 2>&1 \
  && pass "zeroship-migrate-server healthy" || { fail "migrated did not come up"; tail -30 "$WORK/migrated.log"; exit 1; }

# The worker needs --db: without it the env.db namespace is absent BY DESIGN
# and every handler fails loudly, which would read as an app bug.
"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --blob-store "$WORK/bundles" --poll-interval 2 \
 > "$WORK/worker.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 \
  && pass "worker healthy" || { fail "worker did not come up"; tail -30 "$WORK/worker.log"; exit 1; }

"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" \
  --blob-store "$WORK/bundles" --broker-secret-file "$WORK/gate-secret" \
  --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 \
  && pass "gateway healthy" || { fail "gateway did not come up"; tail -30 "$WORK/gate.log"; exit 1; }

# --defer-deploy: this app's .zship carries a runtime schema descriptor, and
# control refuses to make such a deploy live until the migrations that produced
# that descriptor are applied. The migration service needs the app row first and
# will not create one, so the app is created here and activated after the apply.
OUT=$("$BIN/dev-provision" --db "$DBURL" --blob-store "$WORK/bundles" \
  --name "$APP_NAME" --zship "$ZSHIP" --defer-deploy 2>&1)
APP_ID=$(echo "$OUT" | awk -F= '$1 == "app_id" { print $2 }')
[ -n "$APP_ID" ] || { fail "provision: $OUT"; exit 1; }

# --- apply the creator's recorded migration IR through zeroship-migrate-server ---
# Same mechanism as tests/e2e_db_app_end_to_end.sh: the .zship carries the
# DESCRIPTOR only; migrations travel through the migration service, which is
# the real deployed path. A hand-rolled CREATE TABLE here would test nothing.
# The scope string is the action list the deleted permission_tokens policy
# carried, one scope per Cedar action: control turns `scope` into the token
# policy and intersects it with the owner's own authority.
SCOPE="apps:read apps:write apps:deploy billing:read billing:write"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','devdeploy-db-$CREATOR@zeroship.test'::citext,'DevDeploy DB',NOW());
SQL
seat_app_owner "$APP_ID" "$CREATOR" owner psql_exec
ADMIN_TOKEN="$(e2e_mint_platform_bearer "$CREATOR" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer" || { fail "bearer mint"; exit 1; }

# Database creation is an explicit lifecycle operation. Keep it separate from
# both dev-provision and migration apply so neither path can recreate the old
# deploy-implies-database coupling.
CREATE_CODE="$(curl -sS -o "$WORK/create-database-response.json" -w '%{http_code}' -X POST \
  "http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT/v1/databases/$APP_ID" \
  -H "Authorization: Bearer $ADMIN_TOKEN")"
if [[ "$CREATE_CODE" != 2?? ]]; then
  fail "database create failed (http=$CREATE_CODE): $(cat "$WORK/create-database-response.json")"
  tail -30 "$WORK/migrated.log"; exit 1
fi

# THE BUILD'S OWN APPLY BODY, not a second recording of the same sources. It
# carries `descriptor_sha256` - the hash of the `schema.runtime.json` the same
# `genArtifacts` call emitted, which is what the .zship's manifest is
# content-addressed by and what the activation below is checked against. A
# re-recording produces a body with no descriptor, and the activation would then
# be refused for a reason that has nothing to do with the app.
IR_BODY="$APP/generated/zeroship/migrations.ir.json"
[ -s "$IR_BODY" ] || { fail "the build left no $IR_BODY - run pnpm build in $APP"; exit 1; }
APPLY_CODE="$(curl -s -o "$WORK/apply-response.json" -w '%{http_code}' -X POST \
  "http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT/v1/apps/$APP_ID/migrations/apply" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-binary @"$IR_BODY")"
APPLIED="$(jget '.applied.length' < "$WORK/apply-response.json")"
SKIPPED="$(jget '.skipped.length' < "$WORK/apply-response.json")"
if [ "$APPLY_CODE" = "200" ] && [ -n "$APPLIED" ] && [ "$APPLIED" -ge 1 ] 2>/dev/null; then
  pass "zeroship-migrate-server applied app IR (applied=$APPLIED skipped=${SKIPPED:-0})"
else
  fail "migrated apply failed (http=$APPLY_CODE)"
  cat "$WORK/apply-response.json"; tail -30 "$WORK/migrated.log"; exit 1
fi

# NOW the deploy can go live. Same command, minus --defer-deploy: dev-provision
# reuses the app by name and re-ingests the same content-addressed blobs.
ACT=$("$BIN/dev-provision" --db "$DBURL" --blob-store "$WORK/bundles" \
  --name "$APP_NAME" --zship "$ZSHIP" 2>&1)
LIVE=$(psql_exec -tAc "select coalesce(deploy_hash,'') from zeroship.apps where id = '$APP_ID'" 2>/dev/null | tr -d '[:space:]')
# ONE pass for the pair, not two. The create-then-activate split replaced a
# single `dev-provision` call, and the floor at the bottom of this file is an
# exact measurement - so the assertion moved here rather than multiplying.
[ -n "$LIVE" ] && pass "deployed $APP_NAME ($APP_ID) once its migrations applied" \
  || { fail "activation refused after the apply: $ACT"; exit 1; }
sleep 6   # gateway route-sync poll

RAWFILE="$WORK/deployed.raw"; : > "$RAWFILE"
probe "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME"
grep -q '^seedA .*"id":"user_' "$WORK/deployed.raw" && pass "deployed app answered the probe" \
  || { fail "deployed app never answered"; head -4 "$WORK/deployed.raw"; tail -20 "$WORK/worker.log"; }

# --- 3b. deployed: the cross-REQUEST race -----------------------------------
race "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME" "$WORK/deployed.race"
grep -q '^runs=[1-9]' "$WORK/deployed.race" \
  && pass "deployed ran the concurrent-writer race ($(head -1 "$WORK/deployed.race"))" \
  || { fail "deployed race produced no runs"; head -5 "$WORK/deployed.race" | sed 's/^/    /'; }

# --- 3c. deployed: the transaction-scope probes (destructive; see scope_probe) ---
RAWFILE="$WORK/deployed.raw"
scope_probe "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME"

# Rendered AFTER the scope probes so their rows reach the section-5 diff.
render "$WORK/deployed.raw" "$WORK/deployed.txt"

# ---------------------------------------------------------------------------
# 4. ABSOLUTE verdicts on the DEPLOYED answers.
#
# The diff in section 5 answers "do the two tiers AGREE". It structurally
# cannot answer "is the answer RIGHT": if SQLite and Postgres were broken the
# same way -- ids without prefixes, a version that never increments, a delete
# that does not hide the row -- the two captures are byte-identical and the
# comparison reports success. That is not hypothetical; production shipped
# Error.stack to anonymous callers for weeks while the auth harness stayed
# green, because dev leaked the same stack.
#
# Everything below reads the RAW deployed capture and does not consult dev.
# Expectations come from the FIXTURE CONTRACT (examples/db-todos/src/index.ts
# and migrations/), never from a previous run's output.
# ---------------------------------------------------------------------------
echo ""
echo "--- 4. absolute verdicts on the DEPLOYED answers (dev not consulted) ---"
DR="$WORK/deployed.raw"
drow() { grep -m1 "^$1 " "$DR" | sed -E "s/^$1 +//"; }

# THE CONTROL. Without it, a run where the gateway 401'd every call would
# report a wall of green `reject` verdicts that all mean "never measured".
# A result envelope is `{"json":...}`; a refusal is a bare `{"message":...}`.
# 30 pre-transaction rows + 11 transaction rows (seedTx, txCommit, txRoll,
# txNest, four txIso*, txD9, txD10, txTotal) + 5 concurrency rows (cxSeed,
# cxPar, cxOvl, cxPlain, cxTotal) + 4 transaction-scope rows (bxSeed, txBranch,
# txOrphan, bxTotal). ALL TWENTY are result envelopes: every tx, concurrency
# and scope procedure catches its own failure and returns it as data,
# precisely so a rollback reads as a measured value rather than as a wire
# error. A concurrency probe that came back as a bare `{"message":...}` would
# mean the request itself died, and this control says so.
ROWS_WANT=50
rows_got=$(wc -l < "$DR")
rows_json=$(grep -c ' {"json":' "$DR")
# SEVEN rows are expected NOT to be result envelopes: `orphan` and `dupEmail`
# are errors by design, `p1cur`/`p2cur` are decoded cursors, and
# `tsrel`/`tsupd`/`tsres` are derived scalars.
ROWS_JSON_WANT=$((ROWS_WANT - 7))
if [ "$rows_got" -eq "$ROWS_WANT" ] && [ "$rows_json" -eq "$ROWS_JSON_WANT" ]; then
  pass "CONTROL: $rows_got deployed rows, $rows_json result envelopes (want $ROWS_WANT/$ROWS_JSON_WANT)"
  ABS_OK=1
else
  fail "CONTROL: $rows_got rows, $rows_json envelopes (want $ROWS_WANT/$ROWS_JSON_WANT) -- verdicts below are UNSAFE"
  grep -v ' {"json":' "$DR" | head -6 | sed 's/^/    /'
  ABS_OK=0
fi

want()   { local r; r="$(drow "$1")"; if printf '%s' "$r" | grep -qF "$3"; then pass "deployed $1: $2"; else fail "deployed $1: $2 -- got $(printf '%s' "$r" | cut -c1-260)"; fi; }
reject() { local r; r="$(drow "$1")"; if printf '%s' "$r" | grep -qF "$3"; then fail "deployed $1: $2 -- got $(printf '%s' "$r" | cut -c1-260)"; else pass "deployed $1: $2"; fi; }
wantre() { local r; r="$(drow "$1")"; if printf '%s' "$r" | grep -qE "$3"; then pass "deployed $1: $2"; else fail "deployed $1: $2 -- got $(printf '%s' "$r" | cut -c1-260)"; fi; }

# typed_id: prefix + base62, per crates/zeroship-core/src/typed_id.rs.
wantre seedA 'user id is a prefixed base62 typed_id' '"id":"user_[0-9A-Za-z]{20,24}"'
wantre mkT1  'todo id is a prefixed base62 typed_id' '"id":"todo_[0-9A-Za-z]{20,24}"'

# The camelCase column survives the round trip with its case intact. Asserted
# on the RAW body, because the alias pass would rewrite the value but not the
# KEY -- and the key is the half that Postgres folds.
want mkT1 'the camelCase key userId is preserved'  '"userId":"user_'
reject mkT1 'no lower-cased userid leaked through' '"userid"'

# Declared defaults and declared columns.
want mkT1 'explicit priority round-trips'  '"priority":"low"'
want mkT2 'declared default priority applies' '"priority":"medium"'
want mkT2 'declared default done applies'     '"done":false'
want mkT2 'the json column round-trips as []' '"tags":[]'
want mkT2 'the title round-trips'             '"title":"walk dog"'

# The seven injected system columns, on a row the creator declared none of.
for f in '"created_at":' '"updated_at":' '"created_by":' '"updated_by":' '"version":' '"deleted_at":'; do
  want mkT2 "system column ${f%:} is present" "$f"
done
want mkT2 'a fresh row starts at version 1' '"version":1'
# Epoch MILLIS, not seconds and not an ISO string. This is the magnitude the
# scrub in section 5 blanks.
wantre mkT2 'created_at is a 13-digit epoch-millis number' '"created_at":1[0-9]{12}[,}]'

# The migration-declared foreign key actually refuses the orphan. TWO
# independent verdicts, and they answer different questions:
#
#   the ROW COUNT (`reject` + `orphanN`) is the INTEGRITY verdict -- did the
#   write land. It is deliberately independent of how the failure is worded,
#   so it survives any future rewording of the error.
#
#   the CODE is the CONTRACT verdict -- can a creator branch on the failure.
#
# The comment that stood here said the code was "deliberately NOT asserted"
# because a FK violation reached the caller as an opaque
# {"message":"internal error"} with no code (#231). That was true until
# 2cb3d9b81, which found the cause: both allow-lists in the dispatch
# sanitization rail named the NATIVE spelling `fk_violation`, while
# `@zeroship/db` re-stamps it to `FOREIGN_KEY_VIOLATION` via
# `canonicalErrorCode` INSIDE the isolate, before the throw reaches the rail.
#
# 2cb3d9b81 measured the fix on the DEV TIER ONLY. Whether the deployed tier
# (gateway -> worker -> isolate) delivers the same code was unmeasured until
# now. Measured here 2026-08-10, raw captures, one run, both tiers:
#
#   dev       {"message":"internal error","name":"Error",
#              "code":"FOREIGN_KEY_VIOLATION","request_id":"10"}
#   deployed  {"message":"internal error","name":"Error",
#              "code":"FOREIGN_KEY_VIOLATION","request_id":"5"}
#
# They agree, so the assertion below is absolute rather than a known-divergence
# note. The DISAGREEMENT half needs nothing added: `render` scrubs only
# created_at/updated_at/deleted_at, request_id, continueCursor and the minted
# ids, so `code` reaches the section-5 diff verbatim and a tier that dropped or
# renamed it shows up there as a divergence row. A second cross-tier assertion
# here would be a duplicate of that diff, not extra coverage.
#
# What the code verdict does NOT catch: that the code is CORRECT for the
# constraint that fired. Every constraint class in `is_code_only_public_error`
# is preserved by the same arm, so a rail that answered UNIQUE_VIOLATION for
# an FK violation would still pass this line -- `dupEmail` below is a
# different row, not a discriminating control for this one.
reject orphan 'the orphan insert did not succeed' '"json":{"id":"todo_'
want   orphan 'the FK violation carries its canonical code' '"code":"FOREIGN_KEY_VIOLATION"'
want   orphanN 'no orphan row exists for the dangling FK' '{"json":0}'
want   dupEmail 'the duplicate email is refused' '"message"'
# THE ONE-VARIABLE PARTNER to the FK code assertion above. That one proves a
# constraint code SURVIVES the deployed rail; it cannot prove the codes
# DISCRIMINATE, because every class in `is_code_only_public_error` rides the
# same `matches!` arm -- a rail that collapsed them all to a single code would
# keep it green. This is the pair: two different constraints must carry two
# DIFFERENT codes, so a collapse turns one of them red.
#
# The row above asserts only '"message"', which is the narrowest projection
# available on a body that was already carrying the code. Measured on my own
# run before this line was written:
#   dupEmail {"message":"internal error","name":"Error",
#             "code":"UNIQUE_VIOLATION","request_id":"6"}
want   dupEmail 'the unique violation carries its OWN distinct code' '"code":"UNIQUE_VIOLATION"'

# The relation eager-load replaces the bare FK with the joined row.
wantre withUser 'userId carries the joined user object' '"userId":\{[^}]*"email":'
want   withUser 'the joined row is the seeded user' "alice-$RUN@probe.test"

# Ordering under an EXPLICIT sort({id:-1}): ids are UUIDv7-derived and
# monotonic, so alice's LAST-created todo must come first. `bob task` was
# created after `ship it`, so a row-order verdict that ignored the userId
# filter would also be caught here.
#
# Asserted on the FIRST `"title"` in the row rather than by matching a regex
# across the whole first object: insert responses come back with their keys in
# ALPHABETICAL order and find responses in DESCRIPTOR order (measured), so a
# positional regex would encode one tier's key order as if it were the
# contract.
first_title="$(drow list | grep -oE '"title":"[^"]*"' | head -1 | cut -d'"' -f4)"
if [ "$first_title" = "pay bills" ]; then
  pass "deployed list: sort({id:-1}) puts alice's newest todo first"
else
  fail "deployed list: sort({id:-1}) puts alice's newest todo first -- got '${first_title:-<none>}'"
fi
reject list 'the userId filter excludes bobs todo' '"title":"bob task"'

# Pagination: 5 alice rows at numItems=2 is a genuine 2 / 2 / 1 walk.
want p1 'page 1 is not the last page' '"isDone":false'
want p1 'page 1 carries a cursor'     '"continueCursor":"'
want p2 'page 2 is not the last page' '"isDone":false'
want p3 'the third page ends the walk' '"isDone":true'
wantre p1cur 'the cursor binds the orderBy it was minted under' '"orderBy":\{"id":1\}'

# COMPLETENESS, which the four assertions above do not test.
#
# They check that the cursor advances and that the walk terminates. A paginator
# that silently dropped a row would satisfy every one of them: page 1 would
# still not be last, page 3 would still be `isDone`, and the cursor would still
# carry its orderBy. The pages are a rich artifact and those assertions read a
# narrow projection of it.
#
# That is not hypothetical here. Before typed-id DDL pinned byte ordering,
# SQLite (BINARY) and Postgres (en_US.utf8) ordered ids differently; see
# #236/#255 and the ordering rows this harness caught. Keyset pagination filters
# on the SAME column it sorts by. Measured on a throwaway Postgres 16 with real
# minted ids: with the filter and the sort under the same collation the walk is
# complete (6 rows, 6 emitted, 0 missing); with the filter forced to a different
# collation than the sort, exactly one row VANISHES from the walk and every
# isDone/cursor assertion above still passes.
#
# So: the three pages together must contain alice's five rows, each exactly
# once. Compared as SETS against the `list` probe, not by position -- `list` is
# sorted DESC and the pages ASC, so a positional check would encode one tier's
# ordering as the contract and fail for the wrong reason.
pg_ids="$(for p in p1 p2 p3; do drow "$p"; done \
          | grep -oE '"id":"todo_[^"]*"' | cut -d'"' -f4 | sort)"
list_ids="$(drow list | grep -oE '"id":"todo_[^"]*"' | cut -d'"' -f4 | sort)"
pg_n="$(printf '%s\n' "$pg_ids" | grep -c .)"
pg_u="$(printf '%s\n' "$pg_ids" | sort -u | grep -c .)"
list_n="$(printf '%s\n' "$list_ids" | grep -c .)"

if [ "$pg_n" -eq 5 ]; then
  pass "deployed pagination: the walk emitted 5 rows across 3 pages"
else
  fail "deployed pagination: the walk emitted $pg_n rows, want 5"
fi
if [ "$pg_n" -eq "$pg_u" ]; then
  pass "deployed pagination: no row appears on two pages"
else
  fail "deployed pagination: $(( pg_n - pg_u )) duplicate row(s) across pages"
fi
# The set equality is the load-bearing one: it fails if the walk loses a row
# EVEN IF the count still reaches 5, which is what a skip-plus-duplicate would
# look like.
if [ "$pg_ids" = "$list_ids" ] && [ "$list_n" -eq 5 ]; then
  pass "deployed pagination: the pages are exactly the rows list returns"
else
  fail "deployed pagination: pages != list ($pg_u distinct paged, $list_n listed)"
fi

# System columns under mutation, and the delete.
want setDone 'an update bumps version to 2'  '"version":2'
want archive 'a second update bumps to 3'    '"version":3'
want tsupd   'an update moved updated_at past created_at' 'moved=true'
# Deployed Postgres keeps millisecond resolution, so six inserts ~20ms apart get
# six distinct `created_at` values. Asserted absolutely because the RELATIVE
# diff cannot say which tier is right, only that they differ -- and here they
# do differ (dev collapses all six).
want tsres   'six back-to-back inserts get six distinct created_at' 'distinct_created_at=6 of 6'
want getDel  'the deleted row is no longer readable' '{"json":null}'
reject listAfter 'the deleted row is gone from the list' '"title":"buy milk"'
want countA 'count sees the five todos created for alice' '{"json":5}'

# --- transactions, absolute --------------------------------------------------
# Expectations come from the CONTRACT (crates/zeroship-data-orm/src/transaction/mod.rs
# and examples/db-todos/src/index.ts), not from a previous run.
#
# COMMIT. Two inserts, both visible on the tx connection before COMMIT, both
# present afterwards through the ordinary pool path.
want txCommit 'the transaction committed (no error)'          '"error":null'
want txCommit 'a read inside the open tx sees its own write'  '"seenTitle":"c'
want txCommit 'the in-tx count sees both uncommitted rows'    '"inTxCount":2'
want txCommit 'both rows survive the commit'                  '"committedCount":2'
want txCommit 'a declared default applies inside a tx'        '"bPriority":"high"'

# ROLLBACK. The insert succeeded and was visible inside the tx; the throw must
# undo it. `visibleAfter` is the half a broken rollback cannot fake.
want txRoll 'the row was visible inside the tx before the throw' '"inTxCount":1'
want txRoll "the caller receives the creator's own error code"   '"code":"PROBE_ROLLBACK"'
want txRoll "the caller receives the creator's own message"      '"message":"probe rollback"'
want txRoll 'the transaction returned no data'                   '"data":null'
want txRoll 'the rolled-back row is not in the table'            '"countAfter":0'
want txRoll 'the rolled-back row is not readable'                '"visibleAfter":0'

# NESTED = SAVEPOINT. The inner throw rolls back to the savepoint only.
want txNest 'the outer transaction committed'                  '"error":null'
want txNest "the inner failure surfaces as the inner's own code" '"code":"PROBE_INNER"'
want txNest 'the outer row is still visible after the savepoint rollback' '"outerSeen":1'
want txNest 'the inner row is gone from inside the outer tx'   '"innerSeen":0'
want txNest 'the outer row committed'                          '"outerAfter":1'
want txNest 'the inner row never committed'                    '"innerAfter":0'

# ISOLATION LEVEL. On the deployed (Postgres) tier a valid level is emitted as
# `BEGIN ISOLATION LEVEL ...` and must simply work. The INTERESTING half is
# the relative diff in section 5 -- these rows only pin that a valid level is
# not silently swallowed and an invalid one is refused.
want txIsoNone 'no isolationLevel: the tx commits' '"countAfter":1'
want txIsoSer  'isolationLevel serializable commits'   '"error":null'
want txIsoSer  'the serializable tx wrote its row'     '"countAfter":1'
want txIsoRR   'isolationLevel repeatableRead commits' '"countAfter":1'
# The unknown level is refused before any SQL. It arrives as a rejected Result
# (`error`), NOT as a thrown exception -- transactionImpl catches the native
# TypeError. `threw:null` is the load-bearing half of this pair.
want txIsoBad 'an unknown isolationLevel is refused'          'unknown isolationLevel'
want txIsoBad 'the refusal arrives as error, not as a throw'  '"threw":null'
want txIsoBad 'the refused transaction wrote nothing'         '"countAfter":0'

# SAVEPOINT DEPTH. MAX_SAVEPOINT_DEPTH = 8: BEGIN + 8 SAVEPOINTs is legal.
want txD9  'nine levels open (BEGIN + 8 savepoints)' '"deepestLevel":9'
want txD9  'nine levels is not refused'              '"refusedAtLevel":null'
want txD9  'the innermost write committed'           '"countAfter":1'
want txD10 'the tenth level is refused'              '"refusedAtLevel":10'
want txD10 'refused with savepoint_depth_exceeded'   '"code":"savepoint_depth_exceeded"'
want txD10 'the refused nest wrote nothing'          '"countAfter":0'

# THE INDEPENDENT TALLY. 2 (txCommit) + 0 (txRoll) + 1 (txNest outer) + 3
# (three committing isolation probes) + 0 (txIsoBad) + 1 (txD9) + 0 (txD10).
# Read through todos.count, which none of the tx procedures can influence.
want txTotal 'exactly seven tx-probe rows committed in total' '{"json":7}'

# --- CONCURRENT transactions, absolute --------------------------------------
# The contract these assert is the CREATOR's, taken from docs/reference/db.md
# and docs/reference/api-design-guidelines.md, not from the current
# implementation: `db.transaction(fn)` is an independent unit of work, and a
# transaction that resolves has committed. Nothing in the creator-facing
# surface says a transaction opened while another happens to be in flight for
# the same app stops being its own transaction.
#
# These verdicts were RED when first written (2026-08-10) -- see #244. They are
# the measurement of the gap docs/pilot/e2e-scenarios.md row 3 named.
want cxPar 'two transactions in one Promise.all: leg 1 commits' '{"n":1,"error":null'
want cxPar 'two transactions in one Promise.all: leg 2 commits' '{"n":2,"error":null'
want cxPar 'both concurrent transactions left their row'        '"countAfter":2'
# THE LOAD-BEARING PAIR. B reported success; B's row must exist. A aborted;
# A's row must not. If `bAfter` is 0 while B's `error` is null, one request's
# ROLLBACK destroyed another's committed write.
want cxOvl 'the aborting transaction rolled its own row back' '"aAfter":0'
want cxOvl 'the overlapping transaction reported success'     '"b":{"error":null'
want cxOvl "the overlapping transaction's committed row survived the other's rollback" '"bAfter":1'
# THE DEFAULT PATH. Leg B is a plain insert, not a transaction. It must be
# unaffected by a transaction that merely overlaps it.
#
# RED from 2026-08-10 until #254 was fixed the same day: `exec.rs` routed
# ordinary CRUD onto the open tx connection whenever `has_tx_for(app_id)` was
# true, with no notion of WHOSE transaction it was, so an unrelated write
# joined a stranger's transaction and died with its rollback (`bAfter:0`,
# `cxTotal` 3). Same root cause as the `cxOvl` defect -- ambient state standing
# in for call context -- in a different consumer. The fix captures the async
# scope at each CRUD dispatch site (crates/zeroship-data-orm/src/tx_route.rs).
# Do NOT relax these -- see docs/pilot/e2e-scenarios.md row 3.
#
# DEPLOYED ONLY, and that is the point of section 5: the dev tier still answers
# `bAfter:0`. Routing is now correct on both tiers, but on SQLite there is
# nowhere else to route TO -- `acquire_dedicated_client` returns a handle to
# the SAME single writer connection the shared/autocommit path uses
# (crates/zeroship-data-orm/src/backend/sqlite/mod.rs:514), so an ordinary write still
# physically executes inside whatever transaction that connection is holding.
# Root-caused, NOT worked around: closing it needs a second SQLite connection
# (or a claim-wait, which can deadlock when a transaction awaits a promise
# created before it opened). Left as a measured divergence.
want cxPlain 'the aborting transaction rolled its own row back'          '"aAfter":0'
want cxPlain 'the ordinary overlapping write reported success'           '"inserted":true'
want cxPlain "an ordinary write is not undone by a stranger's rollback"  '"bAfter":1'
want cxTotal 'exactly four concurrency-probe rows committed' '{"json":4}'

# --- the CONVERSE: an op inside a transaction still reaches it --------------
# A routing fix that severed in-transaction ops from their transaction would
# leave every verdict above green (they only assert that unrelated work is
# left alone) while quietly breaking transactions altogether. These are the
# other half of the pair.
#
# txBranch: two writes issued on PARALLEL continuations inside one callback,
# then the callback throws.
#
# `TRANSACTION_CONNECTION_BUSY` is not a disappointment here, it is THE PROOF.
# A transaction owns one connection, so its ops cannot overlap; the branch that
# loses the race is refused. That refusal can only happen if the branch was
# routed to the transaction in the first place -- a branch that had fallen
# through to the pool would have succeeded and autocommitted. So this verdict
# is what establishes that a continuation forked inside the callback still
# carries the transaction scope. Measured on both tiers, 2026-08-10.
want txBranch 'a branch write reached the transaction, not the pool' '"code":"TRANSACTION_CONNECTION_BUSY"'
# And nothing the branches wrote may survive the abort. This one ALSO covers
# the settle path: `exec_settle_top_level` treats an already-drained slot as
# "settled" and issues no ROLLBACK, so a branch still holding the client at
# abort time can leave its row behind. That is what dev answers today
# (`countAfter":1`, and the NEXT transaction then fails to BEGIN) -- see the
# divergence in section 5.
want txBranch 'writes on parallel branches left nothing behind' '"countAfter":0'
# txOrphan: a write dispatched inside the callback but settling AFTER the
# transaction committed. There is no correct connection for it; the outcome
# that must not happen is a silent success. The in-tx row still commits.
# CONTROL first: without it, an orphan promise that never ran would leave
# orphanAfter at 0 and read as three confident greens.
want txOrphan 'CONTROL: the orphaned write was actually started'  '"orphanStarted":1'
want txOrphan 'the transaction itself committed'                     '"error":null'
want txOrphan "the transaction's own row committed"                  '"txAfter":1'
want txOrphan 'the orphaned write did NOT silently commit'           '"orphanAfter":0'
# SCREAMING_CASE, not the native `transaction_scope_expired`: every native code
# that reaches a creator through a COLLECTION op is re-stamped by
# `canonicalErrorCode` (sdks/db/src/errors.ts:27) before the throw leaves the
# isolate. Codes raised by `db.transaction()` itself are not (`txD10` above
# asserts `savepoint_depth_exceeded` in native spelling) -- so the two halves of
# the transaction surface hand creators two different casings. Asserted in the
# spelling that actually arrives, with the inconsistency named rather than
# smoothed over.
want txOrphan 'the orphaned write was refused as an expired scope'   '"code":"TRANSACTION_SCOPE_EXPIRED"'
want bxTotal 'exactly one transaction-scope row committed' '{"json":1}'

# --- the isolation clause and the savepoints, read off the Postgres log ------
# Everything above measures RESPONSES, and the responses cannot see an
# isolation level: no `env.db` call reads `transaction_isolation`, and the two
# tiers answer identically for every level. Without this block the leg would
# report "dev and deployed agree on transactions" while leaving the one
# documented transaction divergence completely unprobed.
#
# What this establishes: the DEPLOYED tier really emits the clause, and really
# opens/releases savepoints rather than flattening nested transactions.
# What it does NOT establish: that dev omits the clause. SQLite has no
# statement log here, so that half is read from
# crates/zeroship-data-orm/src/transaction/mod.rs:384-391 (`let _ =
# build_begin_sql(...)` then a literal `"BEGIN"`), not measured.
PGLOG="$WORK/pg-statements.log"
docker logs "$PGC" > "$PGLOG" 2>&1
pgwant() { # <label> <needle> <min-count>
  local n; n=$(grep -cF "$2" "$PGLOG")
  if [ "$n" -ge "$3" ]; then pass "pg log: $1 (x$n)"; else fail "pg log: $1 -- found $n of $2, want >= $3"; fi
}
# THE MEASUREMENT the divergence row in docs/reference/sqlite-divergences.md
# never had. One serializable probe, one repeatable-read probe.
pgwant 'BEGIN ISOLATION LEVEL SERIALIZABLE was emitted'  'BEGIN ISOLATION LEVEL SERIALIZABLE' 1
pgwant 'BEGIN ISOLATION LEVEL REPEATABLE READ was emitted' 'BEGIN ISOLATION LEVEL REPEATABLE READ' 1
# A nested transaction is a SAVEPOINT on the same connection, and an inner
# throw is a partial rollback -- not a full ROLLBACK of the enclosing tx.
pgwant 'a nested transaction opened SAVEPOINT zs_sp_1'   'SAVEPOINT zs_sp_1' 1
pgwant 'an inner throw rolled back to the savepoint'     'ROLLBACK TO SAVEPOINT zs_sp_1' 1
pgwant 'the depth probe really opened eight savepoints'  'SAVEPOINT zs_sp_8' 1
# The CONTROL for this block: a needle that must NOT be there. Without it a
# grep against a truncated or empty log would report five confident greens.
n_absent=$(grep -cF 'BEGIN ISOLATION LEVEL SNAPSHOT' "$PGLOG")
if [ "$n_absent" -eq 0 ]; then
  pass "pg log CONTROL: the refused isolation level never reached Postgres"
else
  fail "pg log CONTROL: 'snapshot' reached Postgres $n_absent times -- validation is not pre-flight"
fi
# And the control that the log is non-empty in the first place. Matched on
# `LOG:` rather than `statement:` because a driver using the EXTENDED query
# protocol logs `execute <unnamed>: ...` instead; the needles above grep the
# SQL text itself for the same reason, so they survive either protocol.
pgwant 'CONTROL: the statement log is populated' 'LOG:' 50

[ "$ABS_OK" = "1" ] || echo "  (verdicts above are UNSAFE: the control failed)"

# ---------------------------------------------------------------------------
# 4b. THE CROSS-REQUEST RACE, on both tiers.
#
# Section 4's cxPar/cxOvl stage the contention inside ONE request, so they are
# scheduler-independent by construction. This block stages it across two
# requests, which is the shape a real end user produces and the shape the two
# tiers CANNOT match structurally: `pnpm dev` is `zeroship serve --workers=1`
# (one thread, one isolate, cooperative interleaving only), while the worker's
# isolate cache is `thread_local!` over `--worker-threads` threads, so two
# requests may or may not share an isolate.
#
# The verdicts are therefore INTEGRITY invariants that hold whatever the
# scheduler does, not an equality between the tiers:
#
#   overlap    at least one pair must actually have overlapped in wall-clock
#              time. Without this the run measured nothing and every green
#              below means "no race was staged".
#   lostCommit no leg reported success while its row is missing.
#   ghostCommit no leg reported failure while its row landed anyway.
#
# The full tallies are printed either way -- a divergence between the tiers is
# the finding, and a tally is what makes it readable.
# ---------------------------------------------------------------------------
echo ""
echo "--- 4b. cross-request concurrent writers (integrity, both tiers) ---"
for tier in dev deployed; do
  f="$WORK/$tier.race"
  [ -s "$f" ] || { fail "$tier race: no output"; continue; }
  echo "  [$tier] $(head -1 "$f")"
  tail -n +2 "$f" | sed 's/^/    /'
  n_over=$(grep -c 'overlap=true' "$f")
  if [ "$n_over" -ge 1 ]; then
    pass "$tier race CONTROL: $n_over classified outcomes actually overlapped"
  else
    fail "$tier race CONTROL: no pair overlapped -- the race was never staged, verdicts below mean nothing"
  fi
  n_lost=$(grep -c 'lostCommit' "$f")
  n_ghost=$(grep -c 'ghostCommit' "$f")
  [ "$n_lost" -eq 0 ] \
    && pass "$tier race: no leg reported success with its row missing" \
    || fail "$tier race: $n_lost outcome class(es) lost a committed row -- see the tally above"
  [ "$n_ghost" -eq 0 ] \
    && pass "$tier race: no leg reported failure with its row committed" \
    || fail "$tier race: $n_ghost outcome class(es) committed a row after reporting failure"
done

# --- 5. THE RELATIVE COMPARISON -------------------------------------------
#     Green here does NOT mean the platform is right; it means SQLite and
#     Postgres agree. Section 4 is the half that answers "right".
echo ""
echo "--- 5. relative comparison: identical operations, identical results? ---"
if diff -q "$WORK/dev.txt" "$WORK/deployed.txt" >/dev/null 2>&1; then
  pass "dev and deployed results are identical across every probed operation"
else
  fail "dev and deployed DIVERGE -- results below (< dev, > deployed)"
  # Count the divergent rows for the classifier at the bottom. Taken from the
  # SAME diff that is printed, so the number and the evidence cannot disagree.
  DIVERGENT_ROWS="$(diff "$WORK/dev.txt" "$WORK/deployed.txt" | grep -c '^<' || true)"
  # ...and the row NAMES, sorted and deduplicated, for the identity-based
  # classifier at the bottom. Same diff as the count and as the print, so all
  # three agree by construction.
  DIVERGENT_NAMES="$(diff "$WORK/dev.txt" "$WORK/deployed.txt" | grep '^<' \
    | awk '{print $2}' | sort -u | tr '\n' ' ')"
  diff "$WORK/dev.txt" "$WORK/deployed.txt" | cut -c1-400 | head -60
  echo ""
  # Repeat the staleness verdict HERE, not only at boot. Measured 2026-08-10:
  # this run reported three divergence rows, two of which were version skew
  # from a partial rebuild. The freshness check had said so correctly in its
  # first eight lines -- and those lines were 150 lines above the diff, so they
  # were not read, and the skew was re-derived by hand from binary mtimes.
  # A warning belongs where the reader is, and the reader is right here.
  zs_report_staleness_here
  echo "  A divergence here is the finding, not a flaky test. Both backends are"
  echo "  individually plausible; disagreeing on one contract is the defect."
  echo "  See docs/pilot/e2e-scenarios.md before weakening anything above."
fi

# --- The floor: a MEASURED minimum, and the guard against a green run over ---
#     nothing. This script used to exit on $FAIL alone, and $FAIL is 0 both when
#     every assertion passed and when NO assertion ran. The section-4 helpers are
#     the specific hazard: `want`/`reject`/`wantre` all read `drow "$label"`, so a
#     probe label renamed on one side alone makes the row EMPTY -- and `reject`
#     PASSES on an empty row, because the string it forbids is indeed not there.
#     A capture that went entirely missing therefore turns some verdicts green
#     rather than red. This repo has shipped three gates that passed over zero
#     tests (#102/#103/#112). Every sibling leg (kv/env/errors) has this guard;
#     the db leg was the one without it.
#
# THE FLOOR IS A MEASUREMENT. Taken 2026-08-10 on this tree, this script run
# unmodified with its own ephemeral Postgres:
#
#     dev vs deployed (env.db): 120 passed, 1 failed        (exit 1)
#
# The ONE failure is the section-5 divergence, and it is a standing, documented
# finding rather than a flake: SQLite's `tsres` timestamp resolution, plus the
# three dev-tier single-connection rows (`cxPlain`, `txBranch`, `txOrphan`)
# root-caused in docs/pilot/e2e-scenarios.md row 3 and recorded in
# sqlite-divergences.md. So the floor is 120 of 121 verdicts, not 121 of 121.
#
# CROSS-CHECKED against a second, independent instrument: counting CALL SITES in
# the source rather than outcomes in a run.
#
#     27 `pass` call sites outside the helper definitions,
#        of which 3 sit inside the `for tier in dev deployed` loop      27 + 3
#     81 `want`/`reject`/`wantre` invocations,
#        of which 1 sits inside the six-element system-column loop      81 + 5
#        (the recount must be INDENT-TOLERANT. That one loop invocation
#         at ~line 926 is indented; a column-anchored `grep -cE
#         '^(want|reject|wantre) '` returns 80 and looks like drift when
#         nothing has drifted. Re-derive with:
#           grep -v '^[[:space:]]*#' FILE | grep -cE '^[[:space:]]*(want|reject|wantre) '
#         Checked 2026-08-10: both spellings moved by exactly +1 when this
#         assertion was added, which is the property that matters -- the
#         ABSOLUTE values differ by convention, the DELTA does not.)
#      6 `pgwant` invocations                                                + 6
#                                                                        = 122
#
# 122 verdicts emitted, 121 of which pass. The two derivations agree, and they
# fail differently: the dynamic count moves when a tier stops answering or a
# capture is lost, the static one when an assertion leaves the file.
#
# NO HEADROOM, deliberately -- the total is fixed by the source, not discovered
# at run time, so adding an assertion passes untouched and removing one costs a
# deliberate edit here.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Swapping one assertion for an
# easier one keeps the total at 122. Nothing here can see that; review can.
#
# AND WHAT IT OVER-REPORTS, measured rather than predicted: with no headroom
# over a standing FAIL, any ADDITIONAL red also drops PASS below the floor, so
# the block below prints its "assertions do not vanish by accident" advice on a
# run where nothing vanished. Observed on the FK mutation runs (119 passed, 2
# failed). That is noise, not a wrong verdict -- $FAIL had already set exit 1 --
# and it is the price of the floor being a PASS count. A verdicts-EMITTED floor
# (PASS+FAIL) would read 122 either way and so would not notice the hazard this
# guard exists for: a lost capture leaves every verdict emitted, turning the
# `reject`s green and the `want`s red.
DB_MIN_PASSED=121

echo ""
echo "  dev vs deployed (env.db): $PASS passed, $FAIL failed  (floor $DB_MIN_PASSED)"
echo ""
echo "  --- raw deployed bodies (verbatim, truncated to 240 cols) ---"
cut -c1-240 "$WORK/deployed.raw" | sed 's/^/  /'

# --- The classifier: why this script is allowed to be red, and when it is not -
#
# WHY THIS EXISTS. Until 2026-08-12 this harness ran in NO workflow, and the
# reason was never written down. Measured that day: it scores 121 passed, 1
# failed at HEAD, and the single red is the dev-vs-deployed row diff over
# divergences the spine ALREADY documents:
#
#   cxPlain, cxTotal, txBranch, txOrphan   the transaction-context gap that
#                                          #250 and #254 record as open by design
#   tsres  distinct_created_at=2 of 6 dev vs 6 of 6 deployed -- a timestamp
#          granularity split of the same family as the #236 id-ordering one
#
# So the script exited 1 on a red nothing was going to fix, which is exactly why
# wiring it would have been the hollow-arm shape ci.yml refuses elsewhere. Its
# sibling e2e_dev_vs_deployed_auth.sh already had this treatment and IS wired;
# this brings db level, and the wiring is a separate change once it has run
# green here more than once.
#
# WHAT IT DOES NOT DO, in the same words its auth sibling uses: it counts rows,
# not identities. Five divergences that are a DIFFERENT five would still exit 0.
# Pinning the row set needs the per-row verdicts in docs/pilot/e2e-scenarios.md
# to become machine-readable, which they are not today. Stated so the exit code
# is not read as more than it is.
#
# DO NOT WIRE THIS INTO CI YET, and the reason is a measurement the classifier
# itself produced. Three consecutive runs of UNCHANGED code on this machine:
#
#   run 1   5 rows   cxPlain cxTotal tsres txBranch txOrphan
#   run 2   5 rows   cxPlain cxTotal tsres txBranch txOrphan
#   run 3   3 rows   cxPlain cxTotal tsres
#
# txBranch and txOrphan come and go. Their dev-side values are
# TRANSACTION_CONNECTION_BUSY and TRANSACTION_SCOPE_EXPIRED, so they are timing
# dependent, and any fixed expectation would report STALE EXPECTATION on some
# runs and pass on others -- a CI flake, arriving as good news ("rows were
# fixed") which is the most misleading shape available. This is the third
# instance of the host-sensitivity class ci.yml already documents for
# storage and workflows.
#
# THAT IS NOW DONE, by NAME rather than by count. The count-based version above
# is kept only as the fallback when the names are unavailable. Two lists:
#
#   REQUIRED   cxPlain cxTotal tsres    must ALL diverge. If one stops, that is
#                                       either a fix worth recording or a probe
#                                       that quietly stopped running.
#   TOLERATED  txBranch txOrphan        MAY diverge or not. These are the known
#                                       dev cross-request race, which scenario 3
#                                       already documents and explicitly judges
#                                       "on integrity invariants rather than tier
#                                       equality" - so their tier-equality result
#                                       is not a signal in either direction here.
#
# This is strictly stronger than the count it replaces: five divergences that
# are a DIFFERENT five now go RED, where the count let them pass. That closes
# the limitation the auth and login siblings still carry and still state.
#
# ATTRIBUTION, corrected 2026-08-12: I found the 5/5/3 instability by running
# this classifier's own arms and started writing it up as a new defect. It is
# not new - docs/pilot/e2e-scenarios.md scenario 3 names txBranch countAfter:1
# and txOrphan begin_failed, attributes them to a dev cross-request race, and
# warns that probe ORDER matters because the scope probes contaminate it. What
# was new was only that the harness could not tell that shape from a regression.
DB_REQUIRED_DIVERGENT="${DB_REQUIRED_DIVERGENT:-cxPlain cxTotal tsres}"
DB_TOLERATED_DIVERGENT="txBranch txOrphan"
DB_EXPECTED_DIVERGENT=5
DIVERGENT_ROWS="${DIVERGENT_ROWS:-0}"

rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$PASS" -lt "$DB_MIN_PASSED" ]; then
  echo "FAIL: only $PASS assertions passed, fewer than the $DB_MIN_PASSED this gate expects." >&2
  echo "      Assertions do not vanish by accident: either the deployed capture lost" >&2
  echo '      rows (in which case reject verdicts are passing on EMPTY rows and mean' >&2
  echo "      nothing) or an assertion was removed. If the removal was deliberate," >&2
  echo "      lower DB_MIN_PASSED in the same change and say why." >&2
  rc=1
fi

# Only the row diff may be forgiven, and only at the documented count. Anything
# else -- a second failure, a breached floor, a changed divergence count -- keeps
# the non-zero exit it already has.
if [ "$rc" -ne 0 ] && [ "$FAIL" -eq 1 ] && [ "$PASS" -ge "$DB_MIN_PASSED" ] \
   && [ -n "${DIVERGENT_NAMES:-}" ]; then
  # Identity comparison. Unexpected = diverged but named in neither list.
  # Missing = required but did not diverge.
  db_unexpected=""; db_missing=""
  for row in $DIVERGENT_NAMES; do
    case " $DB_REQUIRED_DIVERGENT $DB_TOLERATED_DIVERGENT " in
      *" $row "*) ;;
      *) db_unexpected="$db_unexpected $row" ;;
    esac
  done
  for row in $DB_REQUIRED_DIVERGENT; do
    case " $DIVERGENT_NAMES " in
      *" $row "*) ;;
      *) db_missing="$db_missing $row" ;;
    esac
  done
  if [ -z "$db_unexpected" ] && [ -z "$db_missing" ]; then
    echo "" >&2
    echo "CLASSIFIER: exit 0 on the documented red - every divergent row is a KNOWN one." >&2
    echo "  required, all present:$(printf ' %s' $DB_REQUIRED_DIVERGENT)" >&2
    echo "  tolerated (dev cross-request race, scenario 3):$(printf ' %s' $DB_TOLERATED_DIVERGENT)" >&2
    echo "  this run diverged on: $DIVERGENT_NAMES" >&2
    echo "  Row IDENTITIES are compared, not just the count, so a different set" >&2
    echo "  of the same size is RED." >&2
    rc=0
  else
    echo "" >&2
    [ -n "$db_unexpected" ] && {
      echo "CLASSIFIER: REGRESSION. Divergent rows nobody documented:$db_unexpected" >&2
      echo "  dev and deployed now disagree somewhere new. The diff above has them." >&2
    }
    [ -n "$db_missing" ] && {
      echo "CLASSIFIER: STALE EXPECTATION. Required rows that did NOT diverge:$db_missing" >&2
      echo "  Either they were FIXED - record which, and drop them from" >&2
      echo "  DB_REQUIRED_DIVERGENT - or the probe stopped running, which is not" >&2
      echo "  good news at all. Check which before believing the cheerful reading." >&2
    }
  fi
elif [ "$rc" -ne 0 ] && [ "$FAIL" -eq 1 ] && [ "$PASS" -ge "$DB_MIN_PASSED" ]; then
  # Fallback: names unavailable (no diff captured), so fall back to the count.
  if [ "$DIVERGENT_ROWS" -eq "$DB_EXPECTED_DIVERGENT" ]; then
    echo "" >&2
    echo "CLASSIFIER: exit 0 on the documented red - $DIVERGENT_ROWS divergent rows," >&2
    echo "  which is the KNOWN dev-vs-deployed env.db gap (transaction context per" >&2
    echo "  #250/#254, timestamp granularity per the #236 family), not a passing" >&2
    echo "  comparison. The diff above is the evidence; this only says the SHAPE" >&2
    echo "  has not changed." >&2
    rc=0
  elif [ "$DIVERGENT_ROWS" -gt "$DB_EXPECTED_DIVERGENT" ]; then
    echo "" >&2
    echo "CLASSIFIER: REGRESSION. $DIVERGENT_ROWS divergent rows, expected $DB_EXPECTED_DIVERGENT." >&2
    echo "  dev and deployed disagree on MORE of env.db than they did. The new rows" >&2
    echo "  are in the diff above; find them before changing this number." >&2
  else
    echo "" >&2
    echo "CLASSIFIER: STALE EXPECTATION. $DIVERGENT_ROWS divergent rows, expected $DB_EXPECTED_DIVERGENT." >&2
    echo "  Rows were FIXED and nobody updated the count. This is good news failing" >&2
    echo "  loudly on purpose: set DB_EXPECTED_DIVERGENT=$DIVERGENT_ROWS and record" >&2
    echo "  WHICH rows closed in docs/pilot/e2e-scenarios.md." >&2
  fi
fi
exit "$rc"
