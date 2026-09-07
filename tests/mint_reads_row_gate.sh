#!/usr/bin/env bash
# MINT-READS-ROW: keep the witness unforgeable and keep every mint asking for it.
#
# WHY THIS EXISTS. `zeroship_auth::session_store::ValidatedSession` is the whole
# of fence F1 in docs/proposals/2026-09-05-auth-foundation-redesign.md: a
# credential may be minted only from a validating read of a session row, and
# that is enforced by a type with private fields and no public constructor
# rather than by a check somebody has to reach. The compiler already refuses a
# mint that does not PASS one. What the compiler cannot refuse is somebody
# MAKING one somewhere else - a `ValidatedSession { .. }` literal in a sibling
# module, a `From<SessionRow>`, a `#[cfg(test)]` constructor that a non-test
# path can still call. Each of those hands back exactly the property the type
# exists to provide, and each compiles cleanly.
#
# So this gate rules on two questions the build cannot:
#
#   construction  is the value built anywhere except its own module?
#   mints         does every subject-credential mint still take one?
#
# WHAT IT DOES NOT DO. It does not check that the mint USES the witness for
# anything, and it does not check that the validating statements are correct -
# `crates/zeroship-auth/tests/session_object_test.rs` binds that against live
# PostgreSQL, and `session_store`'s own `predicate_tests` bind the spelling.
# A gate over source text can only say who may build the value and who must
# hold it.
#
# Run the detectors' own positive/control pair: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init mint_reads_row

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

STORE="crates/zeroship-auth/src/session_store.rs"
ISSUER="crates/zeroship-auth/src/oidc/issuer.rs"

for f in "$STORE" "$ISSUER"; do
  if [ ! -f "$f" ]; then
    echo "  FAIL missing $f - this gate cannot rule on a tree it cannot read" >&2
    gate_arms_finish
    exit 2
  fi
done

# --- the detectors, as functions so --self-test can drive them -------------
#
# NO `2>/dev/null` ON EITHER. Both feed a count, and a command that failed and a
# command that found nothing print the same number.

# $1 = root to search. Every site that CONSTRUCTS a ValidatedSession: a struct
# literal, or an impl of a conversion trait for it. The store's own file is
# excluded by the caller, not here, so the self-test can point this at the store
# and see it fire.
#
# A FUNCTION RETURNING ONE IS NOT A CONSTRUCTION SITE, and an earlier draft of
# this detector said it was. `-> ValidatedSession` matched every helper that
# merely PASSES ONE ALONG - including a test fixture that gets its witness from
# `establish_session` - so the gate reported a forgery where there was a
# forwarding. What makes a witness forgeable is building one, and building one
# means writing the literal.
# The second `grep -v` is what separates a literal from a RETURN TYPE followed by
# a function body: `-> ValidatedSession {` and `ValidatedSession {` are the same
# characters, and only the second builds anything. A line carrying both would be
# dropped, which is the conservative direction for a signature-shaped line and
# is not a shape rustfmt produces.
construction_sites() {
  grep -rnE \
    'ValidatedSession[[:space:]]*\{|impl[[:space:]].*(From|Into|Default)[^;]*ValidatedSession' \
    "$1" --include='*.rs' \
    | grep -vE '\->[[:space:]]*ValidatedSession[[:space:]]*\{'
}

# The declared fields of the witness struct, one per line, each prefixed `pub`
# or `private`. This is what makes the literal above UNWRITABLE outside the
# module - the compiler enforces it, and this arm is what notices the day
# somebody relaxes it.
witness_fields() {
  awk '
    /^pub struct ValidatedSession \{/ { inside = 1; next }
    inside && /^\}/ { inside = 0 }
    inside && /:/ {
      line = $0
      gsub(/^[[:space:]]+/, "", line)
      if (line ~ /^\/\//) next
      print (line ~ /^pub[ (]/ ? "pub " : "private ") line
    }
  ' "$1"
}

# Every `issue_*` method on the issuer that hands a SUBJECT a credential, and
# whether its parameter list names a ValidatedSession. Emits `<name> <yes|no>`.
#
# `issue_logout_token` is deliberately absent from the required set: it
# authorises nothing and is minted at revocation time, when no live row remains
# to validate. Its own rustdoc carries that argument. It is still ENUMERATED
# here, so deleting the argument silently is not a way to shrink the set.
mint_methods() {
  awk '
    /pub async fn issue_[a-z_]*\(/ {
      name = $0
      sub(/^.*pub async fn /, "", name)
      sub(/\(.*$/, "", name)
      collecting = 1
      sig = ""
    }
    collecting { sig = sig " " $0 }
    collecting && /\)[[:space:]]*->/ {
      print name (index(sig, "ValidatedSession") ? " yes" : " no")
      collecting = 0
    }
  ' "$1"
}

if [ "${1:-}" = "--self-test" ]; then
  echo "mint_reads_row_gate --self-test"
  rc=0

  # POSITIVE: pointed at the store itself, the construction detector must find
  # the sites it exists to detect. An empty answer here means the pattern has
  # stopped matching the shape it is written for, and the real run would then
  # be green over nothing.
  n_pos="$(construction_sites "$STORE" | grep -c .)"
  if [ "$n_pos" -ge 1 ]; then
    echo "  ok   positive control: the detector sees $n_pos construction site(s) in the store"
  else
    echo "  FAIL positive control: the detector found NO construction site in $STORE"
    rc=1
  fi

  # CONTROL, one variable apart: a tree with no mention of the type at all must
  # come back empty.
  tmp="$(mktemp -d)"
  printf 'pub struct Other(());\nfn make() -> Other { Other(()) }\n' > "$tmp/other.rs"
  n_ctl="$(construction_sites "$tmp" | grep -c .)"
  rm -rf "$tmp"
  if [ "$n_ctl" -eq 0 ]; then
    echo "  ok   control: an unrelated tree yields no construction site"
  else
    echo "  FAIL control: the detector matched $n_ctl site(s) in a tree that names no ValidatedSession"
    rc=1
  fi

  # POSITIVE for the field detector: it must see the struct's fields at all.
  n_fields="$(witness_fields "$STORE" | grep -c .)"
  if [ "$n_fields" -ge 1 ]; then
    echo "  ok   positive control: the detector sees $n_fields witness field(s)"
  else
    echo "  FAIL positive control: the detector found NO field on ValidatedSession"
    rc=1
  fi

  # POSITIVE for the mint detector: it must find the methods at all.
  n_methods="$(mint_methods "$ISSUER" | grep -c .)"
  if [ "$n_methods" -ge 4 ]; then
    echo "  ok   positive control: the detector sees $n_methods issue_* method(s)"
  else
    echo "  FAIL positive control: the detector found $n_methods issue_* method(s) in $ISSUER"
    rc=1
  fi
  exit "$rc"
fi

echo "mint_reads_row_gate"

# --- arm 1: the witness is built in exactly one file -----------------------
#
# The count is FILES RULED ON, not hits: every Rust file in the auth crate is
# asked "do you construct this value", and all but the store must answer no.
AUTH_FILES=()
while IFS= read -r f; do
  [ -n "$f" ] && AUTH_FILES+=("$f")
done < <(find crates/zeroship-auth/src crates/zeroship-auth/tests -name '*.rs' -type f | LC_ALL=C sort)

offenders=""
for f in "${AUTH_FILES[@]}"; do
  [ "$f" = "$STORE" ] && continue
  hits="$(construction_sites "$f" | grep -c .)"
  if [ "$hits" -ne 0 ]; then
    offenders="${offenders}${f}
"
  fi
done
if [ -z "$offenders" ]; then
  pass "ValidatedSession is constructed only in $STORE"
else
  fail "ValidatedSession is constructed outside its own module:"
  printf '%s' "$offenders" | sed 's/^/       /'
fi
gate_arm construction "${#AUTH_FILES[@]}" 40

# --- arm 2: the witness's fields are private -------------------------------
#
# This is what makes the literal above unwritable outside the module, and it is
# the one property arm 1 depends on. `pub` on any field turns the whole design
# into a convention, and rustc would still compile every existing caller - so
# nothing but this notices.
fields="$(witness_fields "$STORE")"
n_fields="$(printf '%s\n' "$fields" | grep -c .)"
public_fields="$(printf '%s\n' "$fields" | grep '^pub ' | grep -c .)"
if [ "$n_fields" -ge 1 ] && [ "$public_fields" -eq 0 ]; then
  pass "all $n_fields ValidatedSession field(s) are private"
else
  fail "ValidatedSession has $public_fields public field(s) of $n_fields; the witness is forgeable"
fi
gate_arm field_privacy "$n_fields" 1

# --- arm 3: no escape hatch on the type ------------------------------------
#
# A `pub fn` returning one, a `Default`, or a test-only constructor are all the
# same defect wearing different hats, and the last is the one F1 names
# explicitly: a test-only constructor is fine ONLY if a non-test path cannot
# reach it, and the cheapest way to guarantee that is not to have one.
HATCHES=(
  'impl Default for ValidatedSession'
  'impl From<'
  'pub fn new('
  'pub const fn new('
)
found=""
for h in "${HATCHES[@]}"; do
  if grep -qF "$h" "$STORE"; then
    if [ "$h" = 'impl From<' ] && ! grep -F "$h" "$STORE" | grep -q 'ValidatedSession'; then
      continue
    fi
    found="${found}${h}
"
  fi
done
if [ -z "$found" ]; then
  pass "the store declares no public constructor, Default or From for the witness"
else
  fail "the store declares an escape hatch around the witness:"
  printf '%s' "$found" | sed 's/^/       /'
fi
gate_arm escape_hatches "${#HATCHES[@]}" 4

# --- arm 4: every subject-credential mint takes the witness ----------------
REQUIRED=(
  issue_access_token
  issue_principal_access_token
  issue_id_token
  issue_principal_id_token
)
methods="$(mint_methods "$ISSUER")"
n_seen="$(printf '%s\n' "$methods" | grep -c .)"
missing=""
for m in "${REQUIRED[@]}"; do
  case "$(printf '%s\n' "$methods" | awk -v m="$m" '$1 == m { print $2 }')" in
    yes) ;;
    no) missing="${missing}${m} (declared, but takes no ValidatedSession)
" ;;
    *) missing="${missing}${m} (not found on the issuer at all)
" ;;
  esac
done
if [ -z "$missing" ]; then
  pass "all ${#REQUIRED[@]} subject-credential mints take a ValidatedSession"
else
  fail "a subject-credential mint does not take the witness:"
  printf '%s' "$missing" | sed 's/^/       /'
fi
gate_arm mints "$n_seen" 4

echo "  ${PASS} passed, ${FAIL} failed"
gate_arms_finish || exit 1
[ "$FAIL" -eq 0 ] || exit 1
