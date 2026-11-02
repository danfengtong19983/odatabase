#!/bin/sh
# Runs the Apache Olingo client against a fresh conformance server. Needs Docker.
set -u
. "$(dirname "$0")/server.sh"
docker run --rm --add-host=host.docker.internal:host-gateway \
  -e ODATA_ROOT=$ROOT \
  -v "$PWD/conformance/java:/src:ro" -v odatabase-m2:/root/.m2 \
  maven:3.9-eclipse-temurin-17 \
  sh -c 'cp -r /src /tmp/app && cd /tmp/app && mvn -q -B compile exec:java 2>&1'
