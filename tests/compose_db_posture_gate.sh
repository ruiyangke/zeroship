#!/usr/bin/env bash
# Guard the database posture required by the shipped worker topology.
#
# The worker refuses max_slot_wal_keep_size=-1 before V8 initialization. The
# live worker suite proves that refusal direction. This gate proves the other
# half: the PostgreSQL command shipped in Compose supplies a finite value, so
# the topology can pass that boot check.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="${1:-$ROOT/deploy/compose/docker-compose.yml}"

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [docker-compose.yml]" >&2
  exit 2
fi

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init compose_db_posture

FAILURES=0

# Emit the postgres service, its command key, and every command list item. This
# deliberately understands only the committed block-list shape. An inline or
# otherwise unrecognised command is not evidence that the required argv exists.
parsed=""
if [ ! -f "$COMPOSE" ]; then
  echo "FAIL: compose file does not exist: $COMPOSE" >&2
  FAILURES=$((FAILURES + 1))
else
  parsed="$({
    awk '
    /^[[:space:]]*(#|$)/ { next }
    {
      line = $0
      sub(/\r$/, "", line)
      match(line, /^ */)
      indent = RLENGTH
      rest = substr(line, indent + 1)

      if (indent == 2 && rest ~ /^[A-Za-z0-9_-]+:[[:space:]]*$/) {
        service = rest
        sub(/:.*/, "", service)
        in_command = 0
        if (service == "postgres") print "service"
        next
      }

      if (service != "postgres") next

      if (indent == 4) {
        in_command = (rest ~ /^command:[[:space:]]*$/)
        if (in_command) print "command"
        next
      }

      if (in_command && indent == 6 && rest ~ /^-[[:space:]]+/) {
        sub(/^-[[:space:]]+/, "", rest)
        print "item\t" rest
      }
    }
    ' "$COMPOSE"
  } 2>&1)"
  parse_status=$?
  if [ "$parse_status" -ne 0 ]; then
    echo "FAIL: could not parse postgres command from $COMPOSE" >&2
    printf '%s\n' "$parsed" >&2
    parsed=""
    FAILURES=$((FAILURES + 1))
  fi
fi

service_count="$(printf '%s\n' "$parsed" | grep -c '^service$' || true)"
gate_arm postgres_service "$service_count" 1 || FAILURES=$((FAILURES + 1))

if [ "$service_count" -ne 1 ]; then
  echo "FAIL: expected exactly one postgres service in $COMPOSE, found $service_count" >&2
  FAILURES=$((FAILURES + 1))
fi

command_count="$(printf '%s\n' "$parsed" | grep -c '^command$' || true)"
if [ "$command_count" -ne 1 ]; then
  echo "FAIL: postgres must have exactly one block-list command, found $command_count" >&2
  FAILURES=$((FAILURES + 1))
fi

mapfile -t command_items < <(printf '%s\n' "$parsed" | sed -n $'s/^item\t//p')

assignment_count=0
finite_value=""
for ((i = 0; i < ${#command_items[@]}; i++)); do
  item="${command_items[$i]}"
  case "$item" in
    \"*\") item="${item:1:${#item}-2}" ;;
    \'*\') item="${item:1:${#item}-2}" ;;
  esac

  case "$item" in
    max_slot_wal_keep_size=*)
      assignment_count=$((assignment_count + 1))
      value="${item#*=}"

      if [ "$i" -eq 0 ] || [ "${command_items[$((i - 1))]}" != "-c" ]; then
        echo "FAIL: max_slot_wal_keep_size must be an argument to a preceding -c" >&2
        FAILURES=$((FAILURES + 1))
      fi

      if [[ "$value" =~ ^(0|[1-9][0-9]*)(kB|MB|GB|TB)?$ ]]; then
        finite_value="$value"
      else
        echo "FAIL: max_slot_wal_keep_size must be a finite non-negative PostgreSQL size, got '$value'" >&2
        FAILURES=$((FAILURES + 1))
      fi
      ;;
  esac
done

if [ "$assignment_count" -eq 0 ]; then
  echo "FAIL: postgres command lacks max_slot_wal_keep_size=<finite>" >&2
  echo "  The worker refuses PostgreSQL's unlimited -1 default at boot." >&2
  FAILURES=$((FAILURES + 1))
elif [ "$assignment_count" -gt 1 ]; then
  echo "FAIL: postgres command sets max_slot_wal_keep_size $assignment_count times" >&2
  echo "  Multiple assignments make the effective safety posture order-dependent." >&2
  FAILURES=$((FAILURES + 1))
fi

gate_arms_finish || FAILURES=$((FAILURES + 1))

if [ "$FAILURES" -ne 0 ]; then
  echo "compose database posture gate: FAILED ($FAILURES finding(s))" >&2
  exit 1
fi

echo "compose database posture: $service_count postgres service, max_slot_wal_keep_size=$finite_value"
