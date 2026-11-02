#!/bin/sh
# Converts $metadata with the OASIS odata-openapi tool. Needs Docker.
set -u
. "$(dirname "$0")/server.sh"
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=$ROOT \
  -v "$PWD/conformance/openapi.js:/src/openapi.js:ro" node:22-slim \
  sh -c 'mkdir /tmp/app && cd /tmp/app && npm install --silent --no-audit --no-fund odata-openapi@0.29.0 >/dev/null 2>&1; cp /src/openapi.js . && node openapi.js'
