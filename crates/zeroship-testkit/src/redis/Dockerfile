FROM docker.dragonflydb.io/dragonflydb/dragonfly:v1.37.0
COPY watchdog.sh /usr/local/bin/zeroship-watchdog
COPY cluster.sh /usr/local/bin/zeroship-dragonfly-cluster
RUN chmod 0755 /usr/local/bin/zeroship-watchdog /usr/local/bin/zeroship-dragonfly-cluster
