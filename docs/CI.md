# Continuous integration

[The workflow](../.github/workflows/ci.yml) runs on pushes to `main`, pull
requests and manual dispatch. It uses read-only repository permissions, cancels
superseded runs and selects exact Rust 1.97.1 with locked Cargo dependencies.

Both workspace members receive locked all-target checks and normal synthetic package, SFZ, engine, CLI and host contracts. The existing factory generator runs in check-only mode.

The workflow contains the exact reproducible commands. Historical/exhaustive
auditions and benchmarks remain opt-in. No physical audio, MIDI, DMX, playback,
service activation, media download or deployment is part of these checks.
Compilation and synthetic tests do not establish Raspberry Pi hardware acceptance.
Clippy with warnings denied and release builds are not added as new CI gates.
