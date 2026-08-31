#!/usr/bin/env bash
# Собирает DMG из готового бандла.
#
# GitHub Actions архивирует артефакты сам, поэтому zip внутри превращался бы
# в zip внутри zip с тем же именем — Архиватор macOS на этом спотыкается.
# Образ такой проблемы не создаёт: после распаковки получается сразу .dmg.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="${1:-$ROOT/dist/LLM-dict.app}"
VERSION="${2:-0.1.0}"
OUT="$ROOT/dist/LLM-dict-$VERSION-arm64.dmg"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

rm -f "$OUT"
hdiutil create \
    -volname "LLM-dict" \
    -srcfolder "$STAGE" \
    -ov -format UDZO \
    "$OUT" >/dev/null

echo "собрано: $OUT"
