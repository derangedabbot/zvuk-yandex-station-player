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
use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
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
        }
    }

    /// Играет ли ещё: живы оба — декодер и вывод.
    pub fn is_alive(&mut self) -> bool {
        alive(&mut self.decoder) && alive(&mut self.output)
    }

    /// pid процесса, который держит звук (`aplay`) — его показывает `status`.
    pub fn pid(&mut self) -> Option<u32> {
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
        // сначала вывод: иначе декодер висит на записи в мёртвую трубу
        kill_wait(&mut self.output).await;
        kill_wait(&mut self.decoder).await;
        self.current = None;
        self.paused = false;
        Ok(())
    }

    /// Позиция в секундах: пропуск при возврате плюс время с начала воспроизведения.
    pub fn position(&self) -> u64 {
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
        let (Some(track), Some(file)) = (self.current.clone(), self.file.clone()) else {
            return Ok(());
        };
        let skip = self.skip_frames;
        self.play_from(track, &file, skip).await
    }

    /// Пауза через SIGSTOP: оба процесса замирают, позиция не теряется.
    pub fn pause(&mut self) -> Result<()> {
        let pid = self.pid().ok_or_else(|| anyhow!("не играет"))?;
        signal(pid, "STOP")?;
        if let Some(d) = self.decoder.as_ref().and_then(|c| c.id()) {
            let _ = signal(d, "STOP");
        }
        self.paused = true;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
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

