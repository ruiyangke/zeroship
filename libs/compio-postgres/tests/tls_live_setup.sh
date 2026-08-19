#!/usr/bin/env bash
# Stand up the PostgreSQL servers `tests/tls_live.rs` needs, and write the
# descriptor that test reads.
#
# FIVE servers, because each one is the control for a claim that would
# otherwise be unfalsifiable:
#
#   tls       ssl=on, certificate for `localhost` signed by a private CA.
#             The positive case for every mode that encrypts.
#   plain     TLS off entirely. Without it, "sslmode=require connected" is
#             consistent with a driver that ignores sslmode.
#   mismatch  ssl=on, certificate for a name that is NOT the one the test
#             connects to, signed by the SAME CA. This is the only way to
#             separate verify-ca from verify-full: the chain is good and the
#             name is wrong, so the two modes must reach opposite verdicts on
#             one server differing in one variable.
#   sslonly   ssl=on, pg_hba accepting `hostssl` only. This is the only way to
#             see `allow` do anything: `allow` tries plaintext FIRST and
#             reaches TLS only when the plaintext attempt is refused. Against
#             any ordinary server `allow` is indistinguishable from `disable`.
#   clientcert ssl=on with ssl_ca_file and `cert` authentication: the client
#             MUST present a certificate. The only way to tell "sslcert and
#             sslkey are parsed" from "sslcert and sslkey are sent and used".
#
#   usage: tests/tls_live_setup.sh [tls_port] [plain_port] [mismatch_port] [sslonly_port] [clientcert_port]
#          tests/tls_live_setup.sh --down     # remove the containers
#
# Everything it generates lives in tests/data/live/, which is gitignored.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
live="$here/data/live"
tls_name=compio-pg-tls-test
plain_name=compio-pg-plain-test
mismatch_name=compio-pg-mismatch-test
sslonly_name=compio-pg-sslonly-test
clientcert_name=compio-pg-clientcert-test
password=compio-postgres-tls-test

if [ "${1:-}" = "--down" ]; then
    docker rm -f "$tls_name" "$plain_name" "$mismatch_name" "$sslonly_name" "$clientcert_name" >/dev/null 2>&1 || true
    rm -f "$live/tls_live.conf"
    echo "removed $tls_name, $plain_name, $mismatch_name, $sslonly_name, $clientcert_name"
    exit 0
fi

tls_port="${1:-5447}"
plain_port="${2:-5448}"
mismatch_port="${3:-5449}"
sslonly_port="${4:-5450}"
clientcert_port="${5:-5451}"

# Wipe the previous run's material, but keep the .gitignore that keeps all of
# it out of the repository - `git add -A` after a setup run must not offer to
# commit a private key.
mkdir -p "$live"
find "$live" -mindepth 1 ! -name .gitignore -exec sudo rm -rf {} +
cd "$live"

# A private CA, and two server certificates signed by it. The SAN on the first
# must cover the name the test connects to, or rustls rejects the certificate
# for the right reason but the wrong test. The SAN on the second must NOT.
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt \
    -days 3650 -sha256 -subj "/CN=compio-postgres-test-ca" 2>/dev/null

openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr \
    -sha256 -subj "/CN=localhost" 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' >san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -out server.crt -days 3650 -sha256 -extfile san.ext 2>/dev/null

# Same CA, deliberately wrong name. `.invalid` is reserved by RFC 2606, so this
# can never accidentally match anything resolvable.
openssl req -newkey rsa:2048 -nodes -keyout mismatch.key -out mismatch.csr \
    -sha256 -subj "/CN=not-the-host-you-asked-for.invalid" 2>/dev/null
printf 'subjectAltName=DNS:not-the-host-you-asked-for.invalid\nextendedKeyUsage=serverAuth\n' >mismatch.ext
openssl x509 -req -in mismatch.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -out mismatch.crt -days 3650 -sha256 -extfile mismatch.ext 2>/dev/null

# A CLIENT certificate, signed by the same CA. Its CN must equal the PostgreSQL
# role name, because `cert` authentication maps one to the other.
openssl req -newkey rsa:2048 -nodes -keyout client.key -out client.csr \
    -sha256 -subj "/CN=postgres" 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' >client.ext
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -out client.crt -days 3650 -sha256 -extfile client.ext 2>/dev/null

# `hostssl` only: a plaintext connection is refused during startup, which is
# the trigger `allow` falls back on.
cat >pg_hba_sslonly.conf <<'EOF'
local   all all             trust
hostssl all all 0.0.0.0/0   scram-sha-256
hostssl all all ::0/0       scram-sha-256
EOF

# `cert` authentication: the client MUST present a certificate signed by
# ssl_ca_file, and no password is accepted. This is what turns "sslcert/sslkey
# parse" into "sslcert/sslkey are actually sent and actually used".
cat >pg_hba_clientcert.conf <<'EOF'
local   all all             trust
hostssl all all 0.0.0.0/0   cert
hostssl all all ::0/0       cert
EOF

# PostgreSQL refuses to start if the key file is group/world readable or owned
# by anyone but root or the server user. root:<postgres gid> 0640 satisfies it.
sudo chown 0:999 server.key mismatch.key
sudo chmod 640 server.key mismatch.key

docker rm -f "$tls_name" "$plain_name" "$mismatch_name" "$sslonly_name" "$clientcert_name" >/dev/null 2>&1 || true

start_pg() {
    local name=$1 port=$2
    shift 2
    docker run -d --name "$name" \
        -e POSTGRES_PASSWORD="$password" \
        -e POSTGRES_HOST_AUTH_METHOD=scram-sha-256 \
        -p "127.0.0.1:$port:5432" \
        -v "$live:/certs:ro" \
        postgres:16 "$@" >/dev/null
}

start_pg "$tls_name" "$tls_port" \
    -c ssl=on -c ssl_cert_file=/certs/server.crt -c ssl_key_file=/certs/server.key
start_pg "$mismatch_name" "$mismatch_port" \
    -c ssl=on -c ssl_cert_file=/certs/mismatch.crt -c ssl_key_file=/certs/mismatch.key
start_pg "$sslonly_name" "$sslonly_port" \
    -c ssl=on -c ssl_cert_file=/certs/server.crt -c ssl_key_file=/certs/server.key \
    -c hba_file=/certs/pg_hba_sslonly.conf
start_pg "$clientcert_name" "$clientcert_port" \
    -c ssl=on -c ssl_cert_file=/certs/server.crt -c ssl_key_file=/certs/server.key \
    -c ssl_ca_file=/certs/ca.crt -c hba_file=/certs/pg_hba_clientcert.conf

docker run -d --name "$plain_name" \
    -e POSTGRES_PASSWORD="$password" \
    -e POSTGRES_HOST_AUTH_METHOD=scram-sha-256 \
    -p "127.0.0.1:$plain_port:5432" \
    postgres:16 >/dev/null

for name in "$tls_name" "$plain_name" "$mismatch_name" "$sslonly_name" "$clientcert_name"; do
    for _ in $(seq 60); do
        if docker exec "$name" pg_isready -q -U postgres 2>/dev/null; then break; fi
        sleep 1
    done
    docker exec "$name" pg_isready -U postgres
done

# Fail loudly here rather than letting the suite blame the driver. Each of
# these is a property the suite ASSUMES; if any is false, the results that
# depend on it are meaningless rather than merely wrong.

# The TLS server really came up with ssl=on.
docker exec -e PGPASSWORD="$password" "$tls_name" \
    psql "host=127.0.0.1 user=postgres dbname=postgres sslmode=require" \
    -tAc "select ssl from pg_stat_ssl where pid = pg_backend_pid()" | grep -qx t

# So did the mismatch server - and libpq reaches it at verify-ca (chain good)
# while refusing it at verify-full (name wrong). That is the discriminator the
# whole suite turns on, checked here with libpq rather than with the code under
# test, so a driver bug cannot make it look satisfied.
docker exec -e PGPASSWORD="$password" "$mismatch_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt" \
    -tAc "select ssl from pg_stat_ssl where pid = pg_backend_pid()" | grep -qx t
if docker exec -e PGPASSWORD="$password" "$mismatch_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-full sslrootcert=/certs/ca.crt" \
    -tAc "select 1" >/dev/null 2>&1; then
    echo "FATAL: libpq accepted $mismatch_name at verify-full; its certificate is not mismatched" >&2
    exit 1
fi

# The ssl-only server really refuses plaintext.
if docker exec -e PGPASSWORD="$password" "$sslonly_name" \
    psql "host=127.0.0.1 user=postgres dbname=postgres sslmode=disable" \
    -tAc "select 1" >/dev/null 2>&1; then
    echo "FATAL: $sslonly_name accepted a plaintext connection; hba_file did not apply" >&2
    exit 1
fi

# The client-certificate server really demands one: the same URL that works
# WITH sslcert/sslkey must fail without them, or the test proving they are
# wired would pass against a server that never asked.
if docker exec -e PGPASSWORD="$password" "$clientcert_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt" \
    -tAc "select 1" >/dev/null 2>&1; then
    echo "FATAL: $clientcert_name accepted a connection with no client certificate" >&2
    exit 1
fi

cat >"$live/tls_live.conf" <<EOF
tls_url=host=localhost port=$tls_port user=postgres password=$password dbname=postgres
plain_url=host=localhost port=$plain_port user=postgres password=$password dbname=postgres
mismatch_url=host=localhost port=$mismatch_port user=postgres password=$password dbname=postgres
sslonly_url=host=localhost port=$sslonly_port user=postgres password=$password dbname=postgres
clientcert_url=host=localhost port=$clientcert_port user=postgres dbname=postgres
ca=$live/ca.crt
client_cert=$live/client.crt
client_key=$live/client.key
EOF

echo "wrote $live/tls_live.conf"
