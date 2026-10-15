#!/bin/sh
# Registers margin-ci's runner with GitHub once, on the Mac mini, with the owner's `gh`
# login (the registration token never leaves this command). The registration is kept on
# margin-ci's host share, so it survives Orchard re-cloning the VM.
#
#   deploy/ci/register-margin-ci.sh            # then restart margin-ci or rerun its startup script
#
# Labels: self-hosted, Linux, ARM64 (the runner adds them) and margin. Jobs pick it with
# runs-on: [self-hosted, Linux, ARM64, margin]; the Mac host runner is [..., macOS, ...].
set -eu

repo="${MARGIN_REPO:-Nkr1shna/margin}"
vm="${MARGIN_CI_VM:-margin-ci}"
export ORCHARD_HOME="${ORCHARD_HOME:-$HOME/Library/Application Support/ClusterBar/orchard-home}"

token="$(gh api -X POST "repos/$repo/actions/runners/registration-token" --jq .token)"
orchard ssh vm "$vm" "cd /mnt/shared/margin/runner && ./config.sh --unattended --replace \
  --url https://github.com/$repo --token $token --name $vm --labels margin \
  --work /home/admin/actions-work"
echo "Registered $vm. Start it: orchard ssh vm $vm 'sh -s' < deploy/ci/margin-ci-startup.sh"
