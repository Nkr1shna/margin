#!/bin/sh
# The Linux CI steps, in one place: CI / Linux (ci.yml, on margin-ci) and the local check
# (deploy/local/linux-check.sh, in a throwaway clone of the same image) both run this, so
# what passes locally is what CI checks.
#
#   deploy/ci/linux-ci.sh [--release]      from the repository root, in the Linux image
#
# Format, clippy and the tests for the workspace minus margin-app (the app builds on the
# Mac host), then the deploy scripts' offline test. --release also builds margin-service as
# build-service does, into $MARGIN_RELEASE_TARGET_DIR when set. Uses $CARGO_TARGET_DIR when
# set.
set -eu

release=0
for arg in "$@"; do
  case "$arg" in
    --release) release=1 ;;
    *) echo "linux-ci.sh: unknown option $arg" >&2; exit 2 ;;
  esac
done

export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
# margin-eval's live commands refuse to run when CI is set; tests never load the real
# embedding model.
export CI=true
export MARGIN_EMBED_MODEL_DIR=/nonexistent/margin-embed-model
# Fits the 6 GB runner: line tables instead of full debug info (most of the link's memory;
# backtraces keep their line numbers) and lld instead of GNU ld.
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_TEST_DEBUG=line-tables-only
export RUSTFLAGS="-C link-arg=-fuse-ld=lld"

step() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::group::$1"; else printf '\n== %s\n' "$1"; fi
}
endstep() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::endgroup::"; fi
}

step "Toolchain"
rustc --version
cargo --version
endstep

step "Format"
cargo fmt --all --check
endstep

step "Clippy"
cargo clippy --locked --workspace --exclude margin-app --all-targets -- -D warnings
endstep

step "Tests"
cargo test --locked --workspace --exclude margin-app
endstep

step "Deploy script tests"
sh deploy/scripts/release-snapshot-test.sh
endstep

if [ "$release" = 1 ]; then
  step "Release build (margin-service)"
  if [ -n "${MARGIN_RELEASE_TARGET_DIR:-}" ]; then
    cargo build --release --locked -p margin-service --target-dir "$MARGIN_RELEASE_TARGET_DIR"
  else
    cargo build --release --locked -p margin-service
  fi
  endstep
fi
