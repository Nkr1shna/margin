#!/bin/sh
# Orchard startup script for margin-ci, the Linux GitHub runner VM (docs/plan-deploy.md §4,
# docs/plan-linux-service.md §5). Runs as admin over SSH each time Orchard starts the VM,
# which is a fresh clone of the margin-linux image every time:
#
#   orchard create vm margin-ci --os linux --image margin-linux-v1 --cpu 4 --memory 6144 \
#     --disk-size 80 --restart-policy OnFailure --labels host=mac-mini \
#     --host-dirs margin:$HOME/MarginVMs/margin-ci --startup-script @deploy/ci/margin-ci-startup.sh
#
# The runner's files and registration live on the host share (/mnt/shared/margin/runner),
# so a re-clone keeps them; its work folder and Cargo's build cache live on the VM's disk
# (faster), so only a VM restart costs a full rebuild. Registering is a one-time step:
# deploy/ci/register-margin-ci.sh on the Mac mini.
#
# Orchard types a startup script line by line into an interactive login shell (then
# `exit`), and Ubuntu's interactive bash breaks under `set -u`; so the work runs in a
# non-interactive bash fed by the heredoc below. Running this file with `sh` works too.
bash -eu <<'MARGIN_CI_STARTUP'

share=/mnt/shared
runner="$share/margin/runner"

sudo mkdir -p "$share"
mountpoint -q "$share" || sudo mount -t virtiofs com.apple.virtio-fs.automount "$share"
[ -d "$share/margin" ] || { echo "the host share isn't mounted at $share/margin" >&2; exit 1; }

# First boot ever: the runner's files from the image onto the share.
if [ ! -x "$runner/run.sh" ]; then
  mkdir -p "$runner"
  cp -a /opt/actions-runner-dist/. "$runner/"
fi

mkdir -p "$HOME/actions-work" "$HOME/.cache/margin-ci"

# Swap for the peaks of linking the workspace (rustc and the linker together can pass the
# VM's memory); on the VM's own disk, so a re-clone recreates it.
if ! swapon --show=NAME --noheadings | grep -q '^/swapfile$'; then
  [ -f /swapfile ] || { sudo fallocate -l 4G /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile >/dev/null; }
  sudo swapon /swapfile
fi

if [ ! -f "$runner/.runner" ]; then
  echo "margin-ci: the runner isn't registered yet (run deploy/ci/register-margin-ci.sh on the host)."
  exit 0
fi

# PATH for jobs: Rust from rustup, uv tools, the system.
printf '%s\n' "$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" > "$runner/.path"

sudo tee /etc/systemd/system/margin-ci-runner.service >/dev/null <<EOF
[Unit]
Description=GitHub Actions runner (margin-ci)
After=network-online.target
Wants=network-online.target

[Service]
User=$(id -un)
WorkingDirectory=$runner
ExecStart=$runner/run.sh
Restart=always
RestartSec=5
KillMode=process
KillSignal=SIGTERM
TimeoutStopSec=5min

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now margin-ci-runner.service
systemctl is-active margin-ci-runner.service
MARGIN_CI_STARTUP
