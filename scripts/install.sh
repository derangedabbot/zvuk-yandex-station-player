#!/bin/sh
# Установка zvukd на Яндекс Станцию (нужен root и SSH на устройство).
#
#   STATION=<IP-КОЛОНКИ> scripts/install.sh
#
# Переменные:
#   STATION        адрес станции (обязателен)
#   PORT           порт SSH (по умолчанию 2222)
#   KEY            путь к ключу (по умолчанию ~/.ssh/station_key)
#   ZVUK_TOKEN     токен Звука; если не задан — берётся из файла ZVUK_TOKEN_FILE
#   ZVUK_TOKEN_FILE локальный файл с токеном (по умолчанию ~/.zvuk-token)
#
# На станции нет scp, поэтому файлы передаются через `cat файл | ssh 'cat > путь'`.
# Скрипт идемпотентный: можно запускать повторно.
set -eu

STATION=${STATION:?укажите адрес: STATION=<IP-КОЛОНКИ> scripts/install.sh}
PORT=${PORT:-2222}
KEY=${KEY:-$HOME/.ssh/station_key}
ZVUK_TOKEN_FILE=${ZVUK_TOKEN_FILE:-$HOME/.zvuk-token}

SSH="ssh -i $KEY -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes"
TARGET="root@$STATION"

push() { # push <локальный файл> <путь на станции> <права>
    cat "$1" | $SSH "$TARGET" "cat > '$2' && chmod '$3' '$2'"
}

cd "$(dirname "$0")/.."
[ -f dist/zvukd ] || { echo "нет dist/zvukd — сначала scripts/build.sh"; exit 1; }

echo "== каталоги"
$SSH "$TARGET" 'mkdir -p /data/local/opt/zvuk/bin /data/local/bin /data/local/etc /data/local/tmp/zvukd'

echo "== бинарник"
push dist/zvukd /data/local/opt/zvuk/bin/zvukd 755

echo "== токен"
if [ -n "${ZVUK_TOKEN:-}" ]; then
    printf '%s\n' "$ZVUK_TOKEN" > /tmp/zvuk.token.$$
else
    [ -f "$ZVUK_TOKEN_FILE" ] || {
        echo "нет токена: задайте ZVUK_TOKEN или положите его в $ZVUK_TOKEN_FILE"; exit 1; }
    tr -d '\n' < "$ZVUK_TOKEN_FILE" > /tmp/zvuk.token.$$
    printf '\n' >> /tmp/zvuk.token.$$
fi
push /tmp/zvuk.token.$$ /data/local/etc/zvuk.token 600
rm -f /tmp/zvuk.token.$$

echo "== обёртки"
printf '#!/bin/sh\nexec /data/local/opt/zvuk/bin/zvukd "$@"\n' > /tmp/zvukd.wrap.$$
printf '#!/bin/sh\nexec /data/local/opt/zvuk/bin/zvukd >> /data/local/tmp/zvukd.log 2>&1\n' > /tmp/zvukd.daemon.$$
push /tmp/zvukd.wrap.$$ /data/local/bin/zvukd 755
push /tmp/zvukd.daemon.$$ /data/local/bin/zvukd-daemon.sh 755
rm -f /tmp/zvukd.wrap.$$ /tmp/zvukd.daemon.$$

echo "== автозапуск"
cat <<'OUTER' | $SSH "$TARGET" 'sh -s'
F=/etc/init.d/S97station-root
[ -f "$F" ] || { echo "нет $F — это не та прошивка?"; exit 0; }
mount -o remount,rw / 2>/dev/null || true
if grep -q 'zvukd-daemon.sh' "$F"; then
    echo "уже прописан"
else
    cp "$F" "/data/local/backup-S97station-root.$(date +%s)" 2>/dev/null || true
    awk '
      /# веб-панель/ && !d {
        print "    # проигрыватель Звука";
        print "    [ -x /data/local/bin/zvukd-daemon.sh ] && \\";
        print "      start-stop-daemon -S -b -m -p /run/zvukd.pid -x /bin/sh -- /data/local/bin/zvukd-daemon.sh";
        d = 1
      }
      { print }
    ' "$F" > /tmp/s97.new
    sh -n /tmp/s97.new && chmod 755 /tmp/s97.new && mv /tmp/s97.new "$F"
    echo "прописан"
fi
OUTER

echo "== запуск"
$SSH "$TARGET" '
start-stop-daemon -K -p /run/zvukd.pid 2>/dev/null || true
sleep 1
rm -f /run/zvukd.sock
start-stop-daemon -S -b -m -p /run/zvukd.pid -x /bin/sh -- /data/local/bin/zvukd-daemon.sh
sleep 2
/data/local/bin/zvukd ping || echo "демон не ответил — смотрите /data/local/tmp/zvukd.log"'

echo "== готово"
echo "Проверка: $SSH $TARGET '/data/local/bin/zvukd flow'"
