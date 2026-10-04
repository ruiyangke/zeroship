#!/bin/sh
# PID 1 of a shared test-server container.
#
# It starts the server command it is handed, then watches the host's lease
# through the bind-mounted `session.lock`. A host test process holds a shared
# lock on that file for as long as it uses the server; the probe here takes the
# lock exclusively, which only succeeds once no host holds it. After the idle
# grace passes with the lease free the container deletes the state file naming
# it, stops the server and exits, and the `--rm` on the run removes the
# container.
#
# `$1` is the idle grace in seconds and `$2` the session nonce naming this boot;
# the rest is the server command, started as a child and supervised here. The
# PostgreSQL image's entrypoint execs this watchdog, while the Redpanda image
# runs it directly through `--entrypoint`; either way it is PID 1.
set -u

GRACE="${1:?the idle grace in seconds}"
shift
NONCE="${1:?the session nonce naming this boot}"
shift

STATE=/run/zeroship-testkit/state.json
LEASE=/run/zeroship-testkit/session.lock

# How long a fast shutdown gets before it is escalated.
STOP_BOUND=10

# fd 9 stays open for the life of the container and is the one descriptor every
# probe locks, so a host lock is what each probe competes with.
exec 9<>"$LEASE"

"$@" &
server_pid=$!

idle=0
while :; do
    if ! kill -0 "$server_pid" 2>/dev/null; then
        exit 1
    fi
    if flock -n -x 9; then
        flock -u 9
        idle=$((idle + 1))
    else
        idle=0
    fi
    if [ "$idle" -ge "$GRACE" ]; then
        break
    fi
    sleep 1
done

# Take the lease outright so a new boot cannot start the next container
# underneath this teardown.
flock -x 9

# Delete the state file only when it names this boot's nonce, so a container
# that has already been replaced never removes its successor's state.
if [ -f "$STATE" ]; then
    owner=$(sed -n 's/.*"nonce":"\([^"]*\)".*/\1/p' "$STATE")
    if [ "$owner" = "$NONCE" ]; then
        rm -f "$STATE"
    fi
fi

# Fast shutdown: SIGINT aborts the clients a smart shutdown would wait for, so a
# stray connection cannot hold teardown open. SIGQUIT is immediate, SIGKILL the
# last resort.
kill -INT "$server_pid" 2>/dev/null
stop_within() {
    waited=0
    while kill -0 "$server_pid" 2>/dev/null; do
        [ "$waited" -ge "$1" ] && return 1
        waited=$((waited + 1))
        sleep 1
    done
    return 0
}
if ! stop_within "$STOP_BOUND"; then
    kill -QUIT "$server_pid" 2>/dev/null
    if ! stop_within "$STOP_BOUND"; then
        kill -KILL "$server_pid" 2>/dev/null
    fi
fi
wait "$server_pid" 2>/dev/null
exit 0
