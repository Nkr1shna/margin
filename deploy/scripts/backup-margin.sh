#!/bin/sh
# Back up Margin's data folder: the one backup script (docs/plan-deploy.md §5,
# docs/plan-dogfood.md §5). Used on the owner's Mac by hand, by deploy.sh before every
# deploy, nightly by the com.krishnanelloore.margin.backup LaunchAgent, and by
# restore-into-dev.sh before it replaces dev's data.
#
#   backup-margin.sh [--label TEXT] [--keep N] [--stop-service]
#
# Copies `data_dir` from the service config to `<data_dir>/../backups/<date-time>[-label]/data`.
# In a VM that is the persistent share, so backups live on the Mac mini's disk.
# Refuses while the service is up (a copy of a live database isn't a backup) unless
# --stop-service, which stops the LaunchAgent, backs up, and starts it again even when
# the copy fails. Keeps the newest N backups (default 10; 0 keeps all).
# The config is $MARGIN_CONFIG, else ~/Library/Application Support/Margin/service.toml.
set -eu

label=""
keep=10
stop_service=0
while [ $# -gt 0 ]; do
  case "$1" in
    --label) label="${2:?--label needs a value}"; shift 2 ;;
    --keep) keep="${2:?--keep needs a value}"; shift 2 ;;
    --stop-service) stop_service=1; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "backup-margin: unknown option $1" >&2; exit 2 ;;
  esac
done
case "$keep" in *[!0-9]*|"") echo "backup-margin: --keep takes a number" >&2; exit 2 ;; esac
# Labels name a folder: commit ids, "pre-restore", "nightly".
label="$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '-')"

config="${MARGIN_CONFIG:-$HOME/Library/Application Support/Margin/service.toml}"
if [ ! -f "$config" ]; then
  echo "backup-margin: no service config at $config (set MARGIN_CONFIG)" >&2
  exit 1
fi

# A top-level `key = "value"` of the TOML config.
value() {
  sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\"\(.*\)\"[[:space:]]*\$/\1/p" "$config" | head -n 1
}

data_dir="$(value data_dir)"
bind="$(value bind)"
data_dir="${data_dir:-$(dirname "$config")/data}"
bind="${bind:-127.0.0.1:8787}"
port="${bind##*:}"

if [ ! -d "$data_dir" ]; then
  echo "backup-margin: the data folder $data_dir doesn't exist" >&2
  exit 1
fi

running() {
  lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 \
    || curl -fsS -m 2 "http://$bind/health" >/dev/null 2>&1
}

service="gui/$(id -u)/com.krishnanelloore.margin.service"
plist="$HOME/Library/LaunchAgents/com.krishnanelloore.margin.service.plist"
restart_after=0
if [ "$stop_service" = 1 ] && running; then
  [ -f "$plist" ] || { echo "backup-margin: the service runs but not from $plist; stop it by hand" >&2; exit 1; }
  launchctl bootout "$service" 2>/dev/null || true
  restart_after=1
  # Always start it again, whatever happens below.
  trap 'launchctl bootstrap "gui/$(id -u)" "$plist" 2>/dev/null || launchctl kickstart "$service" 2>/dev/null || true' EXIT
  i=0
  while running; do
    i=$((i + 1))
    [ "$i" -le 60 ] || { echo "backup-margin: the service didn't stop in 60 s" >&2; exit 1; }
    sleep 1
  done
fi

if running; then
  echo "backup-margin: the service is running on $bind; stop it first or pass --stop-service (a copy of a live database isn't a backup)" >&2
  exit 1
fi

backups="$(dirname "$data_dir")/backups"
name="$(date +%Y-%m-%d-%H%M%S)"
[ -n "$label" ] && name="$name-$label"
dest="$backups/$name"
if [ -e "$dest" ]; then
  echo "backup-margin: $dest already exists" >&2
  exit 1
fi
mkdir -p "$dest"
if ! cp -Rp "$data_dir" "$dest/data.partial"; then
  rm -rf "$dest"
  echo "backup-margin: copying $data_dir failed; no backup was made" >&2
  exit 1
fi
mv "$dest/data.partial" "$dest/data"
echo "Backed up $data_dir to $dest/data ($(du -sh "$dest/data" | cut -f1))."

# Keep the newest $keep complete backups (folders named by this script).
if [ "$keep" -gt 0 ]; then
  ls -1 "$backups" | grep -E '^[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6}' | sort -r | tail -n +"$((keep + 1))" |
    while IFS= read -r old; do
      [ -d "$backups/$old/data" ] || continue
      rm -rf "${backups:?}/$old"
      echo "Removed the old backup $backups/$old (keeping $keep)."
    done
fi

[ "$restart_after" = 1 ] && echo "Starting the service again."
echo "To restore: stop the service, move $data_dir aside, copy $dest/data back to it, start the service."
# The path of this backup, last, for deploy.sh.
echo "BACKUP=$dest"
