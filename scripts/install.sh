#!/bin/sh
# Установка плеера «Звук» (zvukd) на Яндекс Станцию — одной командой.
#
#   ./install.sh 192.168.1.50                 # поставит плеер (токен спросит)
#   ./install.sh 192.168.1.50 --with-voice    # ещё и голосовые команды
#   ./install.sh 192.168.1.50 --check         # только проверить, ничего не менять
#
# Параметры:
#   --token <T>     токен «Звука» (иначе спросит, если на колонке его ещё нет)
#   --port <N>      порт SSH (по умолчанию 2222)
#   --key <path>    ключ SSH (по умолчанию ~/.ssh/station_key)
#   --with-voice    поставить слушатель голоса и фразы-триггеры
#   --no-start      не запускать демон после установки
#   --check         только проверка, без изменений
#
# Нужен root и SSH на колонке (как получить — в публичном репозитории про root).
# Скрипт идемпотентный: повторный запуск обновляет файлы и перезапускает демон.
# Автозапуск ставится отдельным файлом /etc/init.d/S96zvukd — чужие скрипты не правим.
set -eu

IP=""; TOKEN=""
PORT="${PORT:-2222}"
KEY="${KEY:-$HOME/.ssh/station_key}"
WITH_VOICE=0; DO_START=1; CHECK_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --token) TOKEN="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --key) KEY="$2"; shift 2 ;;
    --with-voice) WITH_VOICE=1; shift ;;
    --no-start) DO_START=0; shift ;;
    --check) CHECK_ONLY=1; shift ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    -*) echo "неизвестный параметр: $1" >&2; exit 2 ;;
    *) IP="$1"; shift ;;
  esac
done

[ -n "$IP" ] || { echo "укажите адрес колонки: ./install.sh <IP> [параметры]" >&2; exit 2; }
SSH="ssh -i $KEY -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=8 root@$IP"
say() { printf '%s\n' "$*"; }
die() { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }
push() { cat "$1" | $SSH "cat > '$2' && chmod '$3' '$2'" || die "не удалось записать $2"; }
# Для файлов, которые уже запущены (бинарник, скрипт-слушатель), писать «поверх» нельзя —
# ядро отвечает «Text file busy». Пишем рядом и переименовываем: rename атомарный.
push_bin() { cat "$1" | $SSH "cat > '$2.new' && chmod '$3' '$2.new' && mv '$2.new' '$2'" \
  || die "не удалось записать $2"; }

cd "$(dirname "$0")/.."
[ -f dist/zvukd ] || die "нет dist/zvukd — сначала соберите: scripts/build.sh"

say "== 1/7 связь и права"
$SSH true 2>/dev/null || die "нет SSH по ключу $KEY на $IP:$PORT
  Нужен root-доступ и ключ. Другой ключ: --key ~/.ssh/другой"
[ "$($SSH 'id -u')" = "0" ] || die "SSH отвечает, но не от root"
$SSH 'mkdir -p /data/local/opt/zvuk/bin /data/local/bin /data/local/etc /data/local/tmp/zvukd' \
  || die "не удалось создать каталоги в /data"
FREE=$($SSH 'df -k /data | tail -1 | awk "{print \$4}"')
say "   root: да, свободно в /data: $((FREE / 1024)) МБ"

if [ "$CHECK_ONLY" = "1" ]; then
  say "== состояние (ничего не меняю)"
  say "   бинарник: $($SSH '[ -x /data/local/opt/zvuk/bin/zvukd ] && echo есть || echo НЕТ')"
  say "   токен:    $($SSH '[ -s /data/local/etc/zvuk.token ] && echo есть || echo НЕТ')"
  say "   демон:    $($SSH '/data/local/opt/zvuk/bin/zvukd ping 2>&1 | head -1')"
  say "   плеер:    $($SSH '/data/local/opt/zvuk/bin/zvukd status 2>/dev/null | cut -d"|" -f1,2')"
  say "   автозапуск: $($SSH '[ -x /etc/init.d/S96zvukd ] && echo есть || echo НЕТ')"
  exit 0
fi

say "== 2/7 токен «Звука»"
TF=/tmp/zvuk-token.$$
if [ -n "$TOKEN" ]; then
  printf '%s\n' "$TOKEN" > "$TF"
elif [ -n "${ZVUK_TOKEN:-}" ]; then
  printf '%s\n' "$ZVUK_TOKEN" > "$TF"
elif [ -n "${ZVUK_TOKEN_FILE:-}" ] && [ -f "${ZVUK_TOKEN_FILE}" ]; then
  cp "$ZVUK_TOKEN_FILE" "$TF"
elif $SSH '[ -s /data/local/etc/zvuk.token ]'; then
  say "   на колонке токен уже есть — оставляю"
else
  printf '   вставьте токен «Звука» (ввод не виден): '
  stty -echo 2>/dev/null || true
  read -r TOKEN_IN
  stty echo 2>/dev/null || true
  printf '\n'
  [ -n "$TOKEN_IN" ] || die "токен не введён"
  printf '%s\n' "$TOKEN_IN" > "$TF"
fi
if [ -f "$TF" ]; then
  push "$TF" /data/local/etc/zvuk.token 600
  rm -f "$TF"
  say "   токен записан, права 600"
fi

say "== 3/7 бинарник и запуск"
push_bin dist/zvukd /data/local/opt/zvuk/bin/zvukd 755
push device/zvukd-start.sh /data/local/bin/zvukd-start.sh 755
$SSH 'printf "%s\n" "#!/bin/sh" "exec /data/local/opt/zvuk/bin/zvukd \"\$@\"" > /data/local/bin/zvukd && chmod 755 /data/local/bin/zvukd'
say "   разложено в /data/local"

if [ "$WITH_VOICE" = "1" ]; then
  say "== 4/7 голос"
  push_bin device/alice-voice.sh /data/local/bin/alice-voice.sh 755
  push device/voice-restart.sh /data/local/bin/voice-restart.sh 755
  push device/voice-triggers.txt /data/local/etc/voice-triggers.txt 644
  say "   слушатель и фразы-триггеры на месте"
else
  say "== 4/7 голос: пропущен (--with-voice)"
fi

say "== 5/7 автозапуск (/etc/init.d/S96zvukd)"
if [ "$WITH_VOICE" = "1" ]; then VOICE_LINE=1; else VOICE_LINE=0; fi
$SSH "cat > /tmp/S96.new <<'INIT'
#!/bin/sh
# Автозапуск плеера «Звук» (ставится скриптом install.sh)
case \"\$1\" in
  start)
    [ -x /data/local/bin/zvukd-start.sh ] && /data/local/bin/zvukd-start.sh
    if [ $VOICE_LINE = 1 ] && [ -x /data/local/bin/alice-voice.sh ]; then
      # Одна копия: pid из файла должен принадлежать слушателю. Если он устарел и
      # указывает на любой живой /bin/sh, start-stop-daemon ответит «/bin/sh is already
      # running» и не запустит ничего, а следующая попытка поднимет вторую копию.
      av_pid=\$(cat /run/alice-voice.pid 2>/dev/null)
      if [ -n \"\$av_pid\" ] && grep -q alice-voice.sh /proc/\$av_pid/cmdline 2>/dev/null; then
        :
      else
        rm -f /run/alice-voice.pid
        start-stop-daemon -S -b -m -p /run/alice-voice.pid -x /bin/sh -- /data/local/bin/alice-voice.sh
      fi
    fi
    ;;
  stop)
    pkill -f /data/local/opt/zvuk/bin/zvukd 2>/dev/null
    pkill -f /data/local/bin/alice-voice.sh 2>/dev/null
    ;;
esac
exit 0
INIT
mount -o remount,rw / 2>/dev/null || true
sh -n /tmp/S96.new || { echo 'ОШИБКА: синтаксис'; exit 1; }
cp /tmp/S96.new /etc/init.d/S96zvukd && chmod 755 /etc/init.d/S96zvukd
mount -o remount,ro / 2>/dev/null || true
echo '   прописано'"
[ "$WITH_VOICE" = "1" ] || say "   (без голоса: слушатель в автозапуск не включён)"

if [ "$DO_START" = "1" ]; then
  say "== 6/7 запуск"
  $SSH '/data/local/bin/zvukd-start.sh >/dev/null 2>&1 || true'
  sleep 3
  PING=$($SSH '/data/local/opt/zvuk/bin/zvukd ping 2>&1 | head -1')
  case "$PING" in
    pong*) say "   демон отвечает pong" ;;
    *) die "демон не поднялся: $PING (лог /data/local/tmp/zvukd.log)" ;;
  esac
else
  say "== 6/7 запуск пропущен (--no-start)"
fi

say "== 7/7 итог"
$SSH '/data/local/bin/zvukd status | cut -d"|" -f1,2 | sed "s/^/   /"'
if [ "$WITH_VOICE" = "1" ]; then
  $SSH 'sh /data/local/bin/voice-restart.sh >/dev/null 2>&1 || true'
  say "   голос: слушатель перезапущен (лог /data/local/tmp/alice-voice.log)"
fi

say ""
say "Готово. Дальше:"
say "  проверить звук:  ssh ... '/data/local/bin/zvukd play \"radiohead creep\"'"
say "  подсветка:       в приложении «Дом с Алисой» выбрать вариант «выключено»,"
say "                   иначе штатная анимация затирает наше кольцо"
say "  команды:         play / pause / resume / next / stop / flow / status"
