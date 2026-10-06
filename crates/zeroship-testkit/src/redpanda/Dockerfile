# The fixture prepends the FROM line from zeroship_shared_server::images::REDPANDA,
# the one list of fixture images CI restores from its cache.
# The image runs as `redpanda` by default, which cannot write the host's
# bind-mounted lease state. The shared container runs as root so the watchdog can
# maintain that state, and the watchdog starts the broker with the same
# `rpk redpanda start` the image's own entrypoint would run.
USER root
COPY watchdog.sh /usr/local/bin/zeroship-watchdog
RUN chmod 0755 /usr/local/bin/zeroship-watchdog
