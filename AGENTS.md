# Repository instructions

- Keep package loading, sample decoding, SFZ parsing, filesystem access, and
  WAV writing outside `Engine::render_block`.
- The prepared render path may not allocate or deallocate, lock, perform I/O,
  log, format, spawn, or do avoidable per-sample transcendental setup.
- Use only project-authored synthetic material in tests. Generated audition and
  evidence audio belongs below ignored `artifacts/` and is disposable.
- Do not add JACK, ALSA, MIDI, recording, playback, or hardware tests without
  explicit authorization.
- Proprietary sample formats and ConvertWithMoss remain offline external input
  preparation. Never parse or invoke them from this engine.
- Run focused tests while implementing. At milestone/release boundaries run
  formatting, the complete normal suite, warning-denied Clippy, locked check,
  and the locked debug build. Keep large matrices and benchmarks opt-in.
- Do not claim Raspberry Pi performance, latency, polyphony, or sound quality
  without later native evidence.
