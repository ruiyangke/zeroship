#!/bin/sh
# Generate the TLS fixture's material into $1, at image build time.
#
# Runs inside the PostgreSQL 16 image the TLS servers are built from, as root,
# with that image's openssl. The material never leaves the image except as the
# client-side copies `tls.rs` takes out of a running server: the private keys
# are not committed anywhere, and every build of the recipe makes its own CA.
#
#   ca.crt               a private CA in no system trust store
#   server.crt/.key      for `localhost` and 127.0.0.1, signed by the CA, and
#                        revoked by server.crl - the same certificate is
#                        accepted without the CRL and refused with it
#   server.crl           that revocation, issued by the CA
#   server-crl-dir/      the CRL under its OpenSSL issuer-hash lookup name
#   server-crl-wrong-hash-dir/
#                        the same CRL under a valid but wrong hash
#   mismatch.crt/.key    signed by the same CA for a name the tests never dial
#   client.crt/.key      CN=postgres, for `cert` authentication
#   client-encrypted.key the same key as passphrase-encrypted PKCS#8
#   pg_hba_sslonly.conf, pg_hba_clientcert.conf
#   material-id          the CA's fingerprint, naming this build's material
#
#   usage: certs.sh <directory> <client key passphrase>
set -eu

out="$1"
key_password="$2"
mkdir -p "$out"
cd "$out"

openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt \
    -days 3650 -sha256 -subj "/CN=compio-postgres-test-ca" 2>/dev/null

openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr \
    -sha256 -subj "/CN=localhost" 2>/dev/null

# Sign the server certificate through `openssl ca`, rather than the shorter
# `x509` path the other certificates take, because a meaningful CRL needs the
# CA's issuance database to identify the exact serial it revokes.
mkdir ca-newcerts
: >ca-index.txt
printf '1000\n' >ca-serial
printf '1000\n' >ca-crlnumber
cat >ca.cnf <<CNF
[ ca ]
default_ca = test_ca

[ test_ca ]
database = $out/ca-index.txt
new_certs_dir = $out/ca-newcerts
certificate = $out/ca.crt
private_key = $out/ca.key
serial = $out/ca-serial
crlnumber = $out/ca-crlnumber
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
CNF
openssl ca -batch -notext -config ca.cnf -in server.csr -out server.crt 2>/dev/null
openssl ca -batch -config ca.cnf -revoke server.crt 2>/dev/null
openssl ca -batch -config ca.cnf -gencrl -out server.crl 2>/dev/null

# `openssl rehash` normally creates a symlink. A regular file under the exact
# issuer-hash lookup name exercises the same OpenSSL directory contract.
mkdir server-crl-dir
crl_hash="$(openssl crl -in server.crl -hash -noout)"
cp server.crl "server-crl-dir/${crl_hash}.r0"
mkdir server-crl-wrong-hash-dir
cp server.crl server-crl-wrong-hash-dir/00000000.r0

# The pair must discriminate before the code under test sees it: the
# certificate is otherwise valid, and this CRL rejects it specifically because
# its serial is revoked.
openssl verify -CAfile ca.crt server.crt >/dev/null
if crl_verdict=$(openssl verify -CAfile ca.crt -CRLfile server.crl \
    -crl_check server.crt 2>&1); then
    echo "FATAL: the generated CRL did not revoke server.crt" >&2
    exit 1
fi
case "$crl_verdict" in
    *"certificate revoked"*) ;;
    *)
        echo "FATAL: the generated CRL failed for the wrong reason: $crl_verdict" >&2
        exit 1
        ;;
esac

# Same CA, deliberately wrong name. `.invalid` is reserved by RFC 2606, so it
# can never match anything resolvable.
openssl req -newkey rsa:2048 -nodes -keyout mismatch.key -out mismatch.csr \
    -sha256 -subj "/CN=not-the-host-you-asked-for.invalid" 2>/dev/null
printf 'subjectAltName=DNS:not-the-host-you-asked-for.invalid\nextendedKeyUsage=serverAuth\n' >mismatch.ext
openssl x509 -req -in mismatch.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -out mismatch.crt -days 3650 -sha256 -extfile mismatch.ext 2>/dev/null

# A client certificate signed by the same CA. Its CN must equal the PostgreSQL
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
cat >pg_hba_sslonly.conf <<'HBA'
local   all all             trust
hostssl all all 0.0.0.0/0   scram-sha-256
hostssl all all ::0/0       scram-sha-256
HBA

# `cert` authentication: the client MUST present a certificate signed by
# ssl_ca_file, and no password is accepted.
cat >pg_hba_clientcert.conf <<'HBA'
local   all all             trust
hostssl all all 0.0.0.0/0   cert
hostssl all all ::0/0       cert
HBA

openssl x509 -in ca.crt -noout -fingerprint -sha256 \
    | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f' | cut -c1-16 >material-id

# The CA's signing material and its issuance database served the generation
# and nothing after it.
rm -rf ca.key ca.cnf ca-index.txt* ca-serial* ca-crlnumber* ca-newcerts ca.srl \
    ./*.csr ./*.ext

# PostgreSQL refuses a server key that is group or world readable unless it is
# owned by root and readable by its own group.
chmod 0644 ./*.crt ./*.crl ./*.conf material-id server-crl-dir/* server-crl-wrong-hash-dir/*
chmod 0600 client.key client-encrypted.key
chown root:postgres server.key mismatch.key
chmod 0640 server.key mismatch.key
