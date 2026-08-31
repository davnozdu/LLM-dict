#!/usr/bin/env bash
# Собирает LLM-dict.app из готового бинарника.
# Использование: build/bundle.sh <путь-к-бинарнику> [версия]
set -euo pipefail

BIN="${1:-target/release/llm-dict}"
VERSION="${2:-0.1.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/dist/LLM-dict.app"

if [[ ! -f "$BIN" ]]; then
    echo "не найден бинарник: $BIN" >&2
    exit 1
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/llm-dict"
chmod +x "$APP/Contents/MacOS/llm-dict"

sed "s/__VERSION__/$VERSION/g" "$ROOT/build/Info.plist" > "$APP/Contents/Info.plist"

if [[ -f "$ROOT/build/AppIcon.icns" ]]; then
    cp "$ROOT/build/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
    /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" "$APP/Contents/Info.plist" 2>/dev/null || true
fi

echo "собрано: $APP"
