#!/bin/sh
# Runs the Apache Olingo client against a fresh conformance server. Needs Docker.
set -u
cd "$(dirname "$0")/.."
PORT=${PORT:-38921}
cargo build -q -p odatabase --example conformance_server || exit 1
ODATA_ADDR=127.0.0.1:$PORT target/debug/examples/conformance_server >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER' EXIT
sleep 1
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=http://host.docker.internal:$PORT/odata \
  -v "$PWD/conformance/java:/src:ro" -v odatabase-m2:/root/.m2 \
  maven:3.9-eclipse-temurin-17 \
  sh -c 'cp -r /src /tmp/app && cd /tmp/app && mvn -q -B compile exec:java 2>&1'
