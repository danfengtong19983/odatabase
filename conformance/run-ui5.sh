#!/bin/sh
# Runs SAPUI5's OData V4 model in headless Chromium against a fresh server. Needs Docker and
# internet access, for the UI5 runtime on sdk.openui5.org.
set -u
cd "$(dirname "$0")/.."
PORT=${PORT:-38922}
cargo build -q -p odatabase --example conformance_server || exit 1
ODATA_ADDR=127.0.0.1:$PORT target/debug/examples/conformance_server >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER' EXIT
sleep 1
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=http://host.docker.internal:$PORT/odata \
  -v "$PWD/conformance/ui5:/src:ro" \
  mcr.microsoft.com/playwright:v1.49.1-noble \
  sh -c 'cp -r /src /tmp/app && cd /tmp/app && npm install --silent --no-audit --no-fund 2>&1 | tail -2 && node run.mjs'
