#!/usr/bin/env bash
# Keep the WebSocket-subscription STUB and the things that describe it in sync.
#
# WHY THIS EXISTS. `crates/gateway/src/router/dispatch.rs` carried a comment
# saying the transparent WS proxy "is wired in `proxy::forward_subscription`".
# It is not. That function was in no file in this repo -- the `proxy` module is
# real, the function never existed -- and the comment sat 760 lines from
# `handle_subscription_dispatch`, which says plainly that the gateway does NOT
# proxy an upgraded connection and returns 501.
#
# That is the expensive shape: a comment that ANSWERS the auditor's question
# ("is WS proxying handled?") before anyone goes and checks. Nothing in the
# build could see it, because a `//` comment is not a doc comment, so neither
# rustdoc's intra-doc link gate nor the repo's citation gates look at it.
#
# THE FIRST VERSION OF THIS GATE WATCHED THAT ONE IDENTIFIER, so once the
# comment was fixed it watched nothing, and on 2026-08-20 the SAME defect was
# sitting in the same file under a different name. Arm 1 now asks the general
# question -- see its header for the measurement.
#
# WHAT THIS GATE DOES NOT DO: it does not decide whether the proxy should be
# built. It only makes the stub and its descriptions move together. Nor does it
# check that a name resolves to the RIGHT thing: a comment citing a real
# function that does the opposite of what the sentence claims is green here.
#
# Run the detector's own positive/control pair: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."

# Per-arm anti-vacuity accounting. THIS GATE IS WHY THE LIBRARY EXISTS: arm 1
# below examined 0 names and printed green for eight days, underneath a
# gate-level `RAN -lt 3` guard that was correct and green the whole time,
# because three arms did run - one of them over an empty set.
# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init ws_subscription_stub

PASS=0
FAIL=0
RAN=0
pass() { PASS=$((PASS + 1)); RAN=$((RAN + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); RAN=$((RAN + 1)); echo "  FAIL $1"; }

DISPATCH="crates/gateway/src/router/dispatch.rs"
WSDOC="docs/reference/websocket-design.md"

# --- the arm-1 machinery, as functions so --self-test can drive it ---------
#
# NO `2>/dev/null` ANYWHERE IN EITHER. Both feed a count, and a command that
# failed and a command that found nothing print the same number. That
# suppression on a command whose ZERO branch was PASS is what made the old arm
# report success against a tree it could not read.

# $1 = file. Every backticked, snake_case identifier cited in a `//` comment,
# `::`-qualified paths reduced to their last segment, deduplicated.
cited_ids() {
  grep -hE '^[[:space:]]*//' "$1" \
    | grep -oE '`[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*`' \
    | tr -d '`' | sed 's/.*:://' \
    | grep -E '_' | sort -u
}

# $1 = crates root. Rust source with line comments stripped, so a second
# comment repeating a claim cannot vouch for the first.
code_corpus() {
  find "$1" -name '*.rs' -print0 | xargs -0 sed 's|//.*||'
}

self_test() {
  echo "ws subscription stub gate self-test"
  local tmp status=0 found
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/crates/scratch/src"

  # One real function, so the corpus is not empty and the control has a target.
  # POSITIVE: a comment citing a name that is in no file but the comment.
  cat > "$tmp/crates/scratch/src/lib.rs" <<'RS'
// The upgrade is pumped by `zs_only_in_this_comment` before dispatch.
pub fn zs_really_defined() {}
RS
  found="$(cited_ids "$tmp/crates/scratch/src/lib.rs")"
  if printf '%s\n' "$found" | grep -qx 'zs_only_in_this_comment' \
     && ! code_corpus "$tmp/crates" | grep -qw 'zs_only_in_this_comment'; then
    echo "  ok   a name cited only in a comment is extracted and unresolved"
  else
    echo "  FAIL a comment-only name was not caught; the arm detects nothing"
    status=1
  fi

  # NEGATIVE CONTROL, differing in ONE variable: the same comment, same place,
  # same shape -- the cited name now resolves. Without this the positive above
  # proves only that the extraction RAN, not that the resolution DISCRIMINATES:
  # a corpus lookup that always said "absent" would pass the positive too.
  sed -i 's/zs_only_in_this_comment/zs_really_defined/' \
    "$tmp/crates/scratch/src/lib.rs"
  if code_corpus "$tmp/crates" | grep -qw 'zs_really_defined'; then
    echo "  ok   the same citation resolves once the function exists"
  else
    echo "  FAIL the corpus lookup cannot find a function that is right there"
    status=1
  fi

  # The comment-stripping half: a definition that exists ONLY inside a comment
  # must not vouch for the citation. Without this the corpus could be the raw
  # file and every phantom would resolve against the comment that invented it.
  cat > "$tmp/crates/scratch/src/lib.rs" <<'RS'
// The upgrade is pumped by `zs_only_in_this_comment` before dispatch.
// fn zs_only_in_this_comment() {}
pub fn zs_really_defined() {}
RS
  if code_corpus "$tmp/crates" | grep -qw 'zs_only_in_this_comment'; then
    echo "  FAIL a commented-out definition vouched for the citation"
    status=1
  else
    echo "  ok   a definition inside a comment does not resolve a citation"
  fi

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

for f in "$DISPATCH" "$WSDOC"; do
  [ -f "$f" ] || { echo "gate cannot run: $f is missing"; exit 1; }
done

echo "ws subscription stub gate"

# --- Arm 1: no comment in dispatch.rs names a symbol only it knows --------
#
# THIS ARM WAS HARDCODED TO ONE NAME AND SAW NOTHING. It counted mentions of
# `forward_subscription` and passed when the count was ZERO -- which, after the
# 2026-08-12 fix deleted the one mention, it permanently is. Both counts were
# taken with `2>/dev/null` on a command whose zero branch is PASS, so a grep
# that could not read crates/ and a clean tree were byte-identical.
#
# It was not merely vacuous. MEASURED 2026-08-20, with the arm reporting
# "0 mention(s), 0 definition(s) -- no phantom": dispatch.rs:1238 said the WS
# handshake "runs in `proxy_subscription_upgrade`", a name that appears in this
# repo only in that comment and in docs/reviews/gateway-review-2026-08-09.md,
# which had already written it down. The IDENTICAL defect, nine lines from the
# fix commemorating the first one, invisible because the arm was keyed to the
# other name. Two more turned up in the same file: `auth_satisfied` (the real
# short-circuit is `resolve_auth`) and `execute_outcome` (it is
# `execute_resource_tree`). All three are fixed; this arm is what finds the
# fourth.
#
# THE PROPERTY, generalised from the one name to the file: a backticked
# snake_case identifier in a `//` comment in dispatch.rs must appear somewhere
# in crates/ OUTSIDE a comment. A name that lives only in prose is prose
# claiming to be code.
#
# SCOPE, and it is a real limit: dispatch.rs only, which is this gate's
# declared subject. The same enumeration over all of crates/gateway/ reports 21
# such names (measured 2026-08-20), so the defect is crate-wide and this arm
# covers one file of it. Widening is one variable and 21 comments of reading.
DISPATCH_COMMENT_FLOOR=40

# Backticked tokens that are NOT Rust symbols and never will be. Each states
# what it is instead, because a list without reasons becomes a list of things
# nobody remembers excusing. Both are checked in the reverse direction below.
#
#   _obfuscated   an RFC 7239 `Forwarded` node identifier, quoted from the spec
#   reverse_proxy a Caddyfile directive, named as such next to deploy/ops/Caddyfile
NOT_RUST_SYMBOLS='_obfuscated reverse_proxy'

CITED="$(cited_ids "$DISPATCH")"
N_CITED=0
[ -n "$CITED" ] && N_CITED=$(printf '%s\n' "$CITED" | wc -l | tr -d ' ')

CODE="$(mktemp)"
trap 'rm -f "$CODE"' EXIT
code_corpus crates > "$CODE"

phantoms=""
n_checked=0
for id in $CITED; do
  case " $NOT_RUST_SYMBOLS " in *" $id "*) continue ;; esac
  n_checked=$((n_checked + 1))
  grep -qw -- "$id" "$CODE" || phantoms="$phantoms $id"
done

# THE CANNOT-ANSWER BRANCH, which is the whole reason this arm was rewritten,
# now spelled as the shared contract so the refusal NAMES the arm rather than
# just the gate. An extraction that matched nothing reports zero phantoms, and
# so does a clean file. MEASURED 2026-08-20: 88 identifiers cited before the
# three fixes above, 85 after them. The floor sits well under that because
# ordinary comment edits move the number by ones, while the failure it guards
# -- the regex stops matching, or the file moves -- takes it to zero, not to 39.
#
# The count declared is `n_checked`, the identifiers this arm actually RULED
# ON, not `N_CITED` before NOT_RUST_SYMBOLS is subtracted. skip_marker_gate.sh
# was green on 8 raw hits and 8 exclusions; a gate that declares its pre-filter
# total cannot see that happen to itself.
if ! gate_arm comment_citations "$n_checked" "$DISPATCH_COMMENT_FLOOR"; then
  fail "the citation extraction over $DISPATCH ruled on $n_checked identifier(s),
       under its floor of $DISPATCH_COMMENT_FLOOR. Whatever it reports about
       phantoms is meaningless: fix the extraction, do not lower the floor."
elif [ -z "$phantoms" ]; then
  pass "all $n_checked identifier(s) cited in $DISPATCH comments resolve to code"
else
  fail "these names appear in a $DISPATCH comment and NOWHERE in crates/ outside
       a comment:$phantoms
       A comment is telling readers something is wired somewhere it is not --
       and it answers the auditor's question before anyone goes and checks.
       Either build it, or say what actually happens. If the token is not a
       Rust symbol at all (a spec token, a Caddyfile directive), add it to
       NOT_RUST_SYMBOLS in this file WITH the reason."
fi

# REVERSE DIRECTION, and it doubles as this arm's positive control. Each
# excused token must still BE cited: an entry that matches nothing is an
# exemption nobody removed, and -- because these two are known to be present
# today -- their absence is also how a broken extraction announces itself
# before the floor above would.
stale=""
n_excused=0
for id in $NOT_RUST_SYMBOLS; do
  n_excused=$((n_excused + 1))
  printf '%s\n' "$CITED" | grep -qx -- "$id" || stale="$stale $id"
done
# An empty NOT_RUST_SYMBOLS would make this arm vacuously clean while removing
# every exclusion from the arm above. Two entries today; the floor is 1 because
# a legitimate cleanup can take it to one, but not to none while the list is
# still being consulted.
gate_arm excuse_liveness "$n_excused" 1 || stale="$stale (the excuse list is empty)"
if [ -z "$stale" ]; then
  pass "both NOT_RUST_SYMBOLS entries are still cited, so the extraction ran"
else
  fail "NOT_RUST_SYMBOLS excuses$stale, which $DISPATCH no longer cites.
       Either the comment was deleted -- remove the entry -- or the extraction
       stopped matching, in which case the clean result above means nothing."
fi

# --- Arm 2: the stub and the creator-facing doc agree ---------------------
# `docs/reference/websocket-design.md` is what a creator reads before building
# on sockets. While the gateway answers subscriptions with 501, that doc has to
# say so. When the proxy lands and the stub goes, this arm flips and demands
# the doc stop saying it -- which is the point: the doc cannot drift either way.
stub=$(grep -c "StatusCode::NOT_IMPLEMENTED" "$DISPATCH")
doc501=$(grep -c "501" "$WSDOC")
# THE COUNT HERE IS SOURCES COMPARED, NOT MENTIONS FOUND, and the distinction
# is not a dodge: `stub == 0 && doc501 == 0` is a LEGITIMATE green for this arm
# -- it is what a landed proxy looks like -- so a floor on the mention totals
# would fail the very state the arm is designed to accept. What must never
# collapse is the number of sources the comparison has in hand; both files are
# checked for existence above, so this is 2 or the gate never got here.
n_sources=0
for f in "$DISPATCH" "$WSDOC"; do [ -r "$f" ] && n_sources=$((n_sources + 1)); done
gate_arm doc_agreement "$n_sources" 2 || true
if [ "$stub" -ge 1 ] && [ "$doc501" -ge 1 ]; then
  pass "stub present ($stub site) and $WSDOC states the 501"
elif [ "$stub" -eq 0 ] && [ "$doc501" -eq 0 ]; then
  pass "stub gone and the doc no longer claims a 501 -- consistent"
elif [ "$stub" -ge 1 ]; then
  fail "the gateway still answers subscriptions with 501 ($stub site) but
       $WSDOC never mentions it. A creator reading that doc has no way to
       learn their socket works in \`zeroship serve\` and not deployed."
else
  fail "the 501 stub is GONE from $DISPATCH but $WSDOC still tells creators
       subscriptions return 501. If the proxy landed, update the doc."
fi

# --- anti-hollow guard ----------------------------------------------------
# A filtered-green and a real green print the same tally without this.
if [ "$RAN" -lt 3 ]; then
  echo "GATE DID NOT RUN: expected 3 arms, ran $RAN"
  exit 1
fi

gate_arms_finish || FAIL=$((FAIL + 1))

echo "  ws subscription stub gate: $PASS passed, $FAIL failed ($RAN arms ran,"
echo "  $N_CITED identifier(s) cited in $DISPATCH comments, $n_checked checked)"
[ "$FAIL" -eq 0 ] || exit 1
