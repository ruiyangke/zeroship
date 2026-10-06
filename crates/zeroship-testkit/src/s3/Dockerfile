# The fixture prepends the FROM line from zeroship_testkit_server::images::VERSITYGW,
# the one list of fixture images CI restores from its cache.
# The shared watchdog and the lease-refusal probe both check for `flock -E`, and
# the versitygw image is Alpine/BusyBox, whose flock has no `-E`.
RUN apk add --no-cache util-linux
COPY watchdog.sh /usr/local/bin/zeroship-watchdog
RUN chmod 0755 /usr/local/bin/zeroship-watchdog
