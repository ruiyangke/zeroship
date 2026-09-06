#!/usr/bin/env bash
#
# Refuse a `curl -H` that is not followed by a header.
#
# WHY THIS EXISTS, and it is a scar rather than a theory. Deleting the inert
# `X-Api-Key` plumbing from the harnesses was done with a text sweep keyed on
# the header's VALUE - ` "X-Api-Key: $API_KEY"` - which is one token, not one
# argument. Removing it left `-H` behind, and `-H` then swallowed whatever
# came next. Three shapes resulted, all of which a shell parser accepts:
#
#   curl ... "$URL" -H)          a trailing flag with no argument
#   curl ... -H "$BODYCAP_URL"   the URL sent as a header, and NO url left
#   curl ... -H 2>/dev/null      the redirection's target read as the header
#
# `bash -n` passes on every one of them. The second is the worst: curl still
# runs, still exits 0 on some paths, and the harness reads a code it never
# requested. That is a harness measuring nothing while reporting a number.
#
# WHAT IT RULES ON, and the boundary is deliberately narrow. A `-H` that is a
# standalone WORD followed by:
#
#   a closing paren        SOUND. `-H)` cannot be anything but a stripped flag.
#   a redirection          SOUND. `-H 2>/dev/null` reads the redirect as a header.
#   `"$SOMETHING_URL"`     A HEURISTIC, and the only one here. Say so plainly:
#                          `-H "$hdr"` and `-H "$BODYCAP_URL"` are the same
#                          shape textually, and only the NAME distinguishes the
#                          header from the url. So this arm keys on the name.
#                          A url in a variable named `$target` slips through.
#
# WHAT IT DOES NOT RULE ON. It does not prove a header is CORRECT, and it does
# not look inside a variable. The sound test for the middle case would be "this
# curl ended up with no url argument", which needs a real parse of an invocation
# spanning line continuations; that is worth building the day this recurs, and
# is not built here.
#
# The `-H` must be a WORD. Without that the detector matches inside ordinary
# prose - `grep -HnoE`, "ANTI-HOLLOW", "red-at-HEAD" - and arm 1 refuses the
# whole tree while proving nothing. That was the first version of this file.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

FAIL=0
gate_arms_init curl_header_flag

MALFORMED='(^|[[:space:]])-H(\)|[[:space:]]+[0-9]*[<>]|[[:space:]]+"\$[A-Za-z_][A-Za-z0-9_]*(URL|BASE|ENDPOINT|URI)")'
WORD_H='(^|[[:space:]])-H([[:space:]]|\)|$)'

scan() {
  grep -rnE -- "$MALFORMED" "$1"/*.sh 2>/dev/null | grep -v "curl_header_flag_gate.sh:"
}

OFFENDERS="$(scan "$ROOT/tests")"
RULED="$(grep -rocE -- "$WORD_H" "$ROOT"/tests/*.sh 2>/dev/null \
  | awk -F: '{ total += $2 } END { print total + 0 }')"

if [ -n "$OFFENDERS" ]; then
  echo "FAIL: a curl -H is not followed by a header:" >&2
  printf '%s\n' "$OFFENDERS" | sed 's/^/    /' >&2
  FAIL=$((FAIL + 1))
else
  echo "  ok   every curl -H in tests/ is followed by a header"
fi
gate_arm harness_header_flags "${RULED:-0}" 40 || FAIL=$((FAIL + 1))

# Arm 2: the instrument's own control. Arm 1 alone cannot tell "the harnesses
# are clean" from "the detector matches nothing", so plant each of the three
# shapes the sweep actually produced and require every one to be caught.
FIXTURE="$(mktemp -d -t zeroship-curl-hdr.XXXXXX)"
trap 'rm -rf -- "$FIXTURE"' EXIT HUP INT TERM
cat > "$FIXTURE/planted.sh" <<'PLANT'
#!/usr/bin/env bash
code=$(curl -s -o /dev/null -w '%{http_code}' "$URL" -H)
body=$(curl -s -H 'Content-Type: application/json' -H "$BODYCAP_URL" 2>/dev/null)
index=$(curl -sf "$URL" -H 2>/dev/null || echo "")
good=$(curl -sf "$URL" -H 'Content-Type: application/json')
alsogood=$(curl -sf "$URL" -H "$hdr")
PLANT

PLANT_HITS="$(scan "$FIXTURE" | wc -l | tr -d ' ')"
if [ "${PLANT_HITS:-0}" -eq 3 ]; then
  echo "  ok   the detector catches a trailing -H, a URL read as a header, and a redirect read as one"
else
  echo "FAIL: the detector caught ${PLANT_HITS:-0} planted malformed -H, expected 3" >&2
  scan "$FIXTURE" | sed 's/^/    /' >&2
  FAIL=$((FAIL + 1))
fi
# The two well-formed lines in the same fixture must NOT be flagged, or arm 1
# would refuse every harness in the tree while proving nothing.
if scan "$FIXTURE" | grep -qE 'planted\.sh:[45]:'; then
  echo "FAIL: the detector flagged a well-formed -H" >&2
  FAIL=$((FAIL + 1))
else
  echo "  ok   a literal header and a variable header are not flagged"
fi
gate_arm planted_control 5 5 || FAIL=$((FAIL + 1))

gate_arms_finish || FAIL=$((FAIL + 1))
echo "  curl header flag gate: $FAIL failure(s)"
[ "$FAIL" -eq 0 ]
