#!/usr/bin/env bash
# Renders background.svg to the checked-in PNGs (CI never renders; it uses the PNGs).
# Run after editing the SVG: deploy/dmg/render.sh
#   background.png / background@2x.png          "Drag Margin to Applications"
#   background-dev.png / background-dev@2x.png  "Drag Margin Dev to Applications"
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
font="$here/../../crates/app/assets/fonts/AlbertSans-wght.ttf"

render() { # name suffix
  local svg
  svg="$(sed "s/APP_NAME/$1/" "$here/background.svg")"
  uvx --from resvg-py==0.5.0 python - "$svg" "$font" "$here/background$2" <<'PY'
import sys, resvg_py
svg, font, out = sys.argv[1:]
for zoom, tail in ((1, ".png"), (2, "@2x.png")):
    png = resvg_py.svg_to_bytes(svg_string=svg, font_files=[font], skip_system_fonts=True, zoom=zoom)
    open(out + tail, "wb").write(bytes(png))
PY
  # Finder pairs the two into one HiDPI background; mark the @2x as 144 dpi.
  sips -s dpiWidth 144 -s dpiHeight 144 "$here/background$2@2x.png" >/dev/null
}
render "Margin" ""
render "Margin Dev" "-dev"
ls -l "$here"/background*.png
