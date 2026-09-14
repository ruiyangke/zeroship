#!/usr/bin/env bash
# The configuration-name contract, as one CI-blocking gate.
#
# WHAT IT CHECKS, and where each half comes from:
#
#   1 cargo-metadata     every workspace bin target is a registered platform
#                        binary or an explicitly classified non-platform one
#   2 compiled contract  the six linked ConfigSpec registries have no colliding
#                        projection, no declared-but-unread source and no
#                        undeclared reader (crates/zeroship-config-contract/src/contract.rs)
#   4 the reference      docs/reference/env-vars.md's generated region equals a
#                        fresh render of the compiled contract
#   6 Compose           every ZEROSHIP_* variable a platform service sets is a
#                        name that exact binary declares
#   6b alias equality    a container value that is EXACTLY one interpolation
#                        must satisfy LEFT == RIGHT (proposal Section 4.3)
#   6c command argv      no `command:`/`entrypoint:` item carries a credential
#   6d environment       no `environment:` value inlines the cluster superuser's
#                        credential (the env half of 6c's detector A)
#   7 ops TOML           every leaf in deploy/ops/*.toml is a generated overlay
#                        path with a real consumer
#   8 tracked secrets    no secret-classed leaf in ANY tracked *.toml holds a
#                        plaintext literal (proposal Section 4.7)
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
# Check 8 was armed on 2026-08-17. It is the static half of a trade the
# 2026-08-12 amendment made and only half executed: the runtime refusal of a
# plaintext overlay secret was deleted on the understanding that this gate
# would replace it, and until now it did not exist. See its own header.
#
# Check 6d was armed on 2026-08-21 against the last of the credential defaults
# check 8's header had counted and deferred: `migrated`'s provisioning DSN
# carried the postgres SUPERUSER inline. Its own header says why it stops at the
# superuser and what that leaves standing.
#
# Run `tests/config_name_alignment_gate.sh --self-test` to prove checks 6, 7
# and 8 still FAIL on a planted violation. A gate that accepts everything and a
# gate that is broken print the same thing.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init config_name_alignment

COMPOSE="deploy/compose/docker-compose.yml"
PLATFORM_IMAGE="zeroship-platform:dev"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }

# Non-zeroship variables a platform service is allowed to carry. Each is an
# EXTERNAL or ambient input with no ConfigSpec by construction, so the registry
# cannot produce it and a reason has to be written down instead.
#
# SCOPED TO THE PLATFORM IMAGE, which is why there are three entries and not
# six. `compose_service_env` emits rows only for services whose `image` is
# $PLATFORM_IMAGE, so the postgres container's own POSTGRES_DB / POSTGRES_USER /
# POSTGRES_PASSWORD never reach this check at all. They were listed here anyway
# and matched nothing - measured 2026-08-20, the complete set of non-ZEROSHIP
# keys reaching this arm is the three below, all on `control`. A list half of
# which cannot fire reads as a wider allowance than the code grants, and the
# reverse-direction check below now fails an entry that stops firing.
AMBIENT_COMPOSE_KEYS="
SANDBOX_URL      control reaches the extracted zeroship-sandbox project over HTTP
SANDBOX_TOKEN    the same, its bearer
OPENAI_API_KEY   forwarded to creator apps; the platform itself does not read it
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
# crates/zeroship-core/src/config/file.rs and deploy/compose/docker-compose.yml.
FILE_ONLY_OVERLAY_LEAVES="
auth.trusted_oauth_clients  file-and-default only: no flag and no env by design (crates/zeroship-core/src/config/file.rs)
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

# Emit `service<TAB>line<TAB>KEY<TAB>value` for every explicit `environment:`
# assignment, on EVERY service.
#
# WHY THIS EXTRACTOR EXISTS ALONGSIDE `compose_service_env`. That one walks the
# same blocks and throws away all three things check 6d needs. It drops the
# VALUE unless the value is exactly one interpolation (a composite is recorded
# as an empty alias, which is precisely the `${NAME:-<default>}` shape a
# credential default has); it drops the LINE NUMBER, so a violation could not be
# pointed at; and it emits rows only for services running $PLATFORM_IMAGE, so
# the backing services are outside it entirely. This is the same relationship
# `toml_assignments` has to `toml_leaves` a few functions down: the value half
# is what nothing could see.
#
# The KEY and VALUE derivations are copied CHARACTER FOR CHARACTER from
# `compose_service_env` above, so the two extractors cannot come to disagree
# about where a key ends and a value begins.
compose_env_values() {
    awk '
        /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ {
            svc = $1; sub(/:$/, "", svc); inenv = 0; next
        }
        svc == "" { next }
        /^    environment:/ { inenv = 1; next }
        /^    [a-zA-Z_]+:/ { inenv = 0; next }
        inenv && match($0, /^      -?[[:space:]]*[A-Z][A-Z0-9_]*/) {
            key = substr($0, RSTART, RLENGTH)
            gsub(/[ -]/, "", key)
            val = $0
            sub(/^[^:]*:[[:space:]]*/, "", val)
            sub(/[[:space:]]+$/, "", val)
            print svc "\t" NR "\t" key "\t" val
            next
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
# postgres SUPERUSER DSN through the value form of `--database-url`, readable
# through `docker inspect`, `docker ps --no-trunc` and /proc/<pid>/cmdline for
# anything in that PID namespace. Nothing failed, because nothing looked.
# That flag no longer exists - the one-shot declares its DSN `Secret<String>`,
# which generates only a `-file` carrier - so this extractor now guards the
# shape rather than that one instance of it.
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

# Emit `line<TAB>section.leaf<TAB>value` for every scalar assignment in a TOML
# file. The VALUE half is what `toml_leaves` below throws away, and throwing it
# away is why no check in this script could see a secret sitting in a tracked
# overlay until check 8 was written.
#
# A quoted scalar ends at its closing quote - BOTH TOML quote forms, basic `"`
# and literal `'` - so a `#` INSIDE the value stays part of the value. Getting
# that wrong would truncate a secret at its first `#`. Handling `'` matters in
# the other direction too: without it a legitimate `'urn:zeroship:file:/x'`
# keeps its opening quote, fails the prefix test and reads as a violation.
toml_assignments() {
    awk '
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*\[/ {
            sec = $0
            sub(/#.*$/, "", sec)
            gsub(/[][[:space:]]/, "", sec)
            next
        }
        /^[[:space:]]*[A-Za-z_"][A-Za-z0-9_."-]*[[:space:]]*=/ {
            key = $0
            sub(/=.*$/, "", key)
            gsub(/[[:space:]"]/, "", key)
            val = $0
            sub(/^[^=]*=[[:space:]]*/, "", val)
            quote = substr(val, 1, 1)
            if (quote == "\"" || quote == "'\''") {
                val = substr(val, 2)
                q = index(val, quote)
                if (q > 0) val = substr(val, 1, q - 1)
            } else {
                sub(/[[:space:]]*#.*$/, "", val)
                sub(/[[:space:]]+$/, "", val)
            }
            print NR "\t" (sec == "" ? key : sec "." key) "\t" val
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
    local compose_arm="$1" compose="$2" label="$3" contract="$4"
    local rows services=0 bad=0 checked=0
    rows="$(compose_service_env "$compose")"
    if [ -z "$rows" ]; then
        fail "$label: extracted zero environment variables from $compose"
        return 1
    fi
    services="$(echo "$rows" | cut -f1 | sort -u | wc -l)"
    # The services this walk attributed rows to. MEASURED 2026-08-20 on
    # deploy/compose/docker-compose.yml: 5 - control, migrated, gateway, worker,
    # auth. The floor stays the 5 the hand-rolled test enforced, which is equal
    # to today's count and so will fail the day a platform service is legitimately
    # removed; that is deliberate here and was already the behaviour, because the
    # set is enumerated from `image: $PLATFORM_IMAGE` and losing one silently is
    # the failure this is for.
    if ! gate_arm "${compose_arm}_services" "$services" 5; then
        fail "$label: only $services platform services found (expected at least 5);"
        echo "      the extraction stopped matching, so a clean result would mean nothing."
        return 1
    fi
    local ambient_hit=""
    while IFS=$'\t' read -r svc bin key _alias; do
        [ -n "$key" ] || continue
        case "$key" in
            ZEROSHIP_*) ;;
            *)
                if echo "$AMBIENT_COMPOSE_KEYS" | grep -q "^$key "; then
                    ambient_hit="$ambient_hit $key"
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
    # THE POST-FILTER COUNT, and the distinction is the whole point: `rows` is 42
    # on the shipped compose and `checked` is 39, the three ambient keys having
    # been excused by AMBIENT_COMPOSE_KEYS above (measured 2026-08-20). A gate
    # that declared its pre-filter total is how skip_marker_gate.sh came to rule
    # on nothing while reporting 8 hits. Floor 20, as the hand-rolled test had.
    if ! gate_arm "$compose_arm" "$checked" 20; then
        fail "$label: only $checked zeroship variables checked across $services services"
        return 1
    fi
    if [ "$bad" -ne 0 ]; then
        fail "$label: $bad undeclared variable(s) across $services platform services"
        return 1
    fi
    # REVERSE DIRECTION on the ambient allowance. An entry that no longer
    # matches anything is a wider allowance than the code grants, and it reads
    # to the next person as a decision somebody is still making. Three of the
    # six entries here matched nothing on 2026-08-20 - they named the postgres
    # container's own variables, and this walk only ever sees services on the
    # platform image - so the list said "six exceptions" while granting three.
    local unused=""
    while read -r k _rest; do
        [ -n "$k" ] || continue
        case " $ambient_hit " in *" $k "*) ;; *) unused="$unused $k" ;; esac
    done <<<"$AMBIENT_COMPOSE_KEYS"
    if [ -n "$unused" ]; then
        fail "$label: AMBIENT_COMPOSE_KEYS allows$unused, which no platform service sets."
        echo "      Remove the entry, or find out why the extraction stopped seeing it."
        return 1
    fi
    pass "$label: $checked zeroship variables on $services platform services are all declared by the binary that reads them ($(echo $ambient_hit | wc -w) ambient key(s) allowed, all still present)"
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
    local alias_arm="$1" compose="$2" label="$3"
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
    #
    # POST-FILTER, and here the filter is the rule itself: a composite value (a
    # URL built from scheme plus domain) is not an alias and is recorded with an
    # empty alias field, so it is skipped rather than ruled on. MEASURED
    # 2026-08-20: 26 one-to-one aliases out of 42 environment rows. Floor 20, as
    # the hand-rolled test had.
    if ! gate_arm "$alias_arm" "$checked" 20; then
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
#      OWN classification rule, not one invented here: crates/zeroship-migrate-server/src/
#      config.rs classes every DSN `Secret<String>` and says why - "Secret-
#      classed by grammar: a DSN admits userinfo, so the type cannot depend on
#      whether a particular deployment's value happens to carry a password."
#      The same grammar decides it here.
#
#   B. A SECRET'S VALUE FLAG - a `Secret<T>` projects to `--<name>-file` and
#      nothing else, so a command item spelling `--<name>` for any secret the
#      COMPILED contract declares is passing the material where only a path may
#      go. The set comes from the contract dump, so a new secret is covered
#      without anyone editing this file. BOTH argv spellings count: `--flag
#      value` and `--flag=value`. Only the first was checked until 2026-09-04,
#      and the joined form is present in this very compose file (`--check=false`
#      at deploy/compose/docker-compose.yml:776), so the miss was one edit away
#      from being live rather than theoretical. `--self-test` now plants the
#      same secret in both spellings; the joined case fails against the
#      predicate this replaced.
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
    local argv_arm="$1" compose="$2" label="$3" contract="$4"
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
        #
        # THE FLAG IS THE TEXT BEFORE THE SEPARATOR, and argv has two of them:
        # `--flag value` is two elements, `--flag=value` is one. Taking only the
        # first whitespace-delimited token made the joined form compare as
        # `--control-key=t0psecret` against `--control-key` and miss - and the
        # joined form is not hypothetical for this file, which already carries
        # `--check=false`. Strip at whichever separator comes first.
        [ -n "$secret_flags" ] || continue
        local head="${item%%[[:space:]]*}"
        head="${head%%=*}"
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
    # RE-MEASURED 2026-08-20: 84.
    #
    # Every extracted item is ruled on by both detectors, so this is already the
    # post-filter number - the `secret_flags` join narrows detector B's evidence,
    # not the set of items examined.
    local min_items=60
    if ! gate_arm "$argv_arm" "$checked" "$min_items"; then
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

# The cluster SUPERUSER's role name, read out of the compose file's OWN postgres
# service. Emits nothing if that service, or its `environment:` block, cannot be
# found - the caller turns that into a refusal rather than a green.
#
# DERIVED, NOT WRITTEN DOWN. `POSTGRES_USER` is the name the postgres image
# creates as the bootstrap superuser; when it is unset the image creates
# `postgres`, and this file leaves it unset while its own healthcheck spells
# `pg_isready -U postgres`. Taking the declared value when there is one means a
# deployment that renames its superuser is still covered, with nothing to edit
# here. Finding `POSTGRES_PASSWORD` is what proves the environment block was
# actually reached: without it the fallback would be indistinguishable from an
# extractor that matched nothing.
compose_superuser_role() {
    local rows svc key value pg_svc="" user="" saw_password=0
    rows="$(compose_env_values "$1")"
    [ -n "$rows" ] || return 0
    pg_svc="$(awk '
        /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { svc = $1; sub(/:$/, "", svc); next }
        svc != "" && $1 == "image:" && $2 ~ /^postgres(:|$)/ { print svc; exit }
    ' "$1")"
    [ -n "$pg_svc" ] || return 0
    while IFS=$'\t' read -r svc _line key value; do
        [ "$svc" = "$pg_svc" ] || continue
        case "$key" in
            POSTGRES_PASSWORD) saw_password=1 ;;
            POSTGRES_USER) user="$value" ;;
        esac
    done <<<"$rows"
    [ "$saw_password" = 1 ] || return 0
    printf '%s\n' "${user:-postgres}"
}

# No compose `environment:` value may inline a SUPERUSER credential.
#
# THE ARGV HALF OF THIS RULE IS CHECK 6c, AND IT HAS BEEN GREEN OVER A HOLE.
# 6c's detector A refuses `scheme://user:secret@host` in a `command:` item and
# its own header says, correctly, that `environment:` is "a different and
# narrower exposure than argv, not a safe one". Check 8's header then counted
# what that leaves uncovered - eight inline `${NAME:-<default>}` secrets, six of
# them DSNs carrying userinfo - and deferred them as "a KNOWN, COUNTED hole".
# One of the six was `ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL`, whose default
# was the cluster SUPERUSER's DSN. This arm is the half that closes it.
#
# WHY SUPERUSER AND NOT ALL USERINFO, which is what 6c bans in argv. The other
# five are per-service LEAST-PRIVILEGE role DSNs (`zeroship_gateway`,
# `zeroship_auth`, ...), and their passwords are not merely also in the tree -
# they are set BY the tree, in `db/migrations-ts/20260702000100_
# schema_roles_extensions.ts` (`role("zeroship_control").create({ password:
# "zeroship_control" })`), a migration a deployed database has applied and which
# is therefore frozen. Banning them here would not remove one byte of credential
# from git; it would relocate five literals from this compose file into
# `secret_specs()` in crates/zeroship-cli/src/dev.rs, which would have to generate the
# same five strings for `zeroship dev init` to keep working. That is the check-8
# violation this gate exists to prevent, committed to buy a green here.
#
# The superuser is categorically different and the difference is not a judgement
# call: it can CREATE ROLE, DROP DATABASE and read every schema in the cluster,
# which is why `migrated` needs it at all and why no least-privilege role can
# stand in. A deployment hands it to a service or it does not, and a tracked
# file naming it inline hands it to everything that can read the file or the
# container's /proc/<pid>/environ.
#
# WHAT IT DOES NOT CHECK, so a green is not over-read:
#   - the five least-privilege DSN defaults above. Uncovered ON PURPOSE, for the
#     reason given; they remain plaintext credentials in a tracked file.
#   - `POSTGRES_PASSWORD: <literal>` on the postgres service itself. It is the
#     INPUT this arm derives the superuser's identity from, and it carries no
#     URL grammar - check 8's header records the same value as undetectable by
#     any registry-driven means. Refusing it would need a name heuristic.
#   - compose files other than the one passed in. Backing-service overlays
#     are not walked by any check here.
#   - what a container does after parsing. This is a text check on a deploy file.
check_compose_env_superuser() {
    local env_arm="$1" compose="$2" label="$3"
    local rows role bad=0 checked=0 svc line key value

    rows="$(compose_env_values "$compose")"
    if [ -z "$rows" ]; then
        fail "$label: extracted zero environment values from $compose"
        return 1
    fi
    role="$(compose_superuser_role "$compose")"
    if [ -z "$role" ]; then
        fail "$label: could not derive the cluster superuser from $compose"
        echo "      No service runs a postgres image with a POSTGRES_PASSWORD, so there is"
        echo "      nothing to compare against and a clean result would mean nothing."
        return 1
    fi

    while IFS=$'\t' read -r svc line key value; do
        [ -n "$key" ] || continue
        checked=$((checked + 1))
        # The same userinfo grammar check 6c applies to argv, narrowed to the
        # one role whose credential no deployment may inline.
        if printf '%s' "$value" \
            | grep -qE "[a-zA-Z][a-zA-Z0-9+.-]*://${role}:[^/@[:space:]\"]*@"; then
            echo "  $compose:$line ($svc) sets $key to a URL carrying the $role superuser's"
            echo "      credential; a secret-classed value takes urn:zeroship:file:<path>"
            echo "      naming a mounted file, never the material"
            bad=$((bad + 1))
        fi
    done <<<"$rows"

    # Anti-hollow: the detector passes at zero if the extractor stops matching,
    # and that is indistinguishable from a file with no environment blocks.
    # MEASURED on deploy/compose/docker-compose.yml 2026-08-21: 45 values across
    # seven services - the 42 check 6b sees on the platform image (control 13,
    # auth 9, gateway 8, migrated 6, worker 6) plus postgres 2 and caddy 1, and
    # those last three are exactly what dropping the image filter buys. Every
    # extracted value is ruled on, so this is already the post-filter number.
    # The floor is 30 rather than 45 because it must also hold for the mutated
    # copies --self-test drives this same function over.
    local min_values=30
    if ! gate_arm "$env_arm" "$checked" "$min_values"; then
        fail "$label: only $checked environment values scanned, expected at least $min_values"
        echo "      The environment extraction stopped matching, so a clean result would mean nothing."
        return 1
    fi
    if [ "$bad" -ne 0 ]; then
        fail "$label: $bad environment value(s) inline the $role superuser's credential"
        return 1
    fi
    pass "$label: all $checked environment values are free of an inline $role superuser credential"
    return 0
}

check_ops_toml() {
    local ops_arm="$1" file="$2" label="$3" contract="$4"
    local bad=0 checked=0 leaf
    while read -r leaf; do
        [ -n "$leaf" ] || continue
        if echo "$FILE_ONLY_OVERLAY_LEAVES" | grep -q "^$leaf "; then
            continue
        fi
        # Counted AFTER the exclusion, so this is the number of leaves whose
        # verdict the contract join actually decided. It used to be counted
        # before, which made it the raw leaf total: deploy/ops/zeroship.toml has
        # four leaves and ONE of them, auth.trusted_oauth_clients, is excused, so
        # the old number said 4 where 3 were ruled on. That gap is small here and
        # is the entire skip_marker_gate.sh failure at scale - 8 hits, 8 excused,
        # 0 ruled on, green.
        checked=$((checked + 1))
        if ! awk -F'\t' -v t="$leaf" '$6==t {found=1} END{exit !found}' "$contract"; then
            echo "  $file carries $leaf, which is not a generated overlay path"
            bad=$((bad + 1))
        fi
    done < <(toml_leaves "$file")
    # MEASURED 2026-08-20, post-exclusion: deploy/ops/zeroship.toml rules on 3 of
    # its 4 leaves, deploy/ops/zeroship.example.toml on 21 of its 22. The floor
    # stays the 1 the hand-rolled test enforced and cannot be raised towards
    # either number: the smaller file rules on 3, and --self-test drives this
    # same function over a two-line planted fixture holding exactly one leaf.
    if ! gate_arm "$ops_arm" "$checked" 1; then
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

# Proposal Section 4.7: no plaintext secret in a TRACKED file.
#
# WHY THIS EXISTS, and why its absence was a NET LOSS. Until 2026-08-12 a
# secret literal in the TOML overlay was refused at runtime. The 2026-08-12
# amendment (`2d31a4bc5`) deleted that refusal deliberately - the overlay may
# itself BE a mounted Kubernetes Secret, and forbidding a literal by mounted
# file while permitting one by environment had no principled basis - and moved
# the guarantee to a repository gate against the artifact that actually needs
# it. The gate was never written, so for five days the tree was weaker than it
# had been before the trade: nothing at all stopped
# `[control] master_key = "..."` from being committed.
#
# WHAT IT ASSERTS. For every TRACKED `*.toml`, a leaf whose canonical identity
# the COMPILED contract classes `secret` must hold a `urn:`/`arn:` reference
# and nothing else. The classification comes from the registry, so a new
# secret is covered the moment it is declared and nobody edits this file.
#
# NO EXCEPTION LIST, and there must never be one. An allowlist naming the
# secrets that are allowed to be literals is an allowlist for the exact thing
# the check exists to catch, which reads as coverage while providing none.
# There is no empty-value carve-out either: `master_key = ""` is a literal.
#
# WHY IT SCANS EVERY TRACKED `*.toml` AND NOT `deploy/ops/*.toml`. A file list
# is a thing to forget when the next overlay lands somewhere else. Leaves that
# do not join a secret-classed contract row are skipped, so the 40-odd
# `Cargo.toml` files cost a join and contribute nothing.
#
# WHAT IT DOES NOT CHECK, so a green is not over-read:
#
#   - UNTRACKED overlays. Deliberate, and the whole point of the amendment: a
#     mounted Kubernetes Secret never enters git. The gate says nothing about
#     files it cannot see.
#
#   - Compose `environment:` values. MEASURED 2026-08-17 on
#     deploy/compose/docker-compose.yml: 24 secret-classed keys, of which 2
#     carry a `urn:zeroship:file:` reference, 13 are `${NAME:?...}`, 1 is
#     `${NAME:-}`, and 8 carry an inline `${NAME:-<default>}`. SIX of those
#     defaults were DSNs with userinfo, so they ARE plaintext credentials in a
#     tracked file by the same userinfo-grammar rule check 6c applies to argv.
#     FIVE still are; they are per-service LEAST-PRIVILEGE role DSNs and check
#     6d's header says why moving them would relocate the literal rather than
#     remove it. The sixth - `migrated`'s SUPERUSER provisioning DSN - is closed
#     as of 2026-08-21 and check 6d now refuses its return.
#
#     THE REASON THIS PARAGRAPH GAVE FOR DEFERRING ALL SIX WAS FALSE WHEN
#     WRITTEN. It said "`zeroship dev init` does not write a single DSN
#     (crates/zeroship-cli/src/dev.rs:36-43), so `docker compose up` would stop working
#     for every local developer". Those lines are the doc comment of
#     `env_keys()`, which enumerates the .env SECRETS; the DSN is written by the
#     other producer in the same file, `secret_specs()`, whose FIRST entry is
#     `("migrate-dsn", generate_migrate_dsn, validate_migrate_dsn)` - and the
#     compose file has mounted that very file into the `migrate` one-shot since
#     2026-08-16. A citation to real lines that do not say what the sentence
#     claims reads as evidence and is not; the fix cost one mount and one
#     `urn:zeroship:file:` reference.
#
#   - Values with no canonical identity. `POSTGRES_PASSWORD: zeroship` at
#     deploy/compose/docker-compose.yml:108 is a plaintext credential in a
#     tracked file, and no registry-driven gate can see it: the postgres
#     image's own variable has no ConfigSpec to classify. Detecting it would
#     need a name heuristic, which Section 4.7 rules out by design.
#
#   - Tracked `*.env` files. `git ls-files '*.env'` returns NOTHING (measured
#     2026-08-17), so an arm for them would scan zero files and pass on
#     nothing. It is left unwritten rather than written and hollow.
check_tracked_secret_literals() {
    local secret_arm="$1" label="$2" contract="$3"
    shift 3
    local secret_paths bad=0 resolved=0 file line leaf value

    secret_paths="$(awk -F'\t' 'NR > 1 && $3 == "secret" && $6 != "" { print $6 }' \
        "$contract" | sort -u)"
    if [ -z "$secret_paths" ]; then
        fail "$label: the contract declares no secret-classed overlay path; nothing could be checked"
        return 1
    fi
    # The file enumeration, which is `git ls-files '*.toml'` at the call site and
    # so can go to zero without anything else changing. MEASURED 2026-08-20: 44
    # tracked TOML files. The floor is the 1 the hand-rolled test enforced and
    # cannot be raised towards 44, because --self-test calls this same function
    # with a SINGLE planted file to prove it still rejects a literal.
    if ! gate_arm "${secret_arm}_files" "$#" 1; then
        fail "$label: no tracked TOML files were passed; the file enumeration stopped matching"
        return 1
    fi

    for file in "$@"; do
        [ -f "$file" ] || continue
        while IFS=$'\t' read -r line leaf value; do
            [ -n "$leaf" ] || continue
            printf '%s\n' "$secret_paths" | grep -qxF "$leaf" || continue
            resolved=$((resolved + 1))
            case "$value" in
                urn:*|arn:*) ;;
                *)
                    # The value is NOT echoed: this runs in CI, and printing the
                    # material would publish it a second time.
                    echo "  $file:$line $leaf is secret-classed and holds a plaintext literal;"
                    echo "      a tracked file may hold only a urn:/arn: reference"
                    bad=$((bad + 1))
                    ;;
            esac
        done < <(toml_assignments "$file")
    done

    # Anti-hollow: every arm above passes at zero if the extractor, the join or
    # the file enumeration stops matching, and that failure is indistinguishable
    # from a tree with no secrets in it.
    # MEASURED on a clean tree 2026-08-17: 17, all in deploy/ops/zeroship.example.toml.
    #
    # `resolved` is the POST-JOIN count and the right one to declare: 44 files go
    # in and several hundred assignments are read, but only the leaves that match
    # a secret-classed contract row get a verdict. Declaring the assignment total
    # would stay comfortably above any floor while the join itself matched
    # nothing.
    local min_resolved=12
    if ! gate_arm "$secret_arm" "$resolved" "$min_resolved"; then
        fail "$label: only $resolved secret-classed leaves resolved, expected at least $min_resolved"
        echo "      The extraction or the contract join stopped matching, so a clean result would mean nothing."
        return 1
    fi
    if [ "$bad" -ne 0 ]; then
        fail "$label: $bad secret-classed leaf/leaves hold a plaintext literal in a tracked file"
        return 1
    fi
    pass "$label: all $resolved secret-classed leaves in tracked TOML are urn:/arn: references"
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
# THE JOIN KEY EVERY CHECK BELOW USES, so it is declared as an arm of its own:
# checks 6, 6c, 7 and 8 all resolve their verdict against these rows, and a short
# dump makes every one of them pass on nothing at once. The floor of 150 is
# carried over verbatim from the hand-rolled test this replaces; the real count
# could not be re-measured on 2026-08-20 because zeroship-config-contract does
# not build in this worktree (crates/runtime needs `pnpm build` first), which is
# also why this gate is red here.
if ! gate_arm contract_projections "$CONTRACT_ROWS" 150; then
    fail "the compiled contract has only $CONTRACT_ROWS projections; every check below joins"
    echo "      against it, so a short dump would make all of them pass on nothing."
    exit 1
fi
pass "compiled contract dumped: $CONTRACT_ROWS projections"

# Every tracked TOML, for check 8. Enumerated from git rather than listed, so a
# new overlay is covered wherever it lands.
mapfile -t TRACKED_TOML < <(git ls-files '*.toml')

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
    check_compose compose_mutation "$TMP/self/compose.yml" "compose self-test" "$TMP/contract.tsv" >/dev/null 2>&1
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
    check_compose_alias_equality alias_mutation "$TMP/self/alias.yml" "alias self-test" >/dev/null 2>&1
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
    check_compose_command_secrets argv_url_mutation "$TMP/self/cmd_url.yml" "command-url self-test" "$TMP/contract.tsv" >/dev/null 2>&1
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
    check_compose_command_secrets argv_flag_mutation "$TMP/self/cmd_flag.yml" "command-flag self-test" "$TMP/contract.tsv" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: a secret's value flag in a command block is rejected"
    else
        fail "self-test: the command check PASSED a secret passed as a value flag"
    fi

    # Check 6c, detector B, THE OTHER SPELLING. `--flag=value` is one argv
    # element, not two, and the detector took the first whitespace-delimited
    # token as the flag - so `--control-key=t0psecret` compared as a whole
    # against `--control-key` and did not match. The joined form is not
    # hypothetical for this file: deploy/compose/docker-compose.yml already
    # carries `--check=false`. Same secret, same block, one variable changed.
    sed 's|^        --allow-unsupported-billing$|        --control-key=t0psecret|' \
        "$COMPOSE" >"$TMP/self/cmd_flag_eq.yml"
    if ! grep -q -- '--control-key=t0psecret' "$TMP/self/cmd_flag_eq.yml"; then
        fail "self-test: the joined command-flag mutation did not apply; the run below proves nothing"
        exit 1
    fi
    before=$FAIL
    check_compose_command_secrets argv_flag_eq_mutation "$TMP/self/cmd_flag_eq.yml" "command-flag-joined self-test" "$TMP/contract.tsv" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: a secret's value flag spelled --flag=value is rejected"
    else
        fail "self-test: the command check PASSED a secret spelled --flag=value"
    fi

    # Check 6d: the SUPERUSER credential inlined as an `environment:` default.
    # It is planted at `migrate-server`'s
    # ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL, the shape check
    # 6c cannot see because it reads `command:` blocks alone. The planted
    # password is deliberately NOT the one the postgres service carries, so this
    # also proves the detector keys on the superuser ROLE and not on a value it
    # happened to find elsewhere in the file.
    sed 's|^      ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL: .*|      ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL: ${ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL:-postgres://postgres:hunter2@postgres:5432/zeroship}|' \
        "$COMPOSE" >"$TMP/self/env_superuser.yml"
    if ! grep -q 'PROVISION_DATABASE_URL: ${ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL:-postgres://postgres:hunter2@' "$TMP/self/env_superuser.yml"; then
        fail "self-test: the environment-superuser mutation did not apply; the run below proves nothing"
        exit 1
    fi
    before=$FAIL
    check_compose_env_superuser env_superuser_mutation "$TMP/self/env_superuser.yml" "environment-superuser self-test" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: an inline superuser DSN in an environment default is rejected"
    else
        fail "self-test: the environment check PASSED a superuser DSN default"
    fi

    printf '[control]\nnot_a_real_setting = 1\n' >"$TMP/self/ops.toml"
    before=$FAIL
    check_ops_toml ops_mutation "$TMP/self/ops.toml" "ops-toml self-test" "$TMP/contract.tsv" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: an overlay leaf outside the contract is rejected"
    else
        fail "self-test: the ops-toml check PASSED a leaf no declaration produces"
    fi

    # Check 8: a secret-classed leaf holding a literal in a TRACKED file. This
    # is the exact shape the deleted runtime refusal used to catch and that
    # nothing caught between 2026-08-12 and this arm: the reference is replaced
    # by material, the leaf still joins the contract, and every other check is
    # blind to it because none of them read a value.
    sed 's|^master_key = .*|master_key = "PLANTED-LITERAL-NOT-A-REFERENCE"|' \
        deploy/ops/zeroship.example.toml >"$TMP/self/secret_literal.toml"
    if ! grep -q '^master_key = "PLANTED-LITERAL-NOT-A-REFERENCE"$' "$TMP/self/secret_literal.toml"; then
        fail "self-test: the secret-literal mutation did not apply; the run below proves nothing"
        exit 1
    fi
    before=$FAIL
    check_tracked_secret_literals secret_mutation "secret-literal self-test" "$TMP/contract.tsv" \
        "$TMP/self/secret_literal.toml" >/dev/null 2>&1
    if [ "$FAIL" -gt "$before" ]; then
        FAIL=$before
        pass "self-test: a secret-classed leaf holding a plaintext literal is rejected"
    else
        fail "self-test: the tracked-secret check PASSED a secret written as a literal"
    fi

    # And the one-variable partner: the SAME checks on the real inputs must pass,
    # or the mutations above proved only that the checks reject everything.
    check_compose compose_control "$COMPOSE" "compose control" "$TMP/contract.tsv"
    check_compose_alias_equality alias_control "$COMPOSE" "alias control"
    check_compose_command_secrets argv_control "$COMPOSE" "command control" "$TMP/contract.tsv"
    check_compose_env_superuser env_superuser_control "$COMPOSE" "environment-superuser control"
    check_ops_toml ops_control "deploy/ops/zeroship.toml" "ops-toml control" "$TMP/contract.tsv"
    check_tracked_secret_literals secret_control "tracked-secret control" "$TMP/contract.tsv" "${TRACKED_TOML[@]}"

    echo ""
    echo "Self-test summary: $PASS passed, $FAIL failed"
    # --self-test is a completed run and owes the same trailer. Its arms are the
    # twelve above: the six mutated inputs and the six real ones, each with its
    # own id, so a mutation whose fixture stopped resembling the real file shows
    # up here as a count that no longer matches its control.
    arms_rc=0
    gate_arms_finish || arms_rc=1
    [ "$FAIL" -eq 0 ] || exit 1
    [ "$arms_rc" -eq 0 ] || exit 1
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
echo "=== 4. docs/reference/env-vars.md is a fresh render ==="
if "$BIN" env-vars-doc --check >"$TMP/doc.log" 2>&1; then
    pass "$(tail -1 "$TMP/doc.log")"
else
    fail "the generated region of docs/reference/env-vars.md is stale"
    sed 's/^/  /' "$TMP/doc.log"
fi

echo ""
echo "=== 6. Compose sets only variables the receiving binary declares ==="
check_compose compose "$COMPOSE" "compose" "$TMP/contract.tsv"

echo ""
echo "=== 6b. Compose one-to-one aliases satisfy LEFT == RIGHT ==="
check_compose_alias_equality compose_alias "$COMPOSE" "compose alias equality"

echo ""
echo "=== 6c. No compose command/entrypoint item carries a credential ==="
check_compose_command_secrets compose_argv "$COMPOSE" "compose command argv" "$TMP/contract.tsv"

echo ""
echo "=== 6d. No compose environment value inlines the superuser credential ==="
check_compose_env_superuser compose_env "$COMPOSE" "compose environment"

echo ""
echo "=== 7. Every ops-TOML leaf is a generated overlay path ==="
for file in deploy/ops/zeroship.toml deploy/ops/zeroship.example.toml; do
    [ -f "$file" ] || { fail "expected $file to exist"; continue; }
    # The arm id is DERIVED from the file, not written out beside it, so a third
    # overlay added to this loop gets its own arm without anyone remembering to
    # name one - and two files can never end up sharing an id and vouching for
    # each other's count. `zeroship.example.toml` yields ops_zeroship_example.
    check_ops_toml "ops_$(basename "$file" .toml | tr . _)" "$file" "ops-toml" "$TMP/contract.tsv"
done

echo ""
echo "=== 8. No tracked file carries a plaintext secret ==="
check_tracked_secret_literals tracked_secrets "tracked secret literals" "$TMP/contract.tsv" "${TRACKED_TOML[@]}"

echo ""
echo "============================================"
echo "Summary: $PASS passed, $FAIL failed"
echo "============================================"

# Printed before the exits below so every completed run carries the trailer, and
# folded in last so it cannot mask the two verdicts that are about the TREE. An
# arm refusal is a statement about the instrument, and it gets its own exit.
arms_rc=0
gate_arms_finish || arms_rc=1

[ "$FAIL" -eq 0 ] || exit 1

# ANTI-HOLLOW FLOOR. Every check above passes at zero if its extraction stops
# matching, and this catches the case where several do at once. MEASURED on a
# clean tree after the raw source scanner was retired.
CONFIG_GATE_MIN_PASSED=11
if [ "$PASS" -lt "$CONFIG_GATE_MIN_PASSED" ]; then
    echo "" >&2
    echo "FLOOR: only $PASS checks passed, expected at least $CONFIG_GATE_MIN_PASSED." >&2
    echo "  Nothing FAILED, so this is not a broken check - it is MISSING ones." >&2
    exit 1
fi

[ "$arms_rc" -eq 0 ] || exit 1
