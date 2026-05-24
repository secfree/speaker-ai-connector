# Resources

Files bundled into `SpeakerAIConnector.app/Contents/Resources/`. XcodeGen
wires the copy step via `shells/macos/project.yml` (see the `Resources`
entry under `sources`).

## `silero_vad.onnx`

Silero v5 voice-activity detector — 16 kHz mono, 512-sample windows.
The Rust core (`core/speaker-core/src/vad_silero.rs`) loads it at session
start when the user has selected the Silero engine in Settings. Path is
handed to the core via `speaker_core_set_silero_model_path` at app
launch.

- **Upstream:** <https://github.com/snakers4/silero-vad>
- **Version:** v5.1.2 (commit-tagged release)
- **License:** MIT (see `THIRD_PARTY_NOTICES.md` at repo root)
- **Size:** ~2.2 MB
- **SHA-256:** `2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f`

Refresh with:

```sh
./fetch-silero.sh
```

The script downloads the pinned release tag and prints the SHA-256 so
you can spot if the upstream tag was retagged.
