//! «Тихая» Алиса: отмена её ответа через локальный IPC (`127.0.0.1:9875`).
//!
//! Зачем: на голосовую фразу реагируют двое — наш слушатель и сама Алиса. Она
//! выполняет своё намерение (включает свою музыку) и занимает ALSA, из-за чего
//! нашему плееру звука не достаётся. Отправка отмены диалога гасит её реакцию.
//!
//! Формат кадра — из `klockky/spotiya` (`src/voice.rs`): 4 байта длины (LE) +
//! protobuf `QuasarMessage`, где поле 252 — вложенное сообщение RPC, поле 253 —
//! тип 210 (команда). Внутри: поле 7 — имя capability, поле N — метод.

use anyhow::{Context, Result};
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

const ALICE: &str = "127.0.0.1:9875";

/// Обёртка сообщения по схеме этой прошивки: `QuasarMessage` с полем **210** = Remoting.
/// (В чужом клиенте, откуда формат брали раньше, были поля 252/253 — здесь они не работают.)
fn quasar(remoting: &[u8]) -> Vec<u8> {
    let mut msg = Vec::new();
    put_bytes(&mut msg, 210, remoting);
    let mut frame = (msg.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&msg);
    frame
}

/// «Нативный» конверт порта 9875: поле 252 — вложенное сообщение, 253 — тип 210 (команда).
fn quasar_native(payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::new();
    put_bytes(&mut msg, 252, payload);
    put_number(&mut msg, 253, 210);
    let mut frame = (msg.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&msg);
    frame
}

/// Имя её внутренней шины: `@<hash>/bus/maind/system` (hash зависит от устройства).
fn bus_name() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/net/unix").ok()?;
    for line in raw.lines() {
        if let Some(idx) = line.find('@') {
            let name = line[idx..].trim();
            if name.contains("/bus/maind/") {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn send_bus(frame: &[u8]) -> Result<()> {
    use std::os::linux::net::SocketAddrExt;
    let name = bus_name().ok_or_else(|| anyhow::anyhow!("не нашёл шину maind в /proc/net/unix"))?;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(
        name.trim_start_matches('@').as_bytes(),
    )
    .context("адрес шины")?;
    let mut sock = std::os::unix::net::UnixStream::connect_addr(&addr)
        .context("подключение к шине maind")?;
    sock.set_write_timeout(Some(Duration::from_millis(500)))?;
    sock.write_all(frame).context("не удалось отправить кадр в шину")?;
    Ok(())
}

/// Попросить её плеер сыграть локальный файл: `FilePlayerCapability`, метод PLAY_SOUND_FILE.
pub fn play_file(path: &str) -> Result<()> {
    let mut method = Vec::new();
    put_number(&mut method, 3, 0); // method = PLAY_SOUND_FILE
    put_number(&mut method, 5, 1); // sendEvents
    put_number(&mut method, 6, 0); // channel = CONTENT_CHANNEL
    put_bytes(&mut method, 25, path.as_bytes()); // filePath

    send(&quasar_native(&method))
}

/// Остановить файл: `FilePlayerCapability`, метод STOP_SOUND_FILE.
pub fn stop_file() -> Result<()> {
    let mut method = Vec::new();
    put_number(&mut method, 3, 1); // STOP_SOUND_FILE
    put_number(&mut method, 6, 0);

    send(&quasar_native(&method))
}

/// Управление её плеером: `PlaybackControlCapability`, поле 1 = метод.
/// 0 = PLAY, 1 = PAUSE, 2 = TOGGLE, 6 = NEXT, 7 = PREV.
pub fn playback(kind: u64) -> Result<()> {
    let mut method = Vec::new();
    put_number(&mut method, 1, kind);
    send(&quasar_native(&method))
}

// ---------------------------------------------------------------------------
// Разбор кадров её шины: 4 байта длины (LE) + protobuf QuasarMessage.
//   внешнее: 252 — вложенное состояние, 253 — тип сообщения (3 = состояние Алисы)
//   внутри:   1 — статус (3 = финальное распознавание), 2 — текст, 7 — id запроса
// ---------------------------------------------------------------------------

enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut out = 0u64;
    let mut shift = 0;
    while *pos < data.len() && shift < 64 {
        let b = data[*pos];
        *pos += 1;
        out |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(out);
        }
        shift += 7;
    }
    None
}

fn fields(data: &[u8]) -> Vec<(u64, Field<'_>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let Some(key) = read_varint(data, &mut pos) else { break };
        let num = key >> 3;
        match key & 7 {
            0 => match read_varint(data, &mut pos) {
                Some(v) => out.push((num, Field::Varint(v))),
                None => break,
            },
            2 => {
                let Some(len) = read_varint(data, &mut pos) else { break };
                let len = len as usize;
                if pos + len > data.len() {
                    break;
                }
                out.push((num, Field::Bytes(&data[pos..pos + len])));
                pos += len;
            }
            1 => pos += 8,
            5 => pos += 4,
            _ => break,
        }
    }
    out
}

/// Финальное распознавание: (id запроса, текст, статус).
fn recognized(data: &[u8]) -> Option<(String, String, u64)> {
    let top = fields(data);
    let mut kind = None;
    let mut state = None;
    for (n, f) in &top {
        match (n, f) {
            (253, Field::Varint(v)) => kind = Some(*v),
            (252, Field::Bytes(b)) => state = Some(*b),
            _ => {}
        }
    }
    // 3 = состояние Алисы; 210 — наши команды, их не разбираем
    if kind != Some(3) {
        return None;
    }
    let inner = fields(state?);
    let mut status = 0u64;
    let mut text = String::new();
    let mut id = String::new();
    for (n, f) in &inner {
        match (n, f) {
            (1, Field::Varint(v)) => status = *v,
            (2, Field::Bytes(b)) => text = String::from_utf8_lossy(b).to_string(),
            (7, Field::Bytes(b)) => id = String::from_utf8_lossy(b).to_string(),
            _ => {}
        }
    }
    if text.is_empty() && id.is_empty() {
        return None;
    }
    Some((id, text, status))
}

/// Слушать её шину и печатать каждый кадр: что услышала Алиса и что ответила.
/// `cancel` — заодно отменять диалог на каждом финальном распознавании.
pub async fn listen(cancel_on_final: bool) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(ALICE)
        .await
        .with_context(|| format!("нет соединения с {ALICE}"))?;
    println!("alice: слушаю {ALICE}");
    loop {
        let size = match sock.read_u32_le().await {
            Ok(v) => v as usize,
            Err(e) => {
                println!("alice: соединение закрыто: {e}");
                return Ok(());
            }
        };
        if size == 0 || size > 512 * 1024 {
            println!("alice: странная длина кадра {size}");
            return Ok(());
        }
        let mut buf = vec![0u8; size];
        sock.read_exact(&mut buf).await?;
        if let Some((id, text, status)) = recognized(&buf) {
            println!("кадр: статус={status} id={id} текст=«{text}»");
            if cancel_on_final && status == 3 {
                let _ = cancel();
                let _ = pause_playback();
                println!("  → отмена и пауза отправлены");
            }
        }
        let _ = sock.flush().await;
    }
}


/// Отменить текущий диалог: `AliceCapability`, метод 8, аргумент `[8, 3]` (kind 3 = cancel).
pub fn cancel() -> Result<()> {
    send(&rpc("AliceCapability", 8, &[8, 3]))
}

/// Поставить её плеер на паузу: на случай, если намерение уже успело запустить музыку.
pub fn pause_playback() -> Result<()> {
    let mut source = Vec::new();
    put_number(&mut source, 1, 14);
    put_number(&mut source, 2, 3);
    put_number(&mut source, 3, 1);
    put_bytes(&mut source, 4, uuid().as_bytes());
    put_bytes(&mut source, 6, b"Zvukd");

    let mut method = vec![8, 1];
    put_bytes(&mut method, 5, &source);
    put_number(&mut method, 7, 0);

    send(&rpc("PlaybackControlCapability", 13, &method))
}

/// «Убрать Алису»: отмена ответа, затем пауза её плеера. Ошибки не критичны —
/// главное, чтобы наш плеер всё равно попробовал занять устройство.
pub fn hush() -> String {
    let mut out: Vec<String> = Vec::new();
    match cancel() {
        Ok(()) => out.push("ответ отменён".into()),
        Err(e) => out.push(format!("отмена не ушла: {e}")),
    }
    std::thread::sleep(Duration::from_millis(120));
    match pause_playback() {
        Ok(()) => out.push("плеер Алисы на паузе".into()),
        Err(e) => out.push(format!("пауза не ушла: {e}")),
    }
    out.join(", ")
}

fn send(frame: &[u8]) -> Result<()> {
    let mut s = TcpStream::connect(ALICE)
        .with_context(|| format!("нет соединения с {ALICE}"))?;
    s.set_write_timeout(Some(Duration::from_millis(500)))?;
    s.write_all(frame).context("не удалось отправить кадр")?;
    let _ = s.shutdown(Shutdown::Write);
    Ok(())
}

fn rpc(capability: &str, field: u64, method: &[u8]) -> Vec<u8> {
    let mut remoting = Vec::new();
    put_bytes(&mut remoting, 7, capability.as_bytes());
    put_bytes(&mut remoting, field, method);

    let mut message = Vec::new();
    put_bytes(&mut message, 252, &remoting);
    put_number(&mut message, 253, 210);

    let mut frame = (message.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}

fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_number(out: &mut Vec<u8>, field: u64, value: u64) {
    put_var(out, field << 3);
    put_var(out, value);
}

fn put_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    put_var(out, (field << 3) | 2);
    put_var(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// Идентификатор запроса: у приложения он случайный, нам хватает времени и pid.
fn uuid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut v = nanos ^ (pid << 64) ^ (pid.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let mut hex = String::new();
    for _ in 0..32 {
        v = v.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let nib = ((v >> 60) & 0xf) as u8;
        hex.push(char::from_digit(nib as u32, 16).unwrap_or('0'));
    }
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_frame_matches_spotiya() {
        // байты из теста klockky/spotiya: rpc("AliceCapability", 8, &[8,3]) без длины
        let expected: [u8; 28] = [
            0xe2, 0x0f, 0x15, 0x3a, 0x0f, b'A', b'l', b'i', b'c', b'e', b'C', b'a', b'p', b'a',
            b'b', b'i', b'l', b'i', b't', b'y', 0x42, 2, 8, 3, 0xe8, 0x0f, 0xd2, 1,
        ];
        let frame = rpc("AliceCapability", 8, &[8, 3]);
        assert_eq!(&frame[4..], &expected);
        assert_eq!(&frame[0..4], &(28u32).to_le_bytes());
    }
}

/// Состояние Алисы из кадра (поле 253 = 3, внутри поле 1):
/// 0 — молчит, 1 — слушает, 2 — говорит, 3 — занята, 5 — распознаёт музыку, 6 — нет.
fn state_of(data: &[u8]) -> Option<u8> {
    let top = fields(data);
    let mut kind = None;
    let mut state = None;
    for (n, f) in &top {
        match (n, f) {
            (253, Field::Varint(v)) => kind = Some(*v),
            (252, Field::Bytes(b)) => state = Some(*b),
            _ => {}
        }
    }
    if kind != Some(3) {
        return None;
    }
    for (n, f) in fields(state?) {
        if n == 1 {
            if let Field::Varint(v) = f {
                return Some(v as u8);
            }
        }
    }
    None
}

/// Следить за состоянием Алисы и отдавать его наружу: плеер уступает ей звук,
/// пока она говорит, и продолжает с того же места, когда закончила.
pub async fn watch_states(tx: tokio::sync::mpsc::Sender<u8>) -> Result<()> {
    use tokio::io::AsyncReadExt;
    loop {
        let mut sock = match tokio::net::TcpStream::connect(ALICE).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        loop {
            let size = match sock.read_u32_le().await {
                Ok(v) => v as usize,
                Err(_) => break,
            };
            if size == 0 || size > 512 * 1024 {
                break;
            }
            let mut buf = vec![0u8; size];
            if sock.read_exact(&mut buf).await.is_err() {
                break;
            }
            if let Some(state) = state_of(&buf) {
                if tx.send(state).await.is_err() {
                    return Ok(());
                }
            }
        }
        // её канал закрылся (перезапуск движка) — переподключаемся
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
