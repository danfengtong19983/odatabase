#!/bin/sh
# Runs the Microsoft.OData.Core client against a fresh conformance server. Needs Docker.
set -u
. "$(dirname "$0")/server.sh"
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=$ROOT \
  -v "$PWD/conformance/dotnet:/src:ro" -v odatabase-nuget:/root/.nuget \
  mcr.microsoft.com/dotnet/sdk:8.0 \
  sh -c 'cp -r /src /tmp/app && cd /tmp/app && dotnet run -c Release > /tmp/out 2>&1; code=$?; grep -v "warning CS" /tmp/out; exit $code'
