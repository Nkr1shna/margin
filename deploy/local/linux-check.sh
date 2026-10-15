#!/bin/sh
# Runs CI / Linux on this Mac before you push: clones the Linux image CI uses
# (ghcr.io/nkr1shna/margin-linux, built by deploy/image and published by
# .github/workflows/image.yml) into a throwaway Tart VM, shares this checkout into it
# read-only and runs deploy/ci/linux-ci.sh, the same steps CI runs. The VM is deleted
# afterwards; Cargo's build output and crate downloads are kept in a cache folder on this
# Mac, so the second run is incremental.
#
#   deploy/local/linux-check.sh [--release] [--no-cache] [--image NAME]
#
# --release   also build margin-service as build-service does; the binary is copied to
#             $cache/out/margin-service
# --no-cache  build in the VM only (a cold build, nothing kept)
# --image     the Tart image to clone (default ghcr.io/nkr1shna/margin-linux:latest; pulled
#             the first time, `tart pull` it again to update; a local name like
#             margin-linux-v1 works too)
# MARGIN_CHECK_CPU / MARGIN_CHECK_MEMORY (MB) size the VM (default 4 / 6144, plus 4 GB of
# swap inside it). The first run is a cold build (several minutes); later runs reuse the
# cache. Give it a Mac with that much memory free: on the Mac mini next to margin-ci and
# the guests it runs out.
#
# Needs Tart on an Apple-silicon Mac (brew install cirruslabs/cli/tart).
set -eu

repo="$(cd "$(dirname "$0")/../.." && pwd)"
image=ghcr.io/nkr1shna/margin-linux:latest
release=""
use_cache=1
while [ $# -gt 0 ]; do
  case "$1" in
    --release) release=--release; shift ;;
    --no-cache) use_cache=0; shift ;;
    --image) image="${2:?--image needs a name}"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "linux-check.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

command -v tart >/dev/null 2>&1 || { echo "linux-check.sh: needs Tart (brew install cirruslabs/cli/tart)" >&2; exit 1; }
if ! tart list --format json | grep -q "\"Name\" *: *\"$image\""; then
  case "$image" in
    */*) echo "Pulling $image (once; a few GB)…"; tart pull "$image" ;;
    *) echo "linux-check.sh: no local Tart image \"$image\" (packer build deploy/image/margin-linux.pkr.hcl)" >&2; exit 1 ;;
  esac
fi

cache="${MARGIN_CHECK_CACHE:-$HOME/.cache/margin-linux-check}"
vm="margin-check-$$"
cleanup() {
  tart stop "$vm" >/dev/null 2>&1 || true
  tart delete "$vm" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

tart clone "$image" "$vm"
tart set "$vm" --cpu "${MARGIN_CHECK_CPU:-4}" --memory "${MARGIN_CHECK_MEMORY:-6144}"
dirs="--dir=repo:$repo:ro"
if [ "$use_cache" = 1 ]; then
  mkdir -p "$cache/target" "$cache/registry" "$cache/out"
  dirs="$dirs --dir=cache:$cache"
fi
# shellcheck disable=SC2086 # two --dir arguments
tart run "$vm" --no-graphics $dirs >/dev/null 2>&1 &

# The guest agent answers once the VM has booted.
i=0
until tart exec "$vm" true >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -le 60 ] || { echo "linux-check.sh: the VM didn't come up in 2 minutes" >&2; exit 1; }
  sleep 2
done

echo "Running CI / Linux in $vm ($image, $(tart get "$vm" --format json | grep -o '"CPU" *: *[0-9]*' | grep -o '[0-9]*') CPU)…"
status=0
tart exec "$vm" bash -c "
  set -eu
  sudo mkdir -p /mnt/host
  mountpoint -q /mnt/host || sudo mount -t virtiofs com.apple.virtio-fs.automount /mnt/host
  # Swap for the peaks of linking the workspace, as on margin-ci.
  sudo fallocate -l 4G /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile >/dev/null && sudo swapon /swapfile
  if [ -d /mnt/host/cache ]; then
    export CARGO_TARGET_DIR=/mnt/host/cache/target
    rm -rf \$HOME/.cargo/registry && ln -s /mnt/host/cache/registry \$HOME/.cargo/registry
  else
    export CARGO_TARGET_DIR=\$HOME/target
  fi
  cd /mnt/host/repo
  sh deploy/ci/linux-ci.sh $release
  if [ -n '$release' ] && [ -d /mnt/host/cache ]; then
    cp \"\$CARGO_TARGET_DIR/release/margin-service\" /mnt/host/cache/out/margin-service
  fi
" || status=$?

if [ "$status" = 0 ]; then
  echo
  echo "CI / Linux passed locally."
  [ -n "$release" ] && [ "$use_cache" = 1 ] && echo "Release binary: $cache/out/margin-service"
else
  echo
  echo "CI / Linux failed locally (exit $status)." >&2
fi
exit "$status"
