#!/usr/bin/env bash
# Static config guard for SEC-8: the private Verdaccio registry must not
# allow open self-registration or $authenticated publish, and its compose
# port must stay loopback-bound.
#
# Parses the committed files (no live registry needed) and asserts:
#   1. config/verdaccio/config.yaml auth.htpasswd.max_users == -1
#      (self-registration disabled; publisher accounts are provisioned
#      out of band by an operator editing the htpasswd file)
#   2. packages['@zeroship/*'] and packages['**'] grant publish/unpublish
#      to a named principal only — never $authenticated / $all / $anonymous
#   3. docker-compose.yml publishes Verdaccio on 127.0.0.1 only, not 0.0.0.0
#
# The live behaviours (npm adduser rejected with registration disabled,
# publish rejected for a non-publisher user) need a running registry to
# confirm end-to-end; see scripts/e2e-private-registry-sandbox.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG="$ROOT/config/verdaccio/config.yaml"
COMPOSE="$ROOT/docker-compose.yml"

PASS=0
FAIL=0

pass() {
    PASS=$((PASS + 1))
    echo "PASS: $1"
}

fail() {
    FAIL=$((FAIL + 1))
    echo "FAIL: $1"
}

# cfg_get <top> <mid> <key> — value of a 4-space-indented key nested under
# the given top-level and 2-space-indented blocks of config.yaml.
cfg_get() {
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
            else if (ind == 4 && L0 == a && L1 == b && key == c) { print val; exit }
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

max_users="$(cfg_get auth htpasswd max_users)"
if [ "${max_users:-}" = "-1" ]; then
    pass "auth.htpasswd.max_users is -1 (self-registration disabled)"
else
    fail "auth.htpasswd.max_users must be -1 to disable self-registration (got: '${max_users:-<absent>}')"
fi

# --- 2. publish/unpublish must not be granted to open principals ---------

for pkg in '@zeroship/*' '**'; do
    # Parser sanity: the block must exist (access is always declared).
    access="$(cfg_get packages "$pkg" access)"
    if [ -n "$access" ]; then
        pass "packages['$pkg'] block found (access: $access)"
    else
        fail "packages['$pkg'] block not found in $CONFIG"
        continue
    fi

    for action in publish unpublish; do
        val="$(cfg_get packages "$pkg" "$action")"
        open_principal=""
        for token in ${val//[\[\],]/ }; do
            case "$token" in
                '$authenticated' | '$all' | '$anonymous') open_principal="$token" ;;
            esac
        done
        if [ -n "$open_principal" ]; then
            fail "packages['$pkg'].$action grants '$open_principal' — must be a named publisher (got: '$val')"
        else
            pass "packages['$pkg'].$action does not grant \$authenticated/\$all/\$anonymous (got: '${val:-<absent: deny-all>}')"
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
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
