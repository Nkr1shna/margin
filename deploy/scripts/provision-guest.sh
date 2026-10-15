#!/bin/sh
# Sets up (or sets up again) the dev or prod VM for Margin: an Ubuntu 24.04 guest from
# the image deploy/image builds, running systemd units (docs/plan-deploy.md §7,
# docs/plan-linux-service.md §5).
#
#   provision-guest.sh dev|prod [--rekey] [--tunnel-uuid UUID]   on the Mac mini
#   provision-guest.sh --startup-script dev|prod                 prints Orchard's startup script
#   provision-guest.sh --in-guest dev|prod [UUID]                in the guest (startup script)
#
# Orchard clones the VM again from its image whenever it restarts or reschedules it, so
# everything that matters lives in the host folder ~/MarginVMs/margin-<env>, shared into
# the guest (Orchard hostDirs "margin:<that folder>") at /mnt/shared/margin. The host side
# copies these scripts and the unit templates into that folder; the in-guest side, which
# the startup script runs on every boot, writes the units and starts what can start, so a
# re-clone heals itself. Idempotent.
#
# --rekey forgets the guest's old SSH host key (a recreated VM has a new one).
# --tunnel-uuid writes cloudflared/config.yml for that tunnel when the share has none.
# Secrets (secrets.toml on the share) come from the GitHub Environments through deploy.sh.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/common.sh"

# ---- In the guest ----
if [ "${1:-}" = "--in-guest" ]; then
  env_name="${2:?environment}"
  tunnel_uuid="${3:-}"
  require_systemd
  check_persistent_mount "$env_name"
  p="$MARGIN_PERSIST_GUEST"
  mkdir -p "$p/data" "$p/backups" "$p/bin" "$p/cloudflared" "$p/templates"
  chmod 700 "$p/cloudflared"
  todo=""

  # service.toml on the share: a starter one, or move an older one's paths onto this mount.
  config="$p/service.toml"
  if [ ! -f "$config" ]; then
    cat > "$config" <<EOF
environment = "$env_name"
public_url = "$(public_url "$env_name")"
bind = "127.0.0.1:8787"
data_dir = "$p/data"
secrets_file = "$p/secrets.toml"
github_client_id = ""
allowed_github_login = "Nkr1shna"
EOF
    echo "Wrote a starter $config."
  fi
  sed -i -e "s|^data_dir = \".*\"$|data_dir = \"$p/data\"|" \
    -e "s|^secrets_file = \".*\"$|secrets_file = \"$p/secrets.toml\"|" "$config"
  grep -q '^secrets_file' "$config" || printf 'secrets_file = "%s/secrets.toml"\n' "$p" >> "$config"
  check_config "$env_name"
  if [ -z "$(toml_value "$config" github_client_id)" ]; then
    todo="$todo
  - github_client_id is empty in $config (the \"Margin $env_name\" OAuth app's client id)"
  fi
  if ! grep -q '^github-client-secret' "$p/secrets.toml" 2>/dev/null; then
    todo="$todo
  - No github-client-secret in $p/secrets.toml yet: the next deploy writes it from the
    GitHub Environment secret MARGIN_GITHUB_CLIENT_SECRET"
  fi

  # The tunnel: its config and credentials on the share.
  tunnel_config="$p/cloudflared/config.yml"
  if [ ! -f "$tunnel_config" ] && [ -n "$tunnel_uuid" ]; then
    host="$(public_url "$env_name")"
    host="${host#https://}"
    sed -e "s|<UUID>|$tunnel_uuid|g" -e "s|<HOST>|$host|g" -e "s|<CREDS_DIR>|$p/cloudflared|g" \
      "$p/templates/cloudflared-config.yml" > "$tunnel_config"
    echo "Wrote $tunnel_config for tunnel $tunnel_uuid ($host)."
  fi
  if [ -f "$tunnel_config" ]; then
    uuid="$(sed -n 's/^tunnel:[[:space:]]*\([0-9a-f-]*\).*/\1/p' "$tunnel_config" | head -n 1)"
    sed -i "s|^credentials-file:.*$|credentials-file: $p/cloudflared/$uuid.json|" "$tunnel_config"
    [ -f "$p/cloudflared/$uuid.json" ] || todo="$todo
  - $p/cloudflared/$uuid.json (the tunnel's credentials) is missing"
  else
    todo="$todo
  - No tunnel config: cloudflared tunnel create $(vm_name "$env_name") (with a Cloudflare login),
    then provision-guest.sh $env_name --tunnel-uuid <UUID> on the host"
  fi

  # The units, from the templates on the share. MemoryMax leaves the guest ~15% headroom.
  mem_kb="$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)"
  memory_max="$((mem_kb * 85 / 100 / 1024))M"
  changed=0
  for unit in "$MARGIN_SERVICE_UNIT.service" "$MARGIN_TUNNEL_UNIT.service" \
    "$MARGIN_BACKUP_UNIT.service" "$MARGIN_BACKUP_UNIT.timer"; do
    tmp="$(mktemp)"
    sed -e "s|__PERSIST__|$p|g" -e "s|__MEMORY_MAX__|$memory_max|g" "$p/templates/$unit" > "$tmp"
    if ! cmp -s "$tmp" "/etc/systemd/system/$unit"; then
      sudo install -m 0644 "$tmp" "/etc/systemd/system/$unit"
      changed=1
    fi
    rm -f "$tmp"
  done
  [ "$changed" = 0 ] || sudo systemctl daemon-reload
  sudo systemctl enable --now "$MARGIN_BACKUP_UNIT.timer" >/dev/null 2>&1
  if [ -f "$tunnel_config" ]; then
    sudo systemctl enable "$MARGIN_TUNNEL_UNIT.service" >/dev/null 2>&1
    if [ "$changed" = 1 ]; then sudo systemctl restart "$MARGIN_TUNNEL_UNIT.service"; else sudo systemctl start "$MARGIN_TUNNEL_UNIT.service"; fi
  fi
  sudo systemctl enable "$MARGIN_SERVICE_UNIT.service" >/dev/null 2>&1
  if [ -x "$p/bin/margin-service" ] && [ -n "$(toml_value "$config" github_client_id)" ]; then
    if [ "$changed" = 1 ]; then sudo systemctl restart "$MARGIN_SERVICE_UNIT.service"; else sudo systemctl start "$MARGIN_SERVICE_UNIT.service"; fi
    echo "margin-service is $(systemctl is-active "$MARGIN_SERVICE_UNIT.service")."
  elif [ ! -x "$p/bin/margin-service" ]; then
    todo="$todo
  - No binary on the share yet: the next deploy-$env_name run (or deploy.sh $env_name …) installs it"
  fi

  echo "Provisioned $(vm_name "$env_name") (systemd units; config, data, secrets and backups on $p)."
  if [ -n "$todo" ]; then
    echo "Still to do:$todo"
  fi
  exit 0
fi

# ---- The Orchard startup script ----
if [ "${1:-}" = "--startup-script" ]; then
  env_name="${2:?environment}"
  name="$(vm_name "$env_name")"
  pub="$MARGIN_DEPLOY_KEY.pub"
  [ -r "$pub" ] || die "no $pub"
  # Orchard types this into a login shell (`set -e`, then `exit`); the work runs in a
  # non-interactive bash because Ubuntu's interactive bash breaks under `set -u`.
  cat <<EOF
bash -eu <<'MARGIN_STARTUP'
sudo hostnamectl set-hostname $name
sudo systemctl restart avahi-daemon 2>/dev/null || true
sudo mkdir -p $MARGIN_SHARE_MOUNT
mountpoint -q $MARGIN_SHARE_MOUNT || sudo mount -t virtiofs com.apple.virtio-fs.automount $MARGIN_SHARE_MOUNT
mkdir -p ~/.ssh && chmod 700 ~/.ssh
grep -qxF '$(cat "$pub")' ~/.ssh/authorized_keys 2>/dev/null || echo '$(cat "$pub")' >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
if [ -f $MARGIN_PERSIST_GUEST/bin/provision-guest.sh ]; then
  sh $MARGIN_PERSIST_GUEST/bin/provision-guest.sh --in-guest $env_name
else
  echo "$name: not provisioned yet (run deploy/scripts/provision-guest.sh $env_name on the host)."
fi
MARGIN_STARTUP
EOF
  exit 0
fi

# ---- On the host ----
[ $# -ge 1 ] || die "usage: provision-guest.sh dev|prod [--rekey] [--tunnel-uuid UUID] | --startup-script dev|prod"
env_name="$1"
shift
vm_name "$env_name" >/dev/null
check_allowed_env "$env_name"
rekey=0
tunnel_uuid=""
while [ $# -gt 0 ]; do
  case "$1" in
    --rekey) rekey=1; shift ;;
    --tunnel-uuid) tunnel_uuid="${2:?--tunnel-uuid needs a value}"; shift 2 ;;
    *) die "unknown option $1" ;;
  esac
done
case "$tunnel_uuid" in *[!0-9a-f-]*) die "--tunnel-uuid takes the tunnel's UUID" ;; esac

host_dir="$(host_persist_dir "$env_name")"
mkdir -p "$host_dir"
chmod 700 "$host_dir"
if [ ! -f "$host_dir/$MARGIN_MARKER" ]; then
  vm_name "$env_name" > "$host_dir/$MARGIN_MARKER"
  echo "Created $host_dir. Share it into $(vm_name "$env_name"): Orchard hostDirs \"margin:$host_dir\"."
fi
[ "$(cat "$host_dir/$MARGIN_MARKER")" = "$(vm_name "$env_name")" ] || die "$host_dir/$MARGIN_MARKER names another VM"

# The scripts and templates go straight into the share (a folder on this Mac).
repo="$(cd "$here/../.." && pwd)"
mkdir -p "$host_dir/bin" "$host_dir/templates"
for f in common.sh backup-margin.sh deploy.sh provision-guest.sh; do
  install -m 0755 "$here/$f" "$host_dir/bin/$f"
done
for unit in "$MARGIN_SERVICE_UNIT.service" "$MARGIN_TUNNEL_UNIT.service" \
  "$MARGIN_BACKUP_UNIT.service" "$MARGIN_BACKUP_UNIT.timer"; do
  install -m 0644 "$repo/deploy/systemd/$unit" "$host_dir/templates/$unit"
done
install -m 0644 "$repo/deploy/cloudflared/config.yml" "$host_dir/templates/cloudflared-config.yml"
echo "Copied the scripts and unit templates into $host_dir."

if [ "$rekey" = 1 ]; then
  ssh-keygen -R "$(guest_host "$env_name")" -f "$MARGIN_KNOWN_HOSTS" >/dev/null 2>&1 || true
  echo "Forgot the old host key of $(guest_host "$env_name")."
fi

guest_os="$(guest_ssh "$env_name" uname -s)" || die "couldn't ssh to $(guest_host "$env_name"); is the startup script (--startup-script $env_name) set in Orchard? A changed host key needs --rekey"
[ "$guest_os" = Linux ] || die "$(guest_host "$env_name") runs $guest_os; the guests are Linux (deploy/image)"
guest_ssh "$env_name" "test -f '$MARGIN_PERSIST_GUEST/$MARGIN_MARKER'" \
  || die "$host_dir isn't shared into $(vm_name "$env_name") at $MARGIN_PERSIST_GUEST: add Orchard hostDirs \"margin:$host_dir\" and restart the VM"
guest_ssh "$env_name" "sh '$MARGIN_PERSIST_GUEST/bin/provision-guest.sh' --in-guest $env_name $tunnel_uuid"
