#!/bin/sh
# Keeps the runners' cargo caches ($HOME/.cache/margin-ci) from filling the disk. Every CI
# job that builds into that folder calls this first: CI / Linux and CI / macOS (ci.yml) and
# build-service.yml, which all share the margin-ci VM's disk on Linux.
#
#   deploy/ci/prune-cache.sh <target dir this job uses> [min free GB, default 25]
#
# 1. Caches of other environments unused for a week go.
# 2. This job's incremental state goes (never needed across runs, and it only grows).
# 3. Under the minimum free: the other caches go (the test and release caches compete).
# 4. Still under it: this job's cache goes too. A cold build is slower, not wrong.
set -eu

target="${1:?usage: prune-cache.sh <target dir> [min free GB]}"
min_gb="${2:-25}"
cache="$(dirname "$target")"
name="$(basename "$target")"
mkdir -p "$cache"

free_gb() { df -Pk "$cache" | awk 'NR==2 {print int($4 / 1048576)}'; }
others() { find "$cache" -mindepth 1 -maxdepth 1 -name 'target-*' ! -name "$name" "$@"; }

others -mtime +7 -exec rm -rf {} + 2>/dev/null || true
rm -rf "$target/debug/incremental" "$target/release/incremental" 2>/dev/null || true

free="$(free_gb)"
echo "Free disk for the cargo cache: ${free} GB (minimum ${min_gb} GB)"
if [ "${free:-0}" -lt "$min_gb" ]; then
  echo "::warning::under ${min_gb} GB free; removing the other cargo caches"
  others -exec rm -rf {} + 2>/dev/null || true
  free="$(free_gb)"
fi
if [ "${free:-0}" -lt "$min_gb" ]; then
  echo "::warning::still under ${min_gb} GB free; removing this job's cargo cache ($name)"
  rm -rf "$target"
  free="$(free_gb)"
fi
echo "Free disk now: ${free} GB"
