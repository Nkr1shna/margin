#!/bin/sh
# Restore drill for dev (docs/phase5-brief.md D5, docs/plan-deploy.md §5): proves that a
# dev backup restores and serves. Runs on the Mac mini, by hand or from
# .github/workflows/restore-drill.yml (workflow_dispatch, the host runner,
# MARGIN_DEPLOY_ONLY=dev). Dev only, always: prod -> dev is restore-into-dev.sh.
#
#   restore-drill.sh dev [BACKUP_NAME] [--hard-stop]
#
#   1. picks the newest CHECKED backup in dev's share (~/MarginVMs/margin-dev/backups), or
#      BACKUP_NAME, and runs `margin-service check-data` on it in the guest: the counts
#      the restored service must show
#   2. copies it beside dev's data on this Mac (data.restoring)
#   3. in dev: stops the service, backs dev up (backup-margin.sh --check --label pre-drill)
#   4. swaps the copy in (dev's data is kept as data.before-drill-<time>), runs
#      `margin-service scrub-restored`, starts the service, waits for /health
#   5. checks /health and /api/version on the public URL, signs in afresh (prints the
#      GitHub URL to open and waits up to 10 minutes), compares GET /api/library with
#      the counts of step 1, runs one POST /api/context/search, and signs the drill's
#      device out again
#   6. --hard-stop: ingests a long document and stops the service hard under it
#      (SIGKILL in the guest; or $MARGIN_HARD_STOP_CMD, run on this Mac, for a VM-level
#      stop such as an Orchard restart), then waits for /health, searches and deletes
#      the document (the SurrealKV-on-virtio-fs check of docs/plan-deploy.md §5)
#
# Dev runs on the restored backup afterwards: what dev had since that backup is in the
# pre-drill backup and in data.before-drill-<time>. Writes backups/drills/<date-time>.txt
# in dev's share with every check, and exits 1 if any failed.
# MARGIN_DRILL_URL overrides the URL the checks use (default: dev's public URL);
# MARGIN_DRILL_QUERY the search text (default "notes").
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/common.sh"

env_name="${1:-}"
[ "$env_name" = dev ] || die "usage: restore-drill.sh dev [BACKUP_NAME] [--hard-stop] (drills run on dev only)"
shift
name=""
hard_stop=0
for a in "$@"; do
  case "$a" in
    --hard-stop) hard_stop=1 ;;
    -*) die "unknown option $a" ;;
    *) [ -z "$name" ] || die "one backup name at most"; name="$a" ;;
  esac
done
check_allowed_env dev
command -v jq >/dev/null 2>&1 || die "needs jq (brew install jq)"

dev_dir="$(host_persist_dir dev)"
[ -f "$dev_dir/$MARGIN_MARKER" ] || die "no dev share at $dev_dir"
[ "$(cat "$dev_dir/$MARGIN_MARKER")" = margin-dev ] || die "$dev_dir isn't dev's share"
backups="$dev_dir/backups"
url="${MARGIN_DRILL_URL:-$(public_url dev)}"
query="${MARGIN_DRILL_QUERY:-notes}"
stamp="$(date +%Y-%m-%d-%H%M%S)"
mkdir -p "$backups/drills"
report="$backups/drills/$stamp.txt"
work="$(mktemp -d "${TMPDIR:-/tmp}/margin-drill.XXXXXX")"
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT

failed=0
log() {
  echo "$*"
  echo "$*" >> "$report"
}
# check DESCRIPTION EXPECTED ACTUAL
check() {
  if [ "$2" = "$3" ]; then
    log "PASS $1: $3"
  else
    log "FAIL $1: $3 (expected $2)"
    failed=1
  fi
}
finish() {
  if [ "$failed" = 0 ]; then log "RESULT pass"; else log "RESULT fail"; fi
  echo "Report: $report"
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] && { echo '```'; cat "$report"; echo '```'; } >> "$GITHUB_STEP_SUMMARY"
  exit "$failed"
}

# ---- 1. The backup and its expected counts ----
if [ -z "$name" ]; then
  name="$(ls -1 "$backups" 2>/dev/null | grep -E '^[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6}' | sort -r |
    while IFS= read -r b; do
      [ -f "$backups/$b/CHECKED" ] && [ -d "$backups/$b/data" ] && { echo "$b"; break; }
    done || true)"
fi
[ -n "$name" ] || die "dev has no checked backup in $backups (backup-margin.sh --check makes one)"
src="$backups/$name/data"
[ -d "$src" ] || die "no $src"
: > "$report"
log "Restore drill of dev, $stamp, backup $name ($(du -sh "$src" | cut -f1))"

guest_ssh dev "test -f '$MARGIN_PERSIST_GUEST/$MARGIN_MARKER'" || die "dev's share isn't mounted in margin-dev"
guest_bin="$MARGIN_PERSIST_GUEST/bin"
cfg="$MARGIN_PERSIST_GUEST/service.toml"
if guest_ssh dev "'$guest_bin/margin-service' check-data '$MARGIN_PERSIST_GUEST/backups/$name/data'" > "$work/counts" 2>&1; then
  check "check-data on the backup" OK "$(tail -n 1 "$work/counts" | cut -d' ' -f1)"
else
  cat "$work/counts" >&2
  log "FAIL check-data on the backup: $(tail -n 1 "$work/counts")"
  failed=1
  finish
fi
count() {
  sed -n "s/^count $1 \([0-9][0-9]*\)\$/\1/p" "$work/counts" | head -n 1
}
passages="$(count passage)"
passages="${passages:-0}"

# ---- 2.-4. Restore it into dev, as restore-into-dev.sh does ----
rm -rf "$dev_dir/data.restoring"
ditto "$src" "$dev_dir/data.restoring"
guest_ssh dev "sudo systemctl stop $MARGIN_SERVICE_UNIT.service; MARGIN_CONFIG='$cfg' /bin/sh '$guest_bin/backup-margin.sh' --check --label pre-drill" \
  || { rm -rf "$dev_dir/data.restoring"; die "dev's pre-drill backup failed; dev was left stopped: sudo systemctl start $MARGIN_SERVICE_UNIT in the guest"; }
[ -d "$dev_dir/data" ] && mv "$dev_dir/data" "$dev_dir/data.before-drill-$stamp"
mv "$dev_dir/data.restoring" "$dev_dir/data"
log "Restored. Dev's previous data is in $dev_dir/data.before-drill-$stamp and the pre-drill backup."
guest_ssh dev "MARGIN_CONFIG='$cfg' '$guest_bin/margin-service' scrub-restored && sudo systemctl start $MARGIN_SERVICE_UNIT.service" \
  || { log "FAIL scrub-restored or start"; failed=1; finish; }

wait_health() {
  i=0
  until guest_ssh dev "curl -fsS -m 2 http://127.0.0.1:8787/health" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -le 100 ] || return 1
    sleep 3
  done
}
if wait_health; then check "dev answers /health in the guest" up up; else check "dev answers /health in the guest" up down; finish; fi

# ---- 5. The service as the app sees it ----
# status METHOD PATH [curl args…]: prints the status; the body is in $work/body.
status() {
  method="$1"
  path="$2"
  shift 2
  # Without -f curl succeeds on any HTTP status; a connection failure prints 000.
  curl -sS -o "$work/body" -w '%{http_code}' -m 30 -X "$method" "$@" "$url$path" 2>/dev/null || true
}
check "GET /health" 200 "$(status GET /health)"
check "GET /api/version" 200 "$(status GET /api/version)"
check "/api/version environment" dev "$(jq -r '.environment // empty' "$work/body" 2>/dev/null)"
log "INFO version $(jq -c '{sha, version, rss_bytes}' "$work/body" 2>/dev/null || true)"

# A fresh sign-in: the scrub removed every device.
code="$(status POST /auth/start -H 'Content-Type: application/json' -d "{\"device_name\":\"Restore drill $stamp\"}")"
check "POST /auth/start" 200 "$code"
[ "$code" = 200 ] || finish
login_id="$(jq -r .login_id "$work/body")"
jq -r .poll_secret "$work/body" > "$work/poll_secret"
authorize="$(jq -r .authorize_url "$work/body")"
echo
echo "Sign in to dev for the drill: open this URL and approve with GitHub (10 minutes):"
echo "  $authorize"
echo
[ -n "${GITHUB_STEP_SUMMARY:-}" ] && echo "Sign in to dev for the restore drill: <$authorize>" >> "$GITHUB_STEP_SUMMARY"
jq -n --arg id "$login_id" --rawfile s "$work/poll_secret" '{login_id: $id, poll_secret: ($s | rtrimstr("\n"))}' > "$work/poll.json"
state=pending
i=0
while [ "$state" = pending ]; do
  i=$((i + 1))
  [ "$i" -le 200 ] || break
  sleep 3
  code="$(status POST /auth/poll -H 'Content-Type: application/json' --data-binary @"$work/poll.json")"
  [ "$code" = 200 ] || continue
  state="$(jq -r .state "$work/body")"
done
check "fresh sign-in" approved "$state"
[ "$state" = approved ] || finish
# The token stays in a 0600 file inside $work, passed to curl as a header file: never on
# argv, never printed.
printf 'Authorization: Bearer %s\n' "$(jq -r .token "$work/body")" > "$work/auth"
rm -f "$work/body"

check "GET /api/me" 200 "$(status GET /api/me -H @"$work/auth")"
device_id="$(jq -r '.device.id // empty' "$work/body")"

check "GET /api/library" 200 "$(status GET /api/library -H @"$work/auth")"
for pair in topics:topic lessons:lesson notes:note videos:video sources:library_sources; do
  field="${pair%%:*}"
  table="${pair#*:}"
  want="$(count "$table")"
  check "library $field match the backup" "${want:-0}" "$(jq -r ".$field | length" "$work/body")"
done

jq -n --arg q "$query" '{text: $q, scope: {scope: "library"}, k: 5}' > "$work/search.json"
search() {
  status POST /api/context/search -H @"$work/auth" -H 'Content-Type: application/json' --data-binary @"$work/search.json"
}
code="$(search)"
check "POST /api/context/search" 200 "$code"
hits="$(jq -r 'length' "$work/body" 2>/dev/null || echo 0)"
if [ "$passages" -gt 0 ]; then
  [ "$hits" -gt 0 ] && r=found || r=none
  check "the search finds passages ($passages in the backup)" found "$r"
else
  log "INFO the backup holds no passages; the search returned $hits hit(s)"
fi

# ---- 6. Hard stop under an ingest ----
if [ "$hard_stop" = 1 ]; then
  {
    echo "# Restore drill $stamp"
    n=0
    while [ "$n" -lt 400 ]; do
      n=$((n + 1))
      echo
      echo "## Section $n"
      echo
      echo "Paragraph $n of the hard-stop document. It exists to keep the ingest worker busy while the service is stopped hard, so the database is written to when it goes away."
    done
  } > "$work/doc.md"
  jq -n --arg name "restore-drill-$stamp.md" --rawfile text "$work/doc.md" \
    '{name: $name, origin: {origin: "agent", name: "restore-drill"}, kind: "text", text: $text}' > "$work/ingest.json"
  code="$(status POST /api/context/ingest -H @"$work/auth" -H 'Content-Type: application/json' --data-binary @"$work/ingest.json")"
  check "POST /api/context/ingest (hard-stop document)" 200 "$code"
  doc_id="$(jq -r '.id // empty' "$work/body" 2>/dev/null)"
  sleep 2
  if [ -n "${MARGIN_HARD_STOP_CMD:-}" ]; then
    log "INFO hard stop: $MARGIN_HARD_STOP_CMD"
    sh -c "$MARGIN_HARD_STOP_CMD" || log "INFO the hard-stop command exited non-zero"
    # A VM-level stop: wait for the guest to answer SSH again.
    i=0
    until guest_ssh dev true >/dev/null 2>&1; do
      i=$((i + 1))
      [ "$i" -le 100 ] || break
      sleep 6
    done
  else
    log "INFO hard stop: SIGKILL of $MARGIN_SERVICE_UNIT under the ingest"
    guest_ssh dev "sudo systemctl kill --signal=SIGKILL $MARGIN_SERVICE_UNIT.service" || true
    sleep 2
  fi
  # systemd restarts the unit (Restart=always); start is a no-op when it already did.
  guest_ssh dev "sudo systemctl start $MARGIN_SERVICE_UNIT.service" || true
  if wait_health; then r=up; else r=down; fi
  check "dev answers /health after the hard stop" up "$r"
  check "POST /api/context/search after the hard stop" 200 "$(search)"
  if [ -n "$doc_id" ]; then
    code="$(status DELETE "/api/context/documents/$doc_id" -H @"$work/auth")"
    case "$code" in 200|204) r=deleted ;; *) r="$code" ;; esac
    check "the hard-stop document is deleted again" deleted "$r"
  fi
fi

# Sign the drill's device out again.
if [ -n "$device_id" ]; then
  code="$(status DELETE "/api/devices/$device_id" -H @"$work/auth")"
  case "$code" in 200|204) r=removed ;; *) r="$code" ;; esac
  check "the drill's device is removed" removed "$r"
fi
finish
