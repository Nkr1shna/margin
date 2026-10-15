# Shared by deploy/scripts/*.sh (sourced, never run). docs/plan-deploy.md §1, §5.
#
# Every guest keeps everything that must outlive the VM in ONE folder on the Mac mini,
# shared into the guest by Orchard (`hostDirs`, i.e. `tart run --dir`). Orchard clones
# the VM again from its image on every restart and every reschedule, so the guest's own
# disk is scratch; this folder is not.
#
#   host:  $MARGIN_VMS_DIR/margin-<env>          (default ~/MarginVMs/margin-<env>)
#   guest: /mnt/shared/margin                    (the virtiofs share "margin", mounted by
#                                                 the Orchard startup script)
#
# Inside it: .margin-persistent (marker, holds "margin-<env>"), service.toml,
# secrets.toml, data/, backups/, bin/ (margin-service, these scripts), cloudflared/,
# templates/. The guests are Ubuntu 24.04 (deploy/image) running systemd units
# (deploy/systemd); logs go to the journal (journalctl -u margin-service).

# Not `set -u` safe to source twice; scripts set their own options.

MARGIN_SERVICE_UNIT="margin-service"
MARGIN_TUNNEL_UNIT="margin-cloudflared"
MARGIN_BACKUP_UNIT="margin-backup"
MARGIN_SHARE_MOUNT="${MARGIN_SHARE_MOUNT:-/mnt/shared}"
MARGIN_PERSIST_GUEST="${MARGIN_PERSIST_GUEST:-$MARGIN_SHARE_MOUNT/margin}"
MARGIN_VMS_DIR="${MARGIN_VMS_DIR:-$HOME/MarginVMs}"
MARGIN_DEPLOY_KEY="${MARGIN_DEPLOY_KEY:-$HOME/.ssh/margin_deploy}"
MARGIN_KNOWN_HOSTS="${MARGIN_KNOWN_HOSTS:-$HOME/.ssh/margin_known_hosts}"
MARGIN_GUEST_USER="${MARGIN_GUEST_USER:-admin}"
MARGIN_MARKER=".margin-persistent"

die() {
  echo "$(basename "$0"): $*" >&2
  exit 1
}

# dev|prod -> the VM's name.
vm_name() {
  case "$1" in
    dev) echo margin-dev ;;
    prod) echo margin-prod ;;
    *) die "the environment is dev or prod, not \"$1\"" ;;
  esac
}

# dev|prod -> the public URL its tunnel serves.
public_url() {
  case "$1" in
    dev) echo https://devmargin.krishnanelloore.com ;;
    prod) echo https://margin.krishnanelloore.com ;;
    *) die "the environment is dev or prod, not \"$1\"" ;;
  esac
}

# The guest's mDNS name (its DHCP address changes; the name doesn't).
guest_host() {
  echo "$(vm_name "$1").local"
}

# The persistent folder of an environment on the host.
host_persist_dir() {
  echo "$MARGIN_VMS_DIR/$(vm_name "$1")"
}

# CI runs one environment per job: MARGIN_DEPLOY_ONLY=dev in deploy-dev.yml means this
# process may never reach prod, whatever its arguments say.
check_allowed_env() {
  if [ -n "${MARGIN_DEPLOY_ONLY:-}" ] && [ "$MARGIN_DEPLOY_ONLY" != "$1" ]; then
    die "this job may only reach $MARGIN_DEPLOY_ONLY, not $1 (MARGIN_DEPLOY_ONLY)"
  fi
}

# ssh to the environment's guest with the deploy key and its own known_hosts.
# A changed host key means Orchard recreated the VM: provision-guest.sh --rekey.
guest_ssh() {
  env_name="$1"
  shift
  [ -r "$MARGIN_DEPLOY_KEY" ] || die "no deploy key at $MARGIN_DEPLOY_KEY"
  ssh -i "$MARGIN_DEPLOY_KEY" \
    -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=20 \
    -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$MARGIN_KNOWN_HOSTS" \
    "$MARGIN_GUEST_USER@$(guest_host "$env_name")" "$@"
}

# Copies a host file to a path in the guest (paths may contain spaces, never quotes).
guest_put() {
  env_name="$1"
  src="$2"
  dest="$3"
  guest_ssh "$env_name" "cat > '$dest.part' && mv -f '$dest.part' '$dest'" < "$src"
}

# A top-level `key = "value"` of a TOML file.
toml_value() {
  sed -n "s/^[[:space:]]*$2[[:space:]]*=[[:space:]]*\"\(.*\)\"[[:space:]]*\$/\1/p" "$1" | head -n 1
}

# One field of a flat JSON object on stdin (jq in the guests, plutil on the Mac).
json_field() {
  if command -v jq >/dev/null 2>&1; then
    jq -r --arg k "$1" '.[$k] // empty' 2>/dev/null
  else
    plutil -extract "$1" raw -o - - 2>/dev/null
  fi
}

# In the guest: the service manager is systemd (the Linux guests).
require_systemd() {
  command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ] \
    || die "this guest has no systemd: the deploy scripts expect the Ubuntu guest image (deploy/image)"
}

# In the guest: the folder is the share, mounted, and belongs to this environment.
# Refuses a folder on the guest's own disk, which Orchard throws away.
check_persistent_mount() {
  env_name="$1"
  dir="$MARGIN_PERSIST_GUEST"
  [ -d "$dir" ] || die "$dir isn't there: is the host folder shared into this VM (Orchard hostDirs \"margin:$(host_persist_dir "$env_name")\")?"
  [ -f "$dir/$MARGIN_MARKER" ] || die "$dir has no $MARGIN_MARKER: it isn't the persistent share (run provision-guest.sh $env_name on the host)"
  want="$(vm_name "$env_name")"
  got="$(cat "$dir/$MARGIN_MARKER")"
  [ "$got" = "$want" ] || die "$dir belongs to $got, not $want: the wrong host folder is shared into this VM"
  # The share is mounted, i.e. not a folder on the guest's own disk.
  mountpoint -q "$MARGIN_SHARE_MOUNT" || die "$MARGIN_SHARE_MOUNT isn't mounted: $dir would be on the guest's own disk, which Orchard recreates: refusing"
}

# In the guest: the service config lives on the share, names this environment and keeps
# its data_dir (and so backups/) on the share.
check_config() {
  env_name="$1"
  config="$MARGIN_PERSIST_GUEST/service.toml"
  [ -f "$config" ] || die "no $config yet: run provision-guest.sh $env_name on the host"
  got_env="$(toml_value "$config" environment)"
  [ "${got_env:-prod}" = "$env_name" ] || die "$config says environment \"${got_env:-prod}\", not \"$env_name\""
  data_dir="$(toml_value "$config" data_dir)"
  case "$data_dir" in
    "$MARGIN_PERSIST_GUEST"/*) ;;
    *) die "data_dir \"$data_dir\" in $config isn't on the persistent share $MARGIN_PERSIST_GUEST: refusing (it would be wiped when Orchard recreates the VM)" ;;
  esac
}

# Writes the secrets the deploy job passes in (MARGIN_SECRET_*, from the GitHub
# Environment's secrets) into the share's secrets.toml, readable only by the owner, and
# points service.toml's secrets_file at it. Orchard re-clones a VM on every restart,
# which empties its Keychain; the share keeps the secrets, and the media signing key the
# service generates there, across re-clones. Values never reach argv or the log.
#   MARGIN_SECRET_GITHUB_CLIENT_SECRET -> github-client-secret
#   MARGIN_SECRET_OPENAI_API_KEY       -> openai-api-key (optional; Settings can set it too)
write_secrets() {
  env_name="$1"
  dir="$(host_persist_dir "$env_name")"
  file="$dir/secrets.toml"
  (
    umask 077
    [ -f "$file" ] || : > "$file"
    chmod 600 "$file"
    for pair in GITHUB_CLIENT_SECRET:github-client-secret OPENAI_API_KEY:openai-api-key; do
      var="MARGIN_SECRET_${pair%%:*}"
      name="${pair#*:}"
      eval "value=\${$var:-}"
      [ -n "$value" ] || continue
      SECRET_NAME="$name" SECRET_VAR="$var" perl -e '
        my ($file, $name) = ($ARGV[0], $ENV{SECRET_NAME});
        my $v = $ENV{$ENV{SECRET_VAR}};
        $v =~ s/\\/\\\\/g; $v =~ s/"/\\"/g;
        open(my $in, "<", $file) or die "$file: $!";
        my @lines = grep { !/^\Q$name\E\s*=/ } <$in>;
        close($in);
        push @lines, "$name = \"$v\"\n";
        open(my $out, ">", "$file.part") or die "$file.part: $!";
        print $out @lines;
        close($out) or die;
        rename("$file.part", $file) or die "$file: $!";
      ' "$file"
      echo "Wrote $name to $file."
    done
  )
  config="$dir/service.toml"
  if [ -f "$config" ] && ! grep -q '^secrets_file' "$config"; then
    printf 'secrets_file = "%s/secrets.toml"\n' "$MARGIN_PERSIST_GUEST" >> "$config"
    echo "Pointed $config at the share's secrets.toml."
  fi
}
