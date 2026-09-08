#!/usr/bin/env bash
# ============================================================================
# A test fixture that boots the auth server and drives a token exchange must
# have a session-secret keyring configured.
#
# WHAT WENT WRONG. `crates/zeroship-control/tests/authz_guard_oauth_test.rs`
# built an `AuthConfig` by hand and set every secret except the refresh
# keyring. Its OP answered the token exchange
# `{"error":"server_error","error_description":"refresh hash key is not
# configured"}`, so the ONE end-to-end proof that a `zeroship login` token
# authorizes a control endpoint was red - not because the control plane was
# wrong, but because the fixture was LESS CONFIGURED THAN ANY REAL DEPLOYMENT.
# `crates/zeroship-auth/src/main.rs` refuses to boot without both files; a
# fixture assembling the config in Rust skips that refusal.
#
# WHY A GATE AND NOT JUST A SHARED HELPER. The helper
# (`zeroship_test_support::session_key_files`) exists and every fixture now
# reaches it, but that is a fact about today's tree. The operation - write a
# refresh-hmac / refresh-idem pair, point an `AuthConfig` at it - was open-coded
# at six sites when it was measured, and the missing copy was found by a red
# test rather than by anything mechanical. The next fixture is written by
# copying a neighbour, and if the neighbour it copies happens to be one that
# does not need a keyring, nothing notices until an exchange 500s.
#
# THE PROPERTY. For every test file that MOUNTS the auth router
# (`server::configure(`) and DRIVES a token exchange (names the `/oauth2/token`
# path or a `grant_type` parameter), the file must reach a session keyring:
# through `session_key_files`, through both `refresh_*_key_file` settings it
# writes itself, or through a `test_auth_config` fixture builder - which arm 3
# separately proves carries the keyring, so that third route is evidence rather
# than a hole.
#
# WHAT THIS GATE DOES NOT CATCH, stated so nobody reads it as complete:
#
#   - IT IS FILE-GRAINED, NOT FUNCTION-GRAINED. A file with two fixtures, one
#     keyed and one not, passes. Deciding which `AuthConfig` reaches which
#     `server::configure` needs a Rust parser, and a shell gate that guessed
#     would fail correct files. The measured defect was a whole file with no
#     keyring anywhere, which this does see.
#   - A FIXTURE THAT MOUNTS THE SERVER AND NEVER EXERCISES THE EXCHANGE IS NOT
#     REQUIRED TO HAVE ONE, and several do not. They are latent, not broken:
#     `server::configure` mounts `/oauth2/token` for all of them, so the day one
#     of those files gains a token request this gate turns red on it. That is
#     the intended trigger, and it is why the requirement is keyed on driving
#     the exchange rather than on mounting the router.
#   - THE EXCHANGE DETECTOR IS TEXTUAL. A fixture that reaches the token
#     endpoint only through discovery metadata - the shape
#     `crates/zeroship-gateway/tests/oidc_rp_e2e.rs` uses - names neither
#     `/oauth2/token` nor `grant_type` at the request site, so it is ruled on
#     only if some other line in the file does. Arm 1's floor is what keeps a
#     detector that has stopped matching from reading as a clean tree.
#   - IT RULES ON CONFIGURATION, NEVER ON KEY MATERIAL. A file pointing at a
#     path that does not exist, or at a malformed keyring, passes here and
#     fails at run time.
#
# Run the detector's own positive/control pair: this script --self-test.
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init session_keyring_fixture

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

AUTH_MAIN="crates/zeroship-auth/src/main.rs"
AUTH_REFRESH="crates/zeroship-auth/src/oidc/refresh.rs"

# --- the detectors, as functions so --self-test can drive them -------------
#
# NO `2>/dev/null` IN ANY OF THEM. Each feeds a verdict whose clean branch is
# PASS, and a grep that could not read the file prints what a clean file
# prints. A missing path must surface as a refusal, never as a pass.

# $1 = test-source root(s). Every file that mounts the auth router.
#
# Keyed on `server::configure(` rather than on a filename pattern: the router
# is what puts `/oauth2/token` in front of the fixture, and it is the same
# call in all three crates that boot an in-process OP.
mounts_auth_server() {
  grep -rl 'server::configure(' --include='*.rs' "$@" | LC_ALL=C sort
}

# $1 = file. True when the file drives a token exchange.
#
# Two spellings, because tests reach the endpoint both ways: by path
# (`/oauth2/token`) and by the form parameter every exchange must send
# (`grant_type`). `token_endpoint` is deliberately NOT in this set - files that
# only assert the discovery document advertises one would be pulled in without
# ever making a request.
drives_token_exchange() {
  grep -l -e 'oauth2/token' -e 'grant_type' -- "$1" >/dev/null
}

# $1 = file. True when the file reaches a session-secret keyring.
#
# Three routes, in the order a reader should think about them:
#   session_key_files      the shared definition of the operation
#   both settings fields   a fixture that writes its own pair, which is correct
#                          if unconsolidated (cli_device_refresh_test.rs)
#   test_auth_config       a fixture builder; ARM 3 is what makes this sound,
#                          by proving every such builder reaches the shared
#                          helper. Without arm 3 this route would accept any
#                          function that happened to carry the name.
has_keyring() {
  local f="$1"
  if grep -l 'session_key_files(' -- "$f" >/dev/null; then return 0; fi
  if grep -l 'refresh_hash_key_file' -- "$f" >/dev/null \
     && grep -l 'refresh_idem_key_file' -- "$f" >/dev/null; then return 0; fi
  grep -l 'test_auth_config' -- "$f" >/dev/null
}

# $1 = test-source root(s). Files defining an `AuthConfig` fixture builder.
#
# `fn test_auth_config` and `fn test_auth_config_with` are the two names in the
# tree; the pattern takes the shared prefix so a third variant is enumerated
# too. Emitted as `<file>:<fn-name>` so a file defining both is ruled on twice
# and the count is definitions, not files.
fixture_builders() {
  grep -rn 'fn test_auth_config' --include='*.rs' "$@" \
    | sed 's/^\([^:]*\):[0-9]*:.*fn \(test_auth_config[a-z_]*\).*/\1:\2/' \
    | LC_ALL=C sort -u
}

self_test() {
  echo "session keyring fixture gate self-test"
  local tmp status=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/crates/scratch/tests"

  # POSITIVE: a fixture that mounts the router, posts a token exchange, and
  # configures no keyring. This is the authz_guard_oauth_test.rs shape.
  cat > "$tmp/crates/scratch/tests/unfenced.rs" <<'RS'
let cfg = AuthConfig::parse_from(["zeroship-auth"]);
web::App::new().state(cfg).configure(server::configure(false, false));
srv.post("/oauth2/token").send_body("grant_type=authorization_code");
RS
  if [ "$(mounts_auth_server "$tmp/crates")" = "$tmp/crates/scratch/tests/unfenced.rs" ] \
     && drives_token_exchange "$tmp/crates/scratch/tests/unfenced.rs" \
     && ! has_keyring "$tmp/crates/scratch/tests/unfenced.rs"; then
    echo "  ok   an exchange-driving fixture with no keyring is caught"
  else
    echo "  FAIL the unfenced fixture was not caught; the arm detects nothing"
    status=1
  fi

  # NEGATIVE CONTROL, differing in ONE variable: the same file, same mount,
  # same exchange - a keyring appears. Without this the positive proves only
  # that the scan RAN, not that `has_keyring` DISCRIMINATES: a predicate that
  # always answered "absent" would pass the positive too.
  cat > "$tmp/crates/scratch/tests/unfenced.rs" <<'RS'
let cfg = AuthConfig::parse_from(["zeroship-auth"]);
let (h, i) = zeroship_test_support::session_key_files();
cfg.settings.refresh_hash_key_file = Operational::new(h);
cfg.settings.refresh_idem_key_file = Operational::new(i);
web::App::new().state(cfg).configure(server::configure(false, false));
srv.post("/oauth2/token").send_body("grant_type=authorization_code");
RS
  if has_keyring "$tmp/crates/scratch/tests/unfenced.rs"; then
    echo "  ok   the same fixture passes once it has a keyring"
  else
    echo "  FAIL a keyring sitting right there was not recognised"
    status=1
  fi

  # THE HALF-CONFIGURED SHAPE, which arm 2 rules on: one field, not both. The
  # hash key alone is what auth's boot check would have refused, and a fixture
  # that sets it and forgets the idempotency file fails on the SECOND read
  # rather than the first - a slower, stranger failure than the one that
  # started all this.
  cat > "$tmp/crates/scratch/tests/half.rs" <<'RS'
cfg.settings.refresh_hash_key_file = Operational::new(h);
RS
  if grep -l 'refresh_hash_key_file' -- "$tmp/crates/scratch/tests/half.rs" >/dev/null \
     && ! grep -l 'refresh_idem_key_file' -- "$tmp/crates/scratch/tests/half.rs" >/dev/null; then
    echo "  ok   a half-configured keyring reads as half, not as configured"
  else
    echo "  FAIL the pair-completeness detector cannot tell one field from two"
    status=1
  fi

  # The builder enumeration, positive and control in one file: two definitions
  # are found, and a call site is NOT mistaken for one. `fixture_builders`
  # keying on the bare name would count every caller as a definition and make
  # arm 3's floor unreachable while ruling on nothing.
  cat > "$tmp/crates/scratch/tests/builders.rs" <<'RS'
fn test_auth_config(db_url: &str) -> AuthConfig { test_auth_config_with(db_url, &[]) }
pub fn test_auth_config_with(db_url: &str, extra: &[&str]) -> AuthConfig { todo!() }
let cfg = test_auth_config(&db_url);
RS
  local found
  found="$(fixture_builders "$tmp/crates" | wc -l | tr -d ' ')"
  if [ "$found" = "2" ]; then
    echo "  ok   builder definitions are enumerated and call sites are not"
  else
    echo "  FAIL builder enumeration found $found definition(s), expected 2"
    status=1
  fi

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

for f in "$AUTH_MAIN" "$AUTH_REFRESH"; do
  [ -f "$f" ] || { echo "gate cannot run: $f is missing"; exit 1; }
done

echo "session keyring fixture gate"

TEST_ROOTS=(crates/*/tests)

# --- Arm 1: every exchange-driving fixture reaches a keyring ----------------
#
# The population is the fixtures whose verdict this gate exists to decide: a
# file that mounts the auth router AND drives a token exchange. Files that
# mount and never exchange are excluded from the COUNT, not merely from the
# failure - declaring the whole mount set here would let the exchange detector
# collapse to nothing while the arm still reported a healthy number, which is
# the pre-filter-total failure `tests/lib/gate_arms.sh` was written for.
#
# FLOOR 6. The exchange-driving set is roughly half the mount set today and
# spans three crates. Ordinary editing moves it by ones; the failures this
# floor must catch take it toward zero - `server::configure` renamed, the
# `crates/*/tests` glob stopping at a moved directory, or the two exchange
# spellings ceasing to be how tests reach the endpoint. Six is far under the
# live count and far over what any of those produces.
n_exchange=0
unfenced=""
for f in $(mounts_auth_server "${TEST_ROOTS[@]}"); do
  drives_token_exchange "$f" || continue
  n_exchange=$((n_exchange + 1))
  has_keyring "$f" || unfenced="$unfenced
    $f"
done

if ! gate_arm exchange_fixtures "$n_exchange" 6; then
  fail "the scan found $n_exchange exchange-driving fixture(s). The enumeration
       stopped matching, so a clean verdict below says nothing about the tree."
elif [ -z "$unfenced" ]; then
  pass "all $n_exchange exchange-driving fixture(s) reach a session keyring"
else
  fail "these fixtures mount the auth router and drive a token exchange with no
       session keyring configured:$unfenced

       Their token exchange answers 500 with 'refresh hash key is not
       configured', and the test that finds it will look like a defect in
       whatever it was actually testing. Call
       zeroship_test_support::session_key_files() and assign both
       cfg.settings.refresh_hash_key_file and .refresh_idem_key_file, or build
       the config through a test_auth_config fixture that already does."
fi

# --- Arm 2: a fixture that sets one keyring field sets both ----------------
#
# The keyring is a PAIR. `crates/zeroship-auth/src/oidc/refresh.rs` reads the
# hash file to hash the session secret and the idempotency file to derive the
# replay key, and auth's boot requires both. A fixture with one of them fails
# later and stranger than one with neither, and arm 1 cannot see it: a file
# naming only `refresh_hash_key_file` is caught by arm 1 only if it also lacks
# `test_auth_config`.
#
# FLOOR 3. The population is every test file that names either field, across
# the auth, control and gateway suites. Below three would mean the settings
# were renamed or the glob collapsed, not that fixtures stopped needing keys.
n_pairs=0
half=""
for f in $(grep -rl -e 'refresh_hash_key_file' -e 'refresh_idem_key_file' \
             --include='*.rs' "${TEST_ROOTS[@]}" | LC_ALL=C sort); do
  n_pairs=$((n_pairs + 1))
  h=1; i=1
  grep -l 'refresh_hash_key_file' -- "$f" >/dev/null || h=0
  grep -l 'refresh_idem_key_file' -- "$f" >/dev/null || i=0
  [ "$h" = "$i" ] || half="$half
    $f (hash=$h idem=$i)"
done

if ! gate_arm keyring_pair "$n_pairs" 3; then
  fail "the scan found $n_pairs file(s) naming a keyring setting. The setting
       names moved, or the test glob did; the verdict below is empty."
elif [ -z "$half" ]; then
  pass "all $n_pairs file(s) naming a keyring setting name BOTH halves of it"
else
  fail "these files configure half a session keyring:$half

       Both files are required. One of them alone is a fixture that serves
       tokens until the first replay check and then fails somewhere that does
       not name the keyring."
fi

# --- Arm 3: every fixture builder reaches the shared helper -----------------
#
# THIS ARM IS WHAT MAKES ARM 1's THIRD ROUTE SOUND. Arm 1 accepts the presence
# of `test_auth_config` as evidence that a file is keyed. That is only true
# while every function by that name really does configure the keyring, and the
# gateway's local builder did NOT until its two callers were folded into it -
# each wrote its own pair afterwards instead. A builder that quietly stops
# setting the keyring would silently un-fence every file that calls it, and arm
# 1 alone would go on reporting them clean.
#
# The check is per DEFINING FILE rather than per function body: `test_auth_config`
# delegates to `test_auth_config_with` in the auth suite, so the assignment
# lives one function away from the name arm 1 matched. A body-scoped check
# would fail that correct arrangement.
#
# FLOOR 2. Two suites define a builder today. One would mean the enumeration
# lost a crate; zero is a renamed helper.
n_builders=0
unbacked=""
for entry in $(fixture_builders "${TEST_ROOTS[@]}"); do
  n_builders=$((n_builders + 1))
  file="${entry%%:*}"
  grep -l 'session_key_files' -- "$file" >/dev/null \
    || unbacked="$unbacked
    $entry"
done

if ! gate_arm fixture_builders "$n_builders" 2; then
  fail "the scan found $n_builders AuthConfig fixture builder(s). Arm 1 treats a
       call to one as proof of a keyring, and that proof now rests on nothing."
elif [ -z "$unbacked" ]; then
  pass "all $n_builders fixture builder(s) reach zeroship_test_support::session_key_files"
else
  fail "these AuthConfig fixture builders do not reach the shared keyring
       helper:$unbacked

       Arm 1 counts a call to one of these as evidence that the caller is
       configured. Point the builder at
       zeroship_test_support::session_key_files() so that stays true."
fi

# --- Arm 4: production still requires what the fixtures are held to --------
#
# The premise, made falsifiable. Everything above is worth enforcing only
# because a real auth boot refuses without both files and the exchange path
# reads both. If that stops being so - the keyring becomes optional, or moves
# behind a different accessor - this arm goes red and the gate gets re-read,
# rather than going on enforcing a requirement production no longer has.
#
# FLOOR 2, and the pair is closed: there are two key files, so an arm that
# ruled on one has lost one. A third would raise the count, not lower it.
n_premise=0
missing=""
for field in refresh_hash_key_file refresh_idem_key_file; do
  n_premise=$((n_premise + 1))
  grep -l "$field" -- "$AUTH_MAIN" >/dev/null || missing="$missing
    $field is not read by $AUTH_MAIN"
  grep -l "$field" -- "$AUTH_REFRESH" >/dev/null || missing="$missing
    $field is not read by $AUTH_REFRESH"
done

if ! gate_arm production_premise "$n_premise" 2; then
  fail "the premise arm ruled on $n_premise keyring field(s)."
elif [ -z "$missing" ]; then
  pass "both keyring fields are read by auth boot and by the exchange path"
else
  fail "the premise this gate rests on has moved:$missing

       Fixtures are held to the keyring because a real boot refuses without it
       and the token exchange reads it. Re-read this gate before restoring the
       greens above."
fi

echo "  $PASS passed, $FAIL failed"
gate_arms_finish || exit 1
[ "$FAIL" -eq 0 ]
