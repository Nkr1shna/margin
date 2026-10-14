#!/bin/sh
# Back up Margin's data folder before installing a service build that migrates it
# (docs/plan-dogfood.md §5). Copies `data_dir` from the service config to
# `<data_dir>/../backups/<date-time>/` — by default
# ~/Library/Application Support/Margin/backups/<date-time>/data.
#
# Refuses to run while the service is up: stop it first, run this, then start it.
# The config is $MARGIN_CONFIG, else ~/Library/Application Support/Margin/service.toml.
set -eu

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

# Up = something listens on the service's port, or it answers /health.
if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 \
  || curl -fsS -m 2 "http://$bind/health" >/dev/null 2>&1; then
  echo "backup-margin: the service is running on $bind; stop it first (a copy of a live database isn't a backup)" >&2
  exit 1
fi

backups="$(dirname "$data_dir")/backups"
dest="$backups/$(date +%Y-%m-%d-%H%M%S)"
if [ -e "$dest" ]; then
  echo "backup-margin: $dest already exists" >&2
  exit 1
fi
mkdir -p "$dest"
cp -Rp "$data_dir" "$dest/data"
echo "Backed up $data_dir to $dest/data ($(du -sh "$dest/data" | cut -f1))."
echo "To restore: stop the service, move $data_dir aside, copy $dest/data back to it, start the service."
