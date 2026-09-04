#!/usr/bin/env bash
# Static config guard for SEC-8: the private Verdaccio registry must not
# allow open self-registration or $authenticated publish, and its compose
# port must stay loopback-bound.
#
# Parses the committed files (no live registry needed) and asserts:
#   1. deploy/verdaccio/config.yaml auth.htpasswd.max_users == -1
#      (self-registration disabled; publisher accounts are provisioned
#      out of band by an operator editing the htpasswd file)
#   2. packages['@zeroship/*'] and packages['**'] are DECLARED, and each
#      declares publish and unpublish EXPLICITLY, naming a principal that is
#      never $authenticated / $all / $anonymous
#   3. deploy/compose/docker-compose.yml publishes Verdaccio on 127.0.0.1 only, not 0.0.0.0
#
# WHY "DECLARED" IS AN ASSERTION AND NOT PEDANTRY. Until 2026-09-04 an absent
# `publish:` was reported as `<absent: deny-all>` and PASSED. Deleting both
# `publish:` and `unpublish:` from both package blocks printed "8 passed, 0
# failed", exit 0 - the same words, the same count and the same exit code as the
# correctly configured tree. So the gate could not tell a registry locked to one
# named publisher from a config somebody had deleted the rule out of, which is
# the whole question it exists to answer.
#
# WHAT VERDACCIO ACTUALLY DOES WITH AN ABSENT KEY, read from its source rather
# than assumed (verdaccio/verdaccio, packages/config/src/package-access.ts and
# packages/auth/src/utils.ts, read 2026-09-04; the compose service pins
# verdaccio/verdaccio:6):
#
#   publish absent   -> normalizeUserList(undefined) returns [], and
#                       allow_action does [].some(...) === false, so every
#                       principal is refused. ABSENT REALLY IS DENY-ALL.
#   unpublish absent -> normalisePackageAccess sets it to `false`, NOT to [],
#                       and handleActionWithPublishFallback answers `undefined`,
#                       which makes Auth delegate to allow_publish. ABSENT IS
#                       NOT DENY-ALL HERE - it inherits whatever publish says.
#                       The same fallback fires for `unpublish: []`.
#
# So the old message was right about one of the two keys and wrong about the
# other, and that is the smaller half of the problem. The larger half is that
# neither reading distinguishes DELIBERATE from ACCIDENTAL: a deny-all that
# nobody wrote is not a security posture, it is an absence that happens to be
# survivable today and would stop being survivable the moment publish is
# widened. The fix is therefore to require the key to be present and non-empty,
# not to reason about what its absence would mean.
#
# The live behaviours (npm adduser rejected with registration disabled,
# publish rejected for a non-publisher user) need a running registry to
# confirm end-to-end; see scripts/e2e-private-registry-sandbox.sh.
#
# MEASURED DISCRIMINATION, 2026-09-04. Each mutation applied to the real
# deploy/verdaccio/config.yaml, gate run, tree restored from a byte copy and
# `git diff` confirmed empty afterwards. "was" is the predicate this replaced:
#
#   publish + unpublish deleted from both blocks   was exit 0  now exit 1
#   publish deleted from both blocks               was exit 0  now exit 1
#   unpublish deleted from both blocks             was exit 0  now exit 1
#   publish declared with an empty value           was exit 0  now exit 1
#   access deleted, block still declared           was exit 1  now exit 1
#   config.yaml truncated to empty                 was exit 1  now exit 1, and
#       by the arm floor (3 config assertions against a floor of 4), not by a
#       message
#
# THE ONE-VARIABLE CONTROLS: `publish: $authenticated` in both blocks -> exit 1,
# naming the principal; `publish: zeroship-releaser` in both blocks -> exit 0.
# The gate reacts to an open or missing grant, not to config.yaml having been
# edited, and it does not hard-code the publisher's name.
#
# WHAT THIS DOES NOT CHECK. It does not model verdaccio's pattern matching, so
# it cannot tell you which block a given package name lands in, and it does not
# model plugin ordering - a third-party auth plugin could answer before the
# default one. It rules on the committed YAML only.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG="$ROOT/deploy/verdaccio/config.yaml"
COMPOSE="$ROOT/deploy/compose/docker-compose.yml"

# Per-arm anti-vacuity accounting (tests/lib/gate_arms.sh). The MIN-PASSED floor
# at the foot of this file already guarded one half of this - assertions that
# quietly stopped being emitted - and it predates the arm contract. It is kept
# and it is not the same question: it counts assertions that PASSED, the arms
# below count assertions RULED ON, and the two differ on exactly the run where
# something failed. They are also SPLIT here, config from compose, because the
# two halves parse different files with different awk programs and either can
# go vacuous alone: a single sum would let seven config assertions vouch for a
# compose parser that stopped matching.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init verdaccio_config

PASS=0
FAIL=0
N_CONFIG=0
N_COMPOSE=0

pass() {
    PASS=$((PASS + 1))
    echo "PASS: $1"
}

fail() {
    FAIL=$((FAIL + 1))
    echo "FAIL: $1"
}

# cfg_lookup <top> <mid> <key> — emits `found<TAB><value>` when the key is
# DECLARED at that path, and NOTHING AT ALL when it is not.
#
# The two-channel result is the whole repair. A function that returns only the
# value cannot distinguish `publish:` absent from `publish:` present and empty
# from `publish: ""`, and every caller that branches on emptiness then treats
# all three as one thing. Callers below branch on the marker first and the value
# second.
cfg_lookup() {
    awk -v a="$1" -v b="$2" -v c="$3" -v sq="'" '
        /^[ \t]*(#|$)/ { next }
        {
            line = $0
            sub(/\r$/, "", line)
            match(line, /^ */)
            ind = RLENGTH
            rest = substr(line, ind + 1)
            sub(/[ \t]#.*$/, "", rest)
            pos = index(rest, ":")
            if (pos == 0) next
            key = substr(rest, 1, pos - 1)
            val = substr(rest, pos + 1)
            gsub(/^[ \t]+|[ \t]+$/, "", val)
            qre = "^[" sq "\"]|[" sq "\"]$"
            gsub(qre, "", key)
            if (ind == 0)      { L0 = key; L1 = "" }
            else if (ind == 2) { L1 = key }
            else if (ind == 4 && L0 == a && L1 == b && key == c) { print "found\t" val; exit }
        }
    ' "$CONFIG"
}

# cfg_value <top> <mid> <key> — the value alone, for the places that have
# already established the key is declared.
cfg_value() { cfg_lookup "$1" "$2" "$3" | awk -F'\t' '$1 == "found" { print $2 }'; }

# cfg_package_blocks — the package-pattern keys declared under top-level
# `packages:`, unquoted, one per line.
#
# A SECOND EXTRACTOR, blind differently from cfg_lookup on purpose. Block
# existence used to be inferred from `access` coming back non-empty, which folds
# three different facts - the block is missing, `access` is missing, `access` is
# empty - into one verdict and reports whichever the message happens to name.
# This one answers only "is the block declared", from the block header itself.
cfg_package_blocks() {
    awk -v sq="'" '
        /^[ \t]*(#|$)/ { next }
        {
            line = $0
            sub(/\r$/, "", line)
            match(line, /^ */)
            ind = RLENGTH
            rest = substr(line, ind + 1)
            sub(/[ \t]#.*$/, "", rest)
            pos = index(rest, ":")
            if (pos == 0) next
            key = substr(rest, 1, pos - 1)
            qre = "^[" sq "\"]|[" sq "\"]$"
            gsub(qre, "", key)
            if (ind == 0) { L0 = key; next }
            if (L0 == "packages" && ind == 2) print key
        }
    ' "$CONFIG"
}

# verdaccio_ports — published-port entries of the verdaccio compose service.
verdaccio_ports() {
    awk -v sq="'" '
        /^[ \t]*(#|$)/ { next }
        {
            match($0, /^ */)
            ind = RLENGTH
            rest = substr($0, ind + 1)
            if (ind == 2 && rest ~ /^[A-Za-z0-9_-]+:[ \t]*$/) {
                svc = rest
                sub(/:.*$/, "", svc)
                inports = 0
            }
            if (svc != "verdaccio") next
            if (ind == 4) inports = (rest ~ /^ports:/) ? 1 : 0
            if (inports && ind >= 6 && rest ~ /^- /) {
                entry = rest
                sub(/^- +/, "", entry)
                gsub("[" sq "\"]", "", entry)
                print entry
            }
        }
    ' "$COMPOSE"
}

# --- 1. self-registration must be disabled -------------------------------

max_users_raw="$(cfg_lookup auth htpasswd max_users)"
max_users="$(printf '%s' "$max_users_raw" | awk -F'\t' '$1 == "found" { print $2 }')"
N_CONFIG=$((N_CONFIG + 1))
if [ -z "$max_users_raw" ]; then
    fail "auth.htpasswd.max_users is not declared; verdaccio then applies its own default and this file disables nothing"
elif [ "$max_users" = "-1" ]; then
    pass "auth.htpasswd.max_users is -1 (self-registration disabled)"
else
    fail "auth.htpasswd.max_users must be -1 to disable self-registration (got: '$max_users')"
fi

# --- 2. publish/unpublish must be declared, and never to an open principal ---

blocks="$(cfg_package_blocks)"

for pkg in '@zeroship/*' '**'; do
    # The block must be DECLARED, asked of the block headers directly. The old
    # probe inferred this from `access` coming back non-empty, which answers
    # three questions at once and reports whichever the message happens to name.
    n_block="$(printf '%s\n' "$blocks" | awk -v p="$pkg" '$0 == p { n++ } END { print n + 0 }')"
    N_CONFIG=$((N_CONFIG + 1))
    if [ "$n_block" -ge 1 ]; then
        pass "packages['$pkg'] block declared"
    else
        fail "packages['$pkg'] block not declared in $CONFIG"
        continue
    fi

    # `access` is not constrained here - reads are open on purpose, `$all` is
    # the intended value - but it must be DECLARED, for the same reason as
    # publish: absent normalizes to [] and denies every read, which is a
    # registry outage nobody chose. This assertion exists because the block
    # probe above stopped depending on it: with the old access-derived probe,
    # deleting `access` was caught as "block not found"; with a real block
    # probe, it would otherwise have become invisible.
    access_raw="$(cfg_lookup packages "$pkg" access)"
    N_CONFIG=$((N_CONFIG + 1))
    if [ -n "$access_raw" ]; then
        pass "packages['$pkg'].access is declared ($(cfg_value packages "$pkg" access))"
    else
        fail "packages['$pkg'].access is not declared; verdaccio normalizes that to an empty list and refuses every read of this pattern"
    fi

    for action in publish unpublish; do
        raw="$(cfg_lookup packages "$pkg" "$action")"
        val="$(printf '%s' "$raw" | awk -F'\t' '$1 == "found" { print $2 }')"
        N_CONFIG=$((N_CONFIG + 1))
        open_principal=""
        for token in ${val//[\[\],]/ }; do
            case "$token" in
                '$authenticated' | '$all' | '$anonymous') open_principal="$token" ;;
            esac
        done
        if [ -z "$raw" ]; then
            fail "packages['$pkg'].$action is not declared - write the grant down. An absent 'publish' denies everyone only as a side effect of normalizeUserList returning []; an absent 'unpublish' does not deny at all, verdaccio falls it back to publish."
        elif [ -z "$val" ]; then
            fail "packages['$pkg'].$action is declared with an empty value, which grants nobody and records no decision. Name the publisher."
        elif [ -n "$open_principal" ]; then
            fail "packages['$pkg'].$action grants '$open_principal' - must be a named publisher (got: '$val')"
        else
            pass "packages['$pkg'].$action is declared and names '$val', not \$authenticated/\$all/\$anonymous"
        fi
    done
done

# --- 3. compose must publish the registry on loopback only ---------------

ports="$(verdaccio_ports)"
registry_ports=0
while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    case "$entry" in
        *4873*)
            registry_ports=$((registry_ports + 1))
            N_COMPOSE=$((N_COMPOSE + 1))
            if [ "$entry" = "127.0.0.1:4873:4873" ]; then
                pass "compose verdaccio port '$entry' is loopback-bound"
            else
                fail "compose verdaccio port '$entry' must be '127.0.0.1:4873:4873' (0.0.0.0/all-interfaces exposure)"
            fi
            ;;
    esac
done <<< "$ports"

if [ "$registry_ports" -eq 0 ]; then
    fail "no verdaccio 4873 port mapping found in $COMPOSE (parser or compose drift)"
fi

# --- result ---------------------------------------------------------------

echo
echo "verdaccio config guard: $PASS passed, $FAIL failed"

status=0
[ "$FAIL" -eq 0 ] || status=1

# MEASURED 2026-09-04: 9 config assertions (max_users, plus block-declared and
# access/publish/unpublish for each of the two package blocks) and 1 compose
# port mapping. It was 7 until the same day: splitting block existence away from
# the `access` read turned one assertion answering two questions into two
# answering one each.
# Floor 4 on the config half - it survives one package block being retired
# deliberately and does not survive the awk parser or the block loop going
# quiet. Floor 1 on the compose half, which is all there is to rule on: one
# mapping is the correct number, and zero is already a FAIL below.
if ! gate_arm config_assertions "$N_CONFIG" 4; then status=1; fi
if ! gate_arm compose_port_mappings "$N_COMPOSE" 1; then status=1; fi
gate_arms_finish || status=1

[ "$status" -eq 0 ] || exit 1

# MINIMUM-PASSED FLOOR. This is DEFENCE IN DEPTH, not a repair -- all eight
# assertions were mutation-proven able to fail on 2026-08-12 (granting publish to
# $authenticated flips exactly one; deleting a packages block trips block-not-
# found; removing config.yaml aborts the run). The gap it closes is narrower and
# structural: assertions here are EMITTED PER BLOCK, so a block that disappears
# takes its sub-assertions with it rather than failing them. Deleting the
# `@zeroship/*` block measured 5 passed / 1 failed -- the count fell 8 -> 6 and
# only the block-not-found check registered the loss. Today that check catches
# it; the floor is what notices if a future refactor ever removes assertions by
# some route that does not.
#
# The number is MEASURED from a full run, not chosen. Raise it when you add
# checks; if you remove one deliberately, lower it deliberately and say so.
# Raised 8 -> 10 on 2026-09-04: the two package blocks each gained a
# block-declared assertion when it was split off the `access` read.
VERDACCIO_GUARD_MIN_PASSED=10
if [ "$PASS" -lt "$VERDACCIO_GUARD_MIN_PASSED" ]; then
    echo "" >&2
    echo "FLOOR: only $PASS assertions passed, expected at least $VERDACCIO_GUARD_MIN_PASSED." >&2
    echo "  Nothing FAILED, so this is not a broken assertion - it is MISSING ones." >&2
    echo "  A packages block was probably renamed or removed, taking its checks with it." >&2
    exit 1
fi
