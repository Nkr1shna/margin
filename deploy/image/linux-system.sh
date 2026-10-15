#!/bin/sh
# Root part of the Linux guest image (margin-linux.pkr.hcl). Pinned versions; uv and the
# GitHub runner are checked against their published SHA-256 (pdfium publishes none).
set -eu

PDFIUM_RELEASE="chromium/8100"   # crates/video/src/tools.rs PDFIUM_RELEASE
UV_VERSION="0.13.1"
RUNNER_VERSION="2.338.0"
MANIMGX_PYTHON="3.14"

apt-get update
apt-get install -y --no-install-recommends \
  build-essential clang lld libclang-dev pkg-config \
  libc++-dev libc++abi-dev libc++1 libc++abi1 \
  libheif-dev libheif1 \
  tesseract-ocr tesseract-ocr-eng ffmpeg bubblewrap \
  python3 ca-certificates curl git jq unzip zstd rsync gnupg

# The scene sandbox (bubblewrap) needs unprivileged user namespaces, which Ubuntu 24.04's
# AppArmor restricts. Allow them for bwrap only, the way Ubuntu documents for one program,
# instead of turning the restriction off system-wide (plan Q6).
cat > /etc/apparmor.d/bwrap <<'EOF'
abi <abi/4.0>,
include <tunables/global>

profile bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,
  include if exists <local/bwrap>
}
EOF
apparmor_parser -r /etc/apparmor.d/bwrap || true

# cloudflared and gh from their vendors' apt repositories.
install -d -m 0755 /usr/share/keyrings
curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg -o /usr/share/keyrings/cloudflare-main.gpg
echo "deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared noble main" \
  > /etc/apt/sources.list.d/cloudflared.list
curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /usr/share/keyrings/githubcli-archive-keyring.gpg
echo "deb [arch=arm64 signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
  > /etc/apt/sources.list.d/github-cli.list
apt-get update
apt-get install -y --no-install-recommends cloudflared gh

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# pdfium (PDF reader), where the service looks first after its own tools folder.
curl -fsSL -o "$tmp/pdfium.tgz" \
  "https://github.com/bblanchon/pdfium-binaries/releases/download/$(echo "$PDFIUM_RELEASE" | sed 's|/|%2F|')/pdfium-linux-arm64.tgz"
mkdir -p "$tmp/pdfium" && tar -xzf "$tmp/pdfium.tgz" -C "$tmp/pdfium"
install -m 0644 "$tmp/pdfium/lib/libpdfium.so" /usr/local/lib/
ldconfig

# uv (system-wide) and manimgx for every user, including the service's `margin` user.
curl -fsSL -o "$tmp/uv.tgz" \
  "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/uv-aarch64-unknown-linux-gnu.tar.gz"
curl -fsSL -o "$tmp/uv.tgz.sha256" \
  "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/uv-aarch64-unknown-linux-gnu.tar.gz.sha256"
[ "$(sha256sum "$tmp/uv.tgz" | cut -d' ' -f1)" = "$(cut -d' ' -f1 "$tmp/uv.tgz.sha256")" ] || { echo "uv checksum mismatch" >&2; exit 1; }
tar -xzf "$tmp/uv.tgz" -C "$tmp"
install -m 0755 "$tmp"/uv-aarch64-unknown-linux-gnu/uv "$tmp"/uv-aarch64-unknown-linux-gnu/uvx /usr/local/bin/
export UV_TOOL_DIR=/opt/uv/tools UV_TOOL_BIN_DIR=/usr/local/bin UV_PYTHON_INSTALL_DIR=/opt/uv/python
uv tool install --python "$MANIMGX_PYTHON" manimgx
chmod -R a+rX /opt/uv
manimgx --version

# The GitHub Actions runner, unpacked but not registered (margin-ci registers it once and
# keeps the registration on its host share).
curl -fsSL -o "$tmp/runner.tgz" \
  "https://github.com/actions/runner/releases/download/v$RUNNER_VERSION/actions-runner-linux-arm64-$RUNNER_VERSION.tar.gz"
want="$(curl -fsSL "https://api.github.com/repos/actions/runner/releases/tags/v$RUNNER_VERSION" \
  | python3 -c 'import json,re,sys; b=json.load(sys.stdin)["body"]; m=re.search(r"<!-- BEGIN SHA linux-arm64 -->([0-9a-f]{64})", b); print(m.group(1) if m else "")')"
[ -n "$want" ] && [ "$(sha256sum "$tmp/runner.tgz" | cut -d' ' -f1)" = "$want" ] || { echo "runner checksum mismatch" >&2; exit 1; }
install -d -m 0755 /opt/actions-runner-dist
tar -xzf "$tmp/runner.tgz" -C /opt/actions-runner-dist
/opt/actions-runner-dist/bin/installdependencies.sh

# Orchard runs startup scripts in a login shell with `set -e`; Ubuntu's stock .bash_logout
# calls clear_console, which fails without a terminal and turns every script's exit
# status into 1. New users get an empty one (admin's is emptied in linux-user.sh).
: > /etc/skel/.bash_logout

apt-get clean
rm -rf /var/lib/apt/lists/*
