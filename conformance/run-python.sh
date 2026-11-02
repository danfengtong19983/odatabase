#!/bin/sh
# Runs python-odata against a fresh conformance server. Needs Docker.
set -u
. "$(dirname "$0")/server.sh"
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=$ROOT \
  -v "$PWD/conformance/python:/src:ro" python:3.12-slim \
  sh -c 'pip install -q --root-user-action=ignore python-odata==0.8.1 2>&1 | grep -v notice; python /src/client.py'
