#!/usr/bin/env bash
# The configuration-name contract, as one CI-blocking gate.
#
# WHAT IT CHECKS, and where each half comes from:
#
#   1 cargo-metadata     every workspace bin target is a registered platform
#                        binary or an explicitly classified non-platform one
#   2 compiled contract  the six linked ConfigSpec registries have no colliding
#                        projection, no declared-but-unread source and no
#                        undeclared reader (crates/config-contract/src/contract.rs)
#   3 THE AUDIT          the syn source extraction re-derives every projection
#                        with its own parser and its own transforms, and the two
#                        sets must be equal in BOTH directions
#   4 the reference      docs/reference/env-vars.md's generated region equals a
#                        fresh render of the compiled contract
#   5 raw environment    no undeclared std::env read outside the planted fixtures,
#                        AND the planted fixtures are still detected
#   6 Compose           every ZEROSHIP_* variable a platform service sets is a
#                        name that exact binary declares
#   6b alias equality    a container value that is EXACTLY one interpolation
#                        must satisfy LEFT == RIGHT (proposal Section 4.3)
#   6c command argv      no `command:`/`entrypoint:` item carries a credential
#   7 ops TOML           every leaf in deploy/ops/*.toml is a generated overlay
#                        path with a real consumer
#
# WHY THE EXPECTED SETS ARE NOT IN THIS FILE. Checks 6 and 7 join against
# `zeroship-config-contract contract`, which prints the COMPILED registry. A
# shell gate carrying its own list of names is satisfiable by editing the list,
# and this repository has been burned by that shape before (the [[inject]]
# ceilings, the AGENTS.md command index). The only names written here are the
# non-zeroship ambient inputs in AMBIENT_COMPOSE_KEYS, which no registry can
# produce and which are listed with a reason each.
#
# WHAT IT DOES NOT CHECK. Check 6b cannot see a variable that exists only in an
# operator's host shell or `.env` - that file is gitignored and generated, so
# the guarantee covers the checked-in compose surface only. The pairing of a
# stale host name with the canonical one it was renamed to is a DEPLOY-time
# concern and lives in `deploy/scripts/deploy-remote.sh` (`rename_suspects`),
# gated by `tests/deploy_scripts_gate.sh`.
#
# Check 6b was armed on 2026-08-13, after the eight violating pairs
# (CONTROL_DATABASE_URL -> ZEROSHIP_CONTROL_DATABASE_URL and seven siblings)
# were renamed so both sides spell the canonical name.
#
# Run `tests/config_name_alignment_gate.sh --self-test` to prove checks 6 and 7
# still FAIL on a planted violation. A gate that accepts everything and a gate
# that is broken print the same thing.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

COMPOSE="deploy/compose/docker-compose.yml"
PLATFORM_IMAGE="zeroship-platform:dev"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }

# Non-zeroship variables a platform service is allowed to carry. Each is an
# EXTERNAL or ambient input with no ConfigSpec by construction, so the registry
# cannot produce it and a reason has to be written down instead.
AMBIENT_COMPOSE_KEYS="
SANDBOX_URL      control reaches the extracted zeroship-sandbox project over HTTP
SANDBOX_TOKEN    the same, its bearer
OPENAI_API_KEY   forwarded to creator apps; the platform itself does not read it
POSTGRES_DB      the postgres image's own variable
POSTGRES_PASSWORD the postgres image's own variable
POSTGRES_USER    the postgres image's own variable
"

# Overlay leaves that are file-and-default ONLY by design, so they have no
# ConfigSpec to join against. Each needs a reason, because the difference
# between "deliberately outside the contract" and "never converted" is invisible
# from the TOML alone.
#
# Kept as short as it is: two entries were REMOVED from this list rather than
# added to it when the check first ran. `[gateway] broker_secret` configured
# nothing (the gateway reads a PATH) and `ZEROSHIP_CONTROL_URL` on the control
# service was read by no code in crates/control/. Both were deleted; see
# crates/core/src/config/file.rs and deploy/compose/docker-compose.yml.
FILE_ONLY_OVERLAY_LEAVES="
auth.trusted_oauth_clients  file-and-default only: no flag and no env by design (crates/core/src/config/file.rs)
"

TMP="$(mktemp -d -t zeroship-config-gate-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------------------
# Extractors. Pure text in, text out; no docker, no network.
# ---------------------------------------------------------------------------

# Emit `service<TAB>binary<TAB>KEY` for every explicit environment variable a
# service using the platform image sets. The binary is taken from the service's
# own command, so there is no service-name-to-binary table to keep in step.
compose_service_env() {
    awk -v image="$PLATFORM_IMAGE" '
        /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ {
            svc = $1; sub(/:$/, "", svc); incmd = 0; inenv = 0; next
        }
        svc == "" { next }
        /^    image:/ { img[svc] = $2; next }
        /^    command:/ { incmd = 1; inenv = 0; next }
        /^    environment:/ { inenv = 1; incmd = 0; next }
        /^    [a-zA-Z_]+:/ { incmd = 0; inenv = 0; next }
        incmd && match($0, /zeroship-[a-z-]+/) {
            if (!(svc in bin)) bin[svc] = substr($0, RSTART, RLENGTH)
            next
        }
        inenv && match($0, /^      -?[[:space:]]*[A-Z][A-Z0-9_]*/) {
            key = substr($0, RSTART, RLENGTH)
            gsub(/[ -]/, "", key)
            n = ++count[svc]
            keys[svc, n] = key
            # The scalar after the key, for the alias-equality rule. Only a
            # value whose WHOLE text is one interpolation is an alias; a
            # composite (a URL assembled from scheme + domain) is excluded by
            # the rule itself, so it is recorded as empty and skipped.
            val = $0
            sub(/^[^:]*:[[:space:]]*/, "", val)
            sub(/[[:space:]]+$/, "", val)
            # Accept every Compose modifier, not just `:-`: the file uses
            # `${VAR:?msg}` for the values `zeroship dev init` must
            # supply, and a pattern that saw only `:-` would silently check a
            # third of the surface and call it clean.
            alias = ""
            if (match(val, /^\$\{[A-Za-z_][A-Za-z0-9_]*(:?[-?+][^}]*)?\}$/)) {
                alias = val
                sub(/^\$\{/, "", alias)
                sub(/[:}].*$/, "", alias)
                sub(/[-?+].*$/, "", alias)
            }
            aliases[svc, n] = alias
            next
        }
        END {
            for (s in img) {
                if (img[s] != image) continue
                b = (s in bin) ? bin[s] : "-"
                for (i = 1; i <= count[s]; i++)
                    print s "\t" b "\t" keys[s, i] "\t" aliases[s, i]
            }
        }
    ' "$1"
}

# Emit `service<TAB>line<TAB>item` for every `command:` / `entrypoint:` item, in
# all four spellings the deploy compose files use: a `- item` list, a `>` folded
# scalar, a `| ` block scalar, and an inline `command: foo --bar` scalar.
#
# WHY THIS EXTRACTOR EXISTS AT ALL. `compose_service_env` above walks the same
# file and reads `command:` too - but ONLY to learn the binary name, at the
# `incmd && match($0, /zeroship-[a-z-]+/)` arm, which takes the first match and
# `next`s past everything else. Every flag and every VALUE in a command block is
# therefore invisible to checks 6 and 6b, which read `environment:` alone. That
# blind spot held a live credential: the platform-migrate one-shot passed a
# postgres SUPERUSER DSN as `--database-url <dsn>`, readable through `docker
# inspect`, `docker ps --no-trunc` and /proc/<pid>/cmdline for anything in that
# PID namespace. Nothing failed, because nothing looked.
#
# A service with NO `environment:` block emits no rows from `compose_service_env`
# at all, so it was doubly unseen - which is exactly what the migrate one-shot
# was.
compose_command_items() {
    awk '
        /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { svc = $1; sub(/:$/, "", svc); incmd = 0; next }
        svc == "" { next }
        # Any other key at service-key depth closes the block.
        /^    [a-zA-Z_][a-zA-Z0-9_-]*:/ {
            if ($0 ~ /^    (command|entrypoint):/) {
                incmd = 1
                rest = $0
                sub(/^    [a-z]+:[[:space:]]*/, "", rest)
                sub(/[[:space:]]+$/, "", rest)
                # `>`/`|` (with an optional chomp indicator) introduce a
                # multi-line scalar and carry no content themselves.
                if (rest != "" && rest !~ /^[>|][-+]?$/) print svc "\t" NR "\t" rest
                next
            }
            incmd = 0; next
        }
        incmd {
            line = $0
            # A YAML list dash is `-` followed by SPACE. Requiring the space is
            # what keeps `--port 9090` in a folded scalar from being read as a
            # list item and silently losing one of its dashes.
            sub(/^[[:space:]]*-[[:space:]]+/, "", line)
            sub(/^[[:space:]]+/, "", line)
            sub(/[[:space:]]+$/, "", line)
            if (line == "" || line ~ /^#/ || line ~ /^[>|][-+]?$/) next
            print svc "\t" NR "\t" line
        }
    ' "$1"
}

# Emit every `section.leaf` in a TOML file, comments and blanks discarded.
toml_leaves() {
    awk '
        { sub(/[[:space:]]*#.*$/, "") }
        /^[[:space:]]*$/ { next }
        /^\[/ { sec = $0; gsub(/[][[:space:]]/, "", sec); next }
        /^[a-z_][a-z_0-9]*[[:space:]]*=/ {
            k = $1; sub(/=.*/, "", k); gsub(/[[:space:]]/, "", k)
            print (sec == "" ? k : sec "." k)
        }
    ' "$1"
}

# ---------------------------------------------------------------------------
# The checks
# ---------------------------------------------------------------------------

check_compose() {
    local compose="$1" label="$2" contract="$3"
    local rows services=0 bad=0 checked=0
    rows="$(compose_service_env "$compose")"
    if [ -z "$rows" ]; then
        fail "$label: extracted zero environment variables from $compose"
        return 1
    fi
    services="$(echo "$rows" | cut -f1 | sort -u | wc -l)"
    if [ "$services" -lt 5 ]; then
        fail "$label: only $services platform services found (expected at least 5);"
        echo "      the extraction stopped matching, so a clean result would mean nothing."
        return 1
    fi
    while IFS=$'\t' read -r svc bin key _alias; do
        [ -n "$key" ] || continue
        case "$key" in
            ZEROSHIP_*) ;;
            *)
                if echo "$AMBIENT_COMPOSE_KEYS" | grep -q "^$key "; then
                    continue
                fi
                echo "  $svc sets $key, which is neither a zeroship name nor a listed ambient input"
                bad=$((bad + 1))
                continue
                ;;
        esac
        checked=$((checked + 1))
        if [ "$bin" = "-" ]; then
            echo "  $svc uses the platform image but names no zeroship binary in its command"
            bad=$((bad + 1))
            continue
        fi
        if ! awk -F'\t' -v b="$bin" -v e="$key" '$1==b && $5==e {found=1} END{exit !found}' "$contract"; then
            echo "  $svc sets $key, which $bin does not declare"
            bad=$((bad + 1))
        fi
    done <<<"$rows"
    if [ "$checked" -lt 20 ]; then
        fail "$label: only $checked zeroship variables checked across $services services"
        return 1
    fi
    if [ "$bad" -ne 0 ]; then
        fail "$label: $bad undeclared variable(s) across $services platform services"
        return 1
    fi
    pass "$label: $checked zeroship variables on $services platform services are all declared by the binary that reads them"
    return 0
}

# Section 4.3 alias equality: when a container value is EXACTLY one
# interpolation, the container key and the interpolated `.env` name must be the
# same string. Composite values (a public URL assembled from scheme + domain)
# are excluded by the rule itself and are not aliases, so `compose_service_env`
# records an empty alias for them and they are skipped here.
#
# Why the rule is worth enforcing: `LEFT: ${RIGHT}` with LEFT != RIGHT is the
# deployment spelling a name twice, and the two spellings drift. It also makes
# a host rename undetectable in the direction that matters - every one of these
# carries a `:-default`, so a host that still sets only the old name renders
# green and silently takes the built-in default.
check_compose_alias_equality() {
    local compose="$1" label="$2"
    local rows bad=0 checked=0
    rows="$(compose_service_env "$compose")"
    if [ -z "$rows" ]; then
        fail "$label: extracted zero environment variables from $compose"
        return 1
    fi
    while IFS=$'\t' read -r svc _bin key alias; do
        [ -n "$key" ] || continue
        [ -n "$alias" ] || continue
        checked=$((checked + 1))
        if [ "$key" != "$alias" ]; then
            echo "  $svc: $key is fed by \${$alias}; a one-to-one alias must use the same name"
            bad=$((bad + 1))
        fi
    done <<<"$rows"
    # Anti-hollow: the rule is vacuous if the value half of the extractor stops
    # matching, and that failure looks exactly like compliance.
    if [ "$checked" -lt 20 ]; then
        fail "$label: only $checked one-to-one aliases found; the value extraction stopped matching"
        return 1
    fi
    if [ "$bad" -ne 0 ]; then
        fail "$label: $bad container variable(s) are fed by a differently-named .env variable"
        return 1
    fi
    pass "$label: all $checked one-to-one compose aliases satisfy LEFT == RIGHT"
    return 0
}

# No compose `command:` / `entrypoint:` item may carry a credential.
#
# ARGV IS NOT PRIVATE. `docker inspect`, `docker ps --no-trunc`, `ps` and
# /proc/<pid>/cmdline all publish it to anything sharing the PID namespace, and
# it lands in shell history and CI logs besides. That is why the platform's own
# rule - stated at deploy/compose/docker-compose.yml "No secret is passed as a
# flag" and implemented by `Secret<T>` generating `--<name>-file PATH` and no
# value flag - keeps credentials out of command lines by CONSTRUCTION for every
# binary that carries a `#[zeroship_config]` declaration.
#
# This check exists for the binaries that do NOT, and for the deploy files a
# macro cannot reach. It is the argv half of check 6.
#
# BOTH DETECTORS ARE DERIVED, NOT LISTED. An allowlist of "the credentials we
# know about" is satisfiable by editing the allowlist, which is worse than no
# check because it reads as coverage.
#
#   A. USERINFO IN A URL - `scheme://user:secret@host`. This is the product's
#      OWN classification rule, not one invented here: crates/migrated/src/
#      config.rs classes every DSN `Secret<String>` and says why - "Secret-
#      classed by grammar: a DSN admits userinfo, so the type cannot depend on
#      whether a particular deployment's value happens to carry a password."
#      The same grammar decides it here.
#
#   B. A SECRET'S VALUE FLAG - a `Secret<T>` projects to `--<name>-file` and
#      nothing else, so a command item spelling `--<name>` for any secret the
#      COMPILED contract declares is passing the material where only a path may
#      go. The set comes from the contract dump, so a new secret is covered
#      without anyone editing this file.
#
# WHAT THIS DOES NOT CHECK, so a green is not over-read:
#   - a bare high-entropy literal with no flag and no URL grammar to mark it.
#     Nothing distinguishes it from an opaque operational value, and a detector
#     that guesses would fire on every image digest and base64 config blob.
#   - `environment:` values. Those are checks 6 and 6b; env is a different and
#     narrower exposure than argv, not a safe one.
#   - what a container does with an item AFTER parsing it (a shell block that
#     reads a mounted file and re-exports it, say). This is a text check on the
#     deploy file, and the mount surface is deploy/scripts/deploy-remote.sh's.
check_compose_command_secrets() {
    local compose="$1" label="$2" contract="$3"
    local rows secret_flags bad=0 checked=0

    rows="$(compose_command_items "$compose")"
    if [ -z "$rows" ]; then
        fail "$label: extracted zero command items from $compose"
        return 1
    fi

    # Every secret's value-flag spelling: the declared `--<name>-file` with the
    # `-file` suffix removed. Empty is not a failure here (it only disarms
    # detector B); the floor below is what catches a broken extraction.
    secret_flags="$(awk -F'\t' '$3 == "secret" && $4 ~ /-file$/ { sub(/-file$/, "", $4); print $4 }' \
        "$contract" | sort -u)"

    while IFS=$'\t' read -r svc line item; do
        [ -n "$item" ] || continue
        checked=$((checked + 1))
        # A: a URL carrying userinfo.
        if printf '%s' "$item" | grep -qE '[a-zA-Z][a-zA-Z0-9+.-]*://[^/@[:space:]"]*:[^/@[:space:]"]*@'; then
            echo "  $compose:$line ($svc) passes a URL with userinfo in argv: $item"
            bad=$((bad + 1))
            continue
        fi
        # B: a declared secret's value flag rather than its `-file` path flag.
        [ -n "$secret_flags" ] || continue
        local head="${item%%[[:space:]]*}"
        case "$head" in
            --*)
                if printf '%s\n' "$secret_flags" | grep -qxF -- "$head"; then
                    echo "  $compose:$line ($svc) passes $head in argv; a secret has only a ${head}-file path flag"
                    bad=$((bad + 1))
                fi
                ;;
        esac
    done <<<"$rows"

    # Anti-hollow: every arm above passes at zero if the extractor stops
    # matching, and that failure is indistinguishable from compliance.
    # MEASURED on deploy/compose/docker-compose.yml 2026-08-16: 84 items.
    local min_items="${COMPOSE_COMMAND_MIN_ITEMS:-60}"
    if [ "$checked" -lt "$min_items" ]; then
        fail "$label: only $checked command items scanned, expected at least $min_items"
        echo "      The command extraction stopped matching, so a clean result would mean nothing."
        return 1
    fi
    if [ "$bad" -ne 0 ]; then
        fail "$label: $bad credential(s) reachable from a process argument list"
        return 1
    fi
    pass "$label: all $checked command items are free of argv-borne credentials"
    return 0
}

check_ops_toml() {
    local file="$1" label="$2" contract="$3"
    local bad=0 checked=0 leaf
    while read -r leaf; do
        [ -n "$leaf" ] || continue
        checked=$((checked + 1))
        if echo "$FILE_ONLY_OVERLAY_LEAVES" | grep -q "^$leaf "; then
            continue
        fi
        if ! awk -F'\t' -v t="$leaf" '$6==t {found=1} END{exit !found}' "$contract"; then
            echo "  $file carries $leaf, which is not a generated overlay path"
            bad=$((bad + 1))
        fi
    done < <(toml_leaves "$file")
    if [ "$checked" -eq 0 ]; then
        fail "$label: extracted zero leaves from $file"
        return 1
    fi
    if [ "$bad" -ne 0 ]; then
        fail "$label: $bad leaf/leaves in $file are not overlay paths"
        return 1
    fi
    pass "$label: all $checked leaves in $file are generated overlay paths"
    return 0
}

# ---------------------------------------------------------------------------
# Drive
# ---------------------------------------------------------------------------

echo "=== Build the compiled checker ==="
if cargo build -q -p zeroship-config-contract 2>"$TMP/build.log"; then
    pass "built zeroship-config-contract"
else
    fail "built zeroship-config-contract"
    sed 's/^/  /' "$TMP/build.log"
    exit 1
fi
BIN="$ROOT/target/debug/zeroship-config-contract"

if ! "$BIN" contract >"$TMP/contract.tsv" 2>"$TMP/contract.err"; then
    fail "dumped the compiled contract"
    sed 's/^/  /' "$TMP/contract.err"
    exit 1
fi
CONTRACT_ROWS=$(($(wc -l <"$TMP/contract.tsv") - 1))
if [ "$CONTRACT_ROWS" -lt 150 ]; then
    fail "the compiled contract has only $CONTRACT_ROWS projections; every check below joins"
    echo "      against it, so a short dump would make all of them pass on nothing."
    exit 1
fi
pass "compiled contract dumped: $CONTRACT_ROWS projections"

if [ "${1:-}" = "--self-test" ]; then
    echo ""
    echo "=== Self-test: the text checks must FAIL on a planted violation ==="
    mkdir -p "$TMP/self"
    sed 's/^      ZEROSHIP_CONTROL_KEY:/      ZEROSHIP_CONTROL_NOT_A_SETTING:/' \
        "$COMPOSE" >"$TMP/self/compose.yml"
    if ! grep -q ZEROSHIP_CONTROL_NOT_A_SETTING "$TMP/self/compose.yml"; then
        fail "self-test: the compose mutation did not apply; the run below proves nothing"
        exit 1
    fi
    before=$FAIL
    check_compose "$TMP/self/compose.yml" "compose self-test" "$TMP/contract.tsv" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: an undeclared ZEROSHIP_* compose variable is rejected"
    else
        fail "self-test: the compose check PASSED a variable no binary declares"
    fi

    # Alias equality: repoint ONE container variable at a differently-named
    # .env variable. This is the exact shape the eight renamed DSNs had.
    sed "s|\${ZEROSHIP_CONTROL_DATABASE_URL:-|\${CONTROL_DATABASE_URL:-|" \
        "$COMPOSE" >"$TMP/self/alias.yml"
    if ! grep -q 'ZEROSHIP_CONTROL_DATABASE_URL: ${CONTROL_DATABASE_URL:-' "$TMP/self/alias.yml"; then
        fail "self-test: the alias mutation did not apply; the run below proves nothing"
        exit 1
    fi
    before=$FAIL
    check_compose_alias_equality "$TMP/self/alias.yml" "alias self-test" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: a container variable fed by a differently-named .env variable is rejected"
    else
        fail "self-test: the alias check PASSED a LEFT != RIGHT pair"
    fi

    # Check 6c, detector A: a credential-bearing URL in a `command:` list item.
    # This is the exact shape that sat unnoticed in the migrate one-shot -- a
    # postgres DSN with userinfo, passed as the value half of a flag pair.
    sed 's|^      - /data/app-storage$|      - postgres://postgres:hunter2@postgres:5432/zeroship|' \
        "$COMPOSE" >"$TMP/self/cmd_url.yml"
    if ! grep -q 'postgres:hunter2@postgres' "$TMP/self/cmd_url.yml"; then
        fail "self-test: the command-URL mutation did not apply; the run below proves nothing"
        exit 1
    fi
    before=$FAIL
    check_compose_command_secrets "$TMP/self/cmd_url.yml" "command-url self-test" "$TMP/contract.tsv" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: a userinfo-bearing URL in a command block is rejected"
    else
        fail "self-test: the command check PASSED a DSN with an embedded password"
    fi

    # Check 6c, detector B: a declared secret spelled as a VALUE flag. The
    # contract gives `--control-key-file` and no `--control-key`, so the latter
    # can only be material.
    sed 's|^        --allow-unsupported-billing$|        --control-key t0psecret|' \
        "$COMPOSE" >"$TMP/self/cmd_flag.yml"
    if ! grep -q -- '--control-key t0psecret' "$TMP/self/cmd_flag.yml"; then
        fail "self-test: the command-flag mutation did not apply; the run below proves nothing"
        exit 1
    fi
    before=$FAIL
    check_compose_command_secrets "$TMP/self/cmd_flag.yml" "command-flag self-test" "$TMP/contract.tsv" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: a secret's value flag in a command block is rejected"
    else
        fail "self-test: the command check PASSED a secret passed as a value flag"
    fi

    printf '[control]\nnot_a_real_setting = 1\n' >"$TMP/self/ops.toml"
    before=$FAIL
    check_ops_toml "$TMP/self/ops.toml" "ops-toml self-test" "$TMP/contract.tsv" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: an overlay leaf outside the contract is rejected"
    else
        fail "self-test: the ops-toml check PASSED a leaf no declaration produces"
    fi

    # And the one-variable partner: the SAME checks on the real inputs must pass,
    # or the mutations above proved only that the checks reject everything.
    check_compose "$COMPOSE" "compose control" "$TMP/contract.tsv"
    check_compose_alias_equality "$COMPOSE" "alias control"
    check_compose_command_secrets "$COMPOSE" "command control" "$TMP/contract.tsv"
    check_ops_toml "deploy/ops/zeroship.toml" "ops-toml control" "$TMP/contract.tsv"

    echo ""
    echo "Self-test summary: $PASS passed, $FAIL failed"
    [ "$FAIL" -eq 0 ] || exit 1
    exit 0
fi

echo ""
echo "=== 1. Every workspace bin target is classified ==="
if "$BIN" check-metadata >"$TMP/meta.log" 2>&1; then
    pass "$(cat "$TMP/meta.log")"
else
    fail "cargo-metadata classification"
    sed 's/^/  /' "$TMP/meta.log"
fi

echo ""
echo "=== 2+3. Compiled contract, and the source extraction that must equal it ==="
if "$BIN" audit >"$TMP/audit.log" 2>&1; then
    pass "$(tail -1 "$TMP/audit.log")"
else
    fail "compiled contract vs source extraction"
    sed 's/^/  /' "$TMP/audit.log"
fi

echo ""
echo "=== 4. docs/reference/env-vars.md is a fresh render ==="
if "$BIN" env-vars-doc --check >"$TMP/doc.log" 2>&1; then
    pass "$(tail -1 "$TMP/doc.log")"
else
    fail "the generated region of docs/reference/env-vars.md is stale"
    sed 's/^/  /' "$TMP/doc.log"
fi

echo ""
echo "=== 5. No undeclared raw environment read ==="
if "$BIN" raw-env --gate >/dev/null 2>"$TMP/rawenv.log"; then
    pass "$(tail -1 "$TMP/rawenv.log")"
else
    fail "raw environment access"
    sed 's/^/  /' "$TMP/rawenv.log"
fi

echo ""
echo "=== 6. Compose sets only variables the receiving binary declares ==="
check_compose "$COMPOSE" "compose" "$TMP/contract.tsv"

echo ""
echo "=== 6b. Compose one-to-one aliases satisfy LEFT == RIGHT ==="
check_compose_alias_equality "$COMPOSE" "compose alias equality"

echo ""
echo "=== 6c. No compose command/entrypoint item carries a credential ==="
check_compose_command_secrets "$COMPOSE" "compose command argv" "$TMP/contract.tsv"

echo ""
echo "=== 7. Every ops-TOML leaf is a generated overlay path ==="
for file in deploy/ops/zeroship.toml deploy/ops/zeroship.example.toml; do
    [ -f "$file" ] || { fail "expected $file to exist"; continue; }
    check_ops_toml "$file" "ops-toml" "$TMP/contract.tsv"
done

echo ""
echo "============================================"
echo "Summary: $PASS passed, $FAIL failed"
echo "============================================"

[ "$FAIL" -eq 0 ] || exit 1

# ANTI-HOLLOW FLOOR. Every check above passes at zero if its extraction stops
# matching, and this catches the case where several do at once. MEASURED on a
# clean tree 2026-08-13: 9, then 10 once check 6b was armed, then 11 once 6c was.
CONFIG_GATE_MIN_PASSED="${CONFIG_GATE_MIN_PASSED:-11}"
if [ "$PASS" -lt "$CONFIG_GATE_MIN_PASSED" ]; then
    echo "" >&2
    echo "FLOOR: only $PASS checks passed, expected at least $CONFIG_GATE_MIN_PASSED." >&2
    echo "  Nothing FAILED, so this is not a broken check - it is MISSING ones." >&2
    exit 1
fi
