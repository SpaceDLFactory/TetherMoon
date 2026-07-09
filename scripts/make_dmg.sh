#!/usr/bin/env bash
# 배포용 .dmg 패키징 — 드래그→Applications 레이아웃 + 첫 실행 안내.
# 전제: ./scripts/make_app.sh 로 dist/TetherMoon.app 가 먼저 만들어져 있어야 함.
# 사용: ./scripts/make_dmg.sh
# 결과: dist/TetherMoon-v<버전>.dmg
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(grep '^version' crsdk_server/Cargo.toml | head -1 | cut -d'"' -f2)"
APP="dist/TetherMoon.app"
DMG="dist/TetherMoon-v${VERSION}.dmg"
STAGE="dist/dmg_stage"

[ -d "$APP" ] || { echo "✗ .app 없음 — 먼저 ./scripts/make_app.sh"; exit 1; }

echo "▶ DMG 스테이징"
rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"   # 드래그 대상

# 첫 실행 안내 (서명 안 된 앱이라 Gatekeeper 우회 방법 명시)
cat > "$STAGE/First launch — read me.txt" <<'TXT'
TetherMoon — install

1) Drag "TetherMoon.app" onto the Applications folder.
2) First launch (the app is adhoc-signed, not notarized). Run in Terminal:
      xattr -dr com.apple.quarantine "/Applications/TetherMoon.app"
   then open the app. Since macOS 15 Sequoia the "right-click -> Open"
   bypass is gone, so this command is what avoids the "app is damaged" error.
3) Connect the Sony A7C by USB. The app opens your browser automatically.
   On a phone: open the LAN URL shown at the bottom of the page (same Wi-Fi).

A7C (ILCE-7C) only. Free & open source (MIT).
Contact: spacedlfactory@gmail.com
TXT

echo "▶ DMG 생성"
hdiutil create -volname "TetherMoon" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"
find . -name .DS_Store -delete 2>/dev/null || true
echo "✓ $DMG ($(du -sh "$DMG" | cut -f1))"
