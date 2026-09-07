#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# The Cedar SCHEMA, the Rust action enum and the request builder must all name
# the same vocabulary.
#
# WHAT GOES WRONG WITHOUT THIS. Cedar's parser has nothing to compare an
# identifier against, so `Action::"apps:raed"` parses, loads, and denies every
# request the band was written to permit - and the audit row it writes is
# indistinguishable from an honest non-match. `deploy/policies/zeroship.cedarschema`
# and `ValidationMode::Strict` close that, and both `build.rs` and
# `engine::load_platform_policies` now refuse on it.
#
# SO WHY A GATE AS WELL. Because strict validation only ever rules on the ids
# the POLICIES quote, and the vocabulary is bigger than that on purpose:
#
#   - `migrations:approve` appears in NO band by design - the operator-versus-
#     creator separation for destructive migrations survives as the ABSENCE of a
#     permit - and `ControlPlaneAuthenticator::authorize` issues it at a real
#     `Resource::App` on every go-live. Drop it from the schema and validation
#     stays green while that route starts returning 500, because `Request::new`
#     refuses an action the schema does not declare.
#   - The request CONTEXT is not mentioned by most policies either. A key
#     supplied by `eval::build_context` and not declared here, or declared and
#     not supplied, fails EVERY request the same way - and again after
#     validation has already passed.
#
# Both failures land as `AuthzError::CedarRequest`, which `eval::enforce` raises
# BEFORE `audit_decision` runs. So they are 500s with no row in
# `zeroship.authz_decisions` to say they happened: the single worst place in
# this subsystem for a defect to hide.
#
# WHAT ELSE BINDS THIS, and why this gate is still worth having. The authz crate
# tests `the_schema_declares_exactly_the_action_vocabulary` and
# `every_action_builds_a_schema_bound_request_at_every_resource_type` make the
# same comparisons in Rust, and they are the stronger instrument - the second
# actually constructs all (action, resource type) pairs against the real schema.
# They live in the crate they check, so one commit can weaken the test and the
# thing it checks together. This is an independent instrument in a different
# language that a crate-local edit cannot silence.
#
# WHAT THIS GATE DOES NOT DO. It compares NAMES. It does not rule on whether an
# action is paired with a sensible resource type, whether a band's rank is the
# right rank (`tests/organization_policy_ladder_gate.sh`), or whether any policy
# ever fires (`crates/zeroship-authz/tests/platform_policies_test.rs`).
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init cedar_schema_vocabulary

POLICY_DIR="deploy/policies"
SCHEMA="deploy/policies/zeroship.cedarschema"
ACTION_RS="crates/zeroship-authz/src/action.rs"
EVAL_RS="crates/zeroship-authz/src/eval.rs"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

for path in "$POLICY_DIR" "$SCHEMA" "$ACTION_RS" "$EVAL_RS"; do
  [ -e "$path" ] || { echo "gate cannot run: $path is missing" >&2; exit 1; }
done

# --- the four extractions, each from the artifact that OWNS the fact ---------

# Every `Action::"..."` a shipped band quotes. This is a SUBSET of the
# vocabulary, never equal to it: an action no band names is a design choice
# here, not a defect.
policy_action_ids() {
  grep -rhoE 'Action::"[^"]+"' "$POLICY_DIR" --include='*.cedar' \
    | sed 's/Action:://; s/"//g' | sort -u
}

# The closed vocabulary, read off `Action::cedar_id`'s match arms - the one
# place a wire id is spelled.
rust_action_vocabulary() {
  grep -oE 'Self::[A-Za-z]+ => "[^"]+"' "$ACTION_RS" \
    | sed 's/.*=> "//; s/"$//' | sort -u
}

schema_action_ids() {
  grep -oE '^action "[^"]+"' "$SCHEMA" | sed 's/^action "//; s/"$//' | sort -u
}

# The keys `build_context` puts in the request context, read from the one map
# literal that builds it.
supplied_context_keys() {
  awk '/let pairs = HashMap::from\(\[/,/^    \]\);/' "$EVAL_RS" \
    | grep -oE '"[a-z_]+"\.to_owned\(\)' | sed 's/"\.to_owned()//; s/^"//' | sort -u
}

schema_context_keys() {
  awk '/^type RequestContext = \{/,/^\};/' "$SCHEMA" \
    | grep -oE '^  "[a-z_]+":' | sed 's/^  "//; s/"://' | sort -u
}

echo "cedar schema vocabulary gate"

# ---------------------------------------------------------------------------
# Arm 1: every action id a band quotes is a real action AND is declared
# ---------------------------------------------------------------------------
QUOTED="$(policy_action_ids)"
VOCABULARY="$(rust_action_vocabulary)"
DECLARED="$(schema_action_ids)"

n_quoted=0
[ -n "$QUOTED" ] && n_quoted=$(printf '%s\n' "$QUOTED" | grep -c .)

unknown=""
undeclared=""
for id in $QUOTED; do
  printf '%s\n' "$VOCABULARY" | grep -qx "$id" || unknown="$unknown
       $id"
  printf '%s\n' "$DECLARED" | grep -qx "$id" || undeclared="$undeclared
       $id"
done

# MEASURED 2026-09-06: the self-service baseline plus the five bands quote most
# of the vocabulary. Floor 10 is far under that and far above the zero a moved
# directory or a renamed `Action::` spelling produces.
if ! gate_arm policy_action_ids "$n_quoted" 10; then
  fail "the extraction found $n_quoted quoted action id(s) in $POLICY_DIR.
       Nothing below is a statement about the policies."
elif [ -n "$unknown" ]; then
  fail "these ids are quoted by a band and spelled by no Action variant:$unknown
       Cedar accepts an unknown action id without complaint, so the band loads,
       matches nothing, and denies with an audit row that reads exactly like an
       honest non-match."
elif [ -n "$undeclared" ]; then
  fail "these ids are quoted by a band and absent from $SCHEMA:$undeclared
       Strict validation refuses the whole set on this, so the service will not
       boot - which is the intended outcome and not a reason to widen the
       schema without deciding the id is real."
else
  pass "all $n_quoted quoted action id(s) are in the vocabulary and the schema"
fi

# ---------------------------------------------------------------------------
# Arm 2: the schema's action list and Action::all() are the SAME set
# ---------------------------------------------------------------------------
#
# This is the arm strict validation cannot replace. It rules on the ids NO
# policy quotes, which is exactly where `migrations:approve` lives.
n_vocabulary=0
[ -n "$VOCABULARY" ] && n_vocabulary=$(printf '%s\n' "$VOCABULARY" | grep -c .)

missing_from_schema=""
for id in $VOCABULARY; do
  printf '%s\n' "$DECLARED" | grep -qx "$id" || missing_from_schema="$missing_from_schema
       $id"
done
phantom_in_schema=""
for id in $DECLARED; do
  printf '%s\n' "$VOCABULARY" | grep -qx "$id" || phantom_in_schema="$phantom_in_schema
       $id"
done

# MEASURED 2026-09-06 against `Action::cedar_id`'s match arms. Floor 10 sits far
# below the vocabulary and far above what a renamed enum spelling would yield.
if ! gate_arm action_vocabulary "$n_vocabulary" 10; then
  fail "the extraction found $n_vocabulary action(s) in $ACTION_RS.
       The clean verdict above says nothing; the match-arm shape moved."
elif [ -n "$missing_from_schema" ]; then
  fail "these actions exist in Rust and are declared in no schema action:$missing_from_schema
       Request::new REFUSES an undeclared action, so the first route to ask for
       one returns 500 - and it does so before audit_decision runs, leaving no
       row in zeroship.authz_decisions. Strict validation cannot see this: it
       only rules on ids the POLICIES quote, and an action deliberately named by
       no band (migrations:approve) is quoted by nothing."
elif [ -n "$phantom_in_schema" ]; then
  fail "these schema actions match no Action variant:$phantom_in_schema
       An action the platform can never issue. It widens what a hand-written
       policy may name without widening what any route can ask for."
else
  pass "all $n_vocabulary action(s) are declared in the schema, and vice versa"
fi

# ---------------------------------------------------------------------------
# Arm 3: build_context supplies exactly the keys the schema declares
# ---------------------------------------------------------------------------
SUPPLIED="$(supplied_context_keys)"
CONTEXT="$(schema_context_keys)"

n_keys=0
[ -n "$SUPPLIED" ] && n_keys=$(printf '%s\n' "$SUPPLIED" | grep -c .)

undeclared_keys=""
for key in $SUPPLIED; do
  printf '%s\n' "$CONTEXT" | grep -qx "$key" || undeclared_keys="$undeclared_keys
       $key"
done
unsupplied_keys=""
for key in $CONTEXT; do
  printf '%s\n' "$SUPPLIED" | grep -qx "$key" || unsupplied_keys="$unsupplied_keys
       $key"
done

# MEASURED 2026-09-06: build_context supplies the clock, the two condition
# inputs and the two authority ranks. Floor 3 is under that and well above the
# zero an edited map literal or a moved function would produce.
if ! gate_arm context_keys "$n_keys" 3; then
  fail "the extraction found $n_keys context key(s) in $EVAL_RS.
       build_context's map literal moved, so this arm ruled on nothing."
elif [ -n "$undeclared_keys" ]; then
  fail "build_context supplies these keys and $SCHEMA declares none of them:$undeclared_keys
       A schema-bound request refuses an EXTRA context key exactly as it refuses
       a missing one, so this fails every request in the fleet."
elif [ -n "$unsupplied_keys" ]; then
  fail "$SCHEMA declares these keys and build_context supplies none of them:$unsupplied_keys
       Same outcome, opposite direction: every request is refused at
       Request::new, before any policy is consulted and before anything is
       audited."
else
  pass "all $n_keys supplied context key(s) are declared, and vice versa"
fi

gate_arms_finish || FAIL=$((FAIL + 1))
echo "  cedar schema vocabulary gate: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
