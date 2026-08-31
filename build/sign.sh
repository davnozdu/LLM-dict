#!/usr/bin/env bash
# Подписывает бандл.
#
# Разрешение «Универсальный доступ» macOS привязывает к подписи. При ad-hoc
# подписи (codesign -s -) хеш меняется с каждой сборкой, и разрешение придётся
# выдавать заново после каждого обновления. Если в секретах репозитория лежит
# самоподписанный сертификат (MACOS_CERT_P12 + MACOS_CERT_PASSWORD), подпись
# остаётся стабильной и разрешение переживает обновления.
#
# Hardened runtime намеренно не включается: без нотаризации он только мешает —
# доступ к микрофону начал бы требовать проверенных Apple entitlements.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="${1:-$ROOT/dist/LLM-dict.app}"
IDENTITY="${SIGN_IDENTITY:--}"

codesign --force --deep --sign "$IDENTITY" \
    --entitlements "$ROOT/build/entitlements.plist" \
    "$APP"

codesign --verify --verbose=2 "$APP"
echo "подписано ($IDENTITY): $APP"
