#!/bin/sh
# Перезапуск слушателя голоса ровно в одной копии.
# Отдельный файл нужен потому, что pkill по шаблону "alice-voice.sh" убивает
# собственную ssh-команду, в тексте которой этот путь тоже присутствует.
for d in /proc/[0-9]*; do
  pid=${d#/proc/}
  [ "$pid" = "$$" ] && continue
  c=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null) || continue
  case "$c" in
    *"/bin/sh /data/local/bin/alice-voice.sh"*|*"socat -u TCP:127.0.0.1:9875"*) kill -9 "$pid" 2>/dev/null ;;
  esac
done
sleep 1
start-stop-daemon -S -b -m -p /run/alice-voice.pid -x /bin/sh -- /data/local/bin/alice-voice.sh
