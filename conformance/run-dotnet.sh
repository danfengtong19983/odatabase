#!/bin/sh
# Runs the Microsoft.OData.Core client against a fresh conformance server. Needs Docker.
set -u
cd "$(dirname "$0")/.."
PORT=${PORT:-38920}
cargo build -q -p odatabase --example conformance_server || exit 1
ODATA_ADDR=127.0.0.1:$PORT target/debug/examples/conformance_server >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER' EXIT
sleep 1
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=http://host.docker.internal:$PORT/odata \
  -v "$PWD/conformance/dotnet:/src:ro" -v odatabase-nuget:/root/.nuget \
  mcr.microsoft.com/dotnet/sdk:8.0 \
  sh -c 'cp -r /src /tmp/app && cd /tmp/app && dotnet run -c Release 2>&1 | grep -v "warning CS"'
