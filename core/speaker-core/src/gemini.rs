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
pub const DEFAULT_MODEL: &str = "models/gemini-3.1-flash-live-preview";
/// Input PCM contract — 16 kHz mono i16, native to libfvad and HFP.
pub const INPUT_SAMPLE_RATE: u32 = 16_000;
/// Gemini Live emits 24 kHz mono i16. The playback path resamples to
/// the output device rate.
pub const OUTPUT_SAMPLE_RATE: u32 = 24_000;

/// System instruction nudged toward a friendly, age-appropriate persona.
/// This is the only safety lever the Live API gives us — `safetySettings`
/// is a REST-only field and `BidiGenerateContentSetup` rejects it. Hard
/// thresholds therefore fall back to Gemini's built-in defaults; see
/// `docs/v0.1-design.md` for the trade-off.
const SYSTEM_INSTRUCTION: &str = concat!(
    "You are a kind, patient voice assistant for a child. ",
    "Speak in short, simple sentences. ",
    "Refuse violent, sexual, or self-harm content gently and redirect to a positive topic. ",
    "Never tell the child to hurt themselves or anyone else. ",
    "If you don't know something, say so."
);

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

/// Items the cpal callback hands to the WebSocket write task. Audio and
/// activity markers share one queue so ordering is preserved without a
/// second mpsc + interleave step (an activityEnd that overtook a trailing
/// audio batch would be a wire-protocol bug).
enum UploadItem {
    Audio(Vec<i16>),
    ActivityStart,
    ActivityEnd,
}

pub struct GeminiSession {
    upload_tx: mpsc::UnboundedSender<UploadItem>,
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

        let (upload_tx, upload_rx) = mpsc::unbounded_channel::<UploadItem>();
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
            .send(UploadItem::Audio(samples.to_vec()))
            .map_err(|_| GeminiError::Other("upload channel closed".into()))
    }

    /// Signal the start of a user turn. Paired with [`send_activity_end`].
    /// Required because the setup envelope disables Gemini's server-side
    /// automatic activity detection — without these markers the model
    /// never commits a turn.
    pub fn send_activity_start(&self) -> Result<(), GeminiError> {
        self.upload_tx
            .send(UploadItem::ActivityStart)
            .map_err(|_| GeminiError::Other("upload channel closed".into()))
    }

    /// Signal end-of-turn. After this fires Gemini decodes the buffered
    /// utterance and emits a model response.
    pub fn send_activity_end(&self) -> Result<(), GeminiError> {
        self.upload_tx
            .send(UploadItem::ActivityEnd)
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
    tx: mpsc::UnboundedSender<UploadItem>,
}

impl UploadHandle {
    pub fn send(&self, samples: &[i16]) -> Result<(), GeminiError> {
        self.tx
            .send(UploadItem::Audio(samples.to_vec()))
            .map_err(|_| GeminiError::Other("upload channel closed".into()))
    }

    pub fn activity_start(&self) -> Result<(), GeminiError> {
        self.tx
            .send(UploadItem::ActivityStart)
            .map_err(|_| GeminiError::Other("upload channel closed".into()))
    }

    pub fn activity_end(&self) -> Result<(), GeminiError> {
        self.tx
            .send(UploadItem::ActivityEnd)
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
        let (closed_tx, _) = mpsc::unbounded_channel::<UploadItem>();
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
    realtime_input_config: RealtimeInputConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    response_modalities: Vec<&'static str>,
}

// Disable Gemini's server-side automatic VAD: the Rust core already gates
// uploads with libfvad/Silero and drops silent frames on the floor, so the
// server never sees a silence transition. Without this flag, Gemini buffers
// indefinitely and never emits a turn. With it, the client owns turn
// boundaries and signals them with activityStart / activityEnd frames.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RealtimeInputConfig {
    automatic_activity_detection: AutomaticActivityDetection,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AutomaticActivityDetection {
    disabled: bool,
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
#[serde(rename_all = "camelCase")]
struct RealtimeEnvelope {
    realtime_input: RealtimeInput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RealtimeInput {
    // Current Live API field. The older `mediaChunks` array was
    // deprecated and is now hard-rejected with WS code 1007. `audio`
    // takes a single `Blob` per envelope; we already coalesce in the
    // upload loop, so one envelope == one batched chunk is fine.
    audio: AudioBlob,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioBlob {
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
    mut upload_rx: mpsc::UnboundedReceiver<UploadItem>,
    sink: Arc<dyn EventSink>,
    shutdown: Arc<AtomicBool>,
    boot_tx: std::sync::mpsc::SyncSender<Result<(), GeminiError>>,
) {
    ensure_crypto_provider();

    let url = format!(
        "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={}",
        api_key
    );
    eprintln!(
        "speaker-core: gemini connecting model={} key=…{} (len {})",
        model,
        // Redact: last 4 chars only — enough to tell two keys apart in
        // a log without leaking the secret.
        &api_key[api_key.len().saturating_sub(4)..],
        api_key.len(),
    );
    let request = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("speaker-core: gemini bad url: {e}");
            let _ = boot_tx.send(Err(GeminiError::Other(format!("bad url: {e}"))));
            return;
        }
    };

    let (ws, resp) = match tokio_tungstenite::connect_async(request).await {
        Ok(x) => x,
        Err(e) => {
            eprintln!("speaker-core: gemini connect_async failed: {e}");
            let _ = boot_tx.send(Err(classify_connect_error(e)));
            return;
        }
    };
    eprintln!(
        "speaker-core: gemini ws upgraded http={} headers={:?}",
        resp.status(),
        resp.headers()
            .iter()
            .map(|(k, v)| format!("{}={}", k, v.to_str().unwrap_or("<binary>")))
            .collect::<Vec<_>>(),
    );

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
            realtime_input_config: RealtimeInputConfig {
                automatic_activity_detection: AutomaticActivityDetection { disabled: true },
            },
        },
    };
    let setup_json = match serde_json::to_string(&setup) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("speaker-core: gemini setup serialize: {e}");
            let _ = boot_tx.send(Err(GeminiError::Other(format!("setup serialize: {e}"))));
            return;
        }
    };
    eprintln!(
        "speaker-core: gemini sending setup ({} bytes): {}",
        setup_json.len(),
        setup_json,
    );
    if let Err(e) = writer.send(Message::Text(setup_json.into())).await {
        eprintln!("speaker-core: gemini setup send failed: {e}");
        let _ = boot_tx.send(Err(GeminiError::Network(format!("setup send: {e}"))));
        return;
    }
    eprintln!("speaker-core: gemini setup sent, waiting for setupComplete");

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
            let Some(msg) = next else {
                eprintln!("speaker-core: gemini read stream ended (server closed without Close frame)");
                break;
            };
            match msg {
                Ok(Message::Text(t)) => {
                    eprintln!("speaker-core: gemini recv text ({} bytes)", t.len());
                    dispatch_server_text(&sink_read, &t);
                }
                Ok(Message::Binary(b)) => {
                    eprintln!("speaker-core: gemini recv binary ({} bytes)", b.len());
                    // Some Live deployments deliver JSON as binary frames.
                    if let Ok(t) = std::str::from_utf8(&b) {
                        dispatch_server_text(&sink_read, t);
                    } else {
                        eprintln!("speaker-core: gemini binary frame is not valid UTF-8");
                    }
                }
                Ok(Message::Close(frame)) => {
                    match frame {
                        Some(cf) => eprintln!(
                            "speaker-core: gemini recv Close code={} reason={:?}",
                            cf.code, cf.reason,
                        ),
                        None => eprintln!("speaker-core: gemini recv Close (no frame body)"),
                    }
                    break;
                }
                Ok(Message::Ping(_)) => eprintln!("speaker-core: gemini recv Ping"),
                Ok(Message::Pong(_)) => eprintln!("speaker-core: gemini recv Pong"),
                Ok(other) => eprintln!("speaker-core: gemini recv other frame: {other:?}"),
                Err(e) => {
                    eprintln!("speaker-core: gemini read error: {e}");
                    sink_read.handle(GeminiEvent::Error(GeminiError::Network(e.to_string())));
                    break;
                }
            }
        }
        sink_read.handle(GeminiEvent::Closed);
    });

    // Upload loop: drain mpsc, base64, send. Coalesces small audio frames
    // into one JSON envelope per iteration to avoid hammering the
    // websocket. Activity markers flush any buffered audio first so the
    // server receives audio strictly before the activityEnd that closes
    // its turn — otherwise it would emit a response from an empty buffer.
    let write_shutdown = shutdown.clone();
    let write_sink = sink.clone();
    let write_task = tokio::spawn(async move {
        // Pending audio waiting to be coalesced. Cleared on flush.
        let mut pending: Vec<i16> = Vec::new();

        async fn flush_audio<W>(
            writer: &mut W,
            pending: &mut Vec<i16>,
            sink: &Arc<dyn EventSink>,
        ) -> bool
        where
            W: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
        {
            if pending.is_empty() {
                return true;
            }
            let bytes: Vec<u8> = pending.iter().flat_map(|s| s.to_le_bytes()).collect();
            let data = B64.encode(bytes);
            let env = RealtimeEnvelope {
                realtime_input: RealtimeInput {
                    audio: AudioBlob {
                        mime_type: format!("audio/pcm;rate={INPUT_SAMPLE_RATE}"),
                        data,
                    },
                },
            };
            pending.clear();
            let json = match serde_json::to_string(&env) {
                Ok(s) => s,
                Err(e) => {
                    sink.handle(GeminiEvent::Error(GeminiError::Other(format!(
                        "upload serialize: {e}"
                    ))));
                    return false;
                }
            };
            if let Err(e) = writer.send(Message::Text(json.into())).await {
                sink.handle(GeminiEvent::Error(GeminiError::Network(format!(
                    "upload send: {e}"
                ))));
                return false;
            }
            true
        }

        async fn send_marker<W>(
            writer: &mut W,
            json: &'static str,
            sink: &Arc<dyn EventSink>,
        ) -> bool
        where
            W: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
        {
            if let Err(e) = writer.send(Message::Text(json.into())).await {
                sink.handle(GeminiEvent::Error(GeminiError::Network(format!(
                    "activity marker send: {e}"
                ))));
                return false;
            }
            true
        }

        'outer: loop {
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
            match first {
                UploadItem::Audio(samples) => {
                    pending.extend_from_slice(&samples);
                }
                UploadItem::ActivityStart => {
                    eprintln!("speaker-core: gemini sending activityStart");
                    if !send_marker(
                        &mut writer,
                        r#"{"realtimeInput":{"activityStart":{}}}"#,
                        &write_sink,
                    )
                    .await
                    {
                        break 'outer;
                    }
                    continue;
                }
                UploadItem::ActivityEnd => {
                    if !flush_audio(&mut writer, &mut pending, &write_sink).await {
                        break 'outer;
                    }
                    eprintln!("speaker-core: gemini sending activityEnd");
                    if !send_marker(
                        &mut writer,
                        r#"{"realtimeInput":{"activityEnd":{}}}"#,
                        &write_sink,
                    )
                    .await
                    {
                        break 'outer;
                    }
                    continue;
                }
            }
            // Opportunistically drain queued audio so a busy capture path
            // doesn't backlog. Bounded so we never base64 an unbounded
            // buffer in one shot. Activity markers break the drain and
            // flush before being emitted.
            while pending.len() < 16_000 {
                match upload_rx.try_recv() {
                    Ok(UploadItem::Audio(v)) => pending.extend_from_slice(&v),
                    Ok(UploadItem::ActivityStart) => {
                        if !flush_audio(&mut writer, &mut pending, &write_sink).await {
                            break 'outer;
                        }
                        eprintln!("speaker-core: gemini sending activityStart");
                        if !send_marker(
                            &mut writer,
                            r#"{"realtimeInput":{"activityStart":{}}}"#,
                            &write_sink,
                        )
                        .await
                        {
                            break 'outer;
                        }
                        continue 'outer;
                    }
                    Ok(UploadItem::ActivityEnd) => {
                        if !flush_audio(&mut writer, &mut pending, &write_sink).await {
                            break 'outer;
                        }
                        eprintln!("speaker-core: gemini sending activityEnd");
                        if !send_marker(
                            &mut writer,
                            r#"{"realtimeInput":{"activityEnd":{}}}"#,
                            &write_sink,
                        )
                        .await
                        {
                            break 'outer;
                        }
                        continue 'outer;
                    }
                    Err(_) => break,
                }
            }
            if !flush_audio(&mut writer, &mut pending, &write_sink).await {
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
        Err(_) => {
            // Live can send error / status frames that don't match our
            // narrow ServerMessage shape — dump them so a setup-time
            // rejection is visible instead of silently dropped. Truncate
            // so a giant inline-data payload can't flood stderr.
            let truncated = if text.len() > 2048 { &text[..2048] } else { text };
            eprintln!("speaker-core: gemini unparsed frame: {truncated}");
            return;
        }
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
