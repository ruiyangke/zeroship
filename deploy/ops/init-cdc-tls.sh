#!/usr/bin/env bash
# Generate local Compose relay TLS material. Production supplies its own chain.
set -euo pipefail
directory="${1:-deploy/compose/secrets}"
hostname="${2:-cdc-relay}"
umask 077
mkdir -p "$directory"
certificate="$directory/cdc-cert.pem"
key="$directory/cdc-key.pem"
if [ -e "$certificate" ] || [ -e "$key" ]; then
  [ -f "$certificate" ] && [ -f "$key" ] || {
    echo "relay certificate and key must both exist; refusing to overwrite a partial pair" >&2
    exit 1
  }
  openssl x509 -in "$certificate" -checkhost "$hostname" -noout
  openssl x509 -in "$certificate" -checkend 0 -noout
  certificate_public=$(openssl x509 -in "$certificate" -pubkey -noout)
  private_public=$(openssl pkey -in "$key" -pubout)
  [ "$certificate_public" = "$private_public" ] || {
    echo "relay certificate does not match its private key" >&2
    exit 1
  }
  exit 0
fi
temporary=$(mktemp -d "$directory/.cdc-tls.XXXXXX")
trap 'rm -r -- "$temporary"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -days 365 -subj "/CN=$hostname" \
  -addext "subjectAltName=DNS:$hostname,DNS:localhost,IP:127.0.0.1" \
  -keyout "$temporary/key.pem" -out "$temporary/cert.pem"
mv "$temporary/key.pem" "$key"
mv "$temporary/cert.pem" "$certificate"
