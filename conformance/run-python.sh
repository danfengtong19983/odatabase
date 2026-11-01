#!/bin/sh
# Runs python-odata against a fresh conformance server. Needs Docker.
set -u
cd "$(dirname "$0")/.."
PORT=${PORT:-38923}
cargo build -q -p odatabase --example conformance_server || exit 1
ODATA_ADDR=127.0.0.1:$PORT target/debug/examples/conformance_server >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER' EXIT
sleep 1
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=http://host.docker.internal:$PORT/odata \
  -v "$PWD/conformance/python:/src:ro" python:3.12-slim \
  sh -c 'pip install -q --root-user-action=ignore python-odata 2>&1 | grep -v notice; python /src/client.py'
