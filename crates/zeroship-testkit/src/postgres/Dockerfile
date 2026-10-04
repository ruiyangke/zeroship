FROM pgvector/pgvector:pg16
RUN apt-get update \
    && apt-get install -y --no-install-recommends postgresql-16-postgis-3 \
    && rm -rf /var/lib/apt/lists/*
COPY watchdog.sh /usr/local/bin/zeroship-watchdog
RUN chmod 0755 /usr/local/bin/zeroship-watchdog
