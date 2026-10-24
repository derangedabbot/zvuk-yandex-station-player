//! Glagol: локальное управление штатным плеером станции.
//!
//! Зачем: вместо `mpg123`+`aplay` (и драки за ALSA) отдаём станции ссылку на наш файл
//! командой `audio_play` — играет её родной плеер, Алиса не глохнет, громкость и пауза штатные.
//!
//! Схема (проверено на `.50` 24.10.2026):
//!   OAuth-токен аккаунта → `quasar.yandex.ru/glagol/token?device_id=…&platform=…`
//!   → короткий Glagol-токен (~30 с) → `wss://127.0.0.1:1961` → JSON-команды.
//!
//! `radio_play` здесь НЕ используется: на этой прошивке он включает радио Яндекса и ссылку игнорирует.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

const TOKEN_URL: &str = "https://quasar.yandex.ru/glagol/token";
const OAUTH_FILE: &str = "/data/local/etc/zvuk.oauth";
const ACCOUNT_STORAGE: &str = "/data/quasar/account_storage.dat";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36";

/// Состояние плеера, как его видит сама станция.
#[derive(Debug, Clone, Default)]
pub struct PlayerState {
    pub connected: bool,
    pub playing: bool,
    pub paused: bool,
    pub duration: f64,
    pub progress: f64,
    pub title: String,
    pub alice_state: String,
}

#[derive(Debug)]
enum Cmd {
    Send(Value),
    Stop,
    Pause,
    Resume,
    AudioPlay { url: String, title: String, subtitle: String, offset_ms: u64 },
    Volume(f32),
}

#[derive(Clone)]
pub struct Glagol {
    tx: mpsc::Sender<Cmd>,
    state: Arc<Mutex<PlayerState>>,
}

impl Glagol {
    /// Подключиться к станции. Токен аккаунта ищем в файле или в `account_storage.dat`.
    pub async fn connect(host: String, device_id: String, platform: String) -> Result<Self> {
        let oauth = oauth_token()?;
        let (tx, rx) = mpsc::channel::<Cmd>(32);
        let state = Arc::new(Mutex::new(PlayerState::default()));
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = actor(host, device_id, platform, oauth, rx, st).await {
                eprintln!("glagol: {e:#}");
            }
        });
        Ok(Self { tx, state })
    }

    pub async fn state(&self) -> PlayerState {
        self.state.lock().await.clone()
    }

    async fn send_cmd(&self, c: Cmd) -> Result<()> {
        self.tx.send(c).await.map_err(|_| anyhow!("glagol: канал закрыт"))
    }

    pub async fn audio_play(&self, url: &str, title: &str, subtitle: &str, offset_ms: u64) -> Result<()> {
        self.send_cmd(Cmd::AudioPlay {
            url: url.to_string(),
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            offset_ms,
        })
        .await
    }

    pub async fn stop(&self) -> Result<()> {
        self.send_cmd(Cmd::Stop).await
    }

    pub async fn pause(&self) -> Result<()> {
        self.send_cmd(Cmd::Pause).await
    }

    pub async fn resume(&self) -> Result<()> {
        self.send_cmd(Cmd::Resume).await
    }

    pub async fn set_volume(&self, v: f32) -> Result<()> {
        self.send_cmd(Cmd::Volume(v.clamp(0.0, 1.0))).await
    }

    // синхронные варианты: нужны там, где API плеера не асинхронный (пауза, стоп)
    pub fn try_stop(&self) {
        let _ = self.tx.try_send(Cmd::Stop);
    }

    pub fn try_pause(&self) {
        let _ = self.tx.try_send(Cmd::Pause);
    }

    pub fn try_resume(&self) {
        let _ = self.tx.try_send(Cmd::Resume);
    }
}

// ---------------------------------------------------------------- актор

async fn actor(
    host: String,
    device_id: String,
    platform: String,
    oauth: String,
    mut rx: mpsc::Receiver<Cmd>,
    state: Arc<Mutex<PlayerState>>,
) -> Result<()> {
    let url = format!("wss://{host}:1961");
    let mut attempt: u32 = 0;

    loop {
        let sock = match connect(&url).await {
            Ok(s) => {
                attempt = 0;
                state.lock().await.connected = true;
                s
            }
            Err(e) => {
                state.lock().await.connected = false;
                attempt += 1;
                let wait = Duration::from_secs(2u64.pow(attempt.min(4)));
                eprintln!("glagol: подключение к {url} не удалось ({e}), повтор через {wait:?}");
                tokio::time::sleep(wait).await;
                continue;
            }
        };

        let (mut sink, mut stream) = sock.split();
        let mut token: Option<(String, Instant)> = None;

        loop {
            tokio::select! {
                incoming = stream.next() => {
                    match incoming {
                        Some(Ok(Message::Ping(p))) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Text(t))) => {
                            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                apply_state(&state, &v).await;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => {
                            eprintln!("glagol: обрыв ({e})");
                            break;
                        }
                        _ => {}
                    }
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return Ok(()) };
                    // токен обновляем не чаще раза в 25 секунд
                    let tok = match &token {
                        Some((t, at)) if at.elapsed() < Duration::from_secs(25) => t.clone(),
                        _ => match fetch_token(&oauth, &device_id, &platform).await {
                            Ok(t) => {
                                token = Some((t.clone(), Instant::now()));
                                t
                            }
                            Err(e) => {
                                eprintln!("glagol: токен не получен: {e}");
                                continue;
                            }
                        },
                    };
                    let payload = match cmd {
                        Cmd::Stop => json!({"command": "stop"}),
                        Cmd::Pause => json!({"command": "pause"}),
                        Cmd::Resume => json!({"command": "play"}),
                        Cmd::Volume(v) => json!({"command": "setVolume", "volume": v}),
                        Cmd::AudioPlay { url, title, subtitle, offset_ms } => json!({
                            "command": "externalCommandBypass",
                            "data": audio_play_data(&url, &title, &subtitle, offset_ms),
                        }),
                        Cmd::Send(p) => p,
                    };
                    let msg = json!({
                        "conversationToken": tok,
                        "id": next_id(),
                        "sentTime": now_ms(),
                        "payload": payload,
                    });
                    if sink.send(Message::Text(msg.to_string())).await.is_err() {
                        break;
                    }
                }
            }
        }

        state.lock().await.connected = false;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Ответ станции: `state.playerState` и прочее. Состояние в ответе отстаёт на одну
/// команду — это нормально, не пугаться.
async fn apply_state(shared: &Arc<Mutex<PlayerState>>, v: &Value) {
    let st = v.get("state").unwrap_or(v);
    let Some(obj) = st.as_object() else { return };
    let mut guard = shared.lock().await;
    if let Some(a) = obj.get("aliceState").and_then(Value::as_str) {
        guard.alice_state = a.to_string();
    }
    if let Some(p) = obj.get("playing").and_then(Value::as_bool) {
        guard.playing = p;
    }
    if let Some(ps) = obj.get("playerState").and_then(Value::as_object) {
        if let Some(d) = ps.get("duration").and_then(Value::as_f64) {
            guard.duration = d;
        }
        if let Some(p) = ps.get("progress").and_then(Value::as_f64) {
            guard.progress = p;
        }
        if let Some(t) = ps.get("title").and_then(Value::as_str) {
            guard.title = t.to_string();
        }
        if let Some(hp) = ps.get("hasPause").and_then(Value::as_bool) {
            // hasPause == false и playing == true → пауза (так это выглядит у станции)
            guard.paused = !hp;
        }
    }
}

// ---------------------------------------------------------------- TLS без проверки серта

#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn connect(url: &str) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>> {
    let provider = rustls::crypto::ring::default_provider();
    let provider = Arc::new(provider);
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
        .with_no_client_auth();
    let connector = tokio_tungstenite::Connector::Rustls(Arc::new(cfg));
    let (ws, _resp) = tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector))
        .await
        .with_context(|| format!("WebSocket {url}"))?;
    Ok(ws)
}

// ---------------------------------------------------------------- токены

fn oauth_token() -> Result<String> {
    if let Ok(p) = std::env::var("ZVUK_OAUTH_FILE") {
        if let Ok(t) = std::fs::read_to_string(&p) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }
    if let Ok(t) = std::env::var("ZVUK_OAUTH") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    if let Ok(t) = std::fs::read_to_string(OAUTH_FILE) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    // запасной путь: вытащить y0_ токен из хранилища аккаунта станции
    let raw = std::fs::read(ACCOUNT_STORAGE).context("нет OAuth-токена и нет account_storage.dat")?;
    let text = String::from_utf8_lossy(&raw);
    for part in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':')) {
        if part.starts_with("y0_") && part.len() > 40 {
            return Ok(part.to_string());
        }
    }
    Err(anyhow!("не нашёл OAuth-токен (y0_…) в {ACCOUNT_STORAGE}"))
}

async fn fetch_token(oauth: &str, device_id: &str, platform: &str) -> Result<String> {
    let client = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(12)).build()?;
    let resp = client
        .get(TOKEN_URL)
        .query(&[("device_id", device_id), ("platform", platform)])
        .header("Authorization", format!("OAuth {oauth}"))
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!("токен: HTTP {status}: {}", body));
    }
    body.get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("в ответе нет token: {body}"))
}

// ---------------------------------------------------------------- утилиты

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", now_ms())
}

/// protobuf: `{1: "audio_play", 2: JSON{stream:{url,format,type,offset_ms},set_pause:false}}` → base64
fn audio_play_data(url: &str, title: &str, subtitle: &str, offset_ms: u64) -> String {
    let mut stream = json!({
        "url": url,
        "format": "MP3",          // прошивка требует именно MP3, кодек определяет сама (FLAC тоже играет)
        "type": "Track",
        "offset_ms": offset_ms,
    });
    if !title.is_empty() {
        stream["title"] = json!(title);
    }
    if !subtitle.is_empty() {
        stream["subtitle"] = json!(subtitle);
    }
    let payload = json!({ "stream": stream, "set_pause": false }).to_string();
    let mut raw = Vec::new();
    pb_string(&mut raw, 1, "audio_play");
    pb_string(&mut raw, 2, &payload);
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(raw)
}

fn pb_string(out: &mut Vec<u8>, field: u8, value: &str) {
    out.push((field << 3) | 2);
    let bytes = value.as_bytes();
    let mut n = bytes.len();
    while n >= 0x80 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    out.extend_from_slice(bytes);
}
