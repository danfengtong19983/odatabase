#!/bin/sh
# Converts $metadata with the OASIS odata-openapi tool. Needs Docker.
set -u
cd "$(dirname "$0")/.."
PORT=${PORT:-38925}
cargo build -q -p odatabase --example conformance_server || exit 1
ODATA_ADDR=127.0.0.1:$PORT target/debug/examples/conformance_server >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER' EXIT
sleep 1
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=http://host.docker.internal:$PORT/odata \
  -v "$PWD/conformance/openapi.js:/src/openapi.js:ro" node:22-slim \
  sh -c 'mkdir /tmp/app && cd /tmp/app && npm install --silent --no-audit --no-fund odata-openapi >/dev/null 2>&1; cp /src/openapi.js . && node openapi.js'
