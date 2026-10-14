#!/bin/sh
# Copies the newest prod backup into dev (docs/plan-deploy.md §5). By hand only, by the
# owner at the Mac mini's terminal; it refuses to run in CI or without a terminal.
#
#   restore-into-dev.sh [BACKUP_NAME]
#
# Both persistent shares are folders on this Mac (~/MarginVMs/margin-{prod,dev}), so
# the copy never goes through the prod VM and needs no SSH key for prod. Steps:
#   1. pick the prod backup (newest, or BACKUP_NAME from backups/) and show what it has
#   2. ask for confirmation
#   3. in dev: stop the service, back up dev's data (label pre-restore)
#   4. on this Mac: copy the prod backup to dev's data/ (dev's old data/ is kept as
#      data.before-restore-<time> beside it)
#   5. in dev: margin-service scrub-restored (deletes devices: prod's devices must not
#      sign in to dev; deletes traces: dev evals must not read prod usage), start
# Secrets are not copied: dev keeps its own Keychain and media-signing-key, so old
# media links stop working in dev.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/common.sh"

if [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ]; then
  die "restores are by hand only, never in CI"
fi
[ -t 0 ] || die "run this at a terminal: it asks before it replaces dev's data"
check_allowed_env dev

prod_dir="$(host_persist_dir prod)"
dev_dir="$(host_persist_dir dev)"
[ -f "$prod_dir/$MARGIN_MARKER" ] || die "no prod share at $prod_dir"
[ -f "$dev_dir/$MARGIN_MARKER" ] || die "no dev share at $dev_dir"
[ "$(cat "$dev_dir/$MARGIN_MARKER")" = margin-dev ] || die "$dev_dir isn't dev's share"

if [ $# -ge 1 ]; then
  name="$1"
else
  name="$(ls -1 "$prod_dir/backups" 2>/dev/null | grep -E '^[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6}' | sort -r | head -n 1 || true)"
fi
[ -n "$name" ] || die "prod has no backups in $prod_dir/backups"
src="$prod_dir/backups/$name/data"
[ -d "$src" ] || die "no $src"

echo "Prod backup: $name ($(du -sh "$src" | cut -f1))"
for d in db sources videos uploads models; do
  [ -e "$src/$d" ] && echo "  $d: $(du -sh "$src/$d" | cut -f1)"
done
echo "It replaces dev's data ($dev_dir/data). Dev's devices and traces are deleted"
echo "after the copy; sign in to dev again from the app. Sources and chats are copied."
printf 'Type "restore" to go on: '
read -r answer
[ "$answer" = restore ] || die "nothing changed"

guest_ssh dev "test -f '$MARGIN_PERSIST_GUEST/$MARGIN_MARKER'" || die "dev's share isn't mounted in margin-dev"
plist="\$HOME/Library/LaunchAgents/$MARGIN_SERVICE_LABEL.plist"
cfg="$MARGIN_PERSIST_GUEST/service.toml"
guest_ssh dev "launchctl bootout gui/\$(id -u)/$MARGIN_SERVICE_LABEL 2>/dev/null; sleep 3; MARGIN_CONFIG='$cfg' /bin/sh '$MARGIN_PERSIST_GUEST/bin/backup-margin.sh' --label pre-restore" \
  || die "dev's pre-restore backup failed; dev was left stopped: start it with launchctl bootstrap gui/\$(id -u) $plist in the guest"

stamp="$(date +%Y%m%d-%H%M%S)"
rm -rf "$dev_dir/data.restoring"
ditto "$src" "$dev_dir/data.restoring"
[ -d "$dev_dir/data" ] && mv "$dev_dir/data" "$dev_dir/data.before-restore-$stamp"
mv "$dev_dir/data.restoring" "$dev_dir/data"
echo "Copied. Dev's previous data is in $dev_dir/data.before-restore-$stamp."

guest_ssh dev "MARGIN_CONFIG='$cfg' '$MARGIN_PERSIST_GUEST/bin/margin-service' scrub-restored && launchctl bootstrap gui/\$(id -u) $plist"
i=0
until guest_ssh dev "curl -fsS -m 2 http://127.0.0.1:8787/health" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -le 60 ] || die "dev didn't answer /health after the restore; see ~/Library/Logs/Margin/service.log in the guest"
  sleep 3
done
echo "Dev runs on prod's $name. Sign in to Dev from the app again."
