#!/bin/sh
set -eu
export KASU_CRITERION_OUTPUT="${KASU_CRITERION_OUTPUT:-/tmp/kasu-criterion}"
export CRITERION_HOME="$KASU_CRITERION_OUTPUT/criterion"
mkdir -p "$KASU_CRITERION_OUTPUT"
exec /out/throughput --bench "$@"
