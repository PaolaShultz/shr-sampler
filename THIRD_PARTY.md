# Dependencies and licensing

SHR Sampler source is MIT licensed. The locked dependency graph was inspected
from Cargo package metadata during the 0.1.2 integration milestone.

Runtime dependencies are:

| Crate | Purpose | Declared licence |
| --- | --- | --- |
| `alsa` | ALSA Sequencer MIDI input only; no ALSA audio | MIT OR Apache-2.0 |
| `hound` | Strict WAV reading and deterministic float WAV writing | Apache-2.0 |
| `libc` | Dynamic JACK symbol loading and Linux FFI types | MIT OR Apache-2.0 |
| `serde`, `serde_json` | Strict native JSON schema | MIT OR Apache-2.0 |
| `sha2` | SHA-256 package verification | MIT OR Apache-2.0 |
| `signal-hook` | SIGINT and SIGTERM flag publication | MIT OR Apache-2.0 |
| `thiserror` | Typed public errors | MIT OR Apache-2.0 |

The transitive runtime graph declares permissive alternatives: MIT,
Apache-2.0, BSD-compatible, Unlicense, or Unicode-3.0 terms. Development-only
`assert_no_alloc` is BSD-1-Clause; `tempfile` and its selected transitive graph
offer MIT/Apache-2.0-compatible choices. Target-specific metadata includes
packages with licence alternatives such as `r-efi` (`MIT OR Apache-2.0 OR
LGPL-2.1-or-later`); this project selects a permissive alternative and does not
incorporate LGPL-only code.

`alsa-sys` is the only native-library binding added by the host and declares
MIT OR Apache-2.0. It links the system ALSA library for Sequencer access. JACK
is loaded dynamically from the system `libjack.so.0`; no JACK crate, header,
source, or library is bundled. `pkg-config`, `cc`, `shlex`, and
`find-msvc-tools` are build-only dependency tooling selected by the locked
graph.

`deny.toml` records the accepted permissive licence, registry, wildcard, and
yanked-package policy used by the milestone dependency gate.

ConvertWithMoss is an external offline user tool, not a dependency. Its
official project is LGPLv3. No ConvertWithMoss source, binaries, proprietary
format parsers, factory instruments, or samples are copied into this
repository.

All committed fixtures are project-authored text. Tests synthesize their WAV
input at runtime. A package author remains responsible for verifying and
preserving the licence, attribution, and redistribution rights of imported
sample libraries.

## Public instrument content

The only installable factory package is named by
`instruments/cleared-instruments.txt`. `SHR Clear Tone` and its PCM waveform
are project-authored for SHR Sampler and dedicated to the public domain under
CC0-1.0. The generator uses only Python's standard library and mathematical
sine functions; it contains no third-party sample, preset, recording, SFZ,
proprietary format data, or converted library content.
