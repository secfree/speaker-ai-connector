//! Gemini Live WebSocket client.
//!
//! Connects to the Live API over WebSocket, streams 16 kHz mono i16
//! PCM up (base64-encoded inside the JSON envelope the API expects),
//! and emits decoded 24 kHz mono i16 PCM bursts plus turn-boundary
//! markers downstream via an [`EventSink`].
//!
//! Threading model: the session owns a dedicated OS thread that hosts
//! a current-thread tokio runtime; cpal callbacks push samples via a
//! sync mpsc sender (cheap, lock-free on the tokio side), and the
//! runtime hosts the read/write halves of the WebSocket as two tasks.
//! Shutdown is cooperative — `stop()` flips an oneshot and joins.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone)]
pub enum GeminiError {
    NoApiKey,
    AuthFailed,
    Network(String),
    SafetyBlocked,
    Other(String),
}

impl GeminiError {
    /// Negative i32 codes for the C ABI. 0 is reserved for success.
    /// Codes are stable across releases — do not reuse a number for a
    /// different variant.
    pub fn code(&self) -> i32 {
        match self {
            GeminiError::NoApiKey => -300,
            GeminiError::AuthFailed => -301,
            GeminiError::Network(_) => -302,
            GeminiError::SafetyBlocked => -303,
            GeminiError::Other(_) => -304,
        }
    }

    /// Stable machine-readable tag the shell uses to pick the menu-bar
    /// message variant. Detail text comes from `message()`.
    pub fn tag(&self) -> &'static str {
        match self {
            GeminiError::NoApiKey => "no_api_key",
            GeminiError::AuthFailed => "auth_failed",
            GeminiError::Network(_) => "network",
            GeminiError::SafetyBlocked => "safety_blocked",
            GeminiError::Other(_) => "other",
        }
    }

    pub fn message(&self) -> String {
        match self {
            GeminiError::NoApiKey => "No API key — open Settings".into(),
            GeminiError::AuthFailed => "Gemini auth failed — check API key".into(),
            GeminiError::Network(m) => format!("Network error: {m}"),
            GeminiError::SafetyBlocked => "Gemini blocked the response (safety)".into(),
            GeminiError::Other(m) => m.clone(),
        }
    }
}

/// Default model. Live API model ids live under `models/…`. The flash
/// tier is the latency target for kid-voice turn-taking; the doc records
/// this choice and why.
pub const DEFAULT_MODEL: &str = "models/gemini-2.0-flash-live-001";
/// Input PCM contract — 16 kHz mono i16, native to libfvad and HFP.
pub const INPUT_SAMPLE_RATE: u32 = 16_000;
/// Gemini Live emits 24 kHz mono i16. The playback path resamples to
/// the output device rate.
pub const OUTPUT_SAMPLE_RATE: u32 = 24_000;

/// System instruction nudged toward a friendly, age-appropriate persona.
/// The hard safety floor lives in `SAFETY_SETTINGS`; this is the soft
/// tone control.
const SYSTEM_INSTRUCTION: &str = concat!(
    "You are a kind, patient voice assistant for a child. ",
    "Speak in short, simple sentences. ",
    "Refuse violent, sexual, or self-harm content gently and redirect to a positive topic. ",
    "Never tell the child to hurt themselves or anyone else. ",
    "If you don't know something, say so."
);

/// Child-appropriate safety floor: block at the lowest probability tier
/// across all four harm categories the Live API exposes. Recorded in
/// `docs/v0.1-design.md`.
const SAFETY_SETTINGS: &[(&str, &str)] = &[
    ("HARM_CATEGORY_HARASSMENT", "BLOCK_LOW_AND_ABOVE"),
    ("HARM_CATEGORY_HATE_SPEECH", "BLOCK_LOW_AND_ABOVE"),
    ("HARM_CATEGORY_SEXUALLY_EXPLICIT", "BLOCK_LOW_AND_ABOVE"),
    ("HARM_CATEGORY_DANGEROUS_CONTENT", "BLOCK_LOW_AND_ABOVE"),
];

/// Events emitted to the audio layer / session recorder. Lifecycle is
/// intentionally narrow — anything richer (transcripts, tool calls)
/// becomes a problem when it does, not before.
#[derive(Debug)]
pub enum GeminiEvent {
    /// Server accepted the setup; safe to start streaming.
    SetupComplete,
    /// 24 kHz mono i16 PCM. May be small — concatenate per turn upstream.
    AudioChunk(Vec<i16>),
    /// Server marked the end of a model burst. Boundary for the
    /// session recorder's output clip.
    TurnComplete,
    /// User interrupted the model. Boundary too, but distinguished so
    /// the playback queue can flush instead of letting the partial
    /// reply drain.
    Interrupted,
    /// Connection ended cleanly (server close or local shutdown).
    Closed,
    Error(GeminiError),
}

pub trait EventSink: Send + Sync + 'static {
    fn handle(&self, event: GeminiEvent);
}

impl<F> EventSink for F
where
    F: Fn(GeminiEvent) + Send + Sync + 'static,
{
    fn handle(&self, event: GeminiEvent) {
        self(event)
    }
}

pub struct GeminiSession {
    upload_tx: mpsc::UnboundedSender<Vec<i16>>,
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl GeminiSession {
    pub fn start(
        api_key: String,
        model: String,
        sink: Arc<dyn EventSink>,
    ) -> Result<Self, GeminiError> {
        if api_key.is_empty() {
            return Err(GeminiError::NoApiKey);
        }

        let (upload_tx, upload_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let sink_thread = sink.clone();

        // Bounded one-shot for the initial connect result so the caller
        // can fail loudly (NoApiKey, AuthFailed, Network) instead of
        // discovering the connection died only by absence of events.
        let (boot_tx, boot_rx) = std::sync::mpsc::sync_channel::<Result<(), GeminiError>>(1);

        let join = thread::Builder::new()
            .name("gemini-live".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = boot_tx.send(Err(GeminiError::Other(format!(
                            "tokio runtime: {e}"
                        ))));
                        return;
                    }
                };
                rt.block_on(run_session(
                    api_key,
                    model,
                    upload_rx,
                    sink_thread,
                    shutdown_thread,
                    boot_tx,
                ));
            })
            .map_err(|e| GeminiError::Other(format!("spawn thread: {e}")))?;

        // Wait for the connect handshake before returning — bounded so
        // a hung DNS doesn't pin the FFI call indefinitely.
        match boot_rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                upload_tx,
                shutdown,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                shutdown.store(true, Ordering::SeqCst);
                let _ = join.join();
                Err(GeminiError::Network("connect timed out".into()))
            }
        }
    }

    /// Hand 16 kHz mono i16 PCM to the upload task. Cheap — pushes
    /// into a tokio mpsc; the network task batches and base64-encodes.
    /// Returns `Err` only if the session has been torn down.
    pub fn send_audio(&self, samples: &[i16]) -> Result<(), GeminiError> {
        self.upload_tx
            .send(samples.to_vec())
            .map_err(|_| GeminiError::Other("upload channel closed".into()))
    }

    /// Cheap clonable handle for cpal callbacks. The capture stream
    /// outlives no callback frame, so `send` from inside one is safe.
    pub fn upload_handle(&self) -> UploadHandle {
        UploadHandle {
            tx: self.upload_tx.clone(),
        }
    }
}

#[derive(Clone)]
pub struct UploadHandle {
    tx: mpsc::UnboundedSender<Vec<i16>>,
}

impl UploadHandle {
    pub fn send(&self, samples: &[i16]) -> Result<(), GeminiError> {
        self.tx
            .send(samples.to_vec())
            .map_err(|_| GeminiError::Other("upload channel closed".into()))
    }

    /// Explicit teardown. Equivalent to dropping the session, but the
    /// FFI surface prefers a named call so its lifecycle reads cleanly.
    pub fn stop(self) {
        // `Drop` does the work.
    }
}

impl Drop for GeminiSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Replace the sender with a dead one so the runtime's recv()
        // returns None on the next tick — without this the write task
        // would idle until the next 100 ms sleep + shutdown poll fires.
        let (closed_tx, _) = mpsc::unbounded_channel::<Vec<i16>>();
        let _ = std::mem::replace(&mut self.upload_tx, closed_tx);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

// --- Wire types ------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupEnvelope<'a> {
    setup: Setup<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Setup<'a> {
    model: &'a str,
    generation_config: GenerationConfig,
    system_instruction: SystemInstruction<'a>,
    safety_settings: Vec<SafetySetting>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    response_modalities: Vec<&'static str>,
}

#[derive(Serialize)]
struct SystemInstruction<'a> {
    parts: Vec<TextPart<'a>>,
}

#[derive(Serialize)]
struct TextPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct SafetySetting {
    category: &'static str,
    threshold: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RealtimeEnvelope {
    realtime_input: RealtimeInput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RealtimeInput {
    media_chunks: Vec<MediaChunk>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaChunk {
    mime_type: String,
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerMessage {
    #[serde(default)]
    setup_complete: Option<serde_json::Value>,
    #[serde(default)]
    server_content: Option<ServerContent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerContent {
    #[serde(default)]
    model_turn: Option<ModelTurn>,
    #[serde(default)]
    turn_complete: Option<bool>,
    #[serde(default)]
    interrupted: Option<bool>,
}

#[derive(Deserialize)]
struct ModelTurn {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponsePart {
    #[serde(default)]
    inline_data: Option<InlineData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InlineData {
    #[serde(default)]
    mime_type: String,
    data: String,
}

// --- Runtime --------------------------------------------------------

/// rustls 0.23 no longer picks a crypto backend implicitly. Install the
/// `ring` provider exactly once before the first TLS handshake; cargo
/// feature auto-detection is fragile across workspaces, so do it
/// explicitly even though we also pin the `ring` feature on rustls.
fn ensure_crypto_provider() {
    static CRYPTO_INIT: std::sync::Once = std::sync::Once::new();
    CRYPTO_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

async fn run_session(
    api_key: String,
    model: String,
    mut upload_rx: mpsc::UnboundedReceiver<Vec<i16>>,
    sink: Arc<dyn EventSink>,
    shutdown: Arc<AtomicBool>,
    boot_tx: std::sync::mpsc::SyncSender<Result<(), GeminiError>>,
) {
    ensure_crypto_provider();

    let url = format!(
        "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={}",
        api_key
    );
    let request = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            let _ = boot_tx.send(Err(GeminiError::Other(format!("bad url: {e}"))));
            return;
        }
    };

    let (ws, _resp) = match tokio_tungstenite::connect_async(request).await {
        Ok(x) => x,
        Err(e) => {
            let _ = boot_tx.send(Err(classify_connect_error(e)));
            return;
        }
    };

    let (mut writer, mut reader) = ws.split();

    // Send setup as the first message.
    let setup = SetupEnvelope {
        setup: Setup {
            model: &model,
            generation_config: GenerationConfig {
                response_modalities: vec!["AUDIO"],
            },
            system_instruction: SystemInstruction {
                parts: vec![TextPart {
                    text: SYSTEM_INSTRUCTION,
                }],
            },
            safety_settings: SAFETY_SETTINGS
                .iter()
                .map(|(c, t)| SafetySetting {
                    category: c,
                    threshold: t,
                })
                .collect(),
        },
    };
    let setup_json = match serde_json::to_string(&setup) {
        Ok(s) => s,
        Err(e) => {
            let _ = boot_tx.send(Err(GeminiError::Other(format!("setup serialize: {e}"))));
            return;
        }
    };
    if let Err(e) = writer.send(Message::Text(setup_json.into())).await {
        let _ = boot_tx.send(Err(GeminiError::Network(format!("setup send: {e}"))));
        return;
    }

    // Connect handshake succeeded. Setup-complete arrives over the read
    // task; the boot signal here is "the WebSocket is up", which is the
    // operationally interesting failure boundary.
    let _ = boot_tx.send(Ok(()));

    let sink_read = sink.clone();
    let shutdown_read = shutdown.clone();
    let read_task = tokio::spawn(async move {
        loop {
            // Poll-with-timeout so shutdown can break a server that
            // never closes the socket on its own (the alternative —
            // aborting the task from the writer — leaves the TLS
            // session in a half-shut state).
            let next = tokio::select! {
                m = reader.next() => m,
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if shutdown_read.load(Ordering::SeqCst) { break; }
                    continue;
                }
            };
            let Some(msg) = next else { break };
            match msg {
                Ok(Message::Text(t)) => dispatch_server_text(&sink_read, &t),
                Ok(Message::Binary(b)) => {
                    // Some Live deployments deliver JSON as binary frames.
                    if let Ok(t) = std::str::from_utf8(&b) {
                        dispatch_server_text(&sink_read, t);
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    sink_read.handle(GeminiEvent::Error(GeminiError::Network(e.to_string())));
                    break;
                }
            }
        }
        sink_read.handle(GeminiEvent::Closed);
    });

    // Upload loop: drain mpsc, base64, send. Coalesces small frames into
    // one JSON envelope per iteration to avoid hammering the websocket.
    let write_shutdown = shutdown.clone();
    let write_sink = sink.clone();
    let write_task = tokio::spawn(async move {
        loop {
            if write_shutdown.load(Ordering::SeqCst) {
                break;
            }
            let first = tokio::select! {
                v = upload_rx.recv() => v,
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    continue;
                }
            };
            let Some(first) = first else { break };
            let mut batch: Vec<i16> = first;
            // Opportunistically drain anything queued so a busy capture
            // path doesn't backlog. Bounded so we never base64 an
            // unbounded buffer in one shot.
            while batch.len() < 16_000 {
                match upload_rx.try_recv() {
                    Ok(v) => batch.extend_from_slice(&v),
                    Err(_) => break,
                }
            }
            let bytes: Vec<u8> = batch
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            let data = B64.encode(bytes);
            let env = RealtimeEnvelope {
                realtime_input: RealtimeInput {
                    media_chunks: vec![MediaChunk {
                        mime_type: format!("audio/pcm;rate={INPUT_SAMPLE_RATE}"),
                        data,
                    }],
                },
            };
            let json = match serde_json::to_string(&env) {
                Ok(s) => s,
                Err(e) => {
                    write_sink.handle(GeminiEvent::Error(GeminiError::Other(format!(
                        "upload serialize: {e}"
                    ))));
                    break;
                }
            };
            if let Err(e) = writer.send(Message::Text(json.into())).await {
                write_sink
                    .handle(GeminiEvent::Error(GeminiError::Network(format!(
                        "upload send: {e}"
                    ))));
                break;
            }
        }
        let _ = writer.close().await;
    });

    let _ = tokio::join!(read_task, write_task);
}

fn classify_connect_error(e: tokio_tungstenite::tungstenite::Error) -> GeminiError {
    use tokio_tungstenite::tungstenite::Error as E;
    let s = e.to_string();
    match e {
        E::Http(resp) => {
            let code = resp.status().as_u16();
            if code == 401 || code == 403 {
                GeminiError::AuthFailed
            } else {
                GeminiError::Network(format!("http {code}"))
            }
        }
        E::ConnectionClosed | E::AlreadyClosed | E::Io(_) | E::Tls(_) | E::Url(_) => {
            GeminiError::Network(s)
        }
        _ => GeminiError::Network(s),
    }
}

fn dispatch_server_text(sink: &Arc<dyn EventSink>, text: &str) {
    let msg: ServerMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => return,
    };
    if msg.setup_complete.is_some() {
        sink.handle(GeminiEvent::SetupComplete);
    }
    let Some(content) = msg.server_content else { return };
    if let Some(turn) = content.model_turn {
        for part in turn.parts {
            if let Some(data) = part.inline_data {
                if !data.mime_type.starts_with("audio/") {
                    continue;
                }
                let bytes = match B64.decode(data.data.as_bytes()) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // i16 little-endian.
                let samples: Vec<i16> = bytes
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
                if !samples.is_empty() {
                    sink.handle(GeminiEvent::AudioChunk(samples));
                }
            }
        }
    }
    if content.interrupted == Some(true) {
        sink.handle(GeminiEvent::Interrupted);
    }
    if content.turn_complete == Some(true) {
        sink.handle(GeminiEvent::TurnComplete);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_settings_cover_all_four_categories() {
        // Compile-time list, but assert the tags so a typo in the
        // category string doesn't slip through silently.
        let cats: Vec<&str> = SAFETY_SETTINGS.iter().map(|(c, _)| *c).collect();
        assert!(cats.contains(&"HARM_CATEGORY_HARASSMENT"));
        assert!(cats.contains(&"HARM_CATEGORY_HATE_SPEECH"));
        assert!(cats.contains(&"HARM_CATEGORY_SEXUALLY_EXPLICIT"));
        assert!(cats.contains(&"HARM_CATEGORY_DANGEROUS_CONTENT"));
        for (_c, t) in SAFETY_SETTINGS {
            assert_eq!(*t, "BLOCK_LOW_AND_ABOVE");
        }
    }

    #[test]
    fn error_codes_stable_and_unique() {
        let codes = [
            GeminiError::NoApiKey.code(),
            GeminiError::AuthFailed.code(),
            GeminiError::Network("x".into()).code(),
            GeminiError::SafetyBlocked.code(),
            GeminiError::Other("x".into()).code(),
        ];
        let mut sorted = codes.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len());
    }

    #[test]
    fn server_message_parses_audio_chunk() {
        let bytes: Vec<u8> = [0i16, 1, -1, 1000]
            .iter()
            .flat_map(|s: &i16| s.to_le_bytes())
            .collect();
        let b64 = B64.encode(&bytes);
        let json = format!(
            r#"{{"serverContent":{{"modelTurn":{{"parts":[{{"inlineData":{{"mimeType":"audio/pcm;rate=24000","data":"{b64}"}}}}]}}}}}}"#
        );
        let captured: Arc<std::sync::Mutex<Vec<i16>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        let sink: Arc<dyn EventSink> = Arc::new(move |e: GeminiEvent| {
            if let GeminiEvent::AudioChunk(s) = e {
                cap.lock().unwrap().extend_from_slice(&s);
            }
        });
        dispatch_server_text(&sink, &json);
        assert_eq!(*captured.lock().unwrap(), vec![0i16, 1, -1, 1000]);
    }

    #[test]
    fn server_message_emits_turn_complete() {
        let json = r#"{"serverContent":{"turnComplete":true}}"#;
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        let sink: Arc<dyn EventSink> = Arc::new(move |e: GeminiEvent| {
            if matches!(e, GeminiEvent::TurnComplete) {
                f.store(true, Ordering::SeqCst);
            }
        });
        dispatch_server_text(&sink, json);
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn no_api_key_rejected_before_thread_spawn() {
        let sink: Arc<dyn EventSink> = Arc::new(|_| {});
        match GeminiSession::start(String::new(), DEFAULT_MODEL.into(), sink) {
            Err(GeminiError::NoApiKey) => {}
            Err(e) => panic!("expected NoApiKey, got {e:?}"),
            Ok(_) => panic!("expected NoApiKey, got Ok"),
        }
    }
}
