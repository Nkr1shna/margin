#!/bin/sh
# User part of the Linux guest image: Rust for `admin`, who runs the CI jobs. The rustup
# installer is checked against its published SHA-256.
set -eu

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
base="https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-gnu"
curl -fsSL -o "$tmp/rustup-init" "$base/rustup-init"
curl -fsSL -o "$tmp/rustup-init.sha256" "$base/rustup-init.sha256"
[ "$(sha256sum "$tmp/rustup-init" | cut -d' ' -f1)" = "$(cut -d' ' -f1 "$tmp/rustup-init.sha256")" ] \
  || { echo "rustup-init checksum mismatch" >&2; exit 1; }
chmod +x "$tmp/rustup-init"
# The toolchain rust-toolchain.toml pins (RUST_TOOLCHAIN from the Packer template), so
# fresh clones don't download it again.
"$tmp/rustup-init" -y --profile minimal --default-toolchain "${RUST_TOOLCHAIN:-stable}" -c clippy -c rustfmt
. "$HOME/.cargo/env"
rustc --version
cargo --version

# See linux-system.sh: an empty .bash_logout, so Orchard's startup scripts keep their status.
: > "$HOME/.bash_logout"
