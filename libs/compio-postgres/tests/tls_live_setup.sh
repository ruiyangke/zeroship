#!/usr/bin/env bash
# Stand up the two PostgreSQL servers `tests/tls_live.rs` needs, and write the
# descriptor that test reads.
#
# Two servers, not one, because a one-sided demonstration proves nothing: a
# connection that succeeds with TLS configured might also have succeeded
# without it. The suite needs a server that speaks TLS AND a server that does
# not, so `sslmode=require` can be shown to connect to the first and to fail
# against the second.
#
# The TLS server gets a private CA and a server certificate signed by it, so
# the suite can also show that verification is real: the same URL that connects
# with `sslrootcert=<ca>` must fail with `sslrootcert=system`.
#
#   usage: tests/tls_live_setup.sh [tls_port] [plain_port]
#          tests/tls_live_setup.sh --down     # remove both containers
#
# Everything it generates lives in tests/data/live/, which is gitignored.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
live="$here/data/live"
tls_name=compio-pg-tls-test
plain_name=compio-pg-plain-test
password=compio-postgres-tls-test

if [ "${1:-}" = "--down" ]; then
    docker rm -f "$tls_name" "$plain_name" >/dev/null 2>&1 || true
    rm -f "$live/tls_live.conf"
    echo "removed $tls_name, $plain_name"
    exit 0
fi

tls_port="${1:-5447}"
plain_port="${2:-5448}"

# Wipe the previous run's material, but keep the .gitignore that keeps all of
# it out of the repository - `git add -A` after a setup run must not offer to
# commit a private key.
mkdir -p "$live"
find "$live" -mindepth 1 ! -name .gitignore -exec sudo rm -rf {} +
cd "$live"

# A private CA, and a server certificate signed by it. The SAN must cover the
# name the test connects to, or rustls rejects the certificate for the right
# reason but the wrong test.
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt \
    -days 3650 -sha256 -subj "/CN=compio-postgres-test-ca" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr \
    -sha256 -subj "/CN=localhost" 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' >san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -out server.crt -days 3650 -sha256 -extfile san.ext 2>/dev/null

# PostgreSQL refuses to start if the key file is group/world readable or owned
# by anyone but root or the server user. root:<postgres gid> 0640 satisfies it.
sudo chown 0:999 server.key
sudo chmod 640 server.key

docker rm -f "$tls_name" "$plain_name" >/dev/null 2>&1 || true

docker run -d --name "$tls_name" \
    -e POSTGRES_PASSWORD="$password" \
    -e POSTGRES_HOST_AUTH_METHOD=scram-sha-256 \
    -p "127.0.0.1:$tls_port:5432" \
    -v "$live:/certs:ro" \
    postgres:16 \
    -c ssl=on -c ssl_cert_file=/certs/server.crt -c ssl_key_file=/certs/server.key >/dev/null

docker run -d --name "$plain_name" \
    -e POSTGRES_PASSWORD="$password" \
    -e POSTGRES_HOST_AUTH_METHOD=scram-sha-256 \
    -p "127.0.0.1:$plain_port:5432" \
    postgres:16 >/dev/null

for name in "$tls_name" "$plain_name"; do
    for _ in $(seq 60); do
        if docker exec "$name" pg_isready -q -U postgres 2>/dev/null; then break; fi
        sleep 1
    done
    docker exec "$name" pg_isready -U postgres
done

# Fail loudly here rather than letting the suite blame the driver: if the TLS
# server did not actually come up with ssl=on, every "require connects" result
# below would be meaningless.
docker exec -e PGPASSWORD="$password" "$tls_name" \
    psql "host=127.0.0.1 user=postgres dbname=postgres sslmode=require" \
    -tAc "select ssl from pg_stat_ssl where pid = pg_backend_pid()" | grep -qx t

cat >"$live/tls_live.conf" <<EOF
tls_url=host=localhost port=$tls_port user=postgres password=$password dbname=postgres
plain_url=host=localhost port=$plain_port user=postgres password=$password dbname=postgres
ca=$live/ca.crt
EOF

echo "wrote $live/tls_live.conf"
