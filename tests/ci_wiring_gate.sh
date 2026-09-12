#!/usr/bin/env bash
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
# Each entry explains why a surviving shell gate is not invoked by CI.
# ---------------------------------------------------------------------------
ALLOWLIST='
rls_binding_gate.sh	Reports unbound row-level-security policies as input to the sessions redesign; enable it in CI when the authority model provides a passing invariant.
'

RESIDUE='
config_check_e2e.sh	e2e harness whose name puts the suffix at the wrong end; wired in the rust job
create_demo_invoices.sh	operator utility that seeds Stripe demo data; produces no verdict
external_chain.sh	manual probe of an outbound request chain; produces no verdict
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

N_NESTED_SCANNED="$(nested_scanned "$TESTS_DIR")"
if ! gate_arm nested_scan "$N_NESTED_SCANNED" 60; then
  fail "the nested-gate scan walked $N_NESTED_SCANNED file(s) below tests/ depth
       1. The find stopped matching, so 'no nested gates' below means 'nothing
       was looked at'."
fi
NESTED="$(nested_gates "$TESTS_DIR")"
if [ -n "$NESTED" ]; then
  fail "these gates live below tests/ depth 1, where this gate's
       enumeration can see them:
$(printf '%s\n' "$NESTED" | sed 's|^|         |')
       Move them to tests/ so CI discovery can find them."
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
       CI discovery."
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
