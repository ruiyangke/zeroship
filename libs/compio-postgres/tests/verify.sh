#!/usr/bin/env bash
# Run every suite shape this crate is called green on, and rule on each.
#
# WHAT THIS EXISTS FOR
#
# "The tests pass" is ambiguous here: the crate has five shapes and the default
# `cargo test -p compio-postgres` is only one of them. The others exercise code
# the default run does not BUILD - the rustls transport, the prepared-statement
# cache's eviction and retry paths, the certificate verifiers - because the
# crate's default feature set is empty. Which shapes constitute a full check
# was documented in prose and otherwise lived in whoever last ran them.
#
# TWO FAILURE MODES THIS SCRIPT REFUSES TO HAVE
#
# 1. A TRUNCATED RUN READS AS GREEN. `cargo test` stops at the first failing
#    binary without `--no-fail-fast`, and a run that built 39 of 78 binaries
#    prints `0 failed` exactly like a complete one. Measured 2026-08-25 during
#    a cross-version check: 39 binaries, 1020 passed, 0 failed, and the suite
#    was still running. So every mode here compares the number of result lines
#    against cargo's OWN inventory for that same mode.
#
# 2. THE INVENTORY BECOMES A CENSUS THAT ROTS. Hardcoding "expect 79 binaries"
#    turns every added test file into a red build, and the repair - bumping the
#    number - is indistinguishable from bumping it to hide a loss. The expected
#    count is therefore derived per run from `cargo test --list` in the same
#    configuration, so it tracks the tree by construction.
#
# A missing fixture is a REFUSAL, not a skip: a mode that cannot run is not a
# mode that passed.
#
# IF A MODE SAYS "cargo listed no binaries", SUSPECT ANOTHER CARGO FIRST. This
# machine is shared, and a `cargo` running in another checkout can make
# `--list` here produce nothing while it holds a build lock - observed
# 2026-08-26 against a concurrent `cargo test -p zero-migrate` in a sibling
# repository, where the same command that had just answered `79 1779` answered
# nothing. That is why the empty case refuses instead of treating zero as an
# expectation and reporting a green run of nothing. Check for other cargo
# processes, then re-run.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo_root"

pg_url="${PG_TEST_URL:-postgres://postgres:zeroship@127.0.0.1:5455/zeroship}"
tls_descriptor="libs/compio-postgres/tests/data/live/tls_live.conf"
failures=0

# Count the test binaries and tests cargo says a configuration HAS, without
# running them. `--list` prints one `<name>: test` line per test and one
# `Running`/`Doc-tests` line per binary.
inventory() {
    # 2>&1, NOT 2>/dev/null: cargo prints its `Running <target>` lines to
    # STDERR, so discarding stderr counts zero binaries and every mode then
    # takes the refusal path below. Cost me one run to notice.
    # STREAM it into awk. Capturing 1779 lines into a variable and echoing
    # them overflows argv ("Argument list too long"), and the failure lands on
    # the awk, so the count comes back empty and every mode is refused.
    PG_TEST_URL="$pg_url" cargo test "$@" -- --list 2>&1 | awk '
        /^ *Running / { bins += 1 }
        /^ *Doc-tests / { bins += 1 }
        /: test$/ { tests += 1 }
        END { printf "%d %d\n", bins, tests }'
}

run_mode() {
    local label=$1; shift
    local expect_bins expect_tests
    read -r expect_bins expect_tests <<<"$(inventory "$@")"

    if [ "${expect_bins:-0}" -eq 0 ]; then
        printf '%-26s REFUSED: cargo listed no binaries for this mode\n' "$label"
        failures=$((failures + 1))
        return
    fi

    local log
    log=$(mktemp)
    # Wait for the process to EXIT. Not for its output to go quiet: a single
    # slow test can be silent for minutes.
    PG_TEST_URL="$pg_url" cargo test "$@" --no-fail-fast -- --test-threads=1 \
        > "$log" 2>&1
    local rc=$?

    local got
    got=$(awk '/^test result:/ { bins += 1; passed += $4; failed += $6 }
               END { printf "%d %d %d\n", bins, passed, failed }' "$log")
    local got_bins got_passed got_failed
    read -r got_bins got_passed got_failed <<<"$got"

    local verdict="ok"
    if [ "$got_failed" -ne 0 ]; then
        verdict="FAILED ($got_failed)"
        failures=$((failures + 1))
    elif [ "$got_bins" -ne "$expect_bins" ]; then
        # The case a failure count alone cannot see.
        verdict="TRUNCATED: ran $got_bins of $expect_bins binaries"
        failures=$((failures + 1))
    elif [ "$rc" -ne 0 ]; then
        verdict="FAILED (exit $rc with no failing test - build or harness error)"
        failures=$((failures + 1))
    fi

    printf '%-26s binaries=%s/%s tests=%s %s\n' \
        "$label" "$got_bins" "$expect_bins" "$got_passed" "$verdict"
    [ "$verdict" = "ok" ] || printf '  log: %s\n' "$log"
}

echo "compio-postgres verification matrix"
echo "server: $pg_url"
echo

run_mode "default"          -p compio-postgres
run_mode "statement-cache"  -p compio-postgres --features suite-with-statement-cache

if [ -f "$tls_descriptor" ]; then
    run_mode "suite-over-tls" -p compio-postgres --features suite-over-tls
    run_mode "tls_live"       -p compio-postgres --features tls,live-tls-tests --test tls_live
else
    echo "REFUSED: $tls_descriptor is absent, so the two TLS modes cannot run."
    echo "  Run libs/compio-postgres/tests/tls_live_setup.sh first - but read its"
    echo "  header: it regenerates a CA into SHARED containers and will break"
    echo "  another checkout's fixtures."
    failures=$((failures + 1))
fi

echo
if [ "$failures" -eq 0 ]; then
    echo "VERIFY: all modes green"
else
    echo "VERIFY: $failures mode(s) not green"
fi

echo
echo "NOT covered by this script, and each is its own runbook under docs/runbooks:"
echo "  - a second server version (compio-postgres-cross-version-check.md)"
echo "  - a transaction pooler   (compio-postgres-transaction-pooler-check.md)"
echo "  - sustained load + chaos (compio-postgres-soak.md)"
echo "  - the workspace lint and doc gates: ./tests/clippy_gate.sh, ./tests/run_doc_gate.sh"

exit $([ "$failures" -eq 0 ] && echo 0 || echo 1)
