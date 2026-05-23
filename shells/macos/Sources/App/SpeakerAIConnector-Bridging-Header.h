#ifndef SPEAKER_AI_CONNECTOR_BRIDGING_HEADER_H
#define SPEAKER_AI_CONNECTOR_BRIDGING_HEADER_H

// Hand-rolled C ABI to the Rust core (core/speaker-core).
// FFI binding strategy decided in M6: hand-written stays. The surface
// is small enough (~25 functions) that codegen tools like uniffi or
// swift-bridge would add build cost for little benefit. See
// docs/v0.1-design.md §"FFI surface".

const char *speaker_core_version(void);

// Returns 0 on success, negative AudioError code on failure.
int speaker_core_audio_loopback_start(void);

// Safe to call when no loopback is running.
void speaker_core_audio_loopback_stop(void);

// Override the macOS system default output to the BT speaker whose UID
// embeds the given MAC address. Returns 0 on success, negative on error
// (-20 CoreAudio failure, -21 no matching device, -100 bad address).
int speaker_core_audio_force_default_output(const char *address);

// Start the M3 VAD diagnostic: default input → 16 kHz mono → libfvad
// relay, logging gate open/close transitions to stderr. `sensitivity`
// is 0..=3 (Quality, LowBitrate, Aggressive, VeryAggressive). Returns
// 0 on success, -101 if sensitivity is out of range, or a negative
// AudioError code on capture failure.
int speaker_core_vad_diagnostic_start(unsigned char sensitivity);

// Safe to call when no diagnostic is running.
void speaker_core_vad_diagnostic_stop(void);

// Session history (M4) — file paths and JSON metadata only; raw PCM
// never crosses the FFI line. All returned strings are UTF-8 NUL-
// terminated and must be freed with speaker_core_string_free.

// Absolute path to the sessions directory. NULL on error.
char *speaker_core_sessions_root(void);

// JSON array of SessionMeta, newest first. NULL on error.
char *speaker_core_sessions_list(void);

// JSON array of ClipMeta for one session. NULL on error / not found.
char *speaker_core_sessions_clips(const char *session_id);

// Absolute path to a clip's WAV file. NULL on error / not found.
char *speaker_core_sessions_clip_path(const char *session_id, const char *clip_file);

// Free a string returned by any of the speaker_core_sessions_*
// functions. Safe to call with NULL.
void speaker_core_string_free(char *ptr);

// --- API key (M5) ---------------------------------------------------
// API key lives in the macOS Keychain via `keyring`. Empty strings are
// rejected; use `_clear` to remove the entry.

// Returns 0 on success, negative ConfigError code on failure.
int speaker_core_api_key_set(const char *key);

// UTF-8 NUL-terminated key, or NULL if unset / on error.
// Caller frees with speaker_core_string_free.
char *speaker_core_api_key_get(void);

// 1 if set, 0 if unset, negative on error.
int speaker_core_api_key_has(void);

// 0 on success (idempotent: 0 if already absent), negative on error.
int speaker_core_api_key_clear(void);

// --- Manual session (M5) --------------------------------------------
// Start the end-to-end developer/debug session: default input → VAD →
// Gemini Live → default output. Reads the API key from Keychain.
// `model` may be NULL for the default model. Blocks ≤15 s on the
// initial WebSocket handshake so an auth failure surfaces synchronously.
//
// Returns 0 on success. Negative codes include:
//   -101  invalid sensitivity (must be 0..=3)
//   -300  NoApiKey       (no key in Keychain)
//   -301  AuthFailed     (Gemini rejected the key)
//   -302  Network        (DNS/TLS/HTTP failure)
//   other AudioError codes for capture/playback issues.
int speaker_core_manual_session_start(unsigned char sensitivity, const char *model);

// Idempotent — safe to call when no manual session is running.
void speaker_core_manual_session_stop(void);

// Last asynchronous session error (set by the Gemini WS task). Polled
// by the shell after the session ends to render a typed menu-bar
// message. Codes mirror the start function's table; tag is stable
// machine-readable ("no_api_key" / "auth_failed" / "network" /
// "safety_blocked" / "other"). Strings are NUL-terminated UTF-8 and
// the caller frees them with speaker_core_string_free.

int speaker_core_last_session_error_code(void);
char *speaker_core_last_session_error_message(void);
char *speaker_core_last_session_error_tag(void);
void speaker_core_last_session_error_clear(void);

// --- Coordinator (M6) -----------------------------------------------
// BTEvent in / SessionCommand in / StatusEvent JSON out. Returned
// strings must be freed with speaker_core_string_free.
//
// StatusEvent JSON shape:
//   { "variant": "idle" | "no_device_selected" | "waiting_for_device"
//              | "session_launching" | "session_active"
//              | "manual_session_launching" | "manual_session_active"
//              | "tearing_down" | "error",
//     "name": "..."    (waiting_for_device / launching / active / tearing_down)
//     "message": "..." (error only)
//   }

char *speaker_core_coord_push_bt_connect(const char *address, const char *name);
char *speaker_core_coord_push_bt_disconnect(const char *address, const char *name);

// command: 0 = Start, 1 = Stop (anything else treated as Stop).
char *speaker_core_coord_push_command(int command);

// Current status snapshot (no state change). Caller frees the JSON.
char *speaker_core_coord_status(void);

// Monotonic revision counter — bumps on every state change. Cheap;
// poll this on a timer to skip JSON decode when nothing has changed.
unsigned long long speaker_core_coord_revision(void);

// Test-now helpers: simulate a connect/disconnect for the configured target.
char *speaker_core_coord_simulate_connect(void);
char *speaker_core_coord_simulate_disconnect(void);

// --- Non-secret settings (M6) ---------------------------------------
// TOML at <data-dir>/config.toml. JSON shape:
//   { "target_address": "aa:bb:..." | null,
//     "model": "models/gemini-...",
//     "vad_sensitivity": "Quality" | "LowBitrate" | "Aggressive" | "VeryAggressive",
//     "silence_timeout_ms": 700,
//     "force_default_output": false }
char *speaker_core_settings_get(void);

// All setters persist to TOML and refresh the coordinator's cached
// snapshot. Return 0 on success, negative ConfigError code on failure
// (-100 invalid input, -101 invalid sensitivity, -402 io, -403 toml).
int speaker_core_settings_set_target(const char *address);   // NULL clears
int speaker_core_settings_set_model(const char *model);
int speaker_core_settings_set_vad_sensitivity(unsigned char level);
int speaker_core_settings_set_silence_timeout_ms(unsigned int ms);
int speaker_core_settings_set_force_default_output(int enabled);

#endif
