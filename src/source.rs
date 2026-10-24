//! Локальный раздатчик файлов для штатного плеера станции.
//!
//! Станция сначала делает `HEAD` (узнаёт `Content-Length`), потом `GET` — и рвёт соединение,
//! если длина не честная (chunked и «бесконечные» потоки не годятся). Здесь отдаём ровно то,
//! что просили: точную длину. Range поддерживаем: без него станция не может начать
//! трек с середины (продолжение после паузы) и молча откатывается на 0-ю секунду.
//!
//! Слушаем на всех интерфейсах: станция качает по своему же LAN-адресу.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
pub struct SourceServer {
    dir: Arc<PathBuf>,
    port: u16,
    /// Сколько байт уже отдали: по приросту видно, что станция реально качает трек
    /// (её собственное состояние в ответах отстаёт, на него нельзя опираться).
    served: Arc<std::sync::atomic::AtomicU64>,
}

impl SourceServer {
    /// Поднять сервер (один раз) и вернуть его настройки.
    pub async fn start(dir: impl AsRef<Path>, port: u16) -> Result<Self> {
        let dir = Arc::new(dir.as_ref().to_path_buf());
        std::fs::create_dir_all(&*dir).ok();
        let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let listener = TcpListener::bind(("0.0.0.0", port))
            .await
            .with_context(|| format!("порт {port} занят"))?;
        let d = dir.clone();
        let counter = served.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _peer)) = listener.accept().await else { continue };
                let d = d.clone();
                let served = counter.clone();
                tokio::spawn(async move {
                    let _ = handle(sock, d, served).await;
                });
            }
        });
        Ok(Self { dir, port, served })
    }

    /// Сколько байт отдано с начала работы.
    pub fn served(&self) -> u64 {
        self.served.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Ссылка, по которой станция заберёт файл. `tag` делает URL уникальным
    /// (станция не любит повторное использование одного и того же адреса).
    pub fn url(&self, host_ip: &str, file_name: &str, tag: impl std::fmt::Display) -> String {
        format!("http://{host_ip}:{}/{}?t={}", self.port, file_name, tag)
    }
}

async fn handle(mut sock: TcpStream, dir: Arc<PathBuf>, served: Arc<std::sync::atomic::AtomicU64>) -> Result<()> {
    let mut buf = vec![0u8; 4096];
    let n = sock.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    // `Range: bytes=<start>[-<end>]` — станция просит так, когда продолжаем с позиции
    let range_start: Option<u64> = req
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .and_then(|l| l.split('=').nth(1))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0);
    let mut lines = req.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    // путь без query (?t=…&vsid=… — станция дописывает своё)
    let path = target.split('?').next().unwrap_or("/").trim_start_matches('/');
    let name = if path.is_empty() { "index" } else { path };
    // защита от выхода из каталога
    if name.contains("..") || name.contains('/') {
        respond(&mut sock, "400 Bad Request", "text/plain", b"bad path", false).await?;
        return Ok(());
    }
    let full = dir.join(name);
    match std::fs::metadata(&full) {
        Ok(meta) if meta.is_file() => {
            let len = meta.len();
            if method.eq_ignore_ascii_case("HEAD") {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                );
                sock.write_all(head.as_bytes()).await?;
                return Ok(());
            }
            let mut f = tokio::fs::File::open(&full).await?;
            let start = range_start.unwrap_or(0).min(len);
            let head = if start > 0 {
                use tokio::io::AsyncSeekExt;
                f.seek(std::io::SeekFrom::Start(start)).await?;
                let rem = len - start;
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mpeg\r\nContent-Length: {rem}\r\nContent-Range: bytes {start}-{last}/{len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    last = len - 1
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                )
            };
            sock.write_all(head.as_bytes()).await?;
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = f.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                if sock.write_all(&chunk[..read]).await.is_err() {
                    break; // станция закрыла соединение — это нормально, если трек прервали
                }
                served.fetch_add(read as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        _ => {
            respond(&mut sock, "404 Not Found", "text/plain", b"no file", true).await?;
        }
    }
    let _ = sock.shutdown().await;
    Ok(())
}

async fn respond(
    sock: &mut TcpStream,
    status: &str,
    ctype: &str,
    body: &[u8],
    with_len: bool,
) -> Result<()> {
    let head = if with_len {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
    } else {
        format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n")
    };
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    Ok(())
}
