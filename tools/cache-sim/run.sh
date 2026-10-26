#!/bin/sh
set -eu
sim_output="${KASU_SIM_OUTPUT:-/tmp/kasu-cache-sim}"
mkdir -p "$sim_output"
for seed in 0 1 42 2026; do
    /out/kasu-sim --scenario cache --seed "$seed" --clients 1000 > "$sim_output/seed-$seed-1.json"
    /out/kasu-sim --scenario cache --seed "$seed" --clients 1000 > "$sim_output/seed-$seed-2.json"
    cmp "$sim_output/seed-$seed-1.json" "$sim_output/seed-$seed-2.json"
done
