# shellcheck shell=bash
# ============================================================================
# tests/lib/usage_producer.sh - assert a producer's usage outbox is LIVE.
#
# WHY THIS EXISTS. A worker or gateway with no brokers configured boots
# normally, serves normally, and drains its meter into nothing. On a billing
# harness that is the worst possible failure: every downstream assertion --
# usage_aggregates rows, priced charges, the spend ladder -- then runs against
# genuine silence and reports exactly what a broken producer would. The harness
# goes green having measured nothing, or red at a rail forty lines below the
# actual cause.
#
# That is not hypothetical. 9b205f6ed (2026-08-16) removed the worker's
# `--config` overlay as a credential boundary; nine harnesses kept passing
# `--config` and the worker exited on an unknown argument. Three of them were
# CI-gated, and two of those CI steps carried a pass/fail count in a comment
# that could not have been true since the day the flag went.
#
# So: after the worker/gateway is health-green, assert on its log that the
# outbox STARTED. One line per producer, at the top of the stack, before any
# usage assertion can be misread.
#
# SOURCED, not executed. Requires the caller's pass()/fail() helpers.
# ============================================================================

# The fixed phrase crates/zeroship-metering/src/outbox.rs prints in the disabled arm
# (`zeroship_metering::OUTBOX_DISABLED_LOG`). Spelled here once so a rename on
# the Rust side is one grep away from every harness that depends on it.
USAGE_OUTBOX_DISABLED_LOG="usage-event outbox DISABLED"
# Both producers' started lines end in this; the worker prefixes "metering",
# the gateway prefixes "gateway".
USAGE_OUTBOX_STARTED_LOG="usage-event outbox started"

# e2e_assert_usage_producer <logfile> <label>
#
# Fails when the log says the outbox is disabled, and ALSO when it says nothing
# either way -- an absent line is absence of evidence, not evidence the
# producer is running.
e2e_assert_usage_producer() {
  local log="$1" label="$2" i
  for i in $(seq 1 20); do
    [ -f "$log" ] && grep -qF "$USAGE_OUTBOX_STARTED_LOG" "$log" && break
    [ -f "$log" ] && grep -qF "$USAGE_OUTBOX_DISABLED_LOG" "$log" && break
    sleep 0.5
  done
  if [ -f "$log" ] && grep -qF "$USAGE_OUTBOX_DISABLED_LOG" "$log"; then
    fail "$label usage outbox is DISABLED - it will drain and DROP every usage event, so every billing assertion below would run against silence"
    grep -F "$USAGE_OUTBOX_DISABLED_LOG" "$log" | head -2
    return 1
  fi
  if [ -f "$log" ] && grep -qF "$USAGE_OUTBOX_STARTED_LOG" "$log"; then
    pass "$label usage outbox started (publishing to the stream)"
    return 0
  fi
  fail "$label never reported an outbox verdict in $log - neither started nor disabled"
  return 1
}
