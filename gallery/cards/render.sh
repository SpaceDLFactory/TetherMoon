#!/usr/bin/env bash
# Regenerate the SNS/OG cards from the HTML templates via headless Chrome.
# Edit card-portrait.html / card-wide.html, then run this.
set -euo pipefail
cd "$(dirname "$0")"
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
r(){ "$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
  --default-background-color=00000000 --window-size="$2" --screenshot="$3" "file://$PWD/$1" >/dev/null 2>&1; }
r card-portrait.html 1600,2000 ../sns-card.png
r card-wide.html     2000,1125 ../sns-wide.png
r card-wide.html     1200,675  ../og-image.png
echo "✓ regenerated sns-card.png, sns-wide.png, og-image.png"
