#!/bin/sh
# A throwaway keychain holding the service's signing identity, for one CI job
# (docs/plan-deploy.md §4). The identity comes from the repository secrets
# MARGIN_SIGNING_P12_BASE64 (the .p12, base64) and MARGIN_SIGNING_P12_PASSWORD, so the
# runner needs nothing in its own login keychain.
#
#   signing-keychain.sh create KEYCHAIN   P12_BASE64 and P12_PASSWORD in the environment;
#                                         prints the identity's SHA-1 for codesign --sign
#   signing-keychain.sh delete KEYCHAIN   always run this afterwards (if: always())
#
# The keychain gets a random password nobody keeps, `codesign` may use the key without a
# prompt (partition list), and it joins the user's search list only while the job runs.
set -eu

die() { echo "signing-keychain.sh: $*" >&2; exit 1; }

cmd="${1:-}"
kc="${2:-}"
[ -n "$kc" ] || die "usage: signing-keychain.sh create|delete KEYCHAIN"

case "$cmd" in
  create)
    [ -n "${P12_BASE64:-}" ] || die "P12_BASE64 is empty (repository secret MARGIN_SIGNING_P12_BASE64)"
    [ -n "${P12_PASSWORD:-}" ] || die "P12_PASSWORD is empty (repository secret MARGIN_SIGNING_P12_PASSWORD)"
    umask 077
    kc_password="$(/usr/bin/openssl rand -hex 24)"
    security delete-keychain "$kc" 2>/dev/null || true
    security create-keychain -p "$kc_password" "$kc"
    # Locks again after 6 h or on sleep; a build takes minutes.
    security set-keychain-settings -lut 21600 "$kc"
    security unlock-keychain -p "$kc_password" "$kc"
    p12="$(mktemp)"
    trap 'rm -f "$p12"' EXIT
    printf '%s' "$P12_BASE64" | base64 --decode > "$p12"
    security import "$p12" -k "$kc" -f pkcs12 -P "$P12_PASSWORD" -T /usr/bin/codesign >/dev/null
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kc_password" "$kc" >/dev/null
    # codesign resolves the identity's chain through the search list.
    existing="$(security list-keychains -d user | sed -e 's/^ *"//' -e 's/" *$//')"
    # shellcheck disable=SC2086 # one keychain path per line, no spaces in them
    security list-keychains -d user -s "$kc" $existing
    sha1="$(security find-identity -p codesigning "$kc" | awk '/[0-9A-F]{40}/ {print $2; exit}')"
    [ -n "$sha1" ] || die "no code-signing identity in the imported .p12"
    echo "$sha1"
    ;;
  delete)
    # Also removes it from the search list.
    security delete-keychain "$kc" 2>/dev/null || true
    ;;
  *)
    die "usage: signing-keychain.sh create|delete KEYCHAIN"
    ;;
esac
