#!/usr/bin/env bash
# Regenerate synthetic local HTTPS fixtures. Rust tests consume the DER files.
set -euo pipefail
FIXTURE_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/tls-fixtures
TLS_WORK_DIR=$(mktemp -d)
trap 'rm -rf -- "$TLS_WORK_DIR"' EXIT
mkdir -p "$FIXTURE_DIR"
openssl req -x509 -newkey rsa:2048 -noenc -keyout "$TLS_WORK_DIR/ca-key.pem" \
  -out "$FIXTURE_DIR/ca.pem" -days 3650 -subj '/CN=Hermes port test CA' \
  -addext 'basicConstraints=critical,CA:TRUE'
openssl req -x509 -newkey rsa:2048 -noenc -keyout "$TLS_WORK_DIR/other-key.pem" \
  -out "$FIXTURE_DIR/other-ca.pem" -days 3650 -subj '/CN=Unrelated Hermes test CA' \
  -addext 'basicConstraints=critical,CA:TRUE'
openssl req -new -newkey rsa:2048 -noenc -keyout "$TLS_WORK_DIR/server-key.pem" \
  -out "$TLS_WORK_DIR/server.csr" -subj '/CN=localhost'
cat > "$TLS_WORK_DIR/server.ext" <<'EXT'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EXT
openssl x509 -req -in "$TLS_WORK_DIR/server.csr" -CA "$FIXTURE_DIR/ca.pem" \
  -CAkey "$TLS_WORK_DIR/ca-key.pem" -set_serial 2 -out "$TLS_WORK_DIR/server.pem" \
  -days 3650 -extfile "$TLS_WORK_DIR/server.ext"
openssl x509 -in "$TLS_WORK_DIR/server.pem" -outform DER -out "$FIXTURE_DIR/server.der"
openssl pkcs8 -topk8 -nocrypt -in "$TLS_WORK_DIR/server-key.pem" -outform DER \
  -out "$FIXTURE_DIR/server-key.der"
