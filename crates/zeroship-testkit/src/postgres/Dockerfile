# The fixture prepends the FROM line from zeroship_shared_server::images::PGVECTOR_16,
# the one list of fixture images CI restores from its cache.
RUN apt-get update \
    && apt-get install -y --no-install-recommends postgresql-16-postgis-3 \
    && rm -rf /var/lib/apt/lists/*
COPY watchdog.sh /usr/local/bin/zeroship-watchdog
RUN chmod 0755 /usr/local/bin/zeroship-watchdog
