# Controlled SFZ import profile

`shr-sampler import-sfz` is an offline converter into native format 1. Its
purpose is to accept a small, reviewable output profile from ConvertWithMoss,
not arbitrary SFZ. ConvertWithMoss remains a separate desktop tool and is never
invoked, linked, or embedded in the engine.

The official ConvertWithMoss site lists SFZ and many proprietary source
formats, including Akai, Ensoniq, Kontakt, Korg, Kurzweil, Roland, and Yamaha.
Its manual also warns that unsupported SFZ input opcodes are merely logged when
that source option is enabled. SHR therefore performs its own strict refusal:

- <https://www.mossgrabers.de/Software/ConvertWithMoss/ConvertWithMoss.html>
- <https://www.mossgrabers.de/Software/ConvertWithMoss/ConvertWithMoss-Installers-Manual/ConvertWithMoss-Manual.pdf>

Use ConvertWithMoss with WAV destination samples, no filters, LFOs, crossfades,
round robins, release triggers, reverse playback, pitch/key tracking changes,
or envelope stages beyond ADSR. A source which needs those features is outside
this milestone and must not be described as faithfully converted.

## Grammar and inheritance

The input is a regular, non-symlink, strict UTF-8 `.sfz` file of at most 1 MiB.
Only `<global>`, `<group>`, and `<region>` headers are accepted. `//` line
comments are accepted; preprocessor directives, includes, block comments,
quoted values, and multiple headers on one line are not.

Opcodes use lowercase `name=value`. Values may continue through spaces until
the next recognizable `name=` on that line, allowing ConvertWithMoss sample
filenames with spaces. A duplicate opcode in one header scope is refused.
Global values are inherited by groups and regions; group values override
global values; region values override both.

Sample references may use `/` or `\` in SFZ input but must resolve to a regular
WAV below the SFZ directory without symlinks. Imported samples are copied to
deterministic native names and hashed. Existing output is never replaced. The
completed temporary package is reloaded through the ordinary native validator
before it is renamed into place.

## Accepted opcodes

| Opcode | Accepted semantics |
| --- | --- |
| `sample` | Required relative WAV path. FLAC, OGG, AIFF, NCW, and escaping paths are refused. |
| `key` | Integer `0..=127`; sets low key, high key, and root. Cannot coexist with `lokey`, `hikey`, or `pitch_keycenter`. |
| `lokey`, `hikey` | Inclusive integer range; defaults `0` and `127`. |
| `lovel`, `hivel` | Inclusive integer range `1..=127`; defaults `1` and `127`. |
| `pitch_keycenter` | Integer root note; defaults to MIDI 60 when `key` is absent. |
| `transpose` | Integer semitones `-48..=48`. |
| `tune` | Cents `-1200..=1200`. |
| `volume` | Decibels `-96..=24`. |
| `pan` | SFZ percent `-100..=100`, converted to native `-1..=1` stereo balance. |
| `offset` | Inclusive first source frame; defaults to zero. |
| `end` | Inclusive last SFZ frame, converted to the native exclusive end. |
| `loop_mode` | `no_loop`, `one_shot`, or `loop_sustain`. Missing means `no_loop`. `loop_continuous` is refused because native format 1 has release-exit loops only. |
| `loop_start`, `loop_end` | Required together for `loop_sustain`; SFZ end is inclusive and is converted to native exclusive end. |
| `ampeg_attack`, `ampeg_decay`, `ampeg_release` | Seconds `0..=60`; defaults to zero. |
| `ampeg_sustain` | Percent `0..=100`; defaults to 100. |

The documented harmless-ignore list is exactly `global_label`, `group_label`,
and `region_label`. The CLI reports which of these appeared.

Every other opcode is refused and reported with its source line and header.
This includes `polyphony`, `seq_length`, `seq_position`, `trigger`, `direction`,
key/velocity crossfades, `pitch_keytrack`, pitch bend, filter and filter
envelope opcodes, pitch/amplitude LFOs, `amp_veltrack`, custom velocity curves,
envelope delay/start/hold/shape, loop type/crossfade/tuning, and alternate loop
spellings. The importer never silently drops sound-changing behavior.
