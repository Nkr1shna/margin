#!/bin/sh
# Sets up (or sets up again) the dev or prod VM for Margin (docs/plan-deploy.md §7).
# Runs on the Mac mini, by the owner or an agent at the host's shell:
#
#   provision-guest.sh dev|prod [--rekey] [--tunnel-uuid UUID]
#   provision-guest.sh --startup-script dev|prod
#
# Orchard clones the VM again from its image whenever it restarts or reschedules it, so
# everything that matters lives in the host folder ~/MarginVMs/margin-<env>, shared into
# the guest (Orchard hostDirs "margin:<that folder>") at /Volumes/My Shared Files/margin.
# This script is idempotent: run it after creating a VM and after every recreation.
# It creates the host folder and its marker, then in the guest: installs cloudflared,
# links ~/.cloudflared to the share, writes a starter service.toml on the share (if
# none), installs the LaunchAgents from deploy/launchd and starts what can start.
#
# --rekey forgets the guest's old SSH host key (a recreated VM has a new one).
# --tunnel-uuid writes cloudflared/config.yml for that tunnel (after `cloudflared tunnel
# create`), when the share has none yet.
# --startup-script prints the Orchard startup script that, on every (re)creation, names
# the guest margin-<env> (so margin-<env>.local resolves) and authorizes the deploy key.
#
# What it can't do, and prints: Keychain secrets and `cloudflared tunnel login` need the
# owner in the VM's own window (Screen Sharing), never over SSH.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/common.sh"

# ---- In the guest ----
if [ "${1:-}" = "--in-guest" ]; then
  env_name="${2:?environment}"
  tunnel_uuid="${3:-}"
  require_launchd
  check_persistent_mount "$env_name"
  p="$MARGIN_PERSIST_GUEST"
  mkdir -p "$p/data" "$p/backups" "$p/bin" "$p/cloudflared" "$p/templates" \
    "$HOME/Library/Logs/Margin" "$HOME/.local/bin" "$HOME/Library/LaunchAgents"
  chmod 700 "$p/cloudflared"
  todo=""

  # service.toml on the share; `margin-service setup` (owner, in the VM's window) adds
  # the GitHub client id and puts the secret in the Keychain.
  config="$p/service.toml"
  if [ ! -f "$config" ]; then
    cat > "$config" <<EOF
environment = "$env_name"
public_url = "$(public_url "$env_name")"
bind = "127.0.0.1:8787"
data_dir = "$p/data"
github_client_id = ""
allowed_github_login = "Nkr1shna"
EOF
    echo "Wrote a starter $config."
  fi
  check_config "$env_name"
  if [ -z "$(toml_value "$config" github_client_id)" ]; then
    todo="$todo
  - In the VM's window: MARGIN_CONFIG=\"$config\" \"$p/bin/margin-service\" setup
    (environment $env_name, public URL $(public_url "$env_name"), the \"Margin $env_name\" OAuth app)"
  fi

  # cloudflared, its credentials on the share.
  brew="/opt/homebrew/bin/brew"
  if ! command -v cloudflared >/dev/null 2>&1 && [ ! -x /opt/homebrew/bin/cloudflared ]; then
    [ -x "$brew" ] || die "no Homebrew in this guest to install cloudflared with"
    "$brew" install cloudflared
  fi
  cloudflared="$(command -v cloudflared 2>/dev/null || echo /opt/homebrew/bin/cloudflared)"
  if [ -d "$HOME/.cloudflared" ] && [ ! -L "$HOME/.cloudflared" ]; then
    # Files from a login done before the link: move them to the share, never overwrite.
    for f in "$HOME/.cloudflared"/*; do
      [ -e "$f" ] || continue
      [ -e "$p/cloudflared/$(basename "$f")" ] || mv "$f" "$p/cloudflared/"
    done
    rmdir "$HOME/.cloudflared" 2>/dev/null || mv "$HOME/.cloudflared" "$HOME/.cloudflared.old-$(date +%s)"
  fi
  [ -L "$HOME/.cloudflared" ] || ln -s "$p/cloudflared" "$HOME/.cloudflared"
  if [ ! -f "$p/cloudflared/config.yml" ] && [ -n "$tunnel_uuid" ]; then
    host="$(public_url "$env_name")"
    host="${host#https://}"
    sed -e "s|<UUID>|$tunnel_uuid|g" -e "s|<HOST>|$host|g" -e "s|<CREDS_DIR>|$p/cloudflared|g" \
      "$p/templates/cloudflared-config.yml" > "$p/cloudflared/config.yml"
    echo "Wrote $p/cloudflared/config.yml for tunnel $tunnel_uuid ($host)."
  fi
  if [ ! -f "$p/cloudflared/cert.pem" ] && [ ! -f "$p/cloudflared/config.yml" ]; then
    todo="$todo
  - In the VM's window: cloudflared tunnel login; then on the host:
    the agent runs cloudflared tunnel create $(vm_name "$env_name") in the guest and
    provision-guest.sh $env_name --tunnel-uuid <UUID>"
  elif [ ! -f "$p/cloudflared/config.yml" ]; then
    todo="$todo
  - cloudflared tunnel create $(vm_name "$env_name") (in the guest), then
    provision-guest.sh $env_name --tunnel-uuid <UUID>"
  elif [ ! -f "$HOME/Library/LaunchAgents/com.cloudflare.cloudflared.plist" ]; then
    "$cloudflared" service install
    echo "Installed the cloudflared LaunchAgent."
  fi

  # The LaunchAgents.
  for label in "$MARGIN_SERVICE_LABEL" "$MARGIN_BACKUP_LABEL"; do
    sed -e "s|__PERSIST__|$p|g" -e "s|__HOME__|$HOME|g" "$p/templates/$label.plist" \
      > "$HOME/Library/LaunchAgents/$label.plist"
    plutil -lint "$HOME/Library/LaunchAgents/$label.plist" >/dev/null
  done
  uid="$(id -u)"
  launchctl bootout "gui/$uid/$MARGIN_BACKUP_LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$uid" "$HOME/Library/LaunchAgents/$MARGIN_BACKUP_LABEL.plist"
  if [ -x "$p/bin/margin-service" ] && [ -n "$(toml_value "$config" github_client_id)" ]; then
    launchctl bootout "gui/$uid/$MARGIN_SERVICE_LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$uid" "$HOME/Library/LaunchAgents/$MARGIN_SERVICE_LABEL.plist"
    echo "Started margin-service ($MARGIN_SERVICE_LABEL)."
  elif [ ! -x "$p/bin/margin-service" ]; then
    todo="$todo
  - No binary on the share yet: the next deploy-dev run (or deploy.sh $env_name …) installs it"
  fi
  if ! security find-generic-password -s com.krishnanelloore.margin -a github-client-secret >/dev/null 2>&1; then
    todo="$todo
  - The Keychain has no github-client-secret (a recreated VM has an empty Keychain):
    run margin-service setup in the VM's window, then sign in from the app and enter the
    model keys in Settings again; click Always Allow on the first Keychain prompt"
  fi

  echo "Provisioned $(vm_name "$env_name"): config, data and backups on $p (the Mac mini's disk)."
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
  # Runs as admin over SSH each time Orchard starts the VM; passwordless sudo is the
  # base image's.
  cat <<EOF
#!/bin/sh
set -eu
sudo scutil --set ComputerName $name
sudo scutil --set LocalHostName $name
sudo scutil --set HostName $name.local
mkdir -p ~/.ssh && chmod 700 ~/.ssh
grep -qxF '$(cat "$pub")' ~/.ssh/authorized_keys 2>/dev/null || echo '$(cat "$pub")' >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
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
  echo "Created $host_dir. Share it into $(vm_name "$env_name"): Orchard hostDirs \"margin:$host_dir\"" \
    "(orchard create vm … --host-dirs margin:$host_dir, or Cluster Bar's shared folder)."
fi
[ "$(cat "$host_dir/$MARGIN_MARKER")" = "$(vm_name "$env_name")" ] || die "$host_dir/$MARGIN_MARKER names another VM"

if [ "$rekey" = 1 ]; then
  ssh-keygen -R "$(guest_host "$env_name")" -f "$MARGIN_KNOWN_HOSTS" >/dev/null 2>&1 || true
  echo "Forgot the old host key of $(guest_host "$env_name")."
fi

guest_os="$(guest_ssh "$env_name" uname -s)" || die "couldn't ssh to $(guest_host "$env_name"); is the startup script (--startup-script $env_name) set in Orchard? A changed host key needs --rekey"
[ "$guest_os" = Darwin ] || die "$(guest_host "$env_name") runs $guest_os; only macOS guests are provisioned here (Linux: docs/plan-linux-service.md)"
guest_ssh "$env_name" "test -f '$MARGIN_PERSIST_GUEST/$MARGIN_MARKER'" \
  || die "$host_dir isn't shared into $(vm_name "$env_name") at $MARGIN_PERSIST_GUEST: add Orchard hostDirs \"margin:$host_dir\" and restart the VM"

repo="$(cd "$here/../.." && pwd)"
guest_ssh "$env_name" "mkdir -p '$MARGIN_PERSIST_GUEST/bin' '$MARGIN_PERSIST_GUEST/templates'"
for f in common.sh backup-margin.sh deploy.sh provision-guest.sh; do
  guest_put "$env_name" "$here/$f" "$MARGIN_PERSIST_GUEST/bin/$f"
done
for label in "$MARGIN_SERVICE_LABEL" "$MARGIN_BACKUP_LABEL"; do
  guest_put "$env_name" "$repo/deploy/launchd/$label.plist" "$MARGIN_PERSIST_GUEST/templates/$label.plist"
done
guest_put "$env_name" "$repo/deploy/cloudflared/config.yml" "$MARGIN_PERSIST_GUEST/templates/cloudflared-config.yml"
guest_ssh "$env_name" "chmod +x '$MARGIN_PERSIST_GUEST/bin/'*.sh"
guest_ssh "$env_name" "/bin/sh '$MARGIN_PERSIST_GUEST/bin/provision-guest.sh' --in-guest $env_name $tunnel_uuid"
