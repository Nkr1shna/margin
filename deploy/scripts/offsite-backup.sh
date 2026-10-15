#!/bin/sh
# Copies the Mac mini's Margin backups off the host (docs/phase5-brief.md D5,
# docs/plan-deploy.md §5 and runbook 13): to an external disk, or any folder another
# tool syncs elsewhere. Runs on the Mac mini, by hand or weekly from a LaunchAgent.
#
#   offsite-backup.sh DEST [--env prod|dev|all]             copy now
#   offsite-backup.sh --install DEST [--env prod|dev|all]   copy weekly (Sunday 04:30)
#   offsite-backup.sh --uninstall                           stop the weekly copy
#
# For each environment (default prod), every CHECKED backup folder in
# ~/MarginVMs/margin-<env>/backups goes to DEST/margin-<env>/<name>, with rsync and
# never --delete: what DEST already holds stays (prune it by hand when the disk fills).
# A folder without CHECKED (a backup that wasn't checked, or one being written) is
# skipped. CHECKED is copied last, after its data arrived whole, so on DEST too a
# folder with CHECKED is complete. Then every copied folder is verified (a second rsync
# pass finds nothing to change) and DEST/margin-<env>/LAST_COPY records the time and the
# newest backup copied. The run fails (exit 1, LAST_COPY untouched) when an environment
# has no backups folder or no backup was copied. A newest backup older than
# MARGIN_OFFSITE_MAX_AGE_DAYS (default 8) is warned about on stderr: the backups
# themselves have stopped. DEST must exist: a disk that isn't mounted is refused rather
# than filled in /Volumes.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/common.sh"

label="com.krishnanelloore.margin.offsite-backup"
plist="$HOME/Library/LaunchAgents/$label.plist"
template="$here/../launchd/$label.plist"

mode=copy
dest=""
envs=prod
while [ $# -gt 0 ]; do
  case "$1" in
    --install) mode=install; shift ;;
    --uninstall) mode=uninstall; shift ;;
    --env) envs="${2:?--env needs prod, dev or all}"; shift 2 ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    -*) die "unknown option $1" ;;
    *) [ -z "$dest" ] || die "one destination at most"; dest="$1"; shift ;;
  esac
done
env_opt="$envs"
case "$envs" in
  prod|dev) ;;
  all) envs="prod dev" ;;
  *) die "--env is prod, dev or all, not \"$envs\"" ;;
esac

if [ "$mode" = uninstall ]; then
  launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
  rm -f "$plist"
  echo "The weekly off-site copy is off."
  exit 0
fi

[ -n "$dest" ] || die "usage: offsite-backup.sh DEST [--env prod|dev|all] (or --install DEST)"
[ -d "$dest" ] || die "$dest doesn't exist (is the disk connected?)"
dest="$(cd "$dest" && pwd)"
case "$dest/" in
  "$MARGIN_VMS_DIR"/*) die "$dest is inside $MARGIN_VMS_DIR: an off-site copy must be elsewhere" ;;
esac

if [ "$mode" = install ]; then
  [ -f "$template" ] || die "no LaunchAgent template at $template"
  mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/Margin"
  # The agent runs a copy beside the shares, so it doesn't depend on this checkout.
  host_bin="$MARGIN_VMS_DIR/host-bin"
  mkdir -p "$host_bin"
  install -m 0755 "$here/offsite-backup.sh" "$host_bin/offsite-backup.sh"
  install -m 0644 "$here/common.sh" "$host_bin/common.sh"
  script="$host_bin/offsite-backup.sh"
  # The template's placeholders hold no quotes or ampersands. Paths are XML-escaped,
  # then escaped for a sed replacement (so a "|", "&" or "\" in DEST stays itself).
  esc() {
    printf '%s' "$1" \
      | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' \
      | sed -e 's/[\\|&]/\\&/g'
  }
  sed -e "s|__SCRIPT__|$(esc "$script")|g" \
      -e "s|__DEST__|$(esc "$dest")|g" \
      -e "s|__ENV__|$env_opt|g" \
      -e "s|__HOME__|$(esc "$HOME")|g" \
      -e "s|__VMS_DIR__|$(esc "$MARGIN_VMS_DIR")|g" \
      "$template" > "$plist.part"
  plutil -lint "$plist.part" >/dev/null || { rm -f "$plist.part"; die "the rendered LaunchAgent isn't a valid plist"; }
  mv -f "$plist.part" "$plist"
  launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$plist"
  echo "Installed $plist: every Sunday at 04:30, $envs backups to $dest."
  echo "Log: ~/Library/Logs/Margin/offsite-backup.log. Run it now: launchctl kickstart gui/$(id -u)/$label"
  exit 0
fi

max_age_days="${MARGIN_OFFSITE_MAX_AGE_DAYS:-8}"
case "$max_age_days" in
  ''|*[!0-9]*) die "MARGIN_OFFSITE_MAX_AGE_DAYS is a whole number of days, not \"$max_age_days\"" ;;
esac

# A backup's name (2026-10-15-043000...) -> seconds since the epoch, or nothing.
stamp_epoch() {
  s="$(printf '%s' "$1" | cut -c1-17)"
  date -j -f '%Y-%m-%d-%H%M%S' "$s" +%s 2>/dev/null \
    || date -d "$(printf '%s' "$s" | sed -E 's/^(.{10})-(..)(..)(..)$/\1 \2:\3:\4/')" +%s 2>/dev/null \
    || true
}

failed=0
for env_name in $envs; do
  src="$(host_persist_dir "$env_name")/backups"
  if [ ! -d "$src" ]; then
    echo "No $env_name backups at $src: nothing to copy." >&2
    failed=1
    continue
  fi
  to="$dest/$(vm_name "$env_name")"
  mkdir -p "$to"
  copied=0
  skipped=0
  newest=""
  for dir in "$src"/*/; do
    [ -d "$dir" ] || continue
    name="$(basename "$dir")"
    echo "$name" | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6}' || continue
    if [ ! -f "$src/$name/CHECKED" ]; then
      echo "Skipped $env_name/$name: it has no CHECKED marker."
      skipped=$((skipped + 1))
      continue
    fi
    # The data first, CHECKED only once the data is whole.
    rsync -a --exclude /CHECKED "$src/$name/" "$to/$name/"
    rsync -a "$src/$name/CHECKED" "$to/$name/CHECKED"
    # The folders' times (adding CHECKED changed the top one's).
    rsync -a "$src/$name/" "$to/$name/"
    # Verify: another pass would change nothing.
    if [ -n "$(rsync -a --dry-run -i "$src/$name/" "$to/$name/")" ]; then
      echo "$env_name/$name differs on $dest after the copy" >&2
      rm -f "$to/$name/CHECKED"
      failed=1
      continue
    fi
    copied=$((copied + 1))
    # The folders come in name (time) order: the last one is the newest.
    newest="$name"
  done
  # The drill reports, when there are any.
  if [ -d "$src/drills" ]; then rsync -a "$src/drills/" "$to/drills/"; fi
  if [ "$copied" = 0 ]; then
    echo "$env_name: no backup copied to $to ($skipped without CHECKED skipped); LAST_COPY left as it was." >&2
    failed=1
    continue
  fi
  printf 'copied_at=%s\nnewest=%s\n' "$(date '+%Y-%m-%d %H:%M:%S %z')" "$newest" > "$to/LAST_COPY"
  echo "$env_name: $copied checked backup(s) on $to, newest $newest ($skipped without CHECKED skipped)."
  taken="$(stamp_epoch "$newest")"
  if [ -z "$taken" ]; then
    echo "WARNING: $env_name: couldn't tell when $newest was taken from its name." >&2
  elif [ $(( $(date +%s) - taken )) -gt $((max_age_days * 86400)) ]; then
    echo "WARNING: $env_name: the newest backup, $newest, is more than $max_age_days days old: are the nightly backups (and their checks) running?" >&2
  fi
done
[ "$failed" = 0 ] || die "the off-site copy to $dest is incomplete (see above)"
