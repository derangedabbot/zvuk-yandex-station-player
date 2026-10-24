//! Проигрывание: `mpg123` декодирует в поток, `aplay` из прошивки отдаёт его в ALSA.
//!
//! Почему не «просто mpg123»: собранный на колонке mpg123 — модульный, драйвер вывода
//! лежит отдельным файлом, которого в наборе нет («failure loading driver module»).
//! Поэтому декодируем в raw PCM на stdout и отдаём в штатный `aplay`: он работает с
//! системной ALSA, ничего доустанавливать не нужно.
//!
//! ALSA-устройство держит `aplay` — по этому признаку `audio-owner.sh` понимает,
//! что звук занят (см. `docs/integration.md`).

use crate::api::Track;
use crate::glagol::Glagol;
use crate::source::SourceServer;
use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::process::{Child, Command};

const ERR_FILE: &str = "/tmp/zvukd-player.err";

pub struct Player {
    decoder: Option<Child>,
    output: Option<Child>,
    pub current: Option<Track>,
    pub queue: Vec<Track>,
    pub idx: usize,
    pub paused: bool,
    /// Файл текущего трека: нужен, чтобы возобновить с места после её реплики.
    file: Option<String>,
    /// С какого кадра начинать (mpg123 -k): докуда доиграли до уступки звука.
    skip_frames: u64,
    /// Когда началось воспроизведение — от этого считается позиция.
    started_at: Option<std::time::Instant>,
    /// Режим «играет штатный плеер станции»: свой раздатчик файлов + Glagol.
    station: Option<Station>,
    /// Живость и позиция из состояния станции (обновляет фоновый сторож).
    live: Arc<AtomicBool>,
    progress_ms: Arc<AtomicU64>,
    /// Когда начался текущий трек (мс эпохи) — запасной источник позиции,
    /// если станция прогресс не отдаёт.
    start_epoch: Arc<AtomicU64>,
    /// Позиция на момент паузы (0 — паузы нет). Пока стоит, счётчик не двигается:
    /// иначе за время паузы позиция «убегает» и продолжение прыгает вперёд.
    frozen_ms: Arc<AtomicU64>,
    /// Что станция считает играющим (её поле title). Нужно, чтобы заметить подмену:
    /// Алиса на «продолжи» включает свой поток, и он затирает наш файл.
    station_title: Arc<std::sync::Mutex<String>>,
    /// Длительность трека, которую показывает станция: по ней видно подмену надёжнее,
    /// чем по названию (у её потока название часто пустое, а длительность другая).
    station_duration: Arc<std::sync::Mutex<f64>>,
}

/// Штатный плеер станции: свой HTTP-раздатчик и канал управления.
struct Station {
    gl: Glagol,
    src: SourceServer,
    host: String,
}

/// Сколько кадров MP3 в секунде при 44,1 кГц (кадр = 1152 сэмпла).
const FRAMES_PER_SEC: f64 = 44100.0 / 1152.0;

impl Player {
    pub fn new() -> Self {
        Self {
            decoder: None,
            output: None,
            current: None,
            queue: Vec::new(),
            idx: 0,
            paused: false,
            file: None,
            skip_frames: 0,
            started_at: None,
            station: None,
            live: Arc::new(AtomicBool::new(false)),
            progress_ms: Arc::new(AtomicU64::new(0)),
            start_epoch: Arc::new(AtomicU64::new(0)),
            frozen_ms: Arc::new(AtomicU64::new(0)),
            station_title: Arc::new(std::sync::Mutex::new(String::new())),
            station_duration: Arc::new(std::sync::Mutex::new(0.0)),
        }
    }

    /// Режим звука: `station` (по умолчанию) — играет штатный плеер станции,
    /// `alsa` — старый путь через mpg123+aplay (оставлен для отката).
    fn station_mode() -> bool {
        !env_or("ZVUK_MODE", "station").eq_ignore_ascii_case("alsa")
    }

    /// Играет ли ещё: живы оба — декодер и вывод.
    pub fn is_alive(&mut self) -> bool {
        if self.station.is_some() {
            // Станция скачивает файл за считанные секунды, поэтому «растут ли
            // отданные байты» про живость не говорит — смотрим её же позицию.
            let dur = self.duration_ms();
            let prog = self.progress_ms.load(Ordering::Relaxed);
            return self.current.is_some() && (dur == 0 || prog + 3000 < dur);
        }
        alive(&mut self.decoder) && alive(&mut self.output)
    }

    /// Станция играет не наш файл, а что-то своё (её поток после «продолжи»).
    /// Признак: она называет трек, и это не название нашего.
    pub fn station_took_over(&self) -> bool {
        if self.station.is_none() || self.paused || self.current.is_none() {
            return false;
        }
        // главный признак: её трек другой длительности (у потока название бывает пустым)
        let ours_s = (self.duration_ms() as f64) / 1000.0;
        let theirs_s = self.station_duration.lock().map(|d| *d).unwrap_or(0.0);
        if ours_s > 1.0 && theirs_s > 1.0 && (theirs_s - ours_s).abs() > 2.0 {
            return true;
        }
        let theirs = self
            .station_title
            .lock()
            .map(|g| g.trim().to_lowercase())
            .unwrap_or_default();
        let ours = self
            .current
            .as_ref()
            .map(|t| t.title.trim().to_lowercase())
            .unwrap_or_default();
        if theirs.is_empty() || ours.is_empty() {
            return false;
        }
        !theirs.contains(&ours)
    }

    /// Вернуть наш файл на место после её подмены.
    pub async fn reassert(&mut self) -> Result<()> {
        if let Ok(mut g) = self.station_title.lock() {
            g.clear();
        }
        self.replay_from_position().await
    }

    /// Длительность текущего трека в миллисекундах (0 — неизвестна).
    fn duration_ms(&self) -> u64 {
        self.current
            .as_ref()
            .map(|t| (t.duration.max(0) as u64) * 1000)
            .unwrap_or(0)
    }

    /// Трек доиграл до конца — можно переходить к следующему.
    pub fn is_finished(&mut self) -> bool {
        if self.station.is_some() {
            let dur = self.duration_ms();
            let prog = self.progress_ms.load(Ordering::Relaxed);
            return dur > 0 && prog + 1500 >= dur && !self.paused;
        }
        self.current.is_some() && !self.is_alive()
    }

    /// pid процесса, который держит звук (`aplay`) — его показывает `status`.
    pub fn pid(&mut self) -> Option<u32> {
        if self.station.is_some() {
            return None; // играет штатный плеер станции, своих процессов нет
        }
        self.output.as_ref().and_then(|c| c.id())
    }

    pub fn set_queue(&mut self, queue: Vec<Track>) {
        self.queue = queue;
        self.idx = 0;
    }

    pub fn next_track(&mut self) -> Option<Track> {
        if self.idx + 1 >= self.queue.len() {
            return None;
        }
        self.idx += 1;
        self.queue.get(self.idx).cloned()
    }

    pub fn prev_track(&mut self) -> Option<Track> {
        if self.idx == 0 {
            return None;
        }
        self.idx -= 1;
        self.queue.get(self.idx).cloned()
    }

    /// Играть локальный файл: `mpg123 -s` (raw PCM) → `aplay`.
    /// До шести попыток: пока Алиса говорит или играет, устройство занято —
    /// ждём до ~10 секунд, потом сдаёмся.
    pub async fn play(&mut self, track: Track, file: &str) -> Result<()> {
        self.play_from(track, file, 0).await
    }

    /// Играть файл с указанного кадра — так продолжается трек после её реплики.
    pub async fn play_from(&mut self, track: Track, file: &str, skip_frames: u64) -> Result<()> {
        if Player::station_mode() {
            return self.play_station(track, file).await;
        }
        self.file = Some(file.to_string());
        self.skip_frames = skip_frames;
        self.started_at = Some(std::time::Instant::now());
        let mut last: Option<anyhow::Error> = None;
        for attempt in 1..=6u8 {
            match self.try_play(track.clone(), file).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    if attempt < 6 {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("не удалось запустить воспроизведение")))
    }

    /// Играть штатным плеером станции: свой HTTP-раздатчик + `audio_play` через Glagol.
    async fn play_station(&mut self, track: Track, file: &str) -> Result<()> {
        self.ensure_station(file).await?;
        let (url, gl) = {
            let st = self.station.as_ref().expect("станция подключена");
            let name = std::path::Path::new(file)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let url = st.src.url(&st.host, &name, crate::glagol::now_ms());
            (url, st.gl.clone())
        };
        // спецификация: stop → пауза ~150 мс → audio_play с новым URL
        gl.stop().await.ok();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        gl.audio_play(&url, &track.title, &track.artists, 0).await?;

        self.file = Some(file.to_string());
        self.current = Some(track);
        self.paused = false;
        self.started_at = Some(std::time::Instant::now());
        self.live.store(true, Ordering::Relaxed);
        self.progress_ms.store(0, Ordering::Relaxed);
        self.start_epoch.store(crate::glagol::now_ms(), Ordering::Relaxed);
        Ok(())
    }

    /// Поднять Glagol и раздатчик (один раз за жизнь процесса).
    async fn ensure_station(&mut self, file: &str) -> Result<()> {
        if self.station.is_some() {
            return Ok(());
        }
        let host = env_or("ZVUK_HOST", "127.0.0.1");
        let device = env_or("ZVUK_DEVICE_ID", "DEVICE_ID");
        let platform = env_or("ZVUK_PLATFORM", "yandexmidi");
        let port: u16 = env_or("ZVUK_SRC_PORT", "8765").parse().unwrap_or(8765);
        let dir = std::path::Path::new(file)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp/zvukd"));

        let gl = Glagol::connect(host.clone(), device, platform).await?;
        let src = SourceServer::start(dir, port).await?;

        // Сторож живости. Опираться на состояние станции нельзя: в ответах оно отстаёт
        // на одну команду и «playing» приходит ложным. Поэтому главный признак —
        // станция реально тянет байты из нашего раздатчика.
        let watcher = gl.clone();
        let srcw = src.clone();
        let live = self.live.clone();
        let prog = self.progress_ms.clone();
        let start_epoch = self.start_epoch.clone();
        let frozen = self.frozen_ms.clone();
        let stitle = self.station_title.clone();
        let sdur = self.station_duration.clone();
        tokio::spawn(async move {
            let mut last_served = 0u64;
            let mut silent_ticks = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                let served = srcw.served();
                if served > last_served {
                    last_served = served;
                    silent_ticks = 0;
                    live.store(true, Ordering::Relaxed);
                } else if live.load(Ordering::Relaxed) {
                    silent_ticks += 1;
                    if silent_ticks >= 8 {
                        live.store(false, Ordering::Relaxed);
                    }
                }
                let st = watcher.state().await;
                // учитываем название только когда она не говорит и реально играет:
                // иначе её реплика/TTS выглядит как «подмена трека»
                if st.playing && (st.alice_state.is_empty() || st.alice_state == "0") {
                    if let Ok(mut g) = stitle.lock() {
                        *g = st.title.clone();
                    }
                    if let Ok(mut d) = sdur.lock() {
                        *d = st.duration;
                    }
                }
                if st.playing || st.paused {
                    live.store(true, Ordering::Relaxed);
                }
                let started = start_epoch.load(Ordering::Relaxed);
                let elapsed = if started > 0 {
                    (crate::glagol::now_ms().saturating_sub(started)) as f64 / 1000.0
                } else {
                    0.0
                };
                if frozen.load(Ordering::Relaxed) > 0 {
                    continue; // пауза: позиция заморожена на месте
                }
                let pos = if st.progress > 0.5 { st.progress } else { elapsed };
                prog.store((pos.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
            }
        });

        self.station = Some(Station { gl, src, host });
        Ok(())
    }

    /// Переиграть текущий файл с позиции (продолжение после паузы/реплики Алисы).
    async fn replay_from_position(&mut self) -> Result<()> {
        let (Some(track), Some(file)) = (self.current.clone(), self.file.clone()) else {
            return Ok(());
        };
        let frozen = self.frozen_ms.swap(0, Ordering::Relaxed);
        let offset_ms = if frozen > 0 { frozen } else { self.progress_ms.load(Ordering::Relaxed) };
        self.progress_ms.store(offset_ms, Ordering::Relaxed);
        self.start_epoch.store(
            crate::glagol::now_ms().saturating_sub(offset_ms),
            Ordering::Relaxed,
        );
        let (url, gl) = {
            let st = self.station.as_ref().ok_or_else(|| anyhow!("станция не подключена"))?;
            let name = std::path::Path::new(&file)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            (st.src.url(&st.host, &name, crate::glagol::now_ms()), st.gl.clone())
        };
        gl.stop().await.ok();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        gl.audio_play(&url, &track.title, &track.artists, offset_ms).await?;
        self.live.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn try_play(&mut self, track: Track, file: &str) -> Result<()> {
        let _ = self.stop().await;
        // осиротевшие процессы прежних запусков иначе играют ВТОРЫМ треком
        kill_strays();
        // звук у Алисы (её ответ или её музыка) — забираем: движок поднимется пустым
        take_device().await;

        let dir = std::path::Path::new("/tmp/zvukd");
        std::fs::create_dir_all(dir).ok();
        let err = std::fs::File::create(ERR_FILE).ok();
        let err2 = err.as_ref().and_then(|f| f.try_clone().ok());

        let bin = env_or("ZVUK_MPG123", "/data/local/opt/airplay/bin/mpg123");
        let libs = env_or(
            "ZVUK_LD_LIBRARY_PATH",
            "/data/local/opt/airplay/lib:/data/local/opt/tools/lib",
        );
        // у бинарников набора прописан /lib/ld-musl-aarch64.so.1, которого на колонке нет
        let loader = env_or("ZVUK_LOADER", "/data/local/opt/tools/lib/ld-musl-aarch64.so.1");
        let aplay = env_or("ZVUK_APLAY", "/usr/bin/aplay");
        let rate = env_or("ZVUK_RATE", "44100");

        let mut dec = Command::new(&loader)
            .env("LD_LIBRARY_PATH", &libs)
            .arg("--library-path")
            .arg(&libs)
            .arg(&bin)
            .args(["-q", "-s", "--rate", &rate, "-e", "s16"])
            .args(if self.skip_frames > 0 {
                vec!["-k".to_string(), self.skip_frames.to_string()]
            } else {
                Vec::new()
            })
            .arg(file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(match err {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            })
            .kill_on_drop(false)
            .spawn()
            .with_context(|| format!("не удалось запустить {bin}"))?;

        let pcm = dec
            .stdout
            .take()
            .ok_or_else(|| anyhow!("не удалось забрать поток из {bin}"))?;
        let pcm_fd = pcm.into_owned_fd().context("не удалось забрать дескриптор потока")?;

        let out = Command::new(&aplay)
            .args(["-q", "-f", "S16_LE", "-r", &rate, "-c", "2", "-"])
            .stdin(Stdio::from(pcm_fd))
            .stdout(Stdio::null())
            .stderr(match err2 {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            })
            .kill_on_drop(false)
            .spawn();

        let out = match out {
            Ok(o) => o,
            Err(e) => {
                let _ = dec.start_kill();
                return Err(anyhow!("не удалось запустить {aplay}: {e}"));
            }
        };

        self.decoder = Some(dec);
        self.output = Some(out);

        // если кто-то из двоих умер сразу — не врём «играет»
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        if !self.is_alive() {
            let why = last_err();
            self.stop().await.ok();
            return Err(anyhow!("звук не пошёл: {why}"));
        }

        self.current = Some(track);
        self.paused = false;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(st) = &self.station {
            st.gl.stop().await.ok();
            self.live.store(false, Ordering::Relaxed);
            self.current = None;
            self.paused = false;
            return Ok(());
        }
        // сначала вывод: иначе декодер висит на записи в мёртвую трубу
        kill_wait(&mut self.output).await;
        kill_wait(&mut self.decoder).await;
        self.current = None;
        self.paused = false;
        Ok(())
    }

    /// Позиция в секундах: пропуск при возврате плюс время с начала воспроизведения.
    pub fn position(&self) -> u64 {
        if self.station.is_some() {
            return self.progress_ms.load(Ordering::Relaxed) / 1000;
        }
        let base = (self.skip_frames as f64) / FRAMES_PER_SEC;
        let played = self.started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        (base + played) as u64
    }

    /// Файл, который играет сейчас (нужен предзагрузке, чтобы его не удалили).
    pub fn current_file(&self) -> Option<String> {
        self.file.clone()
    }

    /// Докуда доиграли (в кадрах) — для логов и проверок.
    pub fn skip(&self) -> u64 {
        self.skip_frames
    }

    /// Уступить звук Алисе: закрыть устройство (иначе она не сможет говорить),
    /// запомнив, докуда доиграли.
    pub async fn duck(&mut self) -> bool {
        if self.station.is_some() {
            // штатный плеер сам уступает Алисе: ставим его на паузу и не рвём файл.
            // ВАЖНО: отдельная команда `pause` им игнорируется — пауза едет вместе с потоком
            // (`set_pause: true`), иначе музыка продолжает играть поверх её речи.
            if !self.is_alive() {
                return false;
            }
            let cur = self.progress_ms.load(Ordering::Relaxed);
            let file = self.file.clone();
            let title = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
            let artist = self.current.as_ref().map(|t| t.artists.clone()).unwrap_or_default();
            if let (Some(st), Some(file)) = (self.station.as_ref(), file) {
                let name = std::path::Path::new(&file)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !name.is_empty() {
                    let url = st.src.url(&st.host, &name, crate::glagol::now_ms());
                    st.gl.try_pause_stream(&url, &title, &artist, cur);
                }
            }
            self.frozen_ms.store(cur.max(1), Ordering::Relaxed);
            self.paused = true;
            return true;
        }
        if !self.is_alive() {
            return false;
        }
        let played = self.started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        self.skip_frames += (played * FRAMES_PER_SEC) as u64;
        self.paused = false;
        let _ = kill_wait(&mut self.output).await;
        let _ = kill_wait(&mut self.decoder).await;
        true
    }

    /// Вернуть звук: продолжить тот же трек с того же места.
    pub async fn unduck(&mut self) -> Result<()> {
        if self.station.is_some() {
            // ВАЖНО: `play` у станции возобновляет ЕЁ прошлую сущность (например, «32 радио»),
            // а не наш файл. Поэтому продолжаем повторным audio_play с текущей позиции.
            self.replay_from_position().await?;
            self.paused = false;
            return Ok(());
        }
        let (Some(track), Some(file)) = (self.current.clone(), self.file.clone()) else {
            return Ok(());
        };
        let skip = self.skip_frames;
        self.play_from(track, &file, skip).await
    }

    /// Пауза через SIGSTOP: оба процесса замирают, позиция не теряется.
    pub fn pause(&mut self) -> Result<()> {
        if self.station.is_some() {
            let cur = self.progress_ms.load(Ordering::Relaxed);
            let file = self.file.clone();
            let title = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
            let artist = self.current.as_ref().map(|t| t.artists.clone()).unwrap_or_default();
            if let (Some(st), Some(file)) = (self.station.as_ref(), file) {
                let name = std::path::Path::new(&file)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !name.is_empty() {
                    // отдельная команда `pause` её плеером игнорируется — пауза приходит
                    // вместе с потоком: `audio_play` + `set_pause: true`
                    let url = st.src.url(&st.host, &name, crate::glagol::now_ms());
                    st.gl.try_pause_stream(&url, &title, &artist, cur);
                }
            }
            // фиксируем секунду паузы: счётчик после resume пойдёт ровно отсюда
            self.frozen_ms.store(cur.max(1), Ordering::Relaxed);
            self.paused = true;
            return Ok(());
        }
        let pid = self.pid().ok_or_else(|| anyhow!("не играет"))?;
        signal(pid, "STOP")?;
        if let Some(d) = self.decoder.as_ref().and_then(|c| c.id()) {
            let _ = signal(d, "STOP");
        }
        self.paused = true;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        if self.station.is_some() && (self.current.is_none() || self.file.is_none()) {
            // после полной остановки продолжать нечего — иначе возьмётся чужой трек
            return Err(anyhow!("нечего продолжать: трек остановлен"));
        }
        if let Some(st) = &self.station {
            // см. unduck: `play` вернёт радио станции, поэтому переигрываем свой файл с позиции
            let gl = st.gl.clone();
            let src = st.src.clone();
            let host = st.host.clone();
            let file = self.file.clone().unwrap_or_default();
            let title = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
            let artist = self.current.as_ref().map(|t| t.artists.clone()).unwrap_or_default();
            let frozen = self.frozen_ms.swap(0, Ordering::Relaxed);
            let offset_ms = if frozen > 0 { frozen } else { self.progress_ms.load(Ordering::Relaxed) };
            self.progress_ms.store(offset_ms, Ordering::Relaxed);
            self.start_epoch.store(
                crate::glagol::now_ms().saturating_sub(offset_ms),
                Ordering::Relaxed,
            );
            tokio::spawn(async move {
                let name = std::path::Path::new(&file)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                gl.stop().await.ok();
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                let url = src.url(&host, &name, crate::glagol::now_ms());
                if let Err(e) = gl.audio_play(&url, &title, &artist, offset_ms).await {
                    eprintln!("zvukd: продолжить не вышло: {e}");
                }
            });
            self.paused = false;
            return Ok(());
        }
        let pid = self.pid().ok_or_else(|| anyhow!("не играет"))?;
        signal(pid, "CONT")?;
        if let Some(d) = self.decoder.as_ref().and_then(|c| c.id()) {
            let _ = signal(d, "CONT");
        }
        self.paused = false;
        Ok(())
    }
}

fn alive(child: &mut Option<Child>) -> bool {
    match child.as_mut() {
        Some(c) => matches!(c.try_wait(), Ok(None)),
        None => false,
    }
}

/// Кто держит звуковое устройство: ищем открытый `pcmC0D0p` по всем процессам.
fn pcm_owner() -> Option<u32> {
    let dir = std::fs::read_dir("/proc").ok()?;
    for entry in dir.flatten() {
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let fds = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                if target.to_string_lossy().contains("pcmC0D0p") {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Погасить осиротевшие процессы вывода: после перезапуска демона его `aplay`/`mpg123`
/// живут дальше, и новый запуск даёт ДВА трека одновременно (проверено на слух).
/// Вызывать только когда своих процессов ещё нет или они уже остановлены.
pub fn kill_strays() {
    let me = std::process::id();
    let parent = std::fs::read_to_string(format!("/proc/{me}/stat"))
        .ok()
        .and_then(|raw| raw.rsplit(')').next().map(str::to_string))
        .and_then(|tail| tail.split_whitespace().nth(1).and_then(|v| v.parse::<u32>().ok()));
    let Ok(dir) = std::fs::read_dir("/proc") else { return };
    for entry in dir.flatten() {
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if pid == me || Some(pid) == parent {
            continue;
        }
        let c = cmdline(pid);
        if c.contains("/usr/bin/aplay") || c.contains("airplay/bin/mpg123") {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .status();
        }
    }
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// Если устройство держит Алиса — гасим её движок. Он поднимается сам и пустым
/// (проверено: после перезапуска музыка не продолжается), поэтому её ответ и её
/// музыка обрываются, а освободившийся звук достаётся нам.
async fn take_device() {
    if let Some(pid) = pcm_owner() {
        if cmdline(pid).contains("maind") {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
            for _ in 0..30 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                match pcm_owner() {
                    Some(p) if cmdline(p).contains("maind") => continue,
                    _ => break,
                }
            }
        }
    }
}

async fn kill_wait(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.start_kill();
        let _ = c.wait().await;
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Последняя строка из журнала ошибок — она попадает в ответ сокета.
fn last_err() -> String {
    let raw = std::fs::read_to_string(ERR_FILE).unwrap_or_default();
    let line = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .unwrap_or("без сообщения")
        .to_string();
    line.chars().take(200).collect()
}

/// Сигналы через `kill` из busybox — не тянем лишний крейт ради двух вызовов.
fn signal(pid: u32, sig: &str) -> Result<()> {
    let status = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()
        .context("kill недоступен")?;
    if !status.success() {
        return Err(anyhow!("kill -{sig} {pid} вернул {status}"));
    }
    Ok(())
}

