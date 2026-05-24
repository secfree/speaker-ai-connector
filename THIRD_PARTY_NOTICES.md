# Third-Party Notices

Speaker AI Connector bundles or links against the following third-party
components. License texts are reproduced (or summarized + linked) below.

Crate dependencies linked into the Rust core are subject to their own
licenses, declared in each crate's metadata; this file covers
components whose runtime artifacts ship inside the application bundle.

## Silero VAD (v5)

- **Component:** `silero_vad.onnx` (ONNX weights, ~2.2 MB)
- **Bundled at:** `shells/macos/Resources/silero_vad.onnx` →
  `SpeakerAIConnector.app/Contents/Resources/silero_vad.onnx`
- **Upstream:** <https://github.com/snakers4/silero-vad>
- **Version pin:** v5.1.2 (see `shells/macos/Resources/fetch-silero.sh`
  for the SHA-256-verified download URL)
- **License:** MIT

```
MIT License

Copyright (c) 2024 snakers4

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## ONNX Runtime

- **Component:** prebuilt `libonnxruntime` dynamic library, downloaded
  at build time by the `ort` Rust crate (with the `download-binaries`
  feature) and linked into `libspeaker_core`.
- **Upstream:** <https://github.com/microsoft/onnxruntime>
- **License:** MIT — see <https://github.com/microsoft/onnxruntime/blob/main/LICENSE>
