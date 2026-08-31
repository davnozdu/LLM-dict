#!/usr/bin/env bash
# Ставит тот же сертификат подписи, которым подписывает CI, в локальную связку
# ключей. Нужно, чтобы локальная сборка и сборка из релиза выглядели для macOS
# одной и той же программой: иначе разрешения и записи Keychain у них разные,
# и получается «доступ выдан, но не работает».
#
# Использование: build/setup-local-cert.sh <cert.p12> <cert.pem> [пароль]
set -euo pipefail

P12="${1:?укажите путь к cert.p12}"
PEM="${2:?укажите путь к cert.pem}"
PASSWORD="${3:-}"

if [[ -z "$PASSWORD" ]]; then
    read -rsp "Пароль от .p12: " PASSWORD
    echo
fi

KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

security import "$P12" -k "$KEYCHAIN" -P "$PASSWORD" -T /usr/bin/codesign -A
security set-key-partition-list -S apple-tool:,apple:,codesign: -k "" "$KEYCHAIN" >/dev/null 2>&1 || true

echo "Теперь нужен пароль администратора: сертификат надо пометить доверенным,"
echo "иначе codesign сочтёт его недействительным."
sudo security add-trusted-cert -d -r trustRoot -p codeSign \
    -k /Library/Keychains/System.keychain "$PEM"

echo
echo "Готово. Проверка:"
security find-identity -v -p codesigning | grep -i "LLM-dict" || {
    echo "identity не найдена — сертификат импортировался, но не стал доверенным" >&2
    exit 1
}
echo
echo "Дальше подписывайте локальные сборки так:"
echo "  SIGN_IDENTITY='LLM-dict Self Signed' build/sign.sh"
