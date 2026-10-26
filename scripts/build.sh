#!/bin/sh
# Собрать zvukd под колонку и вытащить бинарник в dist/.
#
# Требуется Docker (на маке есть). Отдельный Rust на маке не нужен.
set -eu

cd "$(dirname "$0")/.."
mkdir -p dist

echo "== сборка в контейнере (aarch64 + musl, статически) =="
# Собираем стадию builder и забираем бинарник из неё: финальная стадия — FROM scratch,
# а у неё нет ни команды для `docker create`, ни shell. Сборка --target builder
# переиспользует те же слои кеша, что и полная.
docker build --platform linux/arm64 --target builder -t zvukd-builder .

echo "== вынимаю бинарник =="
docker run --rm --platform linux/arm64 zvukd-builder \
    cat /src/target/aarch64-unknown-linux-musl/release/zvukd > dist/zvukd

chmod +x dist/zvukd

# Проверка, что бинарник действительно пересобран: Docker иногда отдаёт слой из кеша,
# и в dist/ остаётся старая сборка — снаружи это выглядит как «правка не работает».
NEWEST=$(ls -t src/*.rs | head -1)
if [ "$NEWEST" -nt dist/zvukd ]; then
    echo "ВНИМАНИЕ: dist/zvukd старше $NEWEST — похоже, сработал кеш Docker."
    echo "          Пересоберите с нуля: docker build --no-cache --target builder -t zvukd-builder ."
fi

echo "== готово: dist/zvukd =="
ls -lh dist/zvukd
file dist/zvukd 2>/dev/null || true
