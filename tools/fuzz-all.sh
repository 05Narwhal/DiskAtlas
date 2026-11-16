#!/bin/bash
# Disk Atlas - A cross-platform filesystem explorer.
#
# Copyright (C) 2026 Julian A. Loy.
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
#
# See the LICENSE file for the full license text.
#
# Runs every cargo-fuzz target in fuzz/fuzz_targets for a fixed time and
# writes a one-line result per target.
#
#   tools/fuzz-all.sh [seconds per target, default 60] [target ...]
#
# Linux or macOS, nightly toolchain, cargo-fuzz installed. Seeds are written
# first. Logs go to fuzz/logs/<target>.log, results to fuzz/logs/summary.txt,
# and a crash leaves its input in fuzz/artifacts/<target>/. Run it from a
# native filesystem (not /mnt/c under WSL): the sanitizer build is slow on
# drvfs.

set -u

SECS="${1:-60}"
shift || true
cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT="$PWD"
TARGETS="${*:-$(cd fuzz/fuzz_targets && ls *.rs | sed 's/\.rs$//')}"

mkdir -p fuzz/logs
: > fuzz/logs/summary.txt

DISK_ATLAS_FUZZ_SEEDS="$ROOT/fuzz/seeds" \
    cargo test -p disk-atlas-lib --lib write_fuzz_seeds -- --ignored \
    > fuzz/logs/seeds.log 2>&1 || echo "seed generation failed, see fuzz/logs/seeds.log" >&2

for t in $TARGETS; do
    mkdir -p "fuzz/corpus/$t" "fuzz/seeds/$t"
    started=$(date +%s)
    # -timeout: a hang is a bug (every loop on disk data is bounded).
    # Absolute corpus paths: cargo-fuzz may run the fuzzer from fuzz/.
    cargo fuzz run --fuzz-dir fuzz "$t" "$ROOT/fuzz/corpus/$t" "$ROOT/fuzz/seeds/$t" \
        -- -max_total_time="$SECS" -max_len=1048576 -timeout=10 -rss_limit_mb=2048 \
        > "fuzz/logs/$t.log" 2>&1
    rc=$?
    took=$(($(date +%s) - started))
    if [ "$rc" -eq 0 ]; then
        stats=$(grep -E '^#[0-9]+.*(DONE|cov:)' "fuzz/logs/$t.log" | tail -1 | cut -c1-110)
        echo "ok     $t (${took}s) $stats" | tee -a fuzz/logs/summary.txt
    else
        echo "FAILED $t (rc $rc, ${took}s) see fuzz/logs/$t.log" | tee -a fuzz/logs/summary.txt
    fi
done
echo "done"
