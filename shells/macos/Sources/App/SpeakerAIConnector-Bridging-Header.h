#ifndef SPEAKER_AI_CONNECTOR_BRIDGING_HEADER_H
#define SPEAKER_AI_CONNECTOR_BRIDGING_HEADER_H

// Hand-rolled C ABI to the Rust core (core/speaker-core).
// Long-term binding strategy (uniffi vs. swift-bridge vs. hand-rolled)
// is an M5 decision; this header stays small and explicit until then.

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

#endif
