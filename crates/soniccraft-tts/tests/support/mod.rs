//! Shared helpers for integration tests: an in-process HTTP/1.1 server with scripted replies (tokio
//! `TcpListener`, one request per connection), request recording and in-flight tracking, plus IR builders.

#![allow(dead_code)] // each test binary uses a different subset

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};
use soniccraft_tts::{EnvSource, SynthesisRequest, TtsOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Body of a scripted reply.
#[derive(Debug, Clone)]
pub enum Body {
    /// A `Content-Length` body.
    Full(Vec<u8>),
    /// `Transfer-Encoding: chunked`, each piece flushed separately with `gap` between pieces.
    Chunked { pieces: Vec<Vec<u8>>, gap: Duration },
    /// `bytes` after a `Content-Length` header claiming `length` (the connection is closed after `bytes`).
    Declared { length: u64, bytes: Vec<u8> },
    /// Close the connection without responding.
    Hangup,
}

/// A scripted reply.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    pub delay: Duration,
}

impl Reply {
    pub fn bytes(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Reply {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: Body::Full(body.into()),
            delay: Duration::ZERO,
        }
    }

    pub fn json(status: u16, value: Value) -> Self {
        Self::bytes(status, "application/json", value.to_string())
    }

    pub fn text(status: u16, text: &str) -> Self {
        Self::bytes(status, "text/plain", text.as_bytes().to_vec())
    }

    /// An SSE stream of `data:` frames, each split in two TCP writes (mid-line), 5 ms apart.
    pub fn sse(frames: &[Value]) -> Self {
        let mut pieces = Vec::new();
        for frame in frames {
            let text = format!("event: message\ndata: {frame}\n\n").into_bytes();
            let mid = text.len() / 2;
            pieces.push(text[..mid].to_vec());
            pieces.push(text[mid..].to_vec());
        }
        Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            body: Body::Chunked {
                pieces,
                gap: Duration::from_millis(5),
            },
            delay: Duration::ZERO,
        }
    }

    pub fn hangup() -> Self {
        Reply {
            status: 0,
            headers: Vec::new(),
            body: Body::Hangup,
            delay: Duration::ZERO,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// A recorded request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    /// Header value by lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

#[derive(Default)]
struct State {
    routes: HashMap<(String, String), VecDeque<Reply>>,
    requests: Vec<Recorded>,
    in_flight: usize,
    max_in_flight: usize,
}

/// The server. Unknown routes answer 404 `{"detail":"Not Found"}` like FastAPI.
#[derive(Clone)]
pub struct TestServer {
    pub addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl TestServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let state = shared.clone();
                tokio::spawn(async move {
                    let _ = handle(socket, state).await;
                });
            }
        });
        TestServer { addr, state }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Script `method path`: replies are served in order and the last one repeats.
    pub fn route(&self, method: &str, path: &str, replies: Vec<Reply>) {
        self.state
            .lock()
            .unwrap()
            .routes
            .insert((method.to_owned(), path.to_owned()), replies.into());
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn requests_to(&self, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.path == path)
            .collect()
    }

    pub fn max_in_flight(&self) -> usize {
        self.state.lock().unwrap().max_in_flight
    }
}

async fn read_request(socket: &mut TcpStream) -> std::io::Result<Option<Recorded>> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Ok(Some(Recorded {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body,
    }))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

async fn handle(mut socket: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    socket.set_nodelay(true)?;
    let Some(request) = read_request(&mut socket).await? else {
        return Ok(());
    };
    let reply = {
        let mut s = state.lock().unwrap();
        s.in_flight += 1;
        s.max_in_flight = s.max_in_flight.max(s.in_flight);
        let key = (request.method.clone(), request.path.clone());
        let reply = match s.routes.get_mut(&key) {
            Some(queue) if queue.len() > 1 => queue.pop_front(),
            Some(queue) => queue.front().cloned(),
            None => None,
        };
        s.requests.push(request);
        reply.unwrap_or_else(|| Reply::json(404, serde_json::json!({"detail": "Not Found"})))
    };
    let result = respond(&mut socket, &reply).await;
    state.lock().unwrap().in_flight -= 1;
    result
}

async fn respond(socket: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    if !reply.delay.is_zero() {
        tokio::time::sleep(reply.delay).await;
    }
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\n",
        reply.status,
        reason(reply.status)
    );
    for (k, v) in &reply.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    match &reply.body {
        Body::Hangup => return socket.shutdown().await,
        Body::Full(bytes) => {
            head.push_str(&format!("Content-Length: {}\r\n\r\n", bytes.len()));
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(bytes).await?;
        }
        Body::Declared { length, bytes } => {
            head.push_str(&format!("Content-Length: {length}\r\n\r\n"));
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(bytes).await?;
        }
        Body::Chunked { pieces, gap } => {
            head.push_str("Transfer-Encoding: chunked\r\n\r\n");
            socket.write_all(head.as_bytes()).await?;
            socket.flush().await?;
            for piece in pieces {
                socket
                    .write_all(format!("{:x}\r\n", piece.len()).as_bytes())
                    .await?;
                socket.write_all(piece).await?;
                socket.write_all(b"\r\n").await?;
                socket.flush().await?;
                tokio::time::sleep(*gap).await;
            }
            socket.write_all(b"0\r\n\r\n").await?;
        }
    }
    socket.flush().await?;
    socket.shutdown().await
}

/// A local TCP port that refuses connections at once: port 1 (`tcpmux`), served on no test machine and never
/// handed to the other tests, which bind ephemeral ports. (A freed ephemeral port can be given to a test
/// running in parallel at once, and a bound socket that does not listen is no substitute: macOS silently
/// drops connection attempts to it instead of refusing them.)
pub fn closed_port() -> u16 {
    1
}

// ---- IR builders -------------------------------------------------------------------------------------------

pub fn instance(t: ProviderType, base_url: Option<String>) -> ProviderInstance {
    let api_key_env = match t {
        ProviderType::Elevenlabs => Some("ELEVENLABS_API_KEY"),
        ProviderType::Openai => Some("OPENAI_API_KEY"),
        ProviderType::Cartesia => Some("CARTESIA_API_KEY"),
        _ => None,
    };
    ProviderInstance {
        name: t.as_str().to_owned(),
        provider_type: t,
        configured: false,
        api_key_env: api_key_env.map(str::to_owned),
        base_url,
        model: None,
        output_format: None,
        max_concurrency: 4,
        timeout_ms: 10_000,
        chars_per_second: None,
        words_per_minute: None,
        settings: Map::new(),
        command: None,
        stdin: None,
        voices_dir: None,
    }
}

pub fn voice(t: ProviderType, voice_id: Option<&str>, settings: Value) -> VoiceIr {
    VoiceIr {
        provider: t.as_str().to_owned(),
        provider_type: t,
        voice_id: voice_id.map(str::to_owned),
        model: None,
        language: None,
        settings: settings.as_object().cloned().unwrap_or_default(),
    }
}

pub fn request(provider: ProviderInstance, voice: VoiceIr, text: &str) -> SynthesisRequest {
    SynthesisRequest {
        cue_id: "cue_01".into(),
        provider,
        voice,
        text: text.into(),
        base_dir: PathBuf::from("."),
    }
}

/// Fast retries and a fixed environment with fake keys.
pub fn options() -> TtsOptions {
    TtsOptions {
        retry_base_delay: Duration::from_millis(5),
        env: EnvSource::fixed([
            ("ELEVENLABS_API_KEY", "el-test-key"),
            ("OPENAI_API_KEY", "sk-test-key"),
            ("CARTESIA_API_KEY", "sk_car_test-key"),
        ]),
        ..TtsOptions::default()
    }
}

/// Little-endian s16 PCM of a ramp of `n` samples.
pub fn pcm16(n: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|i| (((i % 200) as i16 - 100) * 100).to_le_bytes())
        .collect()
}

/// Little-endian f32 PCM.
pub fn pcm_f32(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// A canonical 44-byte-header PCM s16 mono WAV.
pub fn wav_s16(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// A short tone as s16 samples.
pub fn tone(n: usize) -> Vec<i16> {
    (0..n)
        .map(|i| ((i as f64 * 0.05).sin() * 8_000.0) as i16)
        .collect()
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
