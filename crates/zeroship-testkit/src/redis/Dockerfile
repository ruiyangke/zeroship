# The fixture prepends the FROM line from zeroship_testkit_server::images::DRAGONFLY_1_37,
# the one list of fixture images CI restores from its cache.
COPY watchdog.sh /usr/local/bin/zeroship-watchdog
COPY cluster.sh /usr/local/bin/zeroship-dragonfly-cluster
RUN chmod 0755 /usr/local/bin/zeroship-watchdog /usr/local/bin/zeroship-dragonfly-cluster
