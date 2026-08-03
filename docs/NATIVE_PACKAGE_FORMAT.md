# `.shrinst` native package format 1

A native instrument is a real, non-symlink directory ending in `.shrinst`. It
contains exactly one strict UTF-8 `manifest.json` plus the WAV files referenced
by that manifest. Unknown JSON fields, unreferenced files, symlinks at any path
component, special files, unsafe paths, invalid bounds, and unsupported enum
values are errors.

All floating-point values below must be finite. Loading, hashing, WAV decoding,
memory allocation, gain/pan preparation, and package validation finish before
an `Engine` is constructed.

## Package limits

- `manifest.json`: at most 1 MiB;
- package: at most 257 files and 64 subdirectories;
- zones: 1 through 512;
- unique WAV files: at most 256;
- maximum polyphony: 1 through 64 voices;
- sample file: at most 256 MiB on disk;
- total decoded samples: at most 256 MiB, counted as 8 bytes per stereo frame;
- sample rate: 8,000 through 384,000 Hz;
- WAV: mono or stereo integer PCM 8–32 bit, or float32;
- sample paths: slash-separated relative components only, contained in the
  package;
- SHA-256: exactly 64 hexadecimal characters and verified over the source WAV
  bytes.

Decoded channel count, sample rate, and frame count must exactly match the
manifest. Mono is duplicated to stereo. Finite float input outside `[-1, 1]`
is clamped during preparation; non-finite float samples are refused.

## Manifest

Every field is required unless marked optional.

```json
{
  "format_version": 1,
  "instrument_id": "example-piano",
  "display_name": "Example Piano",
  "metadata": {
    "author": "Author",
    "source": "Source and conversion provenance",
    "licence": "Verified licence",
    "attribution": "Required attribution or None",
    "notes": "Package notes"
  },
  "max_polyphony": 16,
  "zones": [
    {
      "id": "zone-001",
      "sample": {
        "path": "samples/0001.wav",
        "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
        "metadata": { "sample_rate": 48000, "channels": 1, "frames": 96000 }
      },
      "key_low": 36,
      "key_high": 60,
      "velocity_low": 1,
      "velocity_high": 127,
      "root_note": 48,
      "coarse_transpose": 0,
      "fine_tune_cents": 0.0,
      "gain_db": -3.0,
      "pan": 0.0,
      "playback_mode": "gated",
      "loop_mode": { "mode": "forward", "start_frame": 12000, "end_frame": 24000 },
      "envelope": {
        "attack_seconds": 0.01,
        "decay_seconds": 0.2,
        "sustain_level": 0.8,
        "release_seconds": 0.3
      },
      "sample_start_frame": 0,
      "sample_end_frame": 90000
    }
  ]
}
```

`instrument_id` and zone `id` use 1–64 lowercase ASCII letters, digits, or
hyphens. Zone IDs are unique. Display and metadata strings are nonblank UTF-8
without control characters and have documented implementation bounds.

## Zone fields and render semantics

- Key ranges are inclusive MIDI notes `0..=127`. Velocity ranges are inclusive
  `1..=127`. Every matching zone layers in manifest order, subject to the voice
  limit. Velocity also applies a linear `velocity / 127` amplitude gain.
- `root_note` is `0..=127`; `coarse_transpose` is `-48..=48` semitones and
  `fine_tune_cents` is `-1200..=1200`. Pitch is prepared at note-on from note,
  root, tuning, source rate, and output rate.
- `gain_db` is `-96..=24`. `pan` is `-1..=1` and acts as stereo balance: center
  preserves both channels, left attenuates right, and right attenuates left.
- `sample_start_frame` defaults to zero. Optional `sample_end_frame` is an
  exclusive bound and defaults to decoded length.
- `loop_mode` is `{"mode":"none"}` or a forward loop whose `start_frame` is
  inclusive and `end_frame` exclusive. It must lie inside the playable sample
  range. A forward loop repeats while the key or sustain pedal holds the voice;
  release exits the loop and plays the remaining sample under the release
  envelope.
- `one-shot` ignores note-off and sustain and plays to the sample end. It cannot
  loop. `gated` enters envelope release on note-off unless sustain is down.
- ADSR time ranges are `0..=60` seconds, sustain is `0..=1`, and segments are
  deterministic linear ramps. Zero times transition immediately.

Pitch shifting uses prepared four-point cubic interpolation. Per-sample work is
bounded. Idle rendering writes exact `0.0`; non-finite intermediate output is
replaced with `0.0`.

Each note-on receives one monotonically ordered age. At the voice limit the
engine deterministically prefers an idle slot, then steals the oldest released
voice, then the oldest held voice, using storage order as the final tie-break.
Note-off releases every gated voice for that MIDI note. Sustain release affects
only voices whose keys were already released. All Notes Off clears every voice
and the pedal immediately.
