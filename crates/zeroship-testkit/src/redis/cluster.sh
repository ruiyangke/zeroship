#!/bin/sh
# The watchdog's single tracked child in the shared Dragonfly cluster container.
#
# `zeroship-watchdog` starts one command as its child and signals only that
# child's PID, so a three-node cluster that must behave like the watchdog's
# one tracked process runs all three Dragonfly nodes under this wrapper: it
# starts one process per node, each in cluster mode on its own port, forwards
# every signal the watchdog sends to every node, and exits the moment ANY node
# exits - whether from a forwarded shutdown or a crash - stopping the
# survivors first.
#
# Exiting on the first node to go, rather than waiting for all three, is what
# keeps a crashed node from leaving a cluster the watchdog still considers
# healthy: the watchdog's own loop only checks that THIS wrapper is alive, so
# a wrapper that outlives one of its three nodes reports ready while the
# cluster is missing a slot owner, every later joiner's readiness probe then
# fails against the two survivors, and `shared::join`'s "a live server that
# does not answer is not replaced" rule refuses to replace it for the rest of
# the run. Exiting here instead makes the watchdog's own `kill -0` on this
# process fail on its very next check, so it tears the container down and the
# next joiner boots a fresh, fully configured cluster.
#
# `$1` is the first node's port and `$2` the node count; nodes bind
# consecutive ports from there, one process each.
set -u

BASE_PORT="${1:?the first node's port}"
NODE_COUNT="${2:?the node count}"

pids=""
i=0
while [ "$i" -lt "$NODE_COUNT" ]; do
    port=$((BASE_PORT + i))
    dragonfly \
        --cluster_mode=yes \
        --port="$port" \
        --cluster_node_id="node-$i" \
        --logtostderr \
        --proactor_threads=1 \
        --maxmemory=256mb &
    pids="$pids $!"
    i=$((i + 1))
done

forward() {
    for pid in $pids; do
        kill -s "$1" "$pid" 2>/dev/null
    done
}
# Every shutdown signal just forwards and falls through to the exit below,
# the same path an unsignalled node crash takes.
trap 'forward INT' INT
trap 'forward TERM' TERM
trap 'forward QUIT' QUIT

any_dead() {
    for pid in $pids; do
        kill -0 "$pid" 2>/dev/null || return 0
    done
    return 1
}

# No portable POSIX `wait` returns as soon as one of several background jobs
# exits (bash's `wait -n` is not available under `/bin/sh` here), so this
# polls once a second - more than fast enough against the many-second bounds
# every caller of this image already waits on for readiness and removal.
while ! any_dead; do
    sleep 1
done

forward TERM
wait
