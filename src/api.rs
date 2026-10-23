//! Клиент API Звука: поиск трека и получение ссылки на поток.
//!
//! Спецификация — в `docs/api.md`. Здесь только то, что нужно проигрывателю:
//! «найди первый трек по строке» и «дай ссылку на mp3 для этого id».

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

const GQL_URL: &str = "https://zvuk.com/api/v1/graphql";
const TINY_URL: &str = "https://zvuk.com/api/tiny";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

const Q_SEARCH: &str = include_str!("../queries/search.graphql");
const Q_QUICK: &str = include_str!("../queries/quickSearch.graphql");
const Q_STREAM: &str = include_str!("../queries/getStream.graphql");
const Q_WAVE: &str = include_str!("../queries/getPersonalWave.graphql");
const Q_RELEASES: &str = include_str!("../queries/getReleases.graphql");
const Q_ARTISTS: &str = include_str!("../queries/getArtists.graphql");
const Q_TRACKS: &str = include_str!("../queries/getTracks.graphql");
const Q_PLAYLIST: &str = include_str!("../queries/getPlaylistTracks.graphql");

#[derive(Debug, Clone)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artists: String,
    pub duration: i64,
    /// Ссылка на обложку релиза (пусто, если Звук её не дал).
    pub cover: String,
    /// Альбом (релиз), из которого трек — чтобы открыть его из панели.
    pub album_id: String,
    pub album: String,
    pub year: String,
}

impl Track {
    pub fn label(&self) -> String {
        if self.artists.is_empty() {
            self.title.clone()
        } else {
            format!("{} — {}", self.artists, self.title)
        }
    }

    /// Короткая запись для панели и очереди.
    pub fn brief(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "artists": self.artists,
            "duration": self.duration,
            "cover": self.cover,
            "album_id": self.album_id,
            "album": self.album,
            "year": self.year,
        })
    }
}

/// Альбом (то, что Звук называет релизом: альбом, сингл, EP).
#[derive(Debug, Clone)]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artists: String,
    pub kind: String,
    pub cover: String,
    /// Год издания (только цифры), пусто если Звук его не дал.
    pub year: String,
}

impl Album {
    pub fn brief(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "artists": self.artists,
            "kind": self.kind,
            "cover": self.cover,
            "year": self.year,
        })
    }
}

/// Плейлист.
#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub cover: String,
    pub tracks: i64,
}

impl Playlist {
    pub fn brief(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "cover": self.cover,
            "tracks": self.tracks,
        })
    }
}

/// Исполнитель.
#[derive(Debug, Clone)]
pub struct Artist {
    pub id: String,
    pub title: String,
    pub cover: String,
}

impl Artist {
    pub fn brief(&self) -> serde_json::Value {
        serde_json::json!({ "id": self.id, "title": self.title, "cover": self.cover })
    }
}

/// Что нашлось по строке: три группы, как в самом Звуке.
#[derive(Debug, Default, Clone)]
pub struct Found {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<Playlist>,
}

impl Found {
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
            && self.albums.is_empty()
            && self.artists.is_empty()
            && self.playlists.is_empty()
    }

    pub fn brief(&self, query: &str) -> serde_json::Value {
        serde_json::json!({
            "query": query,
            "tracks": self.tracks.iter().map(|t| t.brief()).collect::<Vec<_>>(),
            "albums": self.albums.iter().map(|a| a.brief()).collect::<Vec<_>>(),
            "artists": self.artists.iter().map(|a| a.brief()).collect::<Vec<_>>(),
            "playlists": self.playlists.iter().map(|p| p.brief()).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    token: String,
}

impl Api {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("не удалось создать HTTP-клиент")?;
        Ok(Self { http, token })
    }

    /// Анонимный токен: `GET /api/tiny/profile` → result.token.
    /// Нужен только для проверки, что API жив; для воспроизведения не годится.
    pub async fn anonymous_token() -> Result<String> {
        let txt = reqwest::Client::new()
            .get(format!("{TINY_URL}/profile"))
            .header("User-Agent", UA)
            .header("Referer", "https://zvuk.com/")
            .send()
            .await?
            .text()
            .await?;
        let v: Value = serde_json::from_str(&txt)?;
        v.pointer("/result/token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("в ответе /profile нет токена"))
    }

    async fn gql(&self, query: &str, op: &str, vars: Value) -> Result<Value> {
        let body = json!({ "query": query, "operationName": op, "variables": vars });
        let resp = self
            .http
            .post(GQL_URL)
            .header("X-Auth-Token", &self.token)
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Language", "ru-RU,ru;q=0.9")
            .header("Referer", "https://zvuk.com/")
            .header("Origin", "https://zvuk.com")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("GraphQL {op}: сеть недоступна"))?;

        let status = resp.status();
        let txt = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("GraphQL {op}: HTTP {status}: {}", charset(&txt, 300)));
        }
        let v: Value = serde_json::from_str(&txt)
            .with_context(|| format!("GraphQL {op}: не JSON: {}", charset(&txt, 200)))?;
        if let Some(errs) = v.get("errors") {
            return Err(anyhow!("GraphQL {op}: ошибки: {}", charset(&errs.to_string(), 300)));
        }
        Ok(v)
    }

    /// Первый трек по поисковой строке.
    pub async fn search_first(&self, query: &str) -> Result<Track> {
        let items = self.search(query, 10).await?;
        items
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("ничего не нашлось по «{query}»"))
    }

    /// Список треков по поисковой строке (оставлено для `search_first`).
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<Track>> {
        let found = self.search_all(query, limit).await?;
        if found.tracks.is_empty() {
            return Err(anyhow!("ничего не нашлось по «{query}»"));
        }
        Ok(found.tracks)
    }

    /// Поиск по трём группам — так же, как ищет сам Звук: треки, альбомы, исполнители.
    ///
    /// Схема меняется, поэтому берём явные пути (`search` → `tracks/releases/artists.items`,
    /// `quickSearch` → `content`), а если их нет — обходим JSON и собираем всё,
    /// что похоже на трек.
    pub async fn search_all(&self, query: &str, limit: u32) -> Result<Found> {
        let vars = json!({ "query": query, "limit": limit });
        let v = match self.gql(Q_SEARCH, "search", vars).await {
            Ok(v) => v,
            Err(e) => {
                // полный поиск иногда ругается на пустые фильтры — падаем на быстрый
                eprintln!("search не сработал ({e}), пробую quickSearch");
                self.gql(Q_QUICK, "quickSearch", json!({ "query": query, "limit": limit }))
                    .await?
            }
        };
        let data = v.get("data").ok_or_else(|| anyhow!("в ответе нет data"))?;
        let mut out = Found::default();

        if let Some(items) = data
            .pointer("/search/tracks/items")
            .or_else(|| data.pointer("/quickSearch/content"))
            .and_then(Value::as_array)
        {
            for item in items {
                if let Some(t) = track_from(item) {
                    if !out.tracks.iter().any(|x| x.id == t.id) {
                        out.tracks.push(t);
                    }
                }
            }
        }
        if let Some(items) = data.pointer("/search/releases/items").and_then(Value::as_array) {
            for item in items {
                if let Some(a) = album_from(item) {
                    if !out.albums.iter().any(|x| x.id == a.id) {
                        out.albums.push(a);
                    }
                }
            }
        }
        if let Some(items) = data.pointer("/search/artists/items").and_then(Value::as_array) {
            for item in items {
                if let Some(a) = artist_from(item) {
                    if !out.artists.iter().any(|x| x.id == a.id) {
                        out.artists.push(a);
                    }
                }
            }
        }
        if let Some(items) = data.pointer("/search/playlists/items").and_then(Value::as_array) {
            for item in items {
                if let Some(p) = playlist_from(item) {
                    if !out.playlists.iter().any(|x| x.id == p.id) {
                        out.playlists.push(p);
                    }
                }
            }
        }
        if out.is_empty() {
            collect_tracks(data, &mut out.tracks);
        }
        if out.is_empty() {
            return Err(anyhow!("ничего не нашлось по «{query}»"));
        }
        Ok(out)
    }

    /// Трек по его id — когда id пришёл не из поиска (например, из очереди).
    pub async fn track(&self, id: &str) -> Result<Track> {
        let v = self.gql(Q_TRACKS, "getTracks", json!({ "ids": [id] })).await?;
        let item = v
            .pointer("/data/getTracks/0")
            .ok_or_else(|| anyhow!("getTracks: трек {id} не найден"))?;
        track_from(item).ok_or_else(|| anyhow!("getTracks: не разобрал трек {id}"))
    }

    /// Все треки альбома (релиза) — для «играть альбом целиком».
    pub async fn release_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let v = self
            .gql(Q_RELEASES, "getReleases", json!({ "ids": [id] }))
            .await?;
        let release = v
            .pointer("/data/getReleases/0")
            .ok_or_else(|| anyhow!("getReleases: альбом {id} не найден"))?;
        let items = release
            .get("tracks")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("getReleases: у альбома {id} нет треков"))?;
        let tracks: Vec<Track> = items.iter().filter_map(track_from).collect();
        if tracks.is_empty() {
            return Err(anyhow!("альбом {id} пустой"));
        }
        Ok(tracks)
    }

    /// Одна страница треков плейлиста: большие плейлисты приходят частями.
    pub async fn playlist_page(&self, id: &str, offset: u32, limit: u32) -> Result<Vec<Track>> {
        let v = self
            .gql(
                Q_PLAYLIST,
                "getPlaylistTracks",
                json!({ "id": id, "limit": limit, "offset": offset }),
            )
            .await?;
        let items = v
            .pointer("/data/playlistTracks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items.iter().filter_map(track_from).collect())
    }

    /// Сколько примерно треков в плейлисте: точного счётчика API не даёт,
    /// прикидываем по общей длительности (в среднем три с половиной минуты).
    pub async fn playlist_estimate(&self, id: &str) -> u32 {
        let v = self
            .gql(
                "query g($ids: [ID!]!) { getPlaylists(ids: $ids) { id duration } }",
                "g",
                json!({ "ids": [id] }),
            )
            .await;
        let Ok(v) = v else { return 0 };
        let secs = v
            .pointer("/data/getPlaylists/0/duration")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if secs <= 0 {
            return 0;
        }
        ((secs / 210).max(1)) as u32
    }

    /// Треки плейлиста (первая страница) — для «играть плейлист».
    pub async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let v = self
            .gql(Q_PLAYLIST, "getPlaylistTracks", json!({ "id": id, "limit": 100 }))
            .await?;
        let items = v
            .pointer("/data/playlistTracks")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("getPlaylistTracks: плейлист {id} не найден"))?;
        let tracks: Vec<Track> = items.iter().filter_map(track_from).collect();
        if tracks.is_empty() {
            return Err(anyhow!("плейлист {id} пустой"));
        }
        Ok(tracks)
    }

    /// Все альбомы и синглы исполнителя — чтобы выбрать нужный по названию.
    pub async fn artist_albums(&self, id: &str) -> Result<Vec<Album>> {
        let v = self
            .gql(
                Q_ARTISTS,
                "getArtists",
                json!({ "ids": [id], "withReleases": true, "withPopTracks": false }),
            )
            .await?;
        let artist = v
            .pointer("/data/getArtists/0")
            .ok_or_else(|| anyhow!("getArtists: исполнитель {id} не найден"))?;
        let items = artist
            .get("releases")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("getArtists: у исполнителя {id} нет альбомов"))?;
        let mut out = Vec::new();
        for item in items {
            if let Some(a) = album_from(item) {
                out.push(a);
            }
        }
        if out.is_empty() {
            return Err(anyhow!("у исполнителя {id} нет альбомов"));
        }
        Ok(out)
    }

    /// Популярные треки исполнителя — для «играть исполнителя».
    pub async fn artist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let v = self
            .gql(
                Q_ARTISTS,
                "getArtists",
                json!({ "ids": [id], "withPopTracks": true, "withReleases": false }),
            )
            .await?;
        let artist = v
            .pointer("/data/getArtists/0")
            .ok_or_else(|| anyhow!("getArtists: исполнитель {id} не найден"))?;
        let items = artist
            .get("popularTracks")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("getArtists: у исполнителя {id} нет популярных треков"))?;
        let tracks: Vec<Track> = items.iter().filter_map(track_from).collect();
        if tracks.is_empty() {
            return Err(anyhow!("у исполнителя {id} нет треков"));
        }
        Ok(tracks)
    }

    /// «Мой поток» — персональная волна Звука (`personalWaveContent`).
    /// Отдаёт пачку треков; вызывающий решает, сколько играть и когда догружать.
    pub async fn personal_wave(&self, count: u32) -> Result<Vec<Track>> {
        let vars = json!({
            "waveSrc": "AMAZME",
            "first": count,
            "options": { "mood": "energy:0.5,fun:0.5" }
        });
        let v = self.gql(Q_WAVE, "getPersonalWave", vars).await?;
        let items = v
            .pointer("/data/personalWaveContent")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("getPersonalWave: нет personalWaveContent"))?;

        let mut out = Vec::new();
        for item in items {
            if let Some(t) = track_from(item) {
                out.push(t);
            }
        }
        if out.is_empty() {
            return Err(anyhow!("поток вернул пустой список"));
        }
        Ok(out)
    }

    /// Ссылка на поток: high (mp3) → mid (mp3) → flac.
    /// `flacdrm` не играется и намеренно пропускается.
    pub async fn stream_url(&self, track_id: &str) -> Result<String> {
        let v = self
            .gql(Q_STREAM, "getStream", json!({ "ids": [track_id] }))
            .await?;
        let items = v
            .pointer("/data/mediaContents")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("getStream: нет mediaContents"))?;

        for item in items {
            let Some(stream) = item.get("stream") else { continue };
            for key in ["high", "mid", "flac"] {
                if let Some(url) = stream.get(key).and_then(Value::as_str) {
                    if url.starts_with("http") {
                        return Ok(url.to_owned());
                    }
                }
            }
        }

        // запасной путь: Tiny API отдаёт прямой URL
        let url = format!("{TINY_URL}/track/stream?id={track_id}&quality=high");
        let v: Value = self
            .http
            .get(&url)
            .header("X-Auth-Token", &self.token)
            .header("Referer", "https://zvuk.com/")
            .send()
            .await?
            .json()
            .await?;
        v.pointer("/result/stream")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("не удалось получить ссылку на поток для {track_id}"))
    }
}

/// Трек из объекта: Звук то отдаёт `__typename`, то нет — смотрим на форму.
fn track_from(v: &Value) -> Option<Track> {
    let map = v.as_object()?;
    let id = match map.get("id")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let title = map.get("title")?.as_str()?.to_owned();
    // трек отличается от артиста/альбома наличием длительности
    let duration = map.get("duration").and_then(Value::as_i64)?;
    let artists = map
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("title").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    // обложка: сначала картинка релиза, потом картинка исполнителя
    let cover = map
        .get("release")
        .and_then(|r| r.get("image"))
        .and_then(|i| i.get("src"))
        .and_then(Value::as_str)
        .or_else(|| {
            map.get("artists")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|x| x.get("image"))
                .and_then(|i| i.get("src"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_owned();
    // Звук отдаёт шаблон со `{size}` — подставляем размер обложки для панели
    let cover = cover.replace("{size}", "400x400");
    let (album_id, album, year) = map
        .get("release")
        .map(|r| {
            let rid = match r.get("id") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            };
            let title = r.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
            let y = r
                .get("date")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(4)
                .collect::<String>();
            (rid, title, y)
        })
        .unwrap_or_default();
    Some(Track { id, title, artists, duration, cover, album_id, album, year })
}

/// Альбом из объекта поиска.
fn album_from(v: &Value) -> Option<Album> {
    let map = v.as_object()?;
    let id = match map.get("id")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let title = map.get("title")?.as_str()?.to_owned();
    let artists = map
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("title").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let kind = map
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("album")
        .to_lowercase();
    let cover = map
        .get("image")
        .and_then(|i| i.get("src"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("{size}", "400x400");
    let year = map
        .get("date")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(4)
        .collect::<String>();
    Some(Album { id, title, artists, kind, cover, year })
}

/// Трек из короткой записи (сохранённая очередь).
pub fn track_from_brief(v: &Value) -> Option<Track> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let id = s("id");
    if id.is_empty() {
        return None;
    }
    Some(Track {
        id,
        title: s("title"),
        artists: s("artists"),
        duration: v.get("duration").and_then(Value::as_i64).unwrap_or(0),
        cover: s("cover"),
        album_id: s("album_id"),
        album: s("album"),
        year: s("year"),
    })
}

/// Плейлист из объекта поиска.
fn playlist_from(v: &Value) -> Option<Playlist> {
    let map = v.as_object()?;
    // у плейлиста есть длительность в секундах, но нет списка исполнителей —
    // именно по этому признаку и отличаем его от трека
    if map.contains_key("artists") && !map.contains_key("isPublic") {
        return None;
    }
    let id = match map.get("id")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let title = map.get("title")?.as_str()?.to_owned();
    let mut cover = map
        .get("image")
        .and_then(|i| i.get("src"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("{size}", "400x400");
    // обложки плейлистов приходят относительной ссылкой
    if cover.starts_with('/') {
        cover = format!("https://zvuk.com{cover}");
    }
    let tracks = map.get("duration").and_then(Value::as_i64).unwrap_or(0);
    Some(Playlist { id, title, cover, tracks })
}

/// Исполнитель из объекта поиска.
fn artist_from(v: &Value) -> Option<Artist> {
    let map = v.as_object()?;
    // у исполнителя нет длительности — этим он и отличается от трека
    if map.contains_key("duration") {
        return None;
    }
    let id = match map.get("id")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let title = map
        .get("title")
        .or_else(|| map.get("name"))
        .and_then(Value::as_str)?
        .to_owned();
    let cover = map
        .get("image")
        .and_then(|i| i.get("src"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("{size}", "400x400");
    Some(Artist { id, title, cover })
}

fn find_track(v: &Value) -> Option<Track> {
    match v {
        Value::Object(map) => {
            let is_track = map
                .get("__typename")
                .and_then(Value::as_str)
                .map(|t| t == "Track")
                .unwrap_or(false);
            let id = map.get("id").and_then(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            if is_track || id.is_some() {
                if let Some(t) = track_from(v) {
                    return Some(t);
                }
            }
            map.values().find_map(find_track)
        }
        Value::Array(items) => items.iter().find_map(find_track),
        _ => None,
    }
}

/// Собрать все треки из ответа (когда привычных путей в схеме нет).
fn collect_tracks(v: &Value, out: &mut Vec<Track>) {
    match v {
        Value::Object(map) => {
            if let Some(t) = track_from(v) {
                if !out.iter().any(|x| x.id == t.id) {
                    out.push(t);
                }
                return;
            }
            for child in map.values() {
                collect_tracks(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_tracks(child, out);
            }
        }
        _ => {}
    }
}

impl Api {
    /// Скачать картинку (обложку плейлиста) в /tmp/covers и вернуть путь к файлу.
    /// Обложки плейлистов отдаются только с zvuk.com и требуют Referer, поэтому
    /// браузер их напрямую не получает — панель просит картинку у нас.
    pub async fn fetch_cover(&self, url: &str) -> Result<String> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut hasher);
        let name = format!("{:016x}", hasher.finish());
        let dir = std::path::Path::new("/tmp/covers");
        std::fs::create_dir_all(dir).ok();
        let path = dir.join(&name);
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 500 {
            return Ok(path.to_string_lossy().to_string());
        }
        if !url.starts_with("https://zvuk.com/") {
            return Err(anyhow!("обложку берём только с zvuk.com"));
        }
        let resp = self
            .http
            .get(url)
            .header("Referer", "https://zvuk.com/")
            .header("Accept", "image/*")
            .send()
            .await
            .context("обложка: сеть недоступна")?;
        if !resp.status().is_success() {
            return Err(anyhow!("обложка: HTTP {}", resp.status()));
        }
        let bytes = resp.bytes().await.context("обложка: не прочиталась")?;
        if bytes.len() < 500 {
            return Err(anyhow!("обложка: слишком маленький ответ"));
        }
        std::fs::write(&path, &bytes).context("обложка: не записалась")?;
        Ok(path.to_string_lossy().to_string())
    }

    /// Скачать поток в файл: mpg123 на колонке не умеет сеть без curl/wget,
    /// поэтому качаем сами и играем локальный файл.
    pub async fn download(&self, url: &str, path: &std::path::Path) -> Result<u64> {
        let resp = self
            .http
            .get(url)
            .header("Referer", "https://zvuk.com/")
            .send()
            .await
            .context("не удалось запросить поток")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("поток: HTTP {status}"));
        }
        let bytes = resp.bytes().await.context("не удалось прочитать поток")?;
        if bytes.len() < 4096 {
            return Err(anyhow!("поток подозрительно короткий: {} байт", bytes.len()));
        }
        tokio::fs::write(path, &bytes)
            .await
            .with_context(|| format!("не записался файл {}", path.display()))?;
        Ok(bytes.len() as u64)
    }
}

fn charset(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
