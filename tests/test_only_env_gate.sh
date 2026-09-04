#!/usr/bin/env bash
# The test-only environment surface: no NEW ambient name, no DEAD entry, no new
# ambient EXPORT of a test-class name.
#
# THE OPERATOR RULE THIS ENFORCES: never introduce an environment variable in
# the tests. A test states its configuration as a typed value or as an argument;
# a test that needs a name to reach a BINARY gives it to that binary as a
# command prefix. What it must not do is take an input from its OWN process
# environment, because then the result depends on how the run was launched and
# is not reproducible from the invocation.
#
# ---------------------------------------------------------------------------
# WHY THIS IS NOT COVERED BY THE TWO CHECKS THAT ALREADY EXIST
# ---------------------------------------------------------------------------
#
# It is a fair question and it was asked before this was written, so the answer
# is recorded rather than assumed. The two existing checks are:
#
#   clippy.toml `disallowed-methods`   denies std::env::{var,var_os,vars,
#                                      vars_os,set_var,remove_var}
#   crates/core/tests/config_env_access_gate.rs
#                                      parses tracked Rust source for the same
#                                      calls plus an illicit #[allow]
#
# BOTH BIND TO THE SPELLING OF THE ACCESSOR, not to the population of names.
# `zeroship_core::test_env!("ANYTHING")` expands to `declared_env!(test, ...)`
# -> `read_declared_env!` -> the one exempt boundary in
# crates/core/src/config/env.rs, which carries the `#[allow]` those two checks
# require it to carry. So a test that adds a brand-new ambient read as
# `test_env!("MY_NEW_KNOB")` is GREEN under clippy and GREEN under the source
# gate. That is the whole hazard, and neither existing line sees it.
#
# Second gap, and it is total: both checks are Rust. The shell harnesses held
# 141 `export` sites across 38 files when this was written - the gate prints
# both numbers on every run, so read them there rather than trusting this line -
# and clippy and a syn-based Rust scanner see none of them.
# `export CONTROL_TEST_DB="$DSN"` at tests/run_billing_suite.sh:181
# puts a test-class name into the harness's own environment, where a cargo
# child inherits it invisibly - action at a distance, in the exact shape the
# rule forbids - and nothing in the tree could observe it before this file.
#
# So this gate is not a third copy of those two. It is the arm they do not
# have: they rule on HOW the environment is read, this rules on WHICH NAMES are
# read and on how those names are DELIVERED.
#
# ---------------------------------------------------------------------------
# WHAT IS PERMITTED, and the distinction is the point
# ---------------------------------------------------------------------------
#
#   AMBIENT (the defect)      `export NAME=v` in a harness; a human exporting
#                             NAME before invoking. The value reaches the child
#                             through the environment, invisibly, and nothing
#                             at the call site says so.
#
#   COMMAND PREFIX (fine)     `NAME=v cargo test ...` and Rust's
#                             `Command::new(..).env("NAME", "v")`. An explicit
#                             argument at the call site, scoped to one child.
#
# Both spellings put the same bytes in the same child's environment. This gate
# discriminates on the DELIVERY, which is what makes it bound to the hazard
# rather than to a spelling of a name: `ZEROSHIP_DW_E2E=1 \ cargo test` at
# tests/e2e_durable_workflows.sh:687 is green and must stay green, while
# `export CONTROL_TEST_DB=...` is not.
#
# ---------------------------------------------------------------------------
# WHAT THIS GATE DOES NOT DO, stated so nobody reads it as complete
# ---------------------------------------------------------------------------
#
# - It does not rule on `tests/**.sh` names that are NOT test-class. A harness
#   exporting ZEROSHIP_CONTROL_PORT to configure a server it starts is ambient
#   too, by the same argument. That is ~50 more names and it is production
#   config, not a test-only variable, so it is out of this gate's declared
#   subject. Recorded as a known gap, not as an acquittal.
#
# - IT DOES NOT RULE ON SHELL READS AT ALL - `${NAME:-default}` and its family.
#   This is the biggest gap and it is a measured decision, not an oversight.
#   The property wanted is "a name the harness takes from its environment", and
#   in shell that is not decidable by grep: the same `${NAME:-x}` spelling is
#   used both for a real knob and for defaulting a LOCAL. Discriminating on
#   "the file never assigns it" was built and measured against this tree on
#   2026-08-20 - it reported 142 names, and a hand read found the majority are
#   locals assigned through spellings a regex does not see (`local X=`,
#   `X=$(...)` in a nested scope, `while read X`, `for X in`, arrays). That is
#   the same result `tests/lib/env_name_census.sh` records having measured for
#   the same reason, and an inventory of 142 mostly-wrong entries is the stale
#   census this repo has already been bitten by four times.
#   The narrower decidable form was checked too: `${NAME:?}` hard-requires are
#   13 sites tree-wide, every one either a literal inside a rendered template
#   or a lib-internal handoff (`ZS_TEST_OVERLAY`, `TEST_DB`), none a name a
#   human must export. An arm over that set would rule on nothing, which
#   tests/lib/gate_arms.sh exists to forbid.
#   So the shell half this gate DOES rule on is delivery (`export`), which is
#   exactly decidable, and the read half is left uncovered and said so here.
# - It cannot see a name assembled at run time. Both read spellings it scans
#   take a literal, which is why they were chosen as the corpus.
# - It does not check that a name is USED sensibly, only that it is declared,
#   live, and delivered explicitly.
#
# Run the detector's own positive/control pairs: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init test_only_env

PASS=0
FAIL=0
RAN=0
pass() { PASS=$((PASS + 1)); RAN=$((RAN + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); RAN=$((RAN + 1)); echo "  FAIL $1"; }

# ---------------------------------------------------------------------------
# THE INVENTORY: every name first-party TEST code reads from its environment.
# ---------------------------------------------------------------------------
#
# This is the whole test-only surface, and it is a CLOSED set by construction:
# there are exactly two sanctioned spellings for a first-party test read, and
# both take a string literal, so both are enumerable from source.
#
#   1. `zeroship_core::test_env!("X")` / `test_env_os!("X")`  - class `test`,
#      consumer `TestHarness` (crates/core/src/config/declared.rs).
#   2. the sealed key enums at `libs/<crate>/tests/common/env.rs`, which the
#      publishable zeroship-independent driver crates use instead because they
#      cannot depend on zeroship-core.
#
# ADDING A NAME HERE IS THE REVIEW POINT. The gate refuses an extracted name
# that is not listed, so a new ambient read cannot land without editing this
# list - which is where somebody asks whether it should have been an argument.
#
# Arm 2 checks the reverse direction, and it is not ceremony: at the time this
# gate was written docs/reference/env-vars.md listed
# `ZEROSHIP_NET_TEST_DNS_HANG_HOST` and `ZEROSHIP_NET_TEST_DNS_HANG_MS` under
# "The surviving test-only names". Nothing in the tree read either one. They
# were deleted in the same commit as this file.
#
# IT CAUGHT A THIRD ONE, and only on 2026-09-04, because THIS GATE HAD NEVER RUN
# IN CI. `ZEROSHIP_SESSION_SECRET` sat here with no reader anywhere in the tree:
# the SQLite session minter it configured went with the HMAC session anchor the
# operator deleted on 2026-08-27 (AGENTS.md, "Privilege follows the PROCESS").
# Arm 2 had been red the whole time and nothing was looking. The entry is gone,
# with `ZEROSHIP_SESSION_SECRET_PREV` and `ZEROSHIP_SESSION_NONCE_CAPACITY`
# alongside it in docs/reference/env-vars.md, which listed all three as
# surviving. One stale citation survives on purpose in
# crates/zeroship-data-sqlite/src/lib.rs:1470, where a comment still calls the
# deleted minter a pattern to mirror; this gate cannot see a comment.
INVENTORY="
AUTH_TEST_SMTP_SINK
CONTROL_TEST_DB
CPG_FD_PROBE_CHILD
DRAGONFLY_CLUSTER_SEEDS
PG_TEST_URL
REDIS_TEST_URL
REDPANDA_BROKERS
ZERO_MIGRATE_MYSQL_URL
ZERO_MIGRATE_TEST_PG_URL
ZEROSHIP_CONFIG_CONTRACT_FIXTURE_TEST
ZEROSHIP_DW_E2E
ZEROSHIP_DW_E2E_APP_ID
ZEROSHIP_DW_E2E_BLOB_ROOT
ZEROSHIP_DW_E2E_CONTROL_URL
ZEROSHIP_DW_E2E_DEPLOY_ID
ZEROSHIP_DW_E2E_GATEWAY_URL
"

# Test-class names a harness puts in its OWN environment with `export`, each
# with the reason it is not yet a command prefix. Arm 3 refuses a new one and
# refuses a stale one, so this list can only shrink without a deliberate edit.
#
#   CONTROL_TEST_DB  crates/control/tests/workflow_engine_test.rs:59 still
#                    reads it. run_billing_suite.sh:166-180 records why the
#                    export survives and what deletes it. NOTE: this name is
#                    the one docs/reference/env-vars.md:620 claims is DELETED;
#                    it is not, and that claim was corrected with this gate.
#   PG_TEST_URL      the single DSN override, exported by three suites so one
#                    cargo invocation covers every target in the binary. Making
#                    it a prefix means repeating it on each of ~20 cargo lines.
AMBIENT_EXPORTS="
CONTROL_TEST_DB
PG_TEST_URL
"

# ---------------------------------------------------------------------------
# The extraction machinery, as functions so --self-test can drive the SAME code
# the real run uses. A self-test that exercises a copy proves nothing about the
# gate.
#
# NO `2>/dev/null` ON ANY COMMAND THAT FEEDS A COUNT. A grep that could not read
# its corpus and a corpus with nothing in it produce the same number, and every
# zero branch below would be a pass.
# ---------------------------------------------------------------------------

# $1 = repo root to scan. Rust files that may hold a first-party test read.
read_files() {
  find "$1/crates" "$1/libs" -type f -name '*.rs' \
    \( -path '*/tests/*' -o -path '*/test-support/src/*' -o -path '*/src/config/*' \) -print
}

# $1 = repo root. Every `NAME` read via either sanctioned spelling, one per
# line, WITH its file:line so a refusal can name the site.
#
# Spelling 2 (the sealed enums) is matched by path rather than by call shape:
# every uppercase string literal in `libs/*/tests/common/env.rs` is a declared
# name by that file's own rule ("literal arms only, by rule").
read_sites() {
  local root="$1" f
  while IFS= read -r f; do
    grep -HnoE 'test_env(_os)?![[:space:]]*\([[:space:]]*"[A-Z][A-Z0-9_]*"' "$f" \
      | sed -E 's/^([^:]*:[0-9]+):.*"([A-Z][A-Z0-9_]*)"$/\2 \1/'
  done < <(read_files "$root")
  while IFS= read -r f; do
    grep -HnoE '"[A-Z][A-Z0-9_]*"' "$f" \
      | sed -E 's/^([^:]*:[0-9]+):"([A-Z][A-Z0-9_]*)"$/\2 \1/'
  done < <(find "$root/libs" -type f -path '*/tests/common/env.rs' -print)
}

# $1 = repo root. Every AMBIENT export site under tests/, as `NAME file:line`.
#
# `export NAME=` / `export NAME` only. A command prefix (`NAME=v cmd`, and the
# line-continued `NAME=v \` form the e2e harnesses use) never begins with the
# word `export`, and a bare local assignment does not either - which is exactly
# the discrimination this gate exists to make.
export_sites() {
  local root="$1"
  find "$root/tests" -type f -name '*.sh' -print0 \
    | xargs -0 -r grep -HnoE '^[[:space:]]*export[[:space:]]+[A-Z][A-Z0-9_]*' \
    | sed -E 's/^([^:]*:[0-9]+):[[:space:]]*export[[:space:]]+([A-Z][A-Z0-9_]*)$/\2 \1/'
}

# `haystack` contains `needle` as a whole whitespace-delimited word.
has_word() {
  case " $(echo $1) " in *" $2 "*) return 0 ;; esac
  return 1
}

# ---------------------------------------------------------------------------
# --self-test: the detector's own positive/control pairs.
#
# Each positive is paired with a control DIFFERING IN ONE VARIABLE, because a
# positive alone proves the extraction RAN, not that it DISCRIMINATES: a
# detector that flagged everything would pass every positive here.
# ---------------------------------------------------------------------------
self_test() {
  echo "test-only env gate self-test"
  local tmp status=0 got
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/crates/scratch/tests" "$tmp/libs/scratch/tests/common" "$tmp/tests"

  # -- Rust POSITIVE: a brand-new ambient read via the sanctioned macro. This
  #    is the case clippy and config_env_access_gate.rs both pass.
  cat > "$tmp/crates/scratch/tests/a.rs" <<'RS'
fn dsn() -> Option<String> {
    zeroship_core::test_env!("ZS_BRAND_NEW_KNOB")
}
RS
  : > "$tmp/libs/scratch/tests/common/env.rs"
  got="$(read_sites "$tmp" | awk '{print $1}' | sort -u)"
  if printf '%s\n' "$got" | grep -qx 'ZS_BRAND_NEW_KNOB'; then
    echo "  ok   a new test_env! read is extracted"
  else
    echo "  FAIL a new test_env! read was not extracted; the arm detects nothing"
    status=1
  fi

  # -- Rust CONTROL, ONE VARIABLE: the same name, same file, same test, but
  #    DELIVERED to a child as a command prefix instead of read from ambient.
  #    This is `Command::env`, exactly what libs/compio-postgres does with
  #    CPG_FD_PROBE_CHILD, and it must NOT be flagged.
  cat > "$tmp/crates/scratch/tests/a.rs" <<'RS'
fn spawn() {
    std::process::Command::new("child")
        .env("ZS_BRAND_NEW_KNOB", "1")
        .output()
        .expect("spawn");
}
RS
  got="$(read_sites "$tmp" | awk '{print $1}' | sort -u)"
  if printf '%s\n' "$got" | grep -qx 'ZS_BRAND_NEW_KNOB'; then
    echo "  FAIL Command::env was flagged as an ambient read; the gate is bound"
    echo "       to the NAME, not to the hazard, and would break the sanctioned form"
    status=1
  else
    echo "  ok   Command::env of the same name is not an ambient read"
  fi

  # -- Shell POSITIVE: an ambient export of a test-class name.
  cat > "$tmp/tests/h.sh" <<'SH'
export CONTROL_TEST_DB="$DSN"
SH
  got="$(export_sites "$tmp" | awk '{print $1}' | sort -u)"
  if printf '%s\n' "$got" | grep -qx 'CONTROL_TEST_DB'; then
    echo "  ok   an ambient export is extracted"
  else
    echo "  FAIL an ambient export was not extracted"
    status=1
  fi

  # -- Shell CONTROL, ONE VARIABLE: the same name, same file, same value,
  #    delivered as a COMMAND PREFIX. This is the shape at
  #    tests/e2e_durable_workflows.sh:687 and it must stay green.
  cat > "$tmp/tests/h.sh" <<'SH'
CONTROL_TEST_DB="$DSN" \
  cargo test -p zeroship-control
SH
  got="$(export_sites "$tmp" | awk '{print $1}' | sort -u)"
  if printf '%s\n' "$got" | grep -qx 'CONTROL_TEST_DB'; then
    echo "  FAIL a command-prefix env was flagged as ambient; the gate would"
    echo "       forbid the one delivery form the rule explicitly permits"
    status=1
  else
    echo "  ok   a command-prefix env on a child is not an ambient export"
  fi

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

echo "test-only env gate"

# ---------------------------------------------------------------------------
# Corpus enumeration. Every floor below is derived from a live measurement of a
# corpus DIFFERENT from the number it bounds, so no floor is a constant somebody
# measured once against a fixture. A gate whose corpus vanished must refuse
# before any arm, because an empty corpus yields zero findings and zero findings
# is what a clean tree yields.
# ---------------------------------------------------------------------------

READ_FILES=$(read_files . | xargs -r grep -lE 'test_env(_os)?![[:space:]]*\(' | wc -l | tr -d ' ')
READ_FILES=$((READ_FILES + $(find ./libs -type f -path '*/tests/common/env.rs' -print | wc -l | tr -d ' ')))
EXPORT_FILES=$(find ./tests -type f -name '*.sh' -print0 \
  | xargs -0 -r grep -lE '^[[:space:]]*export[[:space:]]+[A-Z]' | wc -l | tr -d ' ')

if [ "$READ_FILES" -lt 1 ] || [ "$EXPORT_FILES" -lt 1 ]; then
  echo "GATE CANNOT RUN: corpus is empty ($READ_FILES read file(s),"
  echo "  $EXPORT_FILES export file(s)). The enumeration broke or the trees moved;"
  echo "  a clean verdict from here would mean nothing."
  exit 1
fi

SITES="$(read_sites .)"
EXPORTS="$(export_sites .)"

INV_COUNT=$(echo $INVENTORY | wc -w | tr -d ' ')

# --- Arm 1: no test read names a variable the inventory does not declare ----
#
# examined = READ SITES ruled on, not distinct names. A site is the thing whose
# verdict this arm decides, and counting sites rather than names means a single
# legitimate name deletion does not push the count toward the floor - which
# would report "examined too little" for what is actually a clean change, and
# a floor that fires on correct edits is a floor that gets lowered.
n_sites=0
[ -n "$SITES" ] && n_sites=$(printf '%s\n' "$SITES" | wc -l | tr -d ' ')

undeclared=""
while read -r name site; do
  [ -z "$name" ] && continue
  has_word "$INVENTORY" "$name" || undeclared="$undeclared
       $name  ($site)"
done <<< "$SITES"

if ! gate_arm undeclared_names "$n_sites" "$READ_FILES"; then
  fail "the test-read extraction ruled on $n_sites site(s) across a corpus of
       $READ_FILES file(s) that contain a read. Whatever it reports about new
       names is meaningless: fix the extraction, do not lower the floor."
elif [ -z "$undeclared" ]; then
  pass "all $n_sites test-read site(s) name a declared test-only variable"
else
  fail "test code reads (an) environment variable(s) the inventory in this file
       does not declare:$undeclared
       This is a NEW ambient environment read in the tests, which the operator
       rule forbids and which clippy's disallowed_methods and
       crates/zeroship-core/tests/config_env_access_gate.rs both pass - see this file's
       header for why. State the value as a typed argument, or if it must reach
       a child BINARY, give it to that child as a command prefix
       (\`NAME=v cargo test ...\` or \`Command::env\`), which this gate permits.
       If it genuinely has to be ambient, add it to INVENTORY with the reason."
fi

# --- Arm 2: no inventory entry has gone dead -------------------------------
#
# THE REVERSE DIRECTION, and the reason it exists is a measured failure, not a
# symmetry: docs/reference/env-vars.md carried ZEROSHIP_NET_TEST_DNS_HANG_HOST
# and _MS under "The surviving test-only names" while NOTHING read either. A
# list of live names that has stopped being live reads exactly like a correct
# one, and a reader who checks the list instead of the tree gets a wrong answer
# with no signal that anything is off.
#
# examined = inventory entries ruled on. The floor is READ_FILES, an
# INDEPENDENT measurement of the corpus: it is what stops somebody silencing
# arm 1 by gutting the inventory, because arm 1's floor is the corpus and this
# arm's count is the list.
declared_names="$(printf '%s\n' "$SITES" | awk '{print $1}' | sort -u)"
dead=""
n_entries=0
for name in $INVENTORY; do
  n_entries=$((n_entries + 1))
  printf '%s\n' "$declared_names" | grep -qx -- "$name" || dead="$dead $name"
done

if ! gate_arm inventory_liveness "$n_entries" "$READ_FILES"; then
  fail "the inventory holds $n_entries entr(ies) against a corpus of $READ_FILES
       file(s) that read the environment. It has been emptied or truncated, so
       arm 1 above is excusing names nobody listed."
elif [ -z "$dead" ]; then
  pass "all $n_entries inventory entr(ies) are still read by live test code"
else
  fail "these inventory entries are read NOWHERE in the tree:$dead
       A variable nothing reads is deleted, not carried. Remove the entry and
       every mention of the name - including in docs/reference/env-vars.md,
       which is where this exact failure was found."
fi

# --- Arm 3: no test-class name is delivered ambiently ----------------------
#
# examined = every export site under tests/, because this arm decides a verdict
# for each one ("is this name test-class, and if so is it excused?"). Declaring
# the number ruled on rather than the number that matched is deliberate:
# skip_marker_gate.sh declared its pre-filter total and could not see itself
# rule on nothing.
n_exports=0
[ -n "$EXPORTS" ] && n_exports=$(printf '%s\n' "$EXPORTS" | wc -l | tr -d ' ')

new_ambient=""
seen_ambient=""
while read -r name site; do
  [ -z "$name" ] && continue
  has_word "$INVENTORY" "$name" || continue
  seen_ambient="$seen_ambient $name"
  has_word "$AMBIENT_EXPORTS" "$name" || new_ambient="$new_ambient
       $name  ($site)"
done <<< "$EXPORTS"

if ! gate_arm ambient_exports "$n_exports" "$EXPORT_FILES"; then
  fail "the export scan ruled on $n_exports site(s) across $EXPORT_FILES shell
       file(s) that contain an export. The enumeration collapsed; its clean
       result says nothing."
elif [ -z "$new_ambient" ]; then
  pass "all $n_exports export site(s) ruled on; no undeclared test-class export"
else
  fail "a harness puts (a) test-class variable(s) into its OWN environment:$new_ambient
       An exported name reaches every child invisibly - action at a distance,
       which is the form of the rule this gate enforces. Give it to the one
       child that needs it as a command prefix instead, on the one command that
       needs it:
           NAME=\"\$value\" cargo test -p ...
       If the export must survive, add the name to AMBIENT_EXPORTS in this file
       WITH the reason and what deletes it."
fi

# REVERSE DIRECTION for arm 3's excuse list, and it doubles as the arm's
# positive control: an excused name that nothing exports any more is an
# exemption nobody removed, and its absence is also how a broken export scan
# announces itself before the floor above would.
stale=""
n_excused=0
for name in $AMBIENT_EXPORTS; do
  n_excused=$((n_excused + 1))
  has_word "$seen_ambient" "$name" || stale="$stale $name"
done
gate_arm excuse_liveness "$n_excused" 1 || stale="$stale (the excuse list is empty)"
if [ -z "$stale" ]; then
  pass "all $n_excused AMBIENT_EXPORTS entr(ies) are still exported, so the scan ran"
else
  fail "AMBIENT_EXPORTS excuses$stale, which no harness exports any more.
       Either the export was removed - delete the entry, that is the win this
       list is waiting for - or the scan stopped matching, in which case arm 3's
       clean result above means nothing."
fi

# --- anti-hollow guard -----------------------------------------------------
# A filtered-green and a real green print the same tally without this.
if [ "$RAN" -lt 4 ]; then
  echo "GATE DID NOT RUN: expected 4 arms, ran $RAN"
  exit 1
fi

gate_arms_finish || FAIL=$((FAIL + 1))

echo "  test-only env gate: $PASS passed, $FAIL failed ($RAN arms ran,"
echo "  $n_sites test-read site(s) over $READ_FILES file(s), $INV_COUNT inventory"
echo "  entr(ies), $n_exports export site(s) over $EXPORT_FILES file(s))"
[ "$FAIL" -eq 0 ] || exit 1
