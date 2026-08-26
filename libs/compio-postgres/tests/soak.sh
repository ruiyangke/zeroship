#!/usr/bin/env bash
# Run the compio-postgres soak harness against an existing PostgreSQL server.
#
# This is deliberately opt-in: neither `cargo test` nor any default gate calls
# it. The defaults target the crate's usual live fixture and run long enough to
# produce a useful memory series. Override them through the variables below:
#
#   PG_TEST_URL=postgres://... \
#   SOAK_DURATION_SECS=180 \
#   SOAK_SAMPLE_INTERVAL_SECS=5 \
#   SOAK_BUILD_WATCHDOG_SECS=600 \
#     libs/compio-postgres/tests/soak.sh
#
# The Rust harness has async phase watchdogs, but those timers need the compio
# runtime to keep being polled. A wedged runtime could strand its own timers,
# so this script also gives the build and run independent process watchdogs.
# The wrapper includes the optional TLS connector; the DSN's sslmode still
# decides whether each connection may use it.

set -euo pipefail

pg_test_url="${PG_TEST_URL:-postgres://postgres:zeroship@127.0.0.1:5455/zeroship}"
duration_secs="${SOAK_DURATION_SECS:-180}"
sample_interval_secs="${SOAK_SAMPLE_INTERVAL_SECS:-5}"
build_watchdog_secs="${SOAK_BUILD_WATCHDOG_SECS:-600}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../../.." && pwd)"

if [ "$#" -ne 0 ]; then
    echo "usage: PG_TEST_URL=... SOAK_DURATION_SECS=..." >&2
    echo "  SOAK_SAMPLE_INTERVAL_SECS=... SOAK_BUILD_WATCHDOG_SECS=... $0" >&2
    exit 2
fi

positive_integer() {
    local name=$1 value=$2
    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        echo "$name must be a positive integer, got '$value'" >&2
        exit 2
    fi
    # Keep the arithmetic below inside a range bash handles consistently.
    if [ "${#value}" -gt 9 ]; then
        echo "$name is too large for this harness, got '$value'" >&2
        exit 2
    fi
}

positive_integer SOAK_DURATION_SECS "$duration_secs"
positive_integer SOAK_SAMPLE_INTERVAL_SECS "$sample_interval_secs"
positive_integer SOAK_BUILD_WATCHDOG_SECS "$build_watchdog_secs"

if [ "$duration_secs" -lt 30 ]; then
    echo "SOAK_DURATION_SECS must be at least 30, got '$duration_secs'" >&2
    exit 2
fi

sample_slots=$((duration_secs / sample_interval_secs))
if [ "$sample_slots" -lt 6 ]; then
    echo "the soak must allow at least 6 periodic samples: duration=$duration_secs," >&2
    echo "sample_interval=$sample_interval_secs allows only $sample_slots" >&2
    exit 2
fi

if [ -z "$pg_test_url" ]; then
    echo "PG_TEST_URL must not be empty" >&2
    exit 2
fi
if ! command -v cargo > /dev/null 2>&1; then
    echo "cargo is required to build and run the soak" >&2
    exit 127
fi
if ! command -v timeout > /dev/null 2>&1; then
    echo "GNU timeout is required for the soak's process watchdogs" >&2
    exit 127
fi

# The executable has independently bounded setup, 35-second warmup, measured
# load, sampler join, pool close, server drain, and driver drain phases. Keep
# the process watchdog outside the sum of those limits so an inner watchdog can
# print the phase that wedged before the shell has to terminate everything.
run_watchdog_secs=$((duration_secs + 180))
cd "$repo_root"

echo "compio-postgres soak (opt-in)"
echo "url=$pg_test_url"
echo "duration_secs=$duration_secs sample_interval_secs=$sample_interval_secs"
echo "build_watchdog_secs=$build_watchdog_secs run_watchdog_secs=$run_watchdog_secs"

echo "phase 1/2: build the release soak harness"
if timeout --foreground --signal=TERM --kill-after=10s "${build_watchdog_secs}s" \
    cargo build --release -p compio-postgres --bench soak --features tls \
    --manifest-path "$repo_root/Cargo.toml"; then
    echo "phase 1/2 complete"
else
    status=$?
    case "$status" in
        124)
            echo "phase 1/2 exceeded its ${build_watchdog_secs}s external watchdog (exit $status)" >&2
            ;;
        137)
            echo "phase 1/2 external watchdog forced termination after its 10s grace (exit $status)" >&2
            ;;
        *)
            echo "phase 1/2 failed (exit $status)" >&2
            ;;
    esac
    exit "$status"
fi

echo "phase 2/2: run the soak"
if timeout --foreground --signal=TERM --kill-after=10s "${run_watchdog_secs}s" \
    cargo bench -p compio-postgres --bench soak --features tls \
    --manifest-path "$repo_root/Cargo.toml" -- \
    --url "$pg_test_url" \
    --duration-secs "$duration_secs" \
    --sample-interval-secs "$sample_interval_secs"; then
    echo "phase 2/2 complete"
else
    status=$?
    case "$status" in
        124)
            echo "phase 2/2 exceeded its ${run_watchdog_secs}s external watchdog (exit $status)" >&2
            ;;
        137)
            echo "phase 2/2 external watchdog forced termination after its 10s grace (exit $status)" >&2
            ;;
        *)
            echo "phase 2/2 failed (exit $status)" >&2
            ;;
    esac
    exit "$status"
fi
