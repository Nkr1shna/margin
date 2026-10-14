#!/bin/sh
# Installs a margin-service build in the dev or prod VM (docs/plan-deploy.md §4, §5).
# Runs on the Mac mini (the GitHub runner, or the owner by hand):
#
#   deploy.sh dev|prod PATH/TO/margin-service SHA
#
# Replaces only the binary. The data folder, backups and config stay on the persistent
# share (common.sh) and are never touched, except for the backup taken first:
#   preflight (macOS guest, share mounted and belonging to this environment, config
#   environment matches, data_dir on the share) -> copy the binary in -> stop the
#   service -> backup-margin.sh --label <old sha> -> swap the binary -> start ->
#   wait for /health -> GET /api/version must report SHA and the environment.
# A build that doesn't come up is rolled back to the previous binary (data is left as
# the new build left it; the backup path is printed for a by-hand restore).
#
# MARGIN_DEPLOY_ONLY=dev (deploy-dev.yml) refuses any other environment.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/common.sh"

# ---- In the guest: `deploy.sh --in-guest ENV SHA` (copied in by the host side) ----
if [ "${1:-}" = "--in-guest" ]; then
  env_name="${2:?environment}"
  sha="${3:?sha}"
  require_launchd
  check_persistent_mount "$env_name"
  check_config "$env_name"
  persist="$MARGIN_PERSIST_GUEST"
  export MARGIN_CONFIG="$persist/service.toml"
  bin="$persist/bin"
  plist="$HOME/Library/LaunchAgents/$MARGIN_SERVICE_LABEL.plist"
  service="gui/$(id -u)/$MARGIN_SERVICE_LABEL"
  [ -f "$plist" ] || die "no $plist: run provision-guest.sh $env_name on the host first"
  [ -x "$bin/margin-service.new" ] || die "the new binary didn't arrive"
  bind="$(toml_value "$MARGIN_CONFIG" bind)"
  bind="${bind:-127.0.0.1:8787}"

  old_sha="$(curl -fsS -m 5 "http://$bind/api/version" 2>/dev/null | json_field sha || true)"
  old_sha="${old_sha:-none}"
  echo "Deploying $sha to $env_name (running: $old_sha)."

  launchctl bootout "$service" 2>/dev/null || true
  i=0
  while curl -fsS -m 2 "http://$bind/health" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -le 60 ] || die "the old service didn't stop in 60 s"
    sleep 1
  done

  # The first deploy has no data yet; every later one backs up first.
  data_dir="$(toml_value "$MARGIN_CONFIG" data_dir)"
  backup=""
  if [ -d "$data_dir" ]; then
    out="$("$bin/backup-margin.sh" --label "before-$sha-from-$old_sha")" || {
      launchctl bootstrap "gui/$(id -u)" "$plist" || true
      die "the backup failed; the old build was started again and nothing changed"
    }
    echo "$out"
    backup="$(printf '%s\n' "$out" | sed -n 's/^BACKUP=//p')"
  fi

  [ -f "$bin/margin-service" ] && mv -f "$bin/margin-service" "$bin/margin-service.prev"
  mv -f "$bin/margin-service.new" "$bin/margin-service"

  wait_up() {
    i=0
    until curl -fsS -m 2 "http://$bind/health" >/dev/null 2>&1; do
      i=$((i + 1))
      # Migrations (the passages-v1 re-embed) run at start; give them time.
      [ "$i" -le 180 ] || return 1
      sleep 1
    done
  }

  launchctl bootstrap "gui/$(id -u)" "$plist"
  ok=1
  if ! wait_up; then
    echo "The new build didn't answer /health in 180 s." >&2
    ok=0
  else
    version="$(curl -fsS -m 5 "http://$bind/api/version")"
    got_sha="$(printf '%s' "$version" | json_field sha || true)"
    got_env="$(printf '%s' "$version" | json_field environment || true)"
    rss="$(printf '%s' "$version" | json_field rss_bytes || true)"
    if [ "$got_sha" != "$sha" ] || [ "$got_env" != "$env_name" ]; then
      echo "/api/version reports sha \"$got_sha\" environment \"$got_env\"; expected \"$sha\" \"$env_name\"." >&2
      ok=0
    else
      echo "$env_name runs $sha (rss ${rss:-?} bytes)."
    fi
  fi
  if [ "$ok" = 0 ]; then
    tail -n 40 "$HOME/Library/Logs/Margin/service.log" >&2 2>/dev/null || true
    if [ -f "$bin/margin-service.prev" ]; then
      echo "Rolling back to the previous binary ($old_sha)." >&2
      launchctl bootout "$service" 2>/dev/null || true
      sleep 2
      mv -f "$bin/margin-service" "$bin/margin-service.failed"
      mv -f "$bin/margin-service.prev" "$bin/margin-service"
      launchctl bootstrap "gui/$(id -u)" "$plist" || true
      wait_up && echo "The previous build is running again." >&2 || echo "The previous build didn't come up either." >&2
    fi
    [ -n "$backup" ] && echo "The data from before this deploy is in $backup (restore by hand if the new build migrated it)." >&2
    exit 1
  fi
  exit 0
fi

# ---- On the host ----
[ $# -eq 3 ] || die "usage: deploy.sh dev|prod PATH/TO/margin-service SHA"
env_name="$1"
binary="$2"
sha="$3"
vm_name "$env_name" >/dev/null
check_allowed_env "$env_name"
[ -f "$binary" ] || die "no binary at $binary"
case "$sha" in *[!0-9a-f]*|"") die "SHA must be a commit id (hex), not \"$sha\"" ;; esac
if [ "$(uname -s)" = Darwin ]; then
  file "$binary" | grep -q 'arm64' || die "$binary isn't an arm64 Mach-O binary"
fi

host_dir="$(host_persist_dir "$env_name")"
[ -f "$host_dir/$MARGIN_MARKER" ] || die "no $host_dir/$MARGIN_MARKER on this Mac: run provision-guest.sh $env_name first"

guest_os="$(guest_ssh "$env_name" uname -s)" || die "couldn't reach $(guest_host "$env_name") (if its host key changed, Orchard recreated the VM: provision-guest.sh $env_name --rekey)"
[ "$guest_os" = Darwin ] || die "$(guest_host "$env_name") runs $guest_os; only macOS guests are deployed by this script (Linux: docs/plan-linux-service.md)"

guest_bin="$MARGIN_PERSIST_GUEST/bin"
guest_ssh "$env_name" "test -f '$MARGIN_PERSIST_GUEST/$MARGIN_MARKER'" \
  || die "the persistent share isn't mounted in $(vm_name "$env_name") at $MARGIN_PERSIST_GUEST (Orchard hostDirs)"
for f in common.sh backup-margin.sh deploy.sh; do
  guest_put "$env_name" "$here/$f" "$guest_bin/$f"
done
guest_put "$env_name" "$binary" "$guest_bin/margin-service.new"
guest_ssh "$env_name" "chmod +x '$guest_bin/'*.sh '$guest_bin/margin-service.new'"
guest_ssh "$env_name" "/bin/sh '$guest_bin/deploy.sh' --in-guest $env_name $sha"
echo "Deployed $sha to $env_name ($(public_url "$env_name"))."
