# API Звука — что нужно для клиента на Rust

Всё выяснено из рабочего Python-клиента `zvuk_music` (лежит в `~/Cursor/zvuk-player/.venv`),
поэтому спецификация точная, а не по догадкам.

## Основа

| Что | Значение |
|---|---|
| GraphQL | `POST https://zvuk.com/api/v1/graphql` |
| Tiny API | `https://zvuk.com/api/tiny` (профиль, прямые ссылки на поток) |
| Авторизация | заголовок **`X-Auth-Token: <token>`** |
| Обязательные заголовки | `Referer: https://zvuk.com/`, `Origin: https://zvuk.com`, `Accept: application/json` |

Токен — те же 32 символа, что лежат в `~/Cursor/zvuk-player/config.json` (поле `token`).
Анонимный можно получить `GET /api/tiny/profile` → `result.token`, но у него урезанный доступ.

## GraphQL-вызов

```json
POST /api/v1/graphql
{"query": "<текст запроса>", "operationName": "getStream", "variables": {"ids": ["12345"]}}
```

Ответ — `{"data": {...}}`; ошибки приходят в `errors[]`.

## Нужные операции

Все тексты запросов — в каталоге [`queries/`](../queries).

### 1. Поиск

`search.graphql` (operation `search`) — полный поиск, отдаёт секции с треками.
`quickSearch.graphql` (operation `quickSearch`) — быстрый, хватает для «включи <название>».

Переменные: `{"query": "radiohead", "limit": 10}`.

Из ответа нужен **id трека** (`content[] ... on Track { id title duration }`).

### 2. Ссылки на поток

`getStream.graphql` (operation `getStream`), переменные `{"ids": ["<trackId>"]}`.

Ответ: `data.mediaContents[].stream` с полями:

| Поле | Что это |
|---|---|
| `mid` | ~128 kbps, mp3 |
| `high` | ~320 kbps, mp3 |
| `flac` | FLAC без защиты (если есть подписка) |
| `flacdrm` | FLAC с DRM — **не играется**, пропускать |
| `expire` / `expireDelta` | когда ссылка протухнет (сек) |

Порядок выбора: `high` → `mid`. `flac`/`flacdrm` не берём: `flacdrm` защищён и не играется
вовсе, а `high` (mp3 ≈320 kbps) на этом усилителе ничем не хуже.

> Отдельно: если отдать FLAC штатному плееру станции (`audio_play` с ссылкой `flac`), он его
> декодирует **сам** — локальный декодер не нужен. Локальный `mpg123`/`flac` нужен только
> в режиме `ZVUK_MODE=alsa`.

### 3. Прямые ссылки (альтернатива)

`GET /api/tiny/track/stream?id=<id>&quality=high` — отдаёт прямой URL без DRM-обёртки.
Полезно, если GraphQL-ссылка окажется завязанной на DRM.

## Воспроизведение на колонке

| Формат | Чем играть |
|---|---|
| mp3 (mid/high) | в режиме станции — её плеер (`audio_play`); в `ZVUK_MODE=alsa` — `mpg123 -q <url>` |
| FLAC | только её плеер (`audio_play`): он декодирует сам, локальный `flac` на колонке не нужен |
| `flacdrm` | никак: снимать защиту мы не будем, берём `high` |

Ссылки отдаются по https — качать можно напрямую, `mpg123` умеет играть по URL (проверить
на практике; если нет — сохранять во временный файл и играть локально).

## Грабли

* `flacdrm` — не пытаться играть, молча пропускать и брать `high`.
* Ссылки протухают (`expire`) — резолвить поток **перед** каждым треком, не кэшировать надолго.
* `X-Auth-Token` не логин/пароль: это токен веб-сессии, при смене пароля/выходе он умирает.
* Anti-bot: без `Referer`/`Origin` и человеческого `User-Agent` GraphQL может отвечать 403.
* Ответы бывают с `__typename` — парсить через `serde_json::Value` и не завязываться на строгие
  структуры, схема меняется.
