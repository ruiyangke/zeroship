#!/usr/bin/env bash
# ============================================================================
# EVERY GATE MUST BE RUN BY CI.
#
# THE MISS THIS EXISTS FOR, found 2026-09-03. `tests/vendor_embedding_gate.sh`
# enforces decision 5 of the crate split ("no non-vendor crate embeds a vendor
# type"), and the tracker closed that item as "mechanically enforced, not a
# convention". IT HAD NEVER RUN IN CI. Measured against git history:
#
#   $ git show be17704ae:.github/workflows/ci.yml | grep -n vendor_embedding
#   455:      # decision 5 beside it had tests/vendor_embedding_gate.sh. The ...
#
# ONE occurrence, and it is a COMMENT - a sentence in the decision-four step
# saying what decision 4 lacked. For three days the only thing enforcing
# decision 5 was whoever remembered to run the script by hand. It is wired now,
# as its own self-test and run steps in the `rust` job, and this gate is what
# stops the next one.
#
# WHY THE TWO META-GATES ALREADY HERE COULD NOT HAVE CAUGHT IT:
#
#   tests/ci_invocable_gate.sh   counts BARE INVOCATIONS found IN ci.yml and
#                                checks their file mode. A gate absent from
#                                that file contributes nothing to the count and
#                                nothing to miss. It answers "can what CI names
#                                actually execute", never "is everything named".
#   tests/gate_arm_census.sh     answers "does every gate declare arms with
#                                floors". A gate can declare perfect arms and
#                                never run.
#
# Nothing asked "is every gate actually wired into CI". That is mechanically
# decidable, so it is decided here.
#
# ---------------------------------------------------------------------------
# DECISION 1 - WHAT COUNTS AS A GATE: `tests/*_gate.sh`, maxdepth 1.
#
# Deliberately the SAME glob `tests/gate_arm_census.sh` enumerates, character
# for character, so the two meta-gates rule on ONE population. A second,
# subtly different population would let a gate satisfy one and be invisible to
# the other, which is the divergence both scripts exist to prevent one level
# down.
#
# The excluded set is NOT silently filtered: arm `excluded_scripts` enumerates
# every other `tests/*.sh`, classifies it by a stated family rule, and REFUSES
# a name that matches no family. So a new checker that is a gate in substance
# but not in name cannot slip out of scope unnoticed - it lands as a refusal
# telling the author to rename it or put it on the residue list with a reason.
# The families, and why each is out:
#
#   e2e_*.sh        end-to-end harnesses. They need a running platform and are
#                   wired job-by-job (dev-vs-deployed, golden-path,
#                   billing-pipeline-e2e); several are deliberately NOT wired
#                   and the dev-vs-deployed job carries the reasons in prose
#                   beside the steps (`IS NOT WIRED HERE` marks them). That is
#                   a different question with a different answer per file.
#   run_*.sh        suite runners (cargo/pnpm wrappers), each owning a CI job.
#   *_selftest.sh   a gate's or library's own positive/control pair. It is run
#                   BESIDE the thing it tests, so its wiring is that thing's
#                   wiring, not an independent obligation.
#   bench_*.sh      benchmarks. Not verdicts.
#   RESIDUE         9 named files that match no family, each with a one-line
#                   classification below. Kept short on purpose: a long list
#                   here is the census that goes stale.
#
# `tests/lib/` is out by directory rule - it holds sourced libraries. That rule
# is not self-evidently safe (tests/ci_invocable_gate.sh's header records that
# CI invokes `tests/lib/dev_ready_selftest.sh` bare, so the directory is not a
# reliable "library" marker), so the maxdepth-1 blind spot is CHECKED rather
# than assumed: a `*_gate.sh` anywhere under `tests/` below depth 1 is a
# refusal, because the enumeration above would not see it.
#
# ---------------------------------------------------------------------------
# DECISION 2 - WHAT COUNTS AS WIRED: the command word of a shell command inside
# a `run:` value.
#
# THIS IS THE CRUX. The real miss was a gate name present in ci.yml and present
# only in a comment, so any predicate built on "does the filename appear in the
# workflow" reports that gate as wired and this whole gate is decorative. These
# are therefore distinguished, and `--self-test` proves every one of them with
# one-variable controls:
#
#   run: tests/x_gate.sh              WIRED
#   run: bash tests/x_gate.sh         WIRED (an interpreter still runs it)
#   run: tests/x_gate.sh --range ...  WIRED (arguments are not the question)
#   # tests/x_gate.sh                 NOT wired (YAML comment - the real miss)
#   run: |                            NOT wired (shell comment in a run block)
#     # tests/x_gate.sh
#   run: grep -c tests/x_gate.sh f    NOT wired (an ARGUMENT, not a command)
#   name: tests/x_gate.sh             NOT wired (not a run: value at all)
#   run: tests/x_gate.sh --self-test  NOT wired ON ITS OWN
#
# THAT LAST ONE WAS FOUND BY WRITING THIS GATE'S RED CONTROL, and it is the
# rule that makes the predicate mean something. Every gate here with a
# self-test is wired as two steps: the self-test proves the instrument
# discriminates, the bare run rules on the tree. Deleting only the second leaves
# a gate that is fully checked and checks NOTHING - this repo's founding bug in
# a different hat - and the first draft of this gate called it wired.
#
# The extractor is YAML-block-scalar aware rather than a grep, because `run: |`
# blocks are where most of this workflow's commands live and a line-oriented
# grep cannot tell block content from the comment three lines above it.
# Interpreter prefixes are skipped, so `bash tests/x_gate.sh` counts (which is
# how tests/project_config_gate.sh is wired) - the question here is "does it
# RUN", not "is it exec-bit clean", which is tests/ci_invocable_gate.sh's
# question and the reason that gate deliberately requires the bare form.
#
# Every `.github/workflows/*.yml` is read, not ci.yml alone: a gate moved into
# a second workflow is still wired, and there is no reason to make this gate go
# red on a refactor it should not have an opinion about.
#
# ---------------------------------------------------------------------------
# DECISION 3 - THE ALLOWLIST, and what a legitimate reason is.
#
# Each row carries a one-line REASON, the way tests/sync_claim_gate.sh's ledger
# and tests/decision_four_gate.sh's baseline carry theirs. A row without one is
# an exemption nobody remembers granting.
#
# NOTE WHAT THE ALLOWLIST IS NOT FOR IN THIS REPO. "It needs a live database"
# is not a reason here: CI runs four live-Postgres jobs (billing-gate,
# auth-gate, plugin-db-live-gate, worker-live-gate) and the `rust` job itself
# runs tests/provision_test_backends.sh before its tests. Neither is "it is
# slow" - that job already runs a full workspace test and a clippy audit under
# `--all-features`, which is minutes of compiling either way. The bar is
# that the gate CANNOT be a step: it needs infrastructure of its own that a
# step cannot bring with it.
#
# ---------------------------------------------------------------------------
# DECISION 4 - BOTH DIRECTIONS. Arm `allowlist_liveness` refuses a row naming a
# gate that no longer exists, and arm `excluded_scripts` refuses a residue row
# naming a file that no longer exists. Without the reverse arm each list rots
# into an exemption list nobody remembers granting, and - the case that matters
# more - a stale row is how the enumeration could stop matching while the
# forward arm stayed green.
#
# WHAT THIS GATE DOES NOT DO, stated so nobody reads it as complete:
#   - it does not check that the wired step RUNS the gate in a job that is
#     reached. A gate wired inside a job with an `if:` that is never true, or
#     behind `continue-on-error`, passes here.
#   - it rules on ONE argument shape. `--self-test` alone is refused; any other
#     argument list is accepted without being read, so a gate wired as
#     `x_gate.sh --dry-run` would pass.
#   - it does not rule on the e2e harnesses, which is the larger unwired
#     population and a per-file judgement rather than a rule.
#
# Run the extractor's own positive/control set: this script --self-test.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

WF_DIR="$ROOT/.github/workflows"
TESTS_DIR="$ROOT/tests"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

# ---------------------------------------------------------------------------
# THE ALLOWLIST. `<gate basename><TAB><reason>`.
#
# Assigned 2026-09-04 by running every unwired gate and reading what it needs.
# Twelve gates were unwired when this file was written; eleven of them are now
# steps in the `rust` job. This is the twelfth.
# ---------------------------------------------------------------------------
ALLOWLIST='
platform_migration_corpus_gate.sh	brings up its OWN postgres:17 container and needs a FRESH database per run; the rust job Postgres it would otherwise reach has the platform corpus already applied, and --static-only exits non-zero by design, so this is a job with a service of its own, not a step
rls_binding_gate.sh	RED BY DESIGN, and the red is the deliverable: it names which row-level-security policies in db/migrations-ts/ bind a role and which bind none, as the premise for the sessions redesign. A step that must fail pins CI red and teaches everyone to ignore it. Wire it the day its verdict is all BOUND, which is the day docs/proposals/2026-09-05-auth-foundation-redesign.md step 5 lands - and note this reason is a different KIND from the row above, which is about infrastructure a step cannot bring
'

# ---------------------------------------------------------------------------
# THE RESIDUE. Non-gate `tests/*.sh` that match no family rule, each with what
# it is. `<basename><TAB><classification>`.
#
# NO LINE NUMBERS IN THESE ROWS, on purpose. Every one of them would point into
# ci.yml, and adding thirteen steps to that file while writing this gate moved
# four of the four I had first written down. A citation that rots on the commit
# introducing it is worse than none; the JOB a step lives in is stable and is
# what a reader needs anyway.
#
# THREE OF THESE WERE GATES IN SUBSTANCE, invisible to the population rule
# because it reads names, and they are gone from this list as of 2026-09-04:
# `source_citation_scan.sh`, `zship_artifact_contract.sh` and
# `verdaccio_config_guard.sh` were renamed to `*_gate.sh` and are now ruled on
# by this gate and by `gate_arm_census.sh` like every other. Two were already
# wired; the third, verdaccio, was WIRED NOWHERE, and the rename is what made
# that a failure here rather than a note in a comment nobody runs.
# ---------------------------------------------------------------------------
RESIDUE='
config_check_e2e.sh	e2e harness whose name puts the suffix at the wrong end; wired in the rust job
create_demo_invoices.sh	operator utility that seeds Stripe demo data; produces no verdict
external_chain.sh	manual probe of an outbound request chain; produces no verdict
gate_arm_census.sh	the arm-declaration meta-gate; wired in the rust job and rules on the same population this gate does
golden_path.sh	the build-locally-then-deploy e2e harness; owns the golden-path job
health_endpoints.sh	manual liveness probe against a running stack
metering_rowlock_pgbench.sh	pgbench load generator; a measurement, not a verdict
provision_test_backends.sh	brings the test backends up; setup, not a check
sweep_test_databases.sh	drops leftover scratch databases; teardown, not a check
'

# ---------------------------------------------------------------------------
# THE EXTRACTOR, as functions so --self-test drives the same code the real run
# uses.
#
# run_commands: every shell command line CI executes, from `run:` values only.
# Tracks block scalars by indentation; drops YAML comment lines (never inside a
# run value in the first place) and shell comment lines (which are); joins
# backslash continuations, so a path sitting on a continuation line is read as
# the argument it is rather than as a command. That last one is not
# hypothetical: ci.yml:139 puts `crates/zeroship-runtime/tests/setup-wpt.sh` at
# the start of a continuation line, as an argument to grep.
# ---------------------------------------------------------------------------
run_commands() {
  awk '
    {
      p = match($0, /[^ \t]/)
      if (p == 0) next
      ind = p - 1
      body = substr($0, p)

      if (inblock) {
        if (ind >= blockind) {
          if (body ~ /^#/) next
          print body
          next
        }
        inblock = 0
      }

      s = body
      sub(/^-[ \t]+/, "", s)
      if (s ~ /^run:[ \t]*[|>]/) { inblock = 1; blockind = ind + 1; next }
      if (s ~ /^run:[ \t]+/) {
        v = s
        sub(/^run:[ \t]+/, "", v)
        gsub(/^["\047]+|["\047]+$/, "", v)
        if (v != "") print v
      }
    }
  ' "$@" | sed -E ':a; /\\$/{N; s/\\\n[[:space:]]*/ /; ba}'
}

# invoked_scripts: `<path><TAB><arguments>` for every repo-relative `.sh` that
# is the COMMAND WORD of some command in the stream above. Reads stdin.
#
# Segments are split on the shell operators, then leading keywords, `VAR=value`
# prefixes and interpreter words are stripped until the command word is bare.
# A token that is not a path (no slash) or not a `.sh` is dropped, which is the
# same shape tests/ci_invocable_gate.sh needed after its first draft reported
# `--exclude=source_citation_gate.sh` as an invocation.
#
# THE ARGUMENTS ARE KEPT because `--self-test` is not wiring. Every gate here
# with a self-test is wired as two steps - the self-test proves the instrument
# discriminates, the bare run rules on the tree - and a gate reduced to the
# first one alone is checked and the TREE IS NOT. That is this repo's founding
# bug wearing a different hat, and it was found by writing the RED control for
# this gate: deleting `run: tests/vendor_embedding_gate.sh` while leaving
# `run: tests/vendor_embedding_gate.sh --self-test` left the gate GREEN.
invoked_scripts() {
  awk '
    {
      line = $0
      gsub(/[|&;()]+/, "\n", line)
      n = split(line, seg, /\n/)
      for (i = 1; i <= n; i++) {
        s = seg[i]
        sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s)
        changed = 1
        while (changed) {
          changed = 0
          if (s ~ /^(if|elif|then|else|do|while|until|!|time|exec|nohup|setsid|command)[ \t]+/) {
            sub(/^[a-z!]+[ \t]+/, "", s); changed = 1; continue
          }
          if (s ~ /^[A-Za-z_][A-Za-z_0-9]*=[^ \t]*[ \t]+/) {
            sub(/^[A-Za-z_][A-Za-z_0-9]*=[^ \t]*[ \t]+/, "", s); changed = 1; continue
          }
          if (s ~ /^(bash|sh|zsh|source|\.)[ \t]+/) {
            sub(/^(bash|sh|zsh|source|\.)[ \t]+/, "", s); changed = 1; continue
          }
        }
        tok = s
        rest = ""
        if (match(s, /[ \t]/)) {
          tok = substr(s, 1, RSTART - 1)
          rest = substr(s, RSTART + 1)
          sub(/^[ \t]+/, "", rest); sub(/[ \t]+$/, "", rest)
        }
        if (tok == "") continue
        if (tok ~ /^-/) continue
        sub(/^\.\//, "", tok)
        if (tok !~ /\.sh$/) continue
        if (tok !~ /\//) continue
        print tok "\t" rest
      }
    }
  ' | LC_ALL=C sort -u
}

# ---------------------------------------------------------------------------
# THE NESTED-GATE FINDERS, and the directory is an ARGUMENT so `--self-test`
# drives the same code the real run does.
#
# THEY WERE INLINE IN THE MAIN BODY UNTIL 2026-09-04, which is why the fix that
# added the second key was unbound: `--self-test` returns at the top of this
# file, long before the block, so nothing could plant a nested gate and watch a
# finder miss it. Both keys return the EMPTY SET against this tree - there is no
# gate below tests/ depth 1 - so the check's live output says nothing about
# whether either finder can find anything, and a revert of the arms key changed
# no verdict anywhere.
#
# nested_scanned reports the population they rule on, for the arm. It is the
# pruned walk, so a vendored node_modules cannot inflate it; the name key walks
# node_modules too, which only ever adds hits.
nested_by_name() {   # <tests-dir> - the old key: a nested file CALLED *_gate.sh
  find "$1" -mindepth 2 -name '*_gate.sh' -type f | LC_ALL=C sort
}
nested_by_arms() {   # <tests-dir> - the structural key: it CALLS gate_arms_init
  nested_scan_files "$1" | xargs -r grep -l 'gate_arms_init' 2>/dev/null | LC_ALL=C sort
}
nested_scan_files() {
  find "$1" -mindepth 2 \( -name node_modules -o -name .git \) -prune -o \
    -type f ! -path "$1/lib/gate_arms.sh" -print 2>/dev/null | LC_ALL=C sort
}
nested_scanned() {
  local n=0 out
  out="$(nested_scan_files "$1")"
  [ -n "$out" ] && n="$(printf '%s\n' "$out" | grep -c .)"
  printf '%s\n' "$n"
}
nested_gates() {     # <tests-dir> - the union; either key alone is blind
  printf '%s\n%s\n' "$(nested_by_name "$1")" "$(nested_by_arms "$1")" \
    | grep -v '^$' | LC_ALL=C sort -u
}

# is_wired <repo-relative path> - true when some invocation of it does
# something other than `--self-test`.
is_wired() {
  printf '%s\n' "$INVOKED" | awk -F'\t' -v P="$1" '
    $1 == P && $2 != "--self-test" { found = 1 }
    END { exit(found ? 0 : 1) }
  '
}

# ---------------------------------------------------------------------------
# --self-test: does the WIRED/NOT-WIRED discrimination hold? Every case is one
# variable away from its partner. Without the negatives, an extractor that
# reported every mention would pass the positives and this gate would report
# the tree it was written to catch as clean.
# ---------------------------------------------------------------------------
self_test() {
  echo "ci wiring gate self-test"
  local tmp status=0 got
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  check() {
    local label="$1" want="$2"
    INVOKED="$(run_commands "$tmp/wf.yml" | invoked_scripts)"
    if is_wired tests/x_gate.sh; then got=1; else got=0; fi
    if [ "$got" = "$want" ]; then
      echo "  ok   $label"
    else
      echo "  FAIL $label: read as wired=$got, expected wired=$want"
      status=1
    fi
  }

  # POSITIVE: a plain single-line run step.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - name: A gate
        run: tests/x_gate.sh
YML
  check "a run: step naming a gate is wired" 1

  # NEGATIVE, ONE VARIABLE: the same line, commented out. THIS IS THE SHAPE
  # THAT HID THE REAL MISS FOR THREE DAYS. If this reports wired, the gate is
  # decorative and must not be believed about anything else.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - name: A gate
        # run: tests/x_gate.sh
        run: true
YML
  check "a YAML comment naming a gate is NOT wiring" 0

  # NEGATIVE: prose in a comment block, which is how be17704ae's ci.yml:455
  # named vendor_embedding_gate.sh while never running it. The sha is part of
  # the citation because that line is history, not a place in the file today.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      # decision 5 beside it had tests/x_gate.sh. The asymmetry showed.
      - name: Something else
        run: tests/other.sh
YML
  check "a gate named in step-level prose is NOT wiring" 0

  # NEGATIVE: a SHELL comment inside a run block. The extractor is inside the
  # run value here, so only the leading `#` separates this from the positive.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - name: A block
        run: |
          set -e
          # tests/x_gate.sh
          tests/other.sh
YML
  check "a shell comment inside a run: block is NOT wiring" 0

  # POSITIVE: the same block, uncommented. Pairs with the case above so the
  # block-scalar extraction is proved to reach that line at all - without this,
  # an extractor that read no block content would pass the negative for the
  # wrong reason.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - name: A block
        run: |
          set -e
          tests/x_gate.sh
          tests/other.sh
YML
  check "a command inside a run: block IS wiring" 1

  # POSITIVE: behind an interpreter. ci.yml:89 wires project_config_gate.sh
  # exactly this way.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - run: bash tests/x_gate.sh
YML
  check "an interpreter prefix still counts as running it" 1

  # POSITIVE: piped into tee, which is how every dev-vs-deployed step is
  # written.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - run: |
          set -o pipefail
          tests/x_gate.sh 2>&1 | tee "$RUNNER_TEMP/x.log"
YML
  check "a gate piped into tee is wired" 1

  # NEGATIVE: named as an ARGUMENT. Mentioning a gate on a command line is not
  # running it, and this is the case a first-token-anywhere match gets wrong.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - run: grep -c tests/x_gate.sh .github/workflows/ci.yml
YML
  check "a gate passed as an argument is NOT wiring" 0

  # NEGATIVE: outside a run: value entirely. A `name:` that quotes the script.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - name: tests/x_gate.sh
        run: true
YML
  check "a gate named in a step name is NOT wiring" 0

  # NEGATIVE: wired ONLY as its own self-test. The instrument is checked and the
  # TREE IS NOT, so this must not read as wiring. Found by writing the RED
  # control for this gate against a copy of ci.yml with
  # `run: tests/vendor_embedding_gate.sh` deleted and the `--self-test` step
  # left: the gate was green, and had no business being.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - name: Gate self-test
        run: tests/x_gate.sh --self-test
YML
  check "a --self-test step alone is NOT wiring" 0

  # POSITIVE, ONE VARIABLE FROM IT: the self-test plus the real run, which is
  # how every gate with a self-test in this repo is wired.
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - name: Gate self-test
        run: tests/x_gate.sh --self-test
      - name: The gate
        run: tests/x_gate.sh
YML
  check "a --self-test step plus the real run IS wiring" 1

  # POSITIVE: an argument that is not --self-test. `gate_arm_census.sh tests`
  # and `commit_msg_gate.sh --range ...` are both wired this way, so the rule
  # is "not only its self-test", never "no arguments".
  cat > "$tmp/wf.yml" <<'YML'
jobs:
  rust:
    steps:
      - run: tests/x_gate.sh --range "$range"
YML
  check "a gate run with other arguments IS wiring" 1

  # -------------------------------------------------------------------------
  # THE NESTED-GATE FINDERS. Both keys return the empty set against the real
  # tests/ directory, so the live check cannot show that either one WORKS - a
  # finder that matches nothing and a tree with no nested gate print the same
  # thing, which is this repo's founding bug sitting inside the check written to
  # distrust a name key.
  #
  # Case B IS THE REGRESSION for the two-key fix: a nested script that calls
  # gate_arms_init under a name the old key never looked at. It is case A with
  # ONE variable changed, the filename. Case C is its mirror - the name key
  # finding what the arms key cannot - so neither key can be deleted while both
  # pass, and case D is the negative that stops "found" meaning "everything".
  # -------------------------------------------------------------------------
  local nt="$tmp/tests" nfound
  mkdir -p "$nt/lib" "$nt/sub"
  printf '#!/usr/bin/env bash\ngate_arms_init a\n' > "$nt/sub/a_gate.sh"
  printf '#!/usr/bin/env bash\ngate_arms_init b\n' > "$nt/sub/check_b.sh"
  printf '#!/usr/bin/env bash\necho no arm accounting here\n' > "$nt/sub/c_gate.sh"
  printf '#!/usr/bin/env bash\necho neither a gate name nor an arm\n' > "$nt/sub/verify_d.sh"
  printf 'gate_arms_init() { :; }\n' > "$nt/lib/gate_arms.sh"
  nfound="$(nested_gates "$nt")"

  ncheck() {   # <label> <path under $nt> <want found: 1|0>
    local label="$1" want="$2" seen=0
    case "
$nfound" in *"
$nt/$want"*) seen=1 ;; esac
    if [ "$seen" = "$3" ]; then
      echo "  ok   $label"
    else
      echo "  FAIL $label: found=$seen, expected found=$3"
      status=1
    fi
  }
  ncheck "a nested *_gate.sh that declares arms is found"            sub/a_gate.sh      1
  ncheck "a nested ARM-DECLARING script under another name is found" sub/check_b.sh     1
  ncheck "a nested *_gate.sh that declares no arm is still found"    sub/c_gate.sh      1
  ncheck "a nested script that is neither is NOT reported"           sub/verify_d.sh    0
  ncheck "the gate_arms.sh that DEFINES the function is excluded"    lib/gate_arms.sh   0

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

gate_arms_init ci_wiring

[ -d "$WF_DIR" ] || {
  echo "FAIL: no $WF_DIR. This gate derives everything it knows from the" >&2
  echo "      workflows; without them it would report every gate unwired," >&2
  echo "      which is a broken instrument, not a finding." >&2
  exit 1
}
[ -d "$TESTS_DIR" ] || { echo "FAIL: no $TESTS_DIR" >&2; exit 1; }

echo "ci wiring gate"

# --- the maxdepth-1 blind spot, checked rather than assumed ----------------
#
# The enumeration below is `find -maxdepth 1 -name '*_gate.sh'`, and
# tests/gate_arm_census.sh uses the same two keys. A gate one directory down is
# invisible to both, which is what this check is for.
#
# IT USED TO ASK FOR `*_gate.sh` - THE VERY NAME IT EXISTS TO DISTRUST. A gate
# one directory down under any other name was invisible to the enumeration for
# being nested AND invisible to this check for being differently named. Two
# blind spots with the same shape do not cross-check each other; they agree.
#
# So this asks TWO questions, blind differently. The first is the old name key,
# kept because a nested file called `*_gate.sh` is a gate whoever wrote it.
# The second is structural, and comes from gate_arms.sh's own contract rather
# than from a filename: a gate is a script that CALLS `gate_arms_init`. That
# key does not care what the file is called or whether it ends in `.sh`.
#
# tests/lib/gate_arms.sh is excluded BY PATH, not by pattern. It DEFINES that
# function, and an exclusion written as a pattern would also hide a real caller
# whose name happened to match it.
#
# node_modules is pruned: tests/e2e-browser/node_modules is vendored third-party
# JavaScript, and nothing in it is one of our gates.
#
# THE ARM COUNTS THE FILES SCANNED, NOT THE GATES FOUND, and that is the only
# honest count here: the finders return the empty set against this tree and are
# meant to. An arm keyed on hits would have to declare a floor of zero, which
# the contract refuses and rightly - "found nothing" and "looked at nothing"
# would print identically. So the number is the population ruled on: 77 files at
# mindepth 2 after the prune, measured 2026-09-04 over six subdirectories.
#
# FLOOR 60, AND IT WAS 25 UNTIL AN ADVERSARIAL RE-MEASUREMENT THE SAME DAY. The
# comment here used to say 25 was "far over the handful a collapsed find
# produces". That is true of a TOTAL collapse and false of the collapse that
# actually happens, which is one step: re-measured with this arm's own predicate,
# mindepth 3 walks 27 files and mindepth 4 walks 4. So `-mindepth 2` drifting to
# `-mindepth 3` - one character - cleared the old floor by two while 50 of the 77
# files, 65% of the population, silently dropped out. A floor is only worth its
# margin against the SMALLEST plausible collapse, not against zero.
#
# 60 refuses the one-step collapse and still leaves room for ordinary editing:
# six subdirectories would have to lose a quarter of their contents to reach it.
# Do not lower it to accommodate a shrinking tests/ tree; re-measure and say why.
#
# That the finders can find anything AT ALL is proved by --self-test, which is
# where the planted nested gates live; this arm only says they were pointed at a
# real population. The self-test DOES catch the one-step collapse independently
# (case B goes red with "found=0"), so this floor is the second of two keys, not
# the only one - but an arm that needs its sibling to cover a collapse it counts
# is not self-sufficient, which is why the floor moved rather than the comment.
# Arm run_commands below is still the first thing to read on a multi-arm failure
# - it is the one every OTHER arm's input derives from.
N_NESTED_SCANNED="$(nested_scanned "$TESTS_DIR")"
if ! gate_arm nested_scan "$N_NESTED_SCANNED" 60; then
  fail "the nested-gate scan walked $N_NESTED_SCANNED file(s) below tests/ depth
       1. The find stopped matching, so 'no nested gates' below means 'nothing
       was looked at'."
fi
NESTED="$(nested_gates "$TESTS_DIR")"
if [ -n "$NESTED" ]; then
  fail "these gates live below tests/ depth 1, where neither this gate's
       enumeration nor tests/gate_arm_census.sh's can see them:
$(printf '%s\n' "$NESTED" | sed 's|^|         |')
       Move them to tests/, or both meta-gates are blind to them."
elif [ "$N_NESTED_SCANNED" -ge 25 ]; then
  pass "no gate hides below tests/ depth 1 ($N_NESTED_SCANNED file(s) scanned, by name AND by arm declaration)"
fi

# --- Arm 1: the extractor still finds commands -----------------------------
#
# The instrument-liveness arm, and it must come first: if the YAML shape moves
# under this extractor - a `run:` written as a flow mapping, an indentation
# change, a block scalar with an indentation indicator - it emits nothing,
# every gate reads as unwired, and arm 2 fails 37 times with the wrong
# diagnosis. This one fails once with the right one.
#
# MEASURED 2026-09-04: 290 command lines across .github/workflows/*.yml, after
# the backslash joins (313 before them - the joins are what turn a path on a
# continuation line back into the argument it is). Floor 100 is far under that
# and far over the handful a half-broken extractor produces. Steps are added
# and removed constantly, so this floor gets slack; what it must catch is a
# collapse, not a diet.
CMDS="$(run_commands "$WF_DIR"/*.yml)"
N_CMDS=0
[ -n "$CMDS" ] && N_CMDS="$(printf '%s\n' "$CMDS" | grep -c .)"

if ! gate_arm run_commands "$N_CMDS" 100; then
  fail "the run: extractor produced $N_CMDS command line(s). Every verdict below
       is derived from that stream, so all of them are meaningless. Fix the
       extractor, do not lower the floor."
else
  pass "$N_CMDS command line(s) extracted from run: steps"
fi

INVOKED="$(printf '%s\n' "$CMDS" | invoked_scripts)"

# --- Arm 2: every gate is wired, or allowlisted with a reason --------------
#
# FLOOR 24 against 38 gates today, and DELIBERATELY SLACK where
# tests/gate_arm_census.sh pins the identical glob at its exact count. Two
# exact pins on one population is two places to update for every added gate,
# and the second one to be forgotten becomes the stale census both scripts
# warn about. That gate owns the exact count; this floor guards only against
# the glob itself collapsing.
GATES=()
while IFS= read -r g; do
  [ -n "$g" ] && GATES+=("$g")
done < <(find "$TESTS_DIR" -maxdepth 1 -name '*_gate.sh' -type f | LC_ALL=C sort)

unwired=""
n_gates=0
n_allowlisted=0
#
# NO `grep -q` IN ANY OF THESE LOOKUPS, here or below. `-q` exits on the first
# match, the writing `printf` takes SIGPIPE, and `pipefail` then reports the
# pipeline as FAILED - so a matched gate would read as unwired the moment the
# input outgrows the pipe buffer. Reading all of stdin costs nothing at this
# size and removes a bug that only appears once the tree is big enough.
for path in "${GATES[@]:-}"; do
  [ -n "$path" ] || continue
  name="$(basename "$path")"
  n_gates=$((n_gates + 1))
  if is_wired "tests/$name"; then
    continue
  fi
  if printf '%s\n' "$ALLOWLIST" | grep "^$name	" >/dev/null; then
    n_allowlisted=$((n_allowlisted + 1))
    continue
  fi
  unwired="$unwired
    tests/$name"
done

if ! gate_arm gate_wiring "$n_gates" 24; then
  fail "the gate enumeration ruled on $n_gates gate(s), under its floor of 24.
       The glob stopped matching; whatever it reports below says nothing."
elif [ -z "$unwired" ]; then
  pass "all $n_gates gate(s) are wired into CI or allowlisted ($n_allowlisted allowlisted)"
else
  fail "these gates are not run by any workflow:$unwired

       A gate nobody invokes and a clean tree produce identical CI output, which
       is how tests/vendor_embedding_gate.sh enforced nothing for three days
       while being cited as mechanical enforcement. Do ONE of these:
         * add a run: step for it in .github/workflows/ci.yml;
         * add a row to ALLOWLIST in this file WITH THE REASON it cannot be a
           step. 'It is slow' and 'it needs a database' are not reasons here -
           the rust job already runs the full workspace suite against a
           provisioned Postgres.
       Do not rename the gate to dodge the glob: that also hides it from
       tests/gate_arm_census.sh."
fi

# --- Arm 3: every allowlist row still names a gate -------------------------
#
# The reverse direction. A row for a deleted gate is an exemption nobody
# remembers granting, and - the case that matters more - it is also how the
# enumeration in arm 2 could stop matching without arm 2's floor noticing.
# Floor 1: an empty allowlist is not a state this arm can rule on, and by the
# gate_arms contract an arm with nothing to enumerate is not an arm. If the
# last row is ever wired away, DELETE this arm and the allowlist with it.
stale=""
n_rows=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  n_rows=$((n_rows + 1))
  rname="$(printf '%s' "$row" | cut -f1)"
  reason="$(printf '%s' "$row" | cut -f2)"
  if [ ! -f "$TESTS_DIR/$rname" ]; then
    stale="$stale
    $rname - no such file under tests/"
  elif [ -z "$reason" ] || [ "$reason" = "$rname" ]; then
    stale="$stale
    $rname - carries no reason"
  elif is_wired "tests/$rname"; then
    stale="$stale
    $rname - is wired now; the exemption is obsolete"
  fi
done <<EOF
$ALLOWLIST
EOF

if ! gate_arm allowlist_liveness "$n_rows" 1; then
  fail "the allowlist parse produced $n_rows row(s); arm 2's exemptions are
       whatever this failed to read."
elif [ -z "$stale" ]; then
  pass "all $n_rows allowlist row(s) name a live, still-unwired gate with a reason"
else
  fail "these allowlist rows no longer describe the tree:$stale
       Delete the row in the same commit as whatever made it stale. An
       allowlist that outlives its subjects is an exemption list nobody
       remembers granting."
fi

# --- Arm 4: the excluded set is enumerated, not silently filtered ----------
#
# Every `tests/*.sh` that is NOT in the gate population is classified here by
# the family rules in this file's header, and a name matching no family is a
# refusal. That is what stops a new checker named `*_guard.sh` or `*_check.sh`
# from being outside this gate's scope without anyone deciding it should be.
#
# MEASURED 2026-09-04: 110 `tests/*.sh` at maxdepth 1, 38 of them gates, so 72
# ruled on here. Floor 40 - the families are stable and large; what this
# catches is the glob collapsing, not a few harnesses being deleted.
unclassified=""
n_excluded=0
seen_residue=""
while IFS= read -r path; do
  [ -n "$path" ] || continue
  name="$(basename "$path")"
  case "$name" in *_gate.sh) continue ;; esac
  n_excluded=$((n_excluded + 1))
  case "$name" in
    e2e_*.sh|run_*.sh|bench_*.sh|*_selftest.sh) continue ;;
  esac
  if printf '%s\n' "$RESIDUE" | grep "^$name	" >/dev/null; then
    seen_residue="$seen_residue $name"
    continue
  fi
  unclassified="$unclassified
    tests/$name"
done < <(find "$TESTS_DIR" -maxdepth 1 -name '*.sh' -type f | LC_ALL=C sort)

stale_residue=""
n_residue=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  n_residue=$((n_residue + 1))
  rname="$(printf '%s' "$row" | cut -f1)"
  rwhat="$(printf '%s' "$row" | cut -f2)"
  case " $seen_residue " in
    *" $rname "*) ;;
    *) stale_residue="$stale_residue
    $rname - matches nothing under tests/, or a family rule now covers it" ;;
  esac
  if [ -z "$rwhat" ] || [ "$rwhat" = "$rname" ]; then
    stale_residue="$stale_residue
    $rname - carries no classification"
  fi
done <<EOF
$RESIDUE
EOF

if ! gate_arm excluded_scripts "$n_excluded" 40; then
  fail "the exclusion enumeration ruled on $n_excluded script(s), under its
       floor of 40. The glob stopped matching."
elif [ -z "$unclassified" ] && [ -z "$stale_residue" ]; then
  pass "all $n_excluded non-gate script(s) classified ($n_residue named residue row(s))"
else
  [ -z "$unclassified" ] || fail "these tests/*.sh match no family rule and no
       residue row, so nothing has decided whether they are gates:$unclassified

       If it produces a verdict, name it *_gate.sh and it joins the population
       both meta-gates rule on. If it does not, add a residue row saying what
       it is."
  [ -z "$stale_residue" ] || fail "these residue rows no longer describe the
       tree:$stale_residue"
fi

echo "  excluded set (not gates, by the family rules in this file's header):"
find "$TESTS_DIR" -maxdepth 1 -name '*.sh' -type f -printf '%f\n' \
  | grep -v '_gate\.sh$' | LC_ALL=C sort | paste -sd' ' - | fold -s -w 74 \
  | sed 's|^|    |'

gate_arms_finish || FAIL=$((FAIL + 1))

echo "  ci wiring gate: $PASS passed, $FAIL failed"
echo "  ($n_gates gate(s), $n_allowlisted allowlisted, $n_excluded non-gate script(s),"
echo "   over $N_CMDS command line(s) from .github/workflows/*.yml)"
[ "$FAIL" -eq 0 ] || exit 1
