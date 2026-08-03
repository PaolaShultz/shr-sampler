# SHR Sampler

SHR Sampler is a separate sample-instrument engine for the SHR music system.
It validates strict `.shrinst` packages, imports one controlled SFZ subset,
renders deterministic stereo `f32` audio, writes offline WAV files, and can run
as a headless JACK/ALSA instrument process. SHR-DAW integration remains a
later milestone.

The library loads and decodes every sample before constructing an engine.
`Engine::render_block` then uses fixed voice storage and performs no allocation,
locking, I/O, logging, formatting, or process work. Tests enforce exact idle
silence, finite output, deterministic event behavior, stable voice stealing,
and identical output across block sizes.

The live host attaches dynamically to an existing JACK server without starting
or changing it. It exposes `out_l`, `out_r`, and one ALSA Sequencer MIDI port
named `input`; it never auto-connects audio or opens ALSA audio. MIDI transfer,
callback work, overflow recovery, faults, and shutdown are fixed and bounded.

## Build and use

The workspace pins Rust 1.97.1.

```sh
cargo build --locked
cargo run --locked -p shr-sampler -- --client-name shr-sampler \
  --instrument path/to/piano.shrinst
cargo run --locked -p shr-sampler -- validate path/to/piano.shrinst
cargo run --locked -p shr-sampler -- render path/to/piano.shrinst artifacts/piano.wav
```

Import deliberately profiled SFZ produced offline by ConvertWithMoss or another
tool:

```sh
cargo run --locked -p shr-sampler -- import-sfz source.sfz piano.shrinst \
  --instrument-id piano --name "Piano" --author "Source author" \
  --source "Library and conversion provenance" --licence "Verified licence"
```

The importer refuses existing output, escaping paths, symlinks, non-WAV
samples, unsupported behavior, and any conversion it cannot represent
faithfully. ConvertWithMoss is neither invoked nor linked by SHR Sampler.

## Documentation

- [Native package format](docs/NATIVE_PACKAGE_FORMAT.md)
- [Controlled SFZ import profile](docs/SFZ_IMPORT_PROFILE.md)
- [Host architecture](docs/HOST_ARCHITECTURE.md)
- [Live process contract](docs/LIVE_PROCESS_CONTRACT.md)
- [Dependencies and licences](THIRD_PARTY.md)

Generated audition and evidence audio belongs below ignored `artifacts/` and is
disposable. Tests generate only project-authored synthetic material.
