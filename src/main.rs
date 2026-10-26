//! `zvukd` — головной проигрыватель Звука для Яндекс Станции.
//!
//! Без интерфейса: на колонке нет экрана, всё управление — командой, локальным
//! сокетом (для Алисы и панели) и голосом. Два режима запуска:
//!
//! ```text
//! zvukd                     # демон: слушает управляющий сокет
//! zvukd play "radiohead"    # клиент: отправил команду и напечатал ответ
//! zvukd status
//! ```

mod alice;
mod api;
mod glagol;
mod player;
mod source;

use anyhow::{anyhow, Context, Result};
use api::{Api, Track};
use player::Player;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

const DEFAULT_SOCK: &str = "/run/zvukd.sock";
/// Где лежит сохранённая очередь, чтобы после перезапуска можно было продолжить.
const STATE_PATH: &str = "/data/local/etc/zvukd.state.json";
const DEFAULT_TOKEN: &str = "/data/local/etc/zvuk.token";

/// Плейлист, который играем: страницы, оценка размера и что уже сыграли.
struct PlaylistState {
    id: String,
    next_offset: u32,
    shuffle: bool,
    estimate: u32,
    played: std::collections::HashSet<String>,
}

/// Простое псевдослучайное число: для перемешивания хватает, лишних зависимостей не тянем.
fn pseudo_random(bound: u32) -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    if bound == 0 { 0 } else { (x % bound as u64) as u32 }
}

struct Core {
    api: Api,
    player: Player,
    autoplay: bool,
    /// Играем «Мой поток»: когда пачка кончилась — догружаем следующую.
    flow: bool,
    /// Результаты последнего поиска: треки, альбомы, исполнители.
    found: api::Found,
    /// Пауза поставлена из-за её речи, а не пользователем — тогда и снимаем сами.
    flow_ducked: bool,
    /// Идёт уступка звука или возврат: сторож автоперехода в это время молчит,
    /// иначе он видит «плеер не играет» и убивает только что запущенный трек.
    resuming: bool,
    /// Позиция из сохранённого состояния: с неё начнём по команде «продолжить».
    saved_position: u64,
    /// Если играем плейлист — здесь его состояние, чтобы догружать страницы.
    playlist: Option<PlaylistState>,
    /// Когда последний раз возвращали наш файл после её подмены (не чаще раза в 4 с).
    last_reassert: Option<std::time::Instant>,
    /// Мы поставили паузу из-за AirPlay (а не человек) — тогда и снимаем сами.
    airplay_paused: bool,
    /// Прошлое состояние Алисы: нужен переход «говорит → молчит», чтобы вернуть наш трек.
    prev_alice: String,
    /// Момент, когда проверить, что играет станция после её ответа.
    restore_check: Option<std::time::Instant>,
    /// Когда последний раз возвращали трек после её речи (не чаще раза в 30 с).
    last_restore: Option<std::time::Instant>,
    /// Когда последний раз переключали трек: защита от двойного перехода
    /// (автопереход по концу + голосовая команда «дальше» давали два «следующий» подряд).
    last_switch: Option<std::time::Instant>,
}

impl Core {
    /// Найти трек и начать играть. Возвращает человекочитаемый ответ.
    async fn play_query(&mut self, query: &str) -> Result<String> {
        let track = self.api.search_first(query).await?;
        self.flow = false;
        self.player.set_queue(vec![track.clone()]);
        self.play_track(track).await
    }

    /// «Мой поток»: персональная волна, играет без остановки.
    /// Идемпотентно: если поток уже играет, второй раз его с начала не запускаем —
    /// иначе повторные срабатывания слушателя выглядят как «перескочил 4 трека».
    async fn play_flow(&mut self) -> Result<String> {
        if self.flow && self.player.is_alive() {
            let label = self
                .player
                .current
                .as_ref()
                .map(|t| t.label())
                .unwrap_or_default();
            return Ok(format!("Уже играет поток: {label}"));
        }
        let tracks = self.api.personal_wave(wave_count()).await?;
        self.flow = true;
        let first = tracks[0].clone();
        self.player.set_queue(tracks);
        self.play_track(first).await
    }

    async fn play_track(&mut self, track: Track) -> Result<String> {
        let url = self.api.stream_url(&track.id).await?;
        let dir = cache_dir();
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join(format!("{}.mp3", track.id));
        // если трек уже скачан (предзагрузка) — играем сразу, без сети
        let cached = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        let size = if cached > 100_000 {
            cached
        } else {
            clean_cache(&dir, &file);
            self.api.download(&url, &file).await?
        };
        if size > MAX_TRACK_BYTES {
            let _ = std::fs::remove_file(&file);
            return Err(anyhow!(
                "поток {} МБ — больше потолка {} МБ, в памяти не держим",
                size / (1024 * 1024),
                MAX_TRACK_BYTES / (1024 * 1024)
            ));
        }
        let label = track.label();
        self.player.play(track, file.to_str().unwrap_or_default()).await?;
        led("start");
        self.prefetch_next();
        self.save_state();
        Ok(format!("Играет: {label} ({} МБ)", size / (1024 * 1024)))
    }

    async fn advance(&mut self) -> Result<String> {
        match self.player.next_track() {
            Some(t) => self.play_track(t).await,
            None => {
                if self.flow && self.autoplay {
                    // пачка потока кончилась — берём следующую (с двумя повторами:
                    // сеть на колонке бывает нестабильной)
                    let mut last: Option<anyhow::Error> = None;
                    for attempt in 1..=3u8 {
                        match self.api.personal_wave(wave_count()).await {
                            Ok(more) if !more.is_empty() => {
                                let first = more[0].clone();
                                self.player.set_queue(more);
                                return self.play_track(first).await;
                            }
                            Ok(_) => last = Some(anyhow!("поток вернул пустую пачку")),
                            Err(e) => last = Some(e),
                        }
                        eprintln!("zvukd: поток не догрузился (попытка {attempt}): {last:?}");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    self.player.stop().await?;
                    Err(last.unwrap_or_else(|| anyhow!("поток не догрузился")))
                } else if self.playlist.is_some() {
                    match self.next_playlist_page().await {
                        Some(res) => res,
                        None => {
                            self.player.stop().await?;
                            Ok("Плейлист закончился".into())
                        }
                    }
                } else {
                    self.player.stop().await?;
                    Ok("Очередь закончилась".into())
                }
            }
        }
    }

    async fn status(&mut self) -> String {
        let alive = self.player.is_alive();
        let state = if alive {
            if self.player.paused { "pause" } else { "play" }
        } else {
            "idle"
        };
        let label = self
            .player
            .current
            .as_ref()
            .map(|t| t.label())
            .unwrap_or_else(|| "—".into());
        let cover = self
            .player
            .current
            .as_ref()
            .map(|t| t.cover.clone())
            .unwrap_or_default();
        let pid = self.player.pid().map(|p| p.to_string()).unwrap_or_else(|| "—".into());
        let pos = if alive { self.player.position() } else { 0 };
        let dur = self.player.current.as_ref().map(|t| t.duration).unwrap_or(0);
        let album = self
            .player
            .current
            .as_ref()
            .map(|t| t.album.clone())
            .unwrap_or_default();
        let album_id = self
            .player
            .current
            .as_ref()
            .map(|t| t.album_id.clone())
            .unwrap_or_default();
        let saved = if !alive && !self.player.queue.is_empty() { 1 } else { 0 };
        // хвост — для панели: обложка, номер трека, длина очереди, позиция,
        // длительность, альбом (id и название) и признак «можно продолжить»
        format!(
            "{state}|{label}|{pid}|{cover}|{}|{}|{pos}|{dur}|{album_id}|{album}|{saved}",
            self.player.idx,
            self.player.queue.len()
        )
    }

    /// Поиск для панели: треки, альбомы и исполнители — как в самом Звуке.
    async fn search(&mut self, query: &str) -> Result<String> {
        let found = self.api.search_all(query, 15).await?;
        let out = found.brief(query);
        self.found = found;
        Ok(out.to_string())
    }

    /// Играть выбранный трек: очередь собирается из последнего поиска, начиная с него.
    async fn play_id(&mut self, id: &str) -> Result<String> {
        // 1) трек из последнего поиска — очередь собираем из найденных
        if let Some(idx) = self.found.tracks.iter().position(|t| t.id == id) {
            let queue = self.found.tracks.clone();
            let track = queue[idx].clone();
            self.flow = false;
            self.player.set_queue(queue);
            self.player.idx = idx;
            return self.play_track(track).await;
        }
        // 2) трек уже в очереди — просто переходим на него.
        // Режим потока при этом НЕ выключаем: иначе после клика по треку
        // в очереди пачка из 10 заканчивалась и поток останавливался.
        if let Some(idx) = self.player.queue.iter().position(|t| t.id == id) {
            let track = self.player.queue[idx].clone();
            self.player.idx = idx;
            return self.play_track(track).await;
        }
        // 3) id пришёл со стороны (кнопка в очереди, прямая ссылка) — берём трек из Звука
        let track = self.api.track(id).await?;
        self.flow = false;
        self.player.set_queue(vec![track.clone()]);
        self.player.idx = 0;
        self.play_track(track).await
    }

    /// Играть альбом целиком: все его треки встают в очередь.
    async fn play_album(&mut self, id: &str) -> Result<String> {
        let tracks = self.api.release_tracks(id).await?;
        self.start_list(tracks, "альбом").await
    }

    /// Играть исполнителя: его популярные треки встают в очередь.
    async fn play_artist(&mut self, id: &str) -> Result<String> {
        let tracks = self.api.artist_tracks(id).await?;
        self.start_list(tracks, "исполнитель").await
    }

    /// Начать плейлист: первая страница, дальше догружаем по мере надобности.
    async fn start_playlist(&mut self, id: &str, shuffle: bool) -> Result<String> {
        let estimate = self.api.playlist_estimate(id).await;
        let mut offset = 0;
        if shuffle && estimate > 1 {
            offset = pseudo_random(estimate.saturating_sub(1));
        }
        let tracks = self.api.playlist_page(id, offset, 100).await?;
        if tracks.is_empty() {
            let tracks = self.api.playlist_page(id, 0, 100).await?;
            if tracks.is_empty() {
                return Err(anyhow!("плейлист {id} пустой"));
            }
            return self.begin_playlist(id, tracks, 100, shuffle, estimate).await;
        }
        self.begin_playlist(id, tracks, offset + 100, shuffle, estimate).await
    }

    async fn begin_playlist(
        &mut self,
        id: &str,
        tracks: Vec<Track>,
        next_offset: u32,
        shuffle: bool,
        estimate: u32,
    ) -> Result<String> {
        let mut played = std::collections::HashSet::new();
        for t in &tracks {
            played.insert(t.id.clone());
        }
        let n = tracks.len();
        self.playlist = Some(PlaylistState {
            id: id.to_string(),
            next_offset,
            shuffle,
            estimate,
            played,
        });
        let first = tracks[0].clone();
        self.flow = false;
        self.player.set_queue(tracks);
        let msg = self.play_track(first).await?;
        Ok(format!(
            "{msg} · плейлист: загружено {n}{}",
            if shuffle { ", вперемешку" } else { "" }
        ))
    }

    /// Следующая страница плейлиста, когда очередь кончилась.
    async fn next_playlist_page(&mut self) -> Option<Result<String>> {
        let mut pl = self.playlist.take()?;
        for _ in 0..3 {
            let offset = if pl.shuffle && pl.estimate > 1 {
                pseudo_random(pl.estimate.saturating_sub(1))
            } else {
                pl.next_offset
            };
            let page = match self.api.playlist_page(&pl.id, offset, 100).await {
                Ok(p) => p,
                Err(e) => {
                    self.playlist = Some(pl);
                    return Some(Err(e));
                }
            };
            let fresh: Vec<Track> = page
                .into_iter()
                .filter(|t| !pl.played.contains(&t.id))
                .collect();
            if fresh.is_empty() {
                if pl.shuffle {
                    continue; // случайная страница оказалась уже сыгранной — пробуем ещё
                }
                break;
            }
            for t in &fresh {
                pl.played.insert(t.id.clone());
            }
            if !pl.shuffle {
                pl.next_offset = offset + 100;
            }
            self.playlist = Some(pl);
            let first = fresh[0].clone();
            let n = fresh.len();
            self.player.set_queue(fresh);
            return Some(self.play_track(first).await.map(|m| format!("{m} · плейлист: +{n}")));
        }
        self.playlist = Some(pl);
        None
    }

    /// Общий запуск готового списка: очередь + первый трек.
    async fn start_list(&mut self, tracks: Vec<Track>, what: &str) -> Result<String> {
        let first = tracks[0].clone();
        let n = tracks.len();
        self.flow = false;
        self.playlist = None;
        self.player.set_queue(tracks);
        let msg = self.play_track(first).await?;
        Ok(format!("{msg} · {what}: {n} трек(ов) в очереди"))
    }

    /// Скачать следующий трек заранее: переключение станет мгновенным.
    fn prefetch_next(&self) {
        let Some(next) = self.player.queue.get(self.player.idx + 1).cloned() else {
            return;
        };
        let dir = cache_dir();
        let next_file = dir.join(format!("{}.mp3", next.id));
        if std::fs::metadata(&next_file).map(|m| m.len()).unwrap_or(0) > 100_000 {
            return;
        }
        let keep_now = self.player.current_file();
        let api = self.api.clone();
        tokio::spawn(async move {
            let Ok(url) = api.stream_url(&next.id).await else { return };
            if api.download(&url, &next_file).await.is_err() {
                let _ = std::fs::remove_file(&next_file);
                return;
            }
            // в памяти держим только текущий и следующий треки
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let path = e.path();
                    let keep = path == next_file
                        || keep_now
                            .as_ref()
                            .map(|f| path == std::path::Path::new(f))
                            .unwrap_or(false);
                    if !keep {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        });
    }

    /// Сохранить очередь и позицию: после перезапуска предложим продолжить.
    fn save_state(&mut self) {
        let state = serde_json::json!({
            "queue": self.player.queue.iter().map(|t| t.brief()).collect::<Vec<_>>(),
            "idx": self.player.idx,
            "position": self.player.position(),
            "playing": self.player.is_alive(),
            "flow": self.flow,
            "at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        if let Some(dir) = std::path::Path::new(STATE_PATH).parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let _ = std::fs::write(STATE_PATH, state.to_string());
    }

    /// Перемешать очередь после текущего трека (текущий не трогаем).
    fn shuffle(&mut self) -> String {
        let start = self.player.idx + 1;
        if start >= self.player.queue.len() {
            return "Дальше нечего перемешивать".into();
        }
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9);
        let tail = &mut self.player.queue[start..];
        for i in (1..tail.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let j = (seed % (i as u64 + 1)) as usize;
            tail.swap(i, j);
        }
        let n = tail.len();
        if let Some(pl) = self.playlist.as_mut() {
            pl.shuffle = true;
        }
        self.save_state();
        format!("Перемешал {n} трек(ов) в очереди")
    }

    /// Продолжить с сохранённого места: очередь уже восстановлена при старте.
    async fn resume_last(&mut self) -> Result<String> {
        if self.player.queue.is_empty() {
            return Err(anyhow!("нечего продолжать — сохранённой очереди нет"));
        }
        let idx = self.player.idx.min(self.player.queue.len() - 1);
        let track = self.player.queue[idx].clone();
        let url = self.api.stream_url(&track.id).await?;
        let dir = cache_dir();
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join(format!("{}.mp3", track.id));
        if std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) < 100_000 {
            clean_cache(&dir, &file);
            self.api.download(&url, &file).await?;
        }
        let frames = (self.saved_position as f64 * 38.28125) as u64;
        self.player.idx = idx;
        self.player.play_from(track, file.to_str().unwrap_or_default(), frames).await?;
        led("start");
        let label = self
            .player
            .current
            .as_ref()
            .map(|t| t.label())
            .unwrap_or_default();
        self.prefetch_next();
        Ok(format!("Продолжаю: {label} с {} с", self.saved_position))
    }

    /// Что стоит в очереди: номер текущего и список.
    fn queue_json(&self) -> String {
        let items: Vec<_> = self.player.queue.iter().map(|t| t.brief()).collect();
        serde_json::json!({
            "index": self.player.idx,
            "current": self.player.current.as_ref().map(|t| t.id.clone()).unwrap_or_default(),
            "items": items,
        })
        .to_string()
    }

    async fn handle(&mut self, line: &str) -> String {
        let line = line.trim();
        let (cmd, rest) = match line.split_once(' ') {
            Some((c, r)) => (c, r.trim()),
            None => (line, ""),
        };
        let res: Result<String> = match cmd {
            "" | "ping" => Ok("pong".into()),
            "play" if !rest.is_empty() => self.play_query(rest).await,
            "play" => Err(anyhow!("нужен запрос: play <что искать>")),
            "flow" | "wave" => self.play_flow().await,
            "search" if !rest.is_empty() => self.search(rest).await,
            "search" => Err(anyhow!("нужен запрос: search <что искать>")),
            "queue" => Ok(self.queue_json()),
            "shuffle" => Ok(self.shuffle()),
            // проверка догрузки страниц плейлиста: ставим курсор в конец очереди
            "queue-tail" => {
                let n = self.player.queue.len();
                if n == 0 {
                    Err(anyhow!("очередь пуста"))
                } else {
                    self.player.idx = n - 1;
                    Ok(format!("курсор на последнем треке ({n})"))
                }
            }
            "resume-last" => self.resume_last().await,
            "cover" if !rest.is_empty() => match self.api.fetch_cover(rest).await {
                Ok(path) => Ok(path),
                Err(e) => Err(e),
            },
            "cover" => Err(anyhow!("нужен адрес: cover <url>")),
            "album-tracks" if !rest.is_empty() => match self.api.release_tracks(rest).await {
                Ok(tracks) => Ok(serde_json::json!({
                    "items": tracks.iter().map(|t| t.brief()).collect::<Vec<_>>()
                })
                .to_string()),
                Err(e) => Err(e),
            },
            "album-tracks" => Err(anyhow!("нужен id: album-tracks <id альбома>")),
            "play-playlist" if !rest.is_empty() => self.start_playlist(rest, false).await,
            "play-playlist-shuffled" if !rest.is_empty() => self.start_playlist(rest, true).await,
            "play-playlist" => Err(anyhow!("нужен id: play-playlist <id плейлиста>")),
            // проверка уступки звука без Алисы: пауза на 3 с и продолжение
            "duck-test" => {
                if !self.player.is_alive() {
                    Err(anyhow!("не играет — нечего уступать"))
                } else {
                    let ok = self.player.duck().await;
                    let frame = self.player.skip();
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    match self.player.unduck().await {
                        Ok(()) => Ok(format!("уступил (кадр {frame}) — продолжил: {ok}")),
                        Err(e) => Err(e),
                    }
                }
            }
            "play-id" if !rest.is_empty() => self.play_id(rest).await,
            "play-id" => Err(anyhow!("нужен id: play-id <id из поиска>")),
            "play-album" if !rest.is_empty() => self.play_album(rest).await,
            "play-album" => Err(anyhow!("нужен id: play-album <id альбома>")),
            "artist-albums" if !rest.is_empty() => match self.api.artist_albums(rest).await {
                Ok(albums) => Ok(serde_json::json!({
                    "items": albums.iter().map(|a| a.brief()).collect::<Vec<_>>()
                })
                .to_string()),
                Err(e) => Err(e),
            },
            "artist-albums" => Err(anyhow!("нужен id: artist-albums <id исполнителя>")),
            "artist-tracks" if !rest.is_empty() => match self.api.artist_tracks(rest).await {
                Ok(tracks) => Ok(serde_json::json!({
                    "items": tracks.iter().map(|t| t.brief()).collect::<Vec<_>>()
                })
                .to_string()),
                Err(e) => Err(e),
            },
            "artist-tracks" => Err(anyhow!("нужен id: artist-tracks <id исполнителя>")),
            "play-artist" if !rest.is_empty() => self.play_artist(rest).await,
            "play-artist" => Err(anyhow!("нужен id: play-artist <id исполнителя>")),
            "pause" => {
                let r = self.player.pause().map(|_| "Пауза".to_string()).map_err(Into::into);
                if r.is_ok() {
                    led("stop");
                }
                r
            }
            "resume" => {
                let r = self.player.resume().map(|_| "Продолжаю".to_string()).map_err(Into::into);
                if r.is_ok() {
                    led("start");
                }
                r
            }
            "stop" => {
                self.flow = false;
                let res = self.player.stop().await.map(|_| "Остановлено".to_string());
                led("stop");
                if res.is_ok() {
                    let cache = cache_dir();
                    clean_cache(&cache, std::path::Path::new("/nonexistent"));
                }
                res
            }
            // переход по очереди без замены самой очереди (клик по треку в панели)
            "jump" if !rest.is_empty() => {
                let len = self.player.queue.len();
                let parsed: Result<usize, _> = rest.trim().parse();
                let n = match parsed {
                    Ok(v) if v >= 1 && v <= len => v,
                    Ok(_) => return format!("ERR в очереди {len} треков"),
                    Err(_) => return "ERR нужен номер: jump <номер>".into(),
                };
                {
                    self.player.idx = n - 1;
                    self.last_switch = Some(std::time::Instant::now());
                    let t = self.player.queue[n - 1].clone();
                    self.play_track(t).await
                }
            }
            "jump" => Err(anyhow!("нужен номер: jump <номер>")),
            // убрать трек из очереди
            // добавить в конец очереди (трек / весь альбом / популярные треки исполнителя)
            "queue-add" if !rest.is_empty() => match self.api.track(rest).await {
                Ok(t) => {
                    let label = t.label();
                    self.player.queue.push(t);
                    self.save_state();
                    Ok(format!("в очередь: {label} (теперь {})", self.player.queue.len()))
                }
                Err(e) => Err(e),
            },
            "queue-add" => Err(anyhow!("нужен id: queue-add <id трека>")),
            "queue-add-album" if !rest.is_empty() => match self.api.release_tracks(rest).await {
                Ok(tracks) => {
                    let n = tracks.len();
                    for t in tracks {
                        self.player.queue.push(t);
                    }
                    self.save_state();
                    Ok(format!("в очередь {n} трек(ов) альбома (теперь {})", self.player.queue.len()))
                }
                Err(e) => Err(e),
            },
            "queue-add-album" => Err(anyhow!("нужен id: queue-add-album <id альбома>")),
            "queue-add-artist" if !rest.is_empty() => match self.api.artist_tracks(rest).await {
                Ok(tracks) => {
                    let n = tracks.len();
                    for t in tracks {
                        self.player.queue.push(t);
                    }
                    self.save_state();
                    Ok(format!("в очередь {n} популярных трек(ов) (теперь {})", self.player.queue.len()))
                }
                Err(e) => Err(e),
            },
            "queue-add-artist" => Err(anyhow!("нужен id: queue-add-artist <id исполнителя>")),
            // очистить очередь: текущий трек оставляем, чтобы музыка не оборвалась на полуслове
            "queue-clear" => {
                // иначе после очистки «поток»/плейлист тут же догрузит новую пачку
                self.flow = false;
                self.playlist = None;
                let current = self.player.current.clone();
                let mut q: Vec<crate::api::Track> = Vec::new();
                if let Some(t) = current {
                    q.push(t);
                }
                let keep = q.len();
                self.player.set_queue(q);
                self.save_state();
                if keep > 0 {
                    Ok("очередь очищена, играет текущий трек".into())
                } else {
                    Ok("очередь очищена".into())
                }
            }
            "queue-del" if !rest.is_empty() => {
                let len = self.player.queue.len();
                let parsed: Result<usize, _> = rest.trim().parse();
                let n = match parsed {
                    Ok(v) if v >= 1 && v <= len => v,
                    Ok(_) => return format!("ERR в очереди {len} треков"),
                    Err(_) => return "ERR нужен номер: queue-del <номер>".into(),
                };
                {
                    self.player.queue.remove(n - 1);
                    if self.player.idx >= n && self.player.idx > 0 {
                        self.player.idx -= 1;
                    }
                    self.save_state();
                    Ok(format!("убрал; в очереди {}", self.player.queue.len()))
                }
            }
            "queue-del" => Err(anyhow!("нужен номер: queue-del <номер>")),
            "next" => {
                self.last_switch = Some(std::time::Instant::now());
                self.advance().await
            }
            // Вернуть наш файл на место после того, как её волна успела влезть:
            // переигрываем ТОТ ЖЕ трек с текущей секунды (не следующий).
            // Безусловная переподстановка: нужна после голосовых команд, когда её поток
            // мог влезть и определить это по её полям не удаётся.
            "replay-now" => match self.player.reassert().await {
                Ok(()) => Ok("Трек переподставлен".into()),
                Err(e) => Ok(format!("Не вышло: {e}")),
            },
            "reassert" => {
                // ничего не делаем, если трек и так наш: лишняя переигровка даёт слышимую заминку
                if !self.player.station_took_over() {
                    Ok("уже наш трек".into())
                } else {
                    match self.player.reassert().await {
                        Ok(()) => Ok("Наш трек на месте".into()),
                        Err(e) => Ok(format!("Не вышло: {e}")),
                    }
                }
            }
            "prev" => match self.player.prev_track() {
                Some(t) => self.play_track(t).await,
                None => Ok("Это первый трек".into()),
            },
            // проверка без звука: нашёлся ли трек и дали ли ссылку на поток
            "find" if !rest.is_empty() => {
                match self.api.search_first(rest).await {
                    Ok(t) => match self.api.stream_url(&t.id).await {
                        Ok(u) => Ok(format!("Есть: {} | id={} | поток: {}", t.label(), t.id, &u[..u.len().min(48)])),
                        Err(e) => Ok(format!("Трек найден ({}), но поток не получен: {e}", t.label())),
                    },
                    Err(e) => Err(e),
                }
            }
            // полная ссылка на поток (для проверки декодером)
            "url" if !rest.is_empty() => match self.api.search_first(rest).await {
                Ok(t) => self.api.stream_url(&t.id).await,
                Err(e) => Err(e),
            },
            "status" => Ok(self.status().await),
            "autoplay" => {
                self.autoplay = rest != "off";
                Ok(format!("Автопереход: {}", if self.autoplay { "вкл" } else { "выкл" }))
            }
            other => Err(anyhow!("неизвестная команда: {other}")),
        };
        match res {
            Ok(s) => s,
            Err(e) => format!("ERR {e}"),
        }
    }
}

/// Прочитать сохранённую очередь: треки, номер текущего и позицию в секундах.
fn load_state() -> Option<(Vec<Track>, usize, u64, bool)> {
    let raw = std::fs::read_to_string(STATE_PATH).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let items = v.get("queue")?.as_array()?;
    let queue: Vec<Track> = items.iter().filter_map(api::track_from_brief).collect();
    if queue.is_empty() {
        return None;
    }
    let idx = v.get("idx").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    let position = v.get("position").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let flow = v.get("flow").and_then(serde_json::Value::as_bool).unwrap_or(false);
    Some((queue, idx, position, flow))
}

/// Подсветка кольца: мягкое зелёное дыхание, пока играет музыка.
/// Скрипт и кадры уже лежат на колонке; ошибки не критичны — просто не светится.
fn led(action: &str) {
    let script = "/data/local/bin/led-lamp.sh";
    if !std::path::Path::new(script).exists() {
        eprintln!("zvukd: свет — нет {script}");
        return;
    }
    let mut cmd = std::process::Command::new(script);
    cmd.arg(action);
    if action == "start" {
        cmd.arg("green");
    }
    match cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => eprintln!("zvukd: свет {action}"),
        Err(e) => eprintln!("zvukd: свет не запустился: {e}"),
    }
}

fn sock_path() -> String {
    std::env::var("ZVUKD_SOCK").unwrap_or_else(|_| DEFAULT_SOCK.to_string())
}

/// Куда качать трек. По умолчанию — `/tmp`, это оперативная память: трек нужен
/// только на время воспроизведения, а флеш на колонке одна на всё (199 МБ, и у неё
/// ограниченное число перезаписей). Переопределяется `ZVUKD_CACHE`.
const DEFAULT_CACHE: &str = "/tmp/zvukd";

/// Потолок размера файла: в памяти держим только то, что реально помещается
/// (tmpfs — четверть ОЗУ, около 245 МБ). Больше — значит качаем не тот поток.
const MAX_TRACK_BYTES: u64 = 80 * 1024 * 1024;

fn cache_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("ZVUKD_CACHE").unwrap_or_else(|_| DEFAULT_CACHE.to_string()),
    )
}

/// Убрать из каталога всё, кроме нужного файла: на колонке 199 МБ на `/data`,
/// и накопленные треки однажды заполняют раздел целиком.
fn clean_cache(dir: &std::path::Path, keep: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path != *keep && path.is_file() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Сколько треков брать из потока за один заход (переопределяется ZVUK_WAVE_COUNT).
fn wave_count() -> u32 {
    std::env::var("ZVUK_WAVE_COUNT")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(10)
}

fn read_token() -> Result<String> {
    let path = std::env::var("ZVUK_TOKEN_FILE").unwrap_or_else(|_| DEFAULT_TOKEN.to_string());
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("нет файла с токеном {path} (см. docs/install.md)"))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        return Err(anyhow!("файл {path} пустой"));
    }
    Ok(token)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // «тихая» Алиса — локальная команда, к демону не ходим: важно успеть до её ответа
    if args.len() == 1 && (args[0] == "hush" || args[0] == "тихо") {
        println!("{}", alice::hush());
        return Ok(());
    }
    // диагностика: печатать смену её состояний (молчит / слушает / говорит)
    if args.len() == 1 && (args[0] == "states" || args[0] == "состояния") {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(32);
        tokio::spawn(async move {
            let _ = alice::watch_states(tx).await;
        });
        while let Some(state) = rx.recv().await {
            let name = match state {
                0 => "молчит",
                1 => "слушает",
                2 => "говорит",
                3 => "занята",
                5 => "распознаёт музыку",
                _ => "иное",
            };
            println!("состояние {state} ({name})");
        }
        return Ok(());
    }
    // разбор её шины: печатаем кадры (и, если попросят, отменяем диалог)
    if !args.is_empty() && (args[0] == "listen" || args[0] == "слушать") {
        let cancel = args.iter().any(|a| a == "--cancel");
        return alice::listen(cancel).await;
    }
    // просьба её плееру сыграть локальный файл (проверка канала директив)
    if args.len() == 2 && args[0] == "play-file" {
        match alice::play_file(&args[1]) {
            Ok(()) => println!("отправлено: играть {}", args[1]),
            Err(e) => println!("ERR {e}"),
        }
        return Ok(());
    }
    if args.len() == 1 && args[0].starts_with("alice-") {
        let kind = match args[0].as_str() {
            "alice-pause" => 1,
            "alice-next" => 6,
            "alice-prev" => 7,
            "alice-play" => 0,
            _ => 2,
        };
        match alice::playback(kind) {
            Ok(()) => println!("отправлено: {}", args[0]),
            Err(e) => println!("ERR {e}"),
        }
        return Ok(());
    }
    if args.len() == 1 && args[0] == "stop-file" {
        match alice::stop_file() {
            Ok(()) => println!("отправлено: стоп файла"),
            Err(e) => println!("ERR {e}"),
        }
        return Ok(());
    }
    if !args.is_empty() {
        return run_client(args).await;
    }
    run_daemon().await
}

/// Клиент: одна команда — один ответ. Для SSH, скриптов и `alice-voice.sh`.
async fn run_client(args: Vec<String>) -> Result<()> {
    let line = args.join(" ");
    let path = sock_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("демон не запущен? нет сокета {path}"))?;
    stream.write_all(format!("{line}\n").as_bytes()).await?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).await?;
    println!("{}", reply.trim_end());
    Ok(())
}

/// Один демон на устройство: второй экземпляр уводил бы сокет и играл свой трек.
fn kill_other_daemons() {
    let me = std::process::id();
    let Ok(dir) = std::fs::read_dir("/proc") else { return };
    for entry in dir.flatten() {
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if pid == me {
            continue;
        }
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
            .unwrap_or_default();
        // демон запускается без аргументов; клиент всегда с командой
        if cmd.trim() == "/data/local/opt/zvuk/bin/zvukd" {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .status();
        }
    }
}

async fn run_daemon() -> Result<()> {
    // второй экземпляр демона и его «дети» — источник двух треков одновременно
    kill_other_daemons();
    player::kill_strays();
    // после аварийной остановки в памяти мог остаться прошлый трек
    let cache = cache_dir();
    std::fs::create_dir_all(&cache).ok();
    clean_cache(&cache, std::path::Path::new("/nonexistent"));
    let token = read_token()?;
    let path = sock_path();
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("не удалось занять сокет {path}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    let mut initial = Core {
        api: Api::new(token)?,
        player: Player::new(),
        autoplay: true,
        flow: false,
        found: api::Found::default(),
        flow_ducked: false,
        resuming: false,
        saved_position: 0,
        playlist: None,
        last_reassert: None,
        prev_alice: String::new(),
        restore_check: None,
        last_restore: None,
        airplay_paused: false,
        last_switch: None,
    };
    // восстановить сохранённую очередь (но не начинать играть без просьбы)
    if let Some((queue, idx, position, flow)) = load_state() {
        let n = queue.len();
        initial.player.set_queue(queue);
        initial.player.idx = idx.min(n.saturating_sub(1));
        initial.saved_position = position;
        initial.flow = flow;
        println!(
            "zvukd: восстановлена очередь ({n} трек(ов), позиция {position} с, поток: {flow})"
        );
    }
    let core = Arc::new(Mutex::new(initial));
    println!("zvukd: слушаю {path}");

    // Её голос важнее нашей музыки: пока она говорит, трек на паузе,
    // после — продолжается с того же места (как делают клиенты Spotify для колонки).
    let duck_core = core.clone();
    tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(16);
        tokio::spawn(async move {
            let _ = alice::watch_states(tx).await;
        });
        let mut ducked = false;
        while let Some(state) = rx.recv().await {
            let mut c = duck_core.lock().await;
            eprintln!("zvukd: её состояние {state}");
            match state {
                2 => {
                    // она заговорила — освобождаем устройство, иначе её не слышно
                    c.resuming = true;
                    if !c.flow_ducked && c.player.is_alive() && !c.player.paused {
                        if c.player.duck().await {
                            led("stop");
                            c.flow_ducked = true;
                            ducked = true;
                            eprintln!("zvukd: уступил звук, кадр {}", c.player.skip());
                        } else {
                            eprintln!("zvukd: уступать нечего — не играем");
                        }
                    }
                    c.resuming = false;
                }
                0 => {
                    // Возвращаем музыку ТОЛЬКО когда она закончила. На состоянии 1
                    // (проснулась и слушает) наши stop/audio_play сбивают её диалог:
                    // она активируется и тут же гаснет.
                    if ducked {
                        c.resuming = true;
                        match c.player.unduck().await {
                            Ok(()) => {
                                led("start");
                                eprintln!("zvukd: продолжил с кадра {}", c.player.skip());
                            }
                            Err(e) => eprintln!("zvukd: продолжить не вышло: {e}"),
                        }
                    }
                    c.flow_ducked = false;
                    ducked = false;
                    c.resuming = false;
                    eprintln!("zvukd: играет: {}", c.player.is_alive());
                }
                1 => {
                    // она слушает — не трогаем звук и не шлём команд
                    eprintln!("zvukd: она слушает — молчим");
                }
                _ => {}
            }
        }
    });

    let tick_core = core.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(700));
        loop {
            iv.tick().await;
            let mut c = tick_core.lock().await;
            // трек доиграл сам — переходим к следующему, если есть
            // Автоматический возврат выключен осознанно (24.10.2026): любая попытка
            // «угадать» подмену давала либо её волну, либо бесконечные перезапуски.
            // Порядок команд решается в слушателе: он выжидает 3 с, пока её облако
            // ответит и запустит своё, и только потом ставит наш трек — последний выигрывает.
            // AirPlay и наш плеер делят одно устройство: пока сессия жива,
            // мы уступаем (пауза), а после её конца продолжаем сами.
            let ap = crate::player::airplay_active();
            if ap && !c.airplay_paused && !c.player.paused && c.player.current.is_some() {
                if c.player.pause().is_ok() {
                    c.airplay_paused = true;
                    c.last_switch = None;
                    eprintln!("zvukd: AirPlay занял звук — пауза");
                }
            } else if !ap && c.airplay_paused {
                if c.player.resume().is_ok() {
                    c.airplay_paused = false;
                    eprintln!("zvukd: AirPlay освободил звук — продолжаю");
                }
            }

            // После ответа Алисы станция часто возвращает не наш поток, а свою музыку.
            // Раньше мы «угадывали» подмену по названию и длительности и получали хаос;
            // теперь смотрим в mediad.log на URI, который станция реально открыла,
            // и возвращаем трек ОДИН раз, с паузой 30 с между попытками.
            let st = c.player.alice_state();
            if c.prev_alice == "2" && st != "2" {
                c.restore_check =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
            }
            c.prev_alice = st;
            if let Some(t) = c.restore_check {
                if std::time::Instant::now() >= t {
                    c.restore_check = None;
                    let cooldown_ok = c
                        .last_restore
                        .map(|t| t.elapsed().as_secs() >= 30)
                        .unwrap_or(true);
                    if cooldown_ok
                        && !ap
                        && !c.player.paused
                        && c.player.current.is_some()
                        && !crate::player::Player::alice_music_request_recent()
                        && c.player.station_took_over()
                    {
                        c.last_restore = Some(std::time::Instant::now());
                        c.last_switch = Some(std::time::Instant::now());
                        match c.player.reassert().await {
                            Ok(_) => eprintln!("zvukd: после ответа Алисы вернул наш трек"),
                            Err(e) => eprintln!("zvukd: вернуть трек не вышло: {e}"),
                        }
                    }
                }
            }

            let switch_ready = c
                .last_switch
                .map(|t| t.elapsed().as_secs() >= 3)
                .unwrap_or(true);
            if switch_ready
                && c.autoplay
                && !c.resuming
                && !c.flow_ducked
                && !c.player.paused
                && c.player.current.is_some()
                && c.player.is_finished()
            {
                c.last_switch = Some(std::time::Instant::now());
                let _ = c.advance().await;
            }
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let core = core.clone();
        tokio::spawn(async move {
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let reply = {
                    let mut c = core.lock().await;
                    c.handle(&line).await
                };
                if wr.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }
}
