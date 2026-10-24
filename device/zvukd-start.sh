#!/bin/sh
# Запуск плеера «Звука», отвязанного от SSH-сессии.
#
# Зачем: демон, поднятый из ssh обычным `&` или через `start-stop-daemon -b`,
# умирает вместе с сессией (SIGHUP). При старте из init (S97station-root) это
# не нужно — там процесс и так живёт в своей сессии.
#
# Второй экземпляр запускать нельзя: две копии дерутся за сокет, и получится
# «play сыграл, а status пустой» — команда ушла одному, чтение попало в другой.
export ZVUK_MODE="${ZVUK_MODE:-station}"
rm -f /run/zvukd.sock
setsid /data/local/opt/zvuk/bin/zvukd >> /data/local/tmp/zvukd.log 2>&1 < /dev/null &
exit 0
