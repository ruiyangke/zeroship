#!/usr/bin/env bash
# Stand up the PostgreSQL server `tests/unix_socket_live.rs` needs, and write
# the descriptor that test reads.
#
# WHY THE PATH IS SHORT, and why that is the whole point of this script.
#
# A Unix socket address is a fixed 108-byte `sockaddr_un.sun_path` on Linux,
# and the server appends `/.s.PGSQL.<port>` - 14 bytes - to the directory it
# is given. A socket directory therefore has about 93 usable bytes, which an
# ordinary scratch path blows through without looking long.
#
# This is not hypothetical. The fixture this suite shipped before 2026-08-25
# mounted its socket under a per-session agent scratchpad, whose directory came
# to 96 bytes, so the socket path came to 110 and NOTHING could connect to it.
# The fixture existed, the container ran, and the one capability it was there to
# prove - that this driver can actually talk over a Unix socket - had no test at
# all. `libs/compio-postgres/tests/suite/unix_socket_path_limit.rs` covers the
# REFUSAL of an overlong path; the successful path was never exercised.
#
# So keep the directory short and OUTSIDE any per-session scratch tree. The
# script refuses to proceed if the resulting socket path would not fit, rather
# than creating another fixture that quietly cannot be used.

set -euo pipefail

container="${CONTAINER:-zs-cpg-unix-5456}"
port="${PORT:-5456}"
sockdir="${SOCKDIR:-/tmp/zscpgsock}"
password="zeroship"
dbname="zeroship"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
descriptor="$here/data/live/unix_socket.conf"

if [ "${1:-}" = "--down" ]; then
    docker rm -f "$container" > /dev/null 2>&1 || true
    rm -f "$descriptor"
    echo "removed $container and $descriptor"
    exit 0
fi

# 14 bytes for `/.s.PGSQL.5432`. Checked BEFORE anything is created, because
# the failure this guards against is a fixture that looks fine and cannot be
# reached.
socket_path_len=$(( ${#sockdir} + 14 ))
if [ "$socket_path_len" -gt 107 ]; then
    echo "SOCKDIR=$sockdir would make a ${socket_path_len}-byte socket path," >&2
    echo "past the 108-byte sun_path limit. Choose a shorter directory." >&2
    exit 1
fi

mkdir -p "$sockdir"
# Best effort: on a re-run the directory is already owned by the container's
# postgres uid, so chmod is refused. That is harmless - the socket check below
# is what actually decides whether this fixture works, and it cannot be
# satisfied by a directory nothing can reach.
chmod 777 "$sockdir" 2>/dev/null || true

docker rm -f "$container" > /dev/null 2>&1 || true
docker run -d --name "$container" -p "127.0.0.1:$port:5432" \
    -e POSTGRES_PASSWORD="$password" \
    -e POSTGRES_DB="$dbname" \
    -e POSTGRES_HOST_AUTH_METHOD=trust \
    -v "$sockdir:/var/run/postgresql" \
    postgres:16 > /dev/null

# Wait until the server ANSWERS on the socket, not merely until the socket file
# exists. The file appears while initdb is still running, and a suite started in
# that window fails to connect - measured 2026-08-25, two tests failed against a
# freshly created container and passed on the next run with nothing changed,
# which is exactly how a fixture race reads.
ready=""
for _ in $(seq 1 60); do
    if [ -S "$sockdir/.s.PGSQL.5432" ] \
        && docker exec "$container" psql -h /var/run/postgresql -U postgres \
            -d "$dbname" -tAc "SELECT 1" > /dev/null 2>&1; then
        ready=yes
        break
    fi
    sleep 1
done

if [ -z "$ready" ]; then
    echo "the server did not answer on $sockdir/.s.PGSQL.5432 within 60s" >&2
    docker logs "$container" 2>&1 | tail -20 >&2
    exit 1
fi

mkdir -p "$(dirname "$descriptor")"
cat > "$descriptor" <<EOF
socket_dir=$sockdir
dbname=$dbname
user=postgres
EOF

echo "socket at $sockdir/.s.PGSQL.5432 (${socket_path_len} bytes), descriptor $descriptor"
