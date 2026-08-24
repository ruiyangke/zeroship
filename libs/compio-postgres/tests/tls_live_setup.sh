#!/usr/bin/env bash
# Stand up the PostgreSQL servers `tests/tls_live.rs` needs, and write the
# descriptor that test reads.
#
# FIVE servers, because each one is the control for a claim that would
# otherwise be unfalsifiable:
#
#   tls       ssl=on, TLS 1.2 only, certificate for `localhost` signed by a
#             private CA. The positive case for every mode that encrypts and
#             the discriminator for the client's minimum-version bound.
#             That exact certificate is also listed in a generated CRL, so
#             the client can prove the same server is accepted without the
#             CRL and refused with it.
#   plain     TLS off entirely. Without it, "sslmode=require connected" is
#             consistent with a driver that ignores sslmode.
#   mismatch  ssl=on, TLS 1.3 only, certificate for a name that is NOT the one
#             the test connects to, signed by the SAME CA. This separates
#             verify-ca from verify-full and discriminates the client's
#             maximum-version bound.
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
key_password=compio-postgres-encrypted-key-test

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

# Sign this certificate through `openssl ca`, rather than the shorter `x509`
# path used by the other fixtures, because a meaningful CRL needs the CA's
# issuance database to identify the exact serial it revokes.
mkdir ca-newcerts
: >ca-index.txt
printf '1000\n' >ca-serial
printf '1000\n' >ca-crlnumber
cat >ca.cnf <<EOF
[ ca ]
default_ca = test_ca

[ test_ca ]
database = $live/ca-index.txt
new_certs_dir = $live/ca-newcerts
certificate = $live/ca.crt
private_key = $live/ca.key
serial = $live/ca-serial
crlnumber = $live/ca-crlnumber
default_md = sha256
default_days = 3650
default_crl_days = 3650
policy = test_policy
x509_extensions = server_ext

[ test_policy ]
commonName = supplied

[ server_ext ]
subjectAltName = DNS:localhost,IP:127.0.0.1
extendedKeyUsage = serverAuth
EOF
openssl ca -batch -notext -config ca.cnf -in server.csr -out server.crt 2>/dev/null
openssl ca -batch -config ca.cnf -revoke server.crt 2>/dev/null
openssl ca -batch -config ca.cnf -gencrl -out server.crl 2>/dev/null

# `openssl rehash` normally creates a symlink. A regular file under the exact
# issuer-hash lookup name exercises the same OpenSSL directory contract while
# keeping this fixture self-contained on filesystems without symlink support.
mkdir server-crl-dir
crl_hash="$(openssl crl -in server.crl -hash -noout)"
cp server.crl "server-crl-dir/${crl_hash}.r0"
mkdir server-crl-wrong-hash-dir
cp server.crl server-crl-wrong-hash-dir/00000000.r0

# Prove the generated pair is discriminating before the code under test sees
# it: the certificate is otherwise valid, and this CRL rejects it specifically
# because its serial is revoked.
openssl verify -CAfile ca.crt server.crt >/dev/null
if crl_verdict=$(openssl verify -CAfile ca.crt -CRLfile server.crl \
    -crl_check server.crt 2>&1); then
    echo "FATAL: generated CRL did not revoke server.crt" >&2
    exit 1
fi
if ! grep -qi 'certificate revoked' <<<"$crl_verdict"; then
    echo "FATAL: generated CRL failed for the wrong reason: $crl_verdict" >&2
    exit 1
fi

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
openssl pkcs8 -topk8 -in client.key -out client-encrypted.key \
    -v2 aes-256-cbc -passout "pass:$key_password" 2>/dev/null

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

# The `tls` server is also the one the REST of the suite runs against when
# built with `--features suite-over-tls`, so it carries the same three
# settings the plaintext review server does: logical decoding for the
# replication tests, prepared transactions for the two-phase ones, and enough
# slots that a full run does not exhaust them. None of the three affects TLS;
# without them those tests would fail on server configuration and be read as
# a TLS divergence.
start_pg "$tls_name" "$tls_port" \
    -c ssl=on -c ssl_cert_file=/certs/server.crt -c ssl_key_file=/certs/server.key \
    -c ssl_min_protocol_version=TLSv1.2 -c ssl_max_protocol_version=TLSv1.2 \
    -c wal_level=logical -c max_prepared_transactions=10 -c max_replication_slots=20
start_pg "$mismatch_name" "$mismatch_port" \
    -c ssl=on -c ssl_cert_file=/certs/mismatch.crt -c ssl_key_file=/certs/mismatch.key \
    -c ssl_min_protocol_version=TLSv1.3 -c ssl_max_protocol_version=TLSv1.3
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
    -tAc "select version from pg_stat_ssl where pid = pg_backend_pid()" | grep -qx TLSv1.2

# The hashed regular file is meaningful to libpq/OpenSSL before it is handed
# to the Rust implementation. A source `.crl` scanned by filename would not
# satisfy this check.
if docker exec -e PGPASSWORD="$password" "$tls_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-full sslrootcert=/certs/ca.crt sslcrldir=/certs/server-crl-dir" \
    -tAc "select 1" >/dev/null 2>&1; then
    echo "FATAL: libpq ignored the hashed CRL in server-crl-dir" >&2
    exit 1
fi

# So did the mismatch server - and libpq reaches it at verify-ca (chain good)
# while refusing it at verify-full (name wrong). That is the discriminator the
# whole suite turns on, checked here with libpq rather than with the code under
# test, so a driver bug cannot make it look satisfied.
docker exec -e PGPASSWORD="$password" "$mismatch_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt" \
    -tAc "select version from pg_stat_ssl where pid = pg_backend_pid()" | grep -qx TLSv1.3
docker exec -e PGPASSWORD="$password" "$mismatch_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt sslcrldir=/certs/server-crl-dir" \
    -tAc "select 1" | grep -qx 1
if docker exec -e PGPASSWORD="$password" "$mismatch_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt sslcrldir=/certs/server-crl-wrong-hash-dir" \
    -tAc "select 1" >/dev/null 2>&1; then
    echo "FATAL: libpq accepted a CRL stored under the wrong issuer hash" >&2
    exit 1
fi
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

# Prove the exact sslcertmode discriminator with libpq before testing our
# driver. Both commands name the same client identity: the cert-auth server
# requests it and satisfies `require`, while the ordinary TLS server must not
# request it and therefore cannot satisfy `require` even though password auth
# would otherwise succeed.
docker exec "$clientcert_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt sslcert=/certs/client.crt sslkey=/certs/client.key sslcertmode=require" \
    -tAc "select 1" | grep -qx 1
if docker exec -e PGPASSWORD="$password" "$tls_name" \
    psql "host=localhost user=postgres dbname=postgres sslmode=require sslcert=/certs/client.crt sslkey=/certs/client.key sslcertmode=require" \
    -tAc "select 1" >/dev/null 2>&1; then
    echo "FATAL: $tls_name requested a client certificate; it cannot discriminate sslcertmode=require" >&2
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
client_encrypted_key=$live/client-encrypted.key
client_key_password=$key_password
server_crl=$live/server.crl
server_crl_dir=$live/server-crl-dir
server_crl_wrong_hash_dir=$live/server-crl-wrong-hash-dir
EOF

echo "wrote $live/tls_live.conf"
