# Audio Pipeline — VAD, Clips, and the Gemini Live Loop

This doc explains how the Rust core moves audio between the speaker's mic, the
local VAD, the Gemini Live WebSocket, the playback queue, and the on-disk session
recorder. Read it before debugging issues in any of those layers — most bugs
that surface as a single ugly log line ("NoActiveClip", "Precondition check
failed", "session ended") are an interaction across two or three of these
pieces, not a defect in one of them.

It is intentionally narrower than [design-v0.1.md](design-v0.1.md): that doc
covers product scope and architectural choices, this one is the mechanism, the
invariants, and the log catalogue.

## Module map

| File | Responsibility |
| --- | --- |
| [`core/speaker-core/src/audio.rs`](../core/speaker-core/src/audio.rs) | `cpal` capture + playback streams; orchestrates a session by wiring VAD → upload, sink → playback queue → output, and recorder begin/end on transitions. Owns the echo guard. |
| [`core/speaker-core/src/vad.rs`](../core/speaker-core/src/vad.rs) | `VadRelay` (sample-rate / frame-size adapter) + `Gate` (preroll, hangover, hysteresis). Engine-agnostic. |
| [`core/speaker-core/src/vad_silero.rs`](../core/speaker-core/src/vad_silero.rs) | Silero v5 engine over `ort`. WebRTC `libfvad` is the alternate engine, defined inline in `vad.rs`. |
| [`core/speaker-core/src/sessions.rs`](../core/speaker-core/src/sessions.rs) | `SessionRecorder` — on-disk WAV clips + manifest. Tracks active `In` and `Out` clips independently. |
| [`core/speaker-core/src/gemini.rs`](../core/speaker-core/src/gemini.rs) | Gemini Live WebSocket: setup envelope, upload task (audio + activity markers), read task (server events). |
| [`core/speaker-core/src/responder.rs`](../core/speaker-core/src/responder.rs) | Tiny abstraction over "the thing that talks back" so the audio path is responder-agnostic (Gemini today; OpenAI Realtime planned). |

The coordinator state machine (`coordinator.rs`) sits one layer above and is
out of scope here — it consumes `ClipEvent`s and decides whether to start /
stop a session, but does not touch frames.

## End-to-end data flow

```
       ┌─────────────────────────────────────────────────────────────┐
       │                       cpal input stream                     │
       │  device sample rate × N channels (f32, interleaved)         │
       └────────────────┬────────────────────────────────────────────┘
                        │ downmix + linear-interp resample
                        ▼
              16 kHz mono i16  (INPUT_SAMPLE_RATE)
                        │
                        ▼
        ┌───────────────────────────────────────────┐
        │           Echo-guard check (audio.rs)     │
        │  out_clip_open  OR  now < play_out + tail │──► drop frame
        └────────────────┬──────────────────────────┘
                         │ (guard clear)
                         ▼
              ┌──────────────────────┐
              │   VadRelay.process   │  Silero v5 (default) or WebRTC
              │  + Gate (preroll,    │
              │     hangover)        │
              └────────┬─────────────┘
                       │ ProcessOutput { opened, closed, frames }
                       ▼
        ┌────────────────────────────────────────────┐
        │  on opened  → activity_start  + begin_clip(In)
        │  per frame  → upload.send     + write_frames(In)
        │  on closed  → activity_end    + end_clip(In)
        └────────────────┬───────────────────────────┘
                         │
                         ▼
                ┌───────────────────┐
                │  Gemini upload    │  base64(PCM) → WS Text
                │  task (gemini.rs) │
                └───────────────────┘
                         │
                         │  realtimeInput envelopes
                         ▼
               ────────────────────────
                Gemini Live WebSocket
               ────────────────────────
                         │
                         │  ServerContent: modelTurn / turnComplete / interrupted
                         ▼
                ┌───────────────────┐
                │  Gemini read task │  decodes binary parts → i16 PCM (24 kHz)
                └─────────┬─────────┘
                          │ GeminiEvent::AudioChunk(samples)
                          ▼
        ┌───────────────────────────────────────────┐
        │  Sink (in audio.rs):                      │
        │   - begin_clip(Out) on first chunk        │
        │   - write_frames(Out, samples)            │
        │   - advance play_out_until_ns             │
        │   - extend playback_queue                 │
        └────────────────┬──────────────────────────┘
                         │
                         ▼
              ┌──────────────────────┐
              │ cpal output stream   │  24 kHz mono i16 → device rate/channels
              └────────────┬─────────┘
                           ▼
                      speaker plays
                           │
                           └──── (acoustic path) ───┐
                                                    │
                                                    ▼ (echo into mic — echo guard exists to gate this out)
```

All audio stays inside the Rust core. The FFI surface exchanges `BTEvent` /
`StatusEvent` / `ClipEvent` with the shell — never PCM, never `Vec<i16>`.

## Sample-rate and framing contract

| Stage | Rate | Format | Frame size |
| --- | --- | --- | --- |
| Mic (cpal) | device-native (commonly 48 kHz) | f32, N channels | cpal buffer (~10 ms typical) |
| After resample | 16 kHz | i16, mono | aggregated to relay frame |
| VAD relay | 16 kHz | i16, mono | 320 samples / 20 ms |
| Silero window | 16 kHz | f32, mono | 512 samples / 32 ms (engine-internal, re-windowed from 320) |
| Gemini upload | 16 kHz | i16 → base64 LE | coalesced ≤16 000 samples per WS Text frame |
| Gemini download | 24 kHz | i16, mono | server-chosen burst sizes |
| Speaker (cpal) | device-native | f32, N channels | cpal buffer |

The two **fixed contracts** the wire and the model bake in:
- `INPUT_SAMPLE_RATE = 16_000` ([gemini.rs](../core/speaker-core/src/gemini.rs))
- `OUTPUT_SAMPLE_RATE = 24_000` (Gemini Live's emit rate)

Changing either requires updating both the resamplers and the Gemini setup
envelope (`mimeType: audio/pcm;rate=…`).

## VAD relay

Two layers. The **engine** (WebRTC `libfvad` or Silero v5) returns a boolean
"is this frame voice?" per 20 ms frame. The **`Gate`** turns that boolean
stream into "should this frame be uploaded?" with preroll and hangover so we
don't clip the first phoneme or the trailing consonant.

### Silero engine (default since v0.3 N3)

- Threshold is fixed-point 0..=1000 in settings / FFI; 500 = 0.5 probability.
- Hysteresis: opens at `threshold`, closes at `threshold − 0.15`. Stops the
  decision from chattering when the score sits right on the boundary.
- Silero v5 wants 512-sample windows; the relay feeds 320-sample frames, so
  the engine accumulates a 512-sample buffer and runs inference each time it
  fills. Between windows it returns the last computed probability so every
  frame still produces a decision.
- The ONNX model is loaded once per session, never inside the audio callback.
  The shell registers the bundled path at launch via
  `speaker_core_set_silero_model_path`.

### Gate (in `vad.rs`)

State machine, fully unit-tested independently of the engine:

```
closed ──voice──▶ open
   │              │
preroll       hangover
buffer        countdown
   │              │
   └──silence ◀── (hangover_remaining == 0)
```

- **Preroll**: while closed, silent frames are buffered in a ring (default
  capacity). When voice fires, the buffer is flushed into the output before
  the voice frame — so the upload contains the soft "h" of "hello" instead
  of just the "ello".
- **Hangover**: while open, silent frames keep being forwarded and decrement
  a counter. The gate closes when the counter hits zero (i.e. silence has
  been sustained for `hangover_total` frames). Voice resets the counter.
- **`ProcessOutput`**: per `process()` call returns
  `{ frames, opened, closed }`. `opened` and `closed` are *transitions* in
  that batch — not "currently open" state. A batch with the gate already
  open will report `opened=false, closed=false, frames=[…]`.

The relay's `frame_samples` is `sample_rate * frame_ms / 1000`. For
16 kHz / 20 ms that's 320 samples. The relay buffers leftover input across
calls so partial-frame inputs don't get dropped.

## Session recorder

`SessionRecorder` is a process-wide singleton. One session is active at a
time. Each session lives at:

```
<data-dir>/sessions/<id>/
    manifest.json
    0001-in.wav
    0002-out.wav
    0003-in.wav
    ...
```

`<id>` is the RFC-3339 UTC timestamp of session start with `:` replaced by
`-`. `<seq>` is a session-local ordinal; the suffix is `in` (VAD-gated mic
capture) or `out` (Gemini's response audio).

### Two active clips, not one

`ActiveSession` carries:

```rust
active_in:  Option<ActiveClip>,
active_out: Option<ActiveClip>,
```

The two slots are independent and each direction is finalised on its own
`end_clip` call.

**In normal operation the slots are used serially.** The turn shape is
`In open → In close → activityEnd → Out open → Out close → tail → next In`.
The echo guard (see below) drops mic frames before VAD whenever `Out` is
open or the tail is still active, so VAD cannot fire `Opened` and the
audio path will not call `begin_clip(In)` while an `Out` clip is live.
The echo guard is the real serializer between directions.

The split slots are **defense-in-depth** against the original
single-slot design. That design had one `active_clip: Option<ActiveClip>`
slot, which produced two confusing failure modes whenever the
serialization assumption broke (early builds without the echo guard, or
sub-millisecond races around `out_clip_open` being set):

1. `begin_clip(In) failed: ActiveClipExists` — the audio path tried to
   open an `In` clip while the Gemini sink was mid-burst with an `Out`
   clip already open.
2. `write_frames(In) failed: NoActiveClip` — same window, downstream
   effect. The recorder returned `NoActiveClip` whenever the requested
   direction didn't match the active slot's direction, so an `In` write
   while `Out` was active looked identical to "no clip at all".

With the split slots both failure modes are structurally impossible:
`begin_clip(In)` while `Out` is open would succeed, and `write_frames(In)`
is checked against the In slot only. In practice the echo guard means
we never exercise this path — but if a future build removes or relaxes
the guard (e.g. once AEC lands and barge-in is allowed), the recorder
no longer needs to change.

### Sequence numbers

`next_seq` increments at **begin** time, not at finalise. So two concurrent
clips always get distinct seq numbers — important because the live
`DialogueView` keys on seq to update the in-progress clip card.

### Lifecycle

| Call | Effect |
| --- | --- |
| `start_session(trigger, addr, rate, responder)` | Creates the session dir, opens an empty manifest in memory. Errors with `AlreadyActive` if a session is open. |
| `begin_clip(direction, rate)` | Opens a WAV writer for that direction's slot. Errors with `ActiveClipExists` only if that direction already has an open clip. |
| `write_frames(direction, samples)` | Appends `i16` samples to that direction's clip. `NoActiveClip` if the slot is empty. |
| `end_clip(direction)` | Finalises the WAV (sample-derived duration, not wall-clock — see below) and appends a `ClipMeta` to the session's clip list. |
| `end_session()` | Finalises any clips still open in either slot, writes `manifest.json`, releases the singleton. |

### Why sample-derived duration

`finalize_clip` computes `duration = samples_written / sample_rate`, not
`Instant::now() - begin_time`. Regression-tested. The original bug was an
`Out` clip whose wall-clock said 18.8 s but whose WAV held ≈4 s of audio,
because Gemini's bursts have gaps the writer paused through.

## Gemini Live client

The wire protocol is the `BidiGenerateContent` API. The client speaks
**manual activity detection**: we tell Gemini to disable its server-side
VAD (`realtimeInputConfig.automaticActivityDetection.disabled = true`) and
take ownership of turn boundaries ourselves via `activityStart` /
`activityEnd` markers.

### Setup envelope

Sent as the first WS Text frame:

```json
{
  "setup": {
    "model": "models/gemini-3.1-flash-live-preview",
    "generationConfig": { "responseModalities": ["AUDIO"] },
    "systemInstruction": { "parts": [ { "text": "<persona>" } ] },
    "realtimeInputConfig": {
      "automaticActivityDetection": { "disabled": true }
    }
  }
}
```

Why we disable server VAD: `libfvad` / Silero already gates uploads
locally and drops silent frames on the floor, so the server would never see
a silence transition. Without manual mode, Gemini buffers indefinitely and
never commits a turn.

Why no `safetySettings`: `BidiGenerateContentSetup` rejects the field —
`safetySettings` is REST-only. The persona text in `systemInstruction` is
the only safety lever the Live endpoint accepts.

The setup is acknowledged by a `setupComplete` server message → `GeminiEvent::SetupComplete`.

### Upload envelopes

Three shapes, all sent over the same mpsc into the WS write task so
ordering is preserved:

```json
{ "realtimeInput": { "activityStart": {} } }
{ "realtimeInput": { "audio": { "mimeType": "audio/pcm;rate=16000",
                                "data": "<base64 LE i16 PCM>" } } }
{ "realtimeInput": { "activityEnd": {} } }
```

The upload task coalesces consecutive `Audio` items into one envelope per
WS frame (bounded at 16 000 samples / 1 s) to avoid hammering the socket.
Activity markers break the coalesce and flush any pending audio before being
emitted — `activityEnd` must arrive on the wire **strictly after** the last
audio frame of its turn, otherwise the server decodes from an empty buffer.

The legacy `mediaChunks` array shape is gone — the Live API hard-rejects it
with WS 1007. The current contract is one `audio` Blob per envelope.

### Server events

The read task parses `ServerMessage` and emits `GeminiEvent`s:

| Server signal | Event | Effect in audio.rs |
| --- | --- | --- |
| `setupComplete` | `SetupComplete` | log only |
| `serverContent.modelTurn.parts[].inlineData` | `AudioChunk(Vec<i16>)` | begin `Out` clip on first, `write_frames(Out)`, advance play-out clock, push to playback queue |
| `serverContent.turnComplete = true` | `TurnComplete` | end `Out` clip |
| `serverContent.interrupted = true` | `Interrupted` | end `Out` clip, **clear playback queue**, rewind play-out clock to `now` |
| WS read error | `Error(_)` | latch `session_dead`, schedule teardown |
| WS close (clean or otherwise) | `Closed` | latch `session_dead`, schedule teardown |

### Activity protocol invariants

The server enforces strict pairing of activity markers. Violating these
causes `WS Close code=1007 reason="Precondition check failed."`:

1. Every `activityEnd` must follow an `activityStart` on the same turn.
2. A new `activityStart` mid-turn interrupts the model (with
   `START_OF_ACTIVITY_INTERRUPTS`, the default). The server responds with
   `interrupted=true`.
3. Audio frames between an `activityStart` and `activityEnd` are accepted;
   audio outside that window is undefined / probably a precondition error.

The echo-loop bug was a violation of #1 driven by acoustic feedback into
the mic — see the next section.

## Echo guard

The HFP profile on a Bluetooth speaker keeps the mic open while the speaker
is playing. So whenever Gemini's response plays through the speaker, the mic
hears it ≈200–400 ms later, and Silero — correctly — recognises it as
speech. Without intervention this triggers a spurious `activityStart`,
which the server treats as the user barging in, and the resulting
half-formed turn often falls afoul of the activity-marker invariants and
the server kicks the WS with 1007.

The mac platform has no AEC available to us on HFP. The accepted
trade-off (per the v0.1 design) is **no barge-in during a model burst**:
while the model is speaking + a short tail, we suppress input entirely.

### Mechanism

Two pieces of shared state between the Gemini sink (`AudioChunk` handler)
and the input callback:

- `out_clip_open: AtomicBool` — true between the first `AudioChunk` of a
  burst and the `TurnComplete` / `Interrupted` that ends it.
- `play_out_until_ns: AtomicU64` — monotonic ns since `session_start`
  marking when all queued model audio will have nominally finished playing.

The sink updates `play_out_until_ns` on every chunk:

```rust
let chunk_ns = samples.len() * 1e9 / OUTPUT_SAMPLE_RATE;
let prev = play_out.load();
play_out.store(max(prev, now_ns) + chunk_ns);
```

`max(prev, now_ns)` is what makes it a play-out clock: back-to-back chunks
accumulate (`prev + chunk_ns`), but a chunk after an idle stretch resumes
from `now` (`now_ns + chunk_ns`).

The input callback short-circuits while either signal says "model is
talking":

```rust
let tail_active = play_out > 0 && now_ns < play_out + ECHO_GUARD_TAIL_NS;
if out_clip_open.load() || tail_active {
    return;  // drop this cpal buffer; don't run VAD
}
```

`ECHO_GUARD_TAIL_NS = 600 ms` covers (a) the cpal output buffer drain,
(b) the Bluetooth HFP transport latency, and (c) the acoustic propagation
back into the mic. Generous on purpose — the cost is missing 600 ms of
post-burst barge-in, which we already accept.

`Interrupted` rewinds the play-out clock to `now` so the guard lifts after
just the tail, not after the (now-cancelled) projected play-out duration.

### What the input callback drops on the floor

Everything: the frame is never resampled, never fed to the VAD, never
touched by the recorder. The relay's internal state (Silero's pending
buffer, the gate's hangover counter) freezes for the duration of the
guard. When the guard lifts, processing resumes from whatever state was
last seen — which is almost always "gate closed" (the previous user turn
ended before the model started bursting), so the next user utterance opens
the gate cleanly.

### Logging

The first cpal buffer suppressed in a burst logs:

```
speaker-core: echo guard armed — input suppressed during model burst + tail
```

The latch is re-armed when the guard lifts. So one line per burst — not
one per buffer.

## Session lifecycle

`audio.rs::start_session` is the only entry point for the full pipeline:

1. Resolve default input + output devices. Sample-format must be f32
   (CoreAudio default).
2. `SessionRecorder.start_session(...)` — open the session directory.
   Errors here roll back before any network spend.
3. Build the playback queue (`VecDeque<i16>`, unbounded; preallocated for
   ~5 s of 24 kHz mono). The queue is intentionally unbounded — Gemini
   bursts arrive faster than real-time, and an earlier 5 s cap with a
   drop-from-front overflow policy garbled mid-response audio while the
   recorded WAV stayed clean (issue #4). A full response is a few
   seconds × 24 kHz × 2 B, well under a megabyte, so growth is fine.
4. Construct the sink closure (Gemini event handler) and pass it to
   `ResponderSession::start` — this opens the WebSocket and blocks ≤15 s
   on `setupComplete`. Any error here calls `end_session` to release the
   on-disk slot.
5. Build the output cpal stream (resamples 24 kHz mono → device rate).
6. Build the VAD relay (engine pinned to the user-selected `VadEngineKind`).
7. Build the input cpal stream (downmix + resample to 16 kHz, echo guard,
   VAD, recorder + upload).
8. `play()` both streams. Stash the `SessionHandle` in the slot.

Teardown is symmetric but split across two paths:

- **Synchronous** (`stop_session` from FFI or the coordinator): drop the
  `SessionHandle` (releases cpal callbacks, joins the Gemini thread on
  `GeminiSession::drop`), then `recorder.end_session()`.
- **Async** (gemini Error / Closed): `schedule_manual_teardown` spawns a
  one-shot thread that calls `stop_session()` from outside the gemini
  thread's own stack — we can't join ourselves. The `session_dead` flag
  is the once-only latch.

The input callback checks `session_dead` first and short-circuits before
the echo guard or the VAD — once Gemini is gone there's no point
running anything.

## Failure modes & log catalogue

This section is the most useful one when debugging from a stderr dump.

### Healthy session

```
speaker-core: session(Manual) default input="…" output="…"
speaker-core: session(…, responder=Gemini) input … 48000Hz/1ch → relay 16000Hz/1ch (Quality); output queue 24000Hz/1ch → … 48000Hz/2ch
speaker-core: gemini sending setup (… bytes): …
speaker-core: gemini setup sent, waiting for setupComplete
speaker-core: gemini setup complete
… user speaks …
speaker-core: vad gate OPEN [silero p=0.812]
speaker-core: gemini sending activityStart
speaker-core: vad gate CLOSED [silero p=0.090]
speaker-core: gemini sending activityEnd
… server responds …
speaker-core: gemini recv binary (NNNN bytes)
speaker-core: gemini recv binary (NNNN bytes)
… speaker plays response, mic is suppressed …
speaker-core: echo guard armed — input suppressed during model burst + tail
… next user turn …
speaker-core: vad gate OPEN [silero p=…]
…
```

### Recorder errors

| Log | Meaning |
| --- | --- |
| `manual begin_clip(In) failed: ActiveClipExists` | An `In` clip is already open — most likely a logic bug, since the only place we open one is on the `Opened` transition which should have already been balanced by a `Closed`. **Not** caused by an active `Out` clip anymore. |
| `gemini begin_clip(Out) failed: ActiveClipExists` | Same on the Out side. The sink begins a clip on the first chunk of a burst and ends it on `TurnComplete`; getting `ActiveClipExists` means a previous `TurnComplete` was lost. |
| `manual write_frames(In) failed: NoActiveClip` | `begin_clip(In)` was skipped or failed earlier. With the split slots this is almost always a bug — the only legitimate window is between Gemini going dead and the input callback noticing `session_dead`. |
| `gemini write_frames(Out) failed: NoActiveClip` | Sink's `out_clip_open` says it began a clip but the recorder disagrees. Treat as a hard bug. |
| `manual end_clip(In) failed: NoActiveClip` | Gate closed without a matching open. Possible if VAD opened then `session_dead` flipped before close — benign in that path. Otherwise investigate. |

### Gemini errors

| Log | Meaning |
| --- | --- |
| `gemini recv Close code=1007 reason="Precondition check failed."` | Activity-marker invariant violated. Almost always the echo-loop bug if it happens right after a model response. The echo guard exists to prevent this. |
| `gemini recv Close code=1011 reason=…` | Server-side internal error. Usually transient — retry the session. |
| `gemini read error: …` | Network failure on the read half. Triggers `GeminiEvent::Error` → teardown. |
| `gemini upload send failed: …` | Local mpsc to the WS write task closed (write task died). Logged **once** per session via the latch in the input callback. Usually a symptom of a closed WS, not the cause. |
| `gemini connection closed` | Read loop hit a clean close. Triggers teardown. |

### Echo guard

| Log | Meaning |
| --- | --- |
| `echo guard armed — input suppressed during model burst + tail` | First cpal buffer of a burst that was dropped. Re-arms once per burst. If you see this repeated rapidly without a model response in between, the play-out clock isn't advancing correctly (or chunks are being delivered with the wrong sample count). |

Absence of this line during a model response is *also* diagnostic — it
means the model produced no audio chunks (likely a safety block or a
malformed response).

## Known limitations and trade-offs

- **No mid-burst barge-in.** The echo guard suppresses input while the
  model is talking + 600 ms of tail. Accepted because HFP doesn't give us
  AEC. If a future build adds AEC, lower or remove the guard.
- **VAD freezes while echo-guarded.** Silero's pending window and the
  Gate's hangover counter both stop advancing for the duration of the
  guard. Effectively that means the relay starts the next user turn from
  whatever state it had at guard-arm time. In practice this is "gate
  closed", which is exactly what we want.
- **Sample-derived clip duration.** A clip's duration in the manifest is
  `samples_written / sample_rate`, not wall-clock. If you ever see a
  duration much smaller than the audio in the WAV implies, suspect a
  bug in `write_frames` (mis-counting samples).
- **One session at a time.** The `SessionRecorder` singleton and the
  audio.rs session slot both enforce this. The coordinator should never
  call `start_session` while one is already open — if you see
  `AlreadyRunning` from FFI, something upstream is double-firing.
- **No tone control on Silero's threshold across engines.** WebRTC's
  sensitivity (`Quality` ... `VeryAggressive`) and Silero's threshold
  (`0..=1000`) are separate settings. The settings UI shows whichever is
  relevant for the selected engine; don't expect one knob to apply to
  both.

## When to update this doc

Update it whenever you change:

- the wire protocol with Gemini (envelope shape, activity markers, model id);
- the recorder's invariants (slot shape, seq numbering, clip directions);
- the VAD pipeline (engine, framing, hysteresis, preroll/hangover);
- the echo-guard mechanism (signals, tail duration, sink hooks);
- the lifecycle ordering between session start, Gemini connect, and stream play.

The log catalogue is the part most likely to drift. Grep the source for the
string before trusting an entry above.
