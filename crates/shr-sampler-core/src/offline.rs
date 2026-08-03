use crate::engine::{Engine, EngineError, Event, TimedEvent};
use crate::package::PreparedInstrument;
use std::path::Path;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderEvent {
    pub frame: u64,
    pub event: Event,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderSpec {
    pub sample_rate: u32,
    pub frames: u64,
    pub block_size: usize,
}

#[derive(Debug, Error)]
pub enum OfflineError {
    #[error("sample rate must be between 8,000 and 384,000 Hz")]
    InvalidSampleRate,
    #[error("render must contain 1 frame through 600 seconds")]
    InvalidFrameCount,
    #[error("block size must be 1..=65,536 frames")]
    InvalidBlockSize,
    #[error("render events must be ordered and lie inside the render")]
    InvalidEvents,
    #[error("render is too large for this platform")]
    PlatformLimit,
    #[error("engine error: {0}")]
    Engine(#[from] EngineError),
    #[error("WAV error: {0}")]
    Wav(#[from] hound::Error),
}

impl RenderSpec {
    fn validate(self) -> Result<usize, OfflineError> {
        if !(8_000..=384_000).contains(&self.sample_rate) {
            return Err(OfflineError::InvalidSampleRate);
        }
        if self.frames == 0 || self.frames > u64::from(self.sample_rate) * 600 {
            return Err(OfflineError::InvalidFrameCount);
        }
        if !(1..=65_536).contains(&self.block_size) {
            return Err(OfflineError::InvalidBlockSize);
        }
        usize::try_from(self.frames).map_err(|_| OfflineError::PlatformLimit)
    }
}

pub fn render(
    instrument: &PreparedInstrument,
    spec: RenderSpec,
    events: &[RenderEvent],
) -> Result<Vec<f32>, OfflineError> {
    let frame_count = spec.validate()?;
    if events.windows(2).any(|pair| pair[0].frame > pair[1].frame)
        || events
            .last()
            .is_some_and(|event| event.frame >= spec.frames)
    {
        return Err(OfflineError::InvalidEvents);
    }
    let sample_count = frame_count
        .checked_mul(2)
        .ok_or(OfflineError::PlatformLimit)?;
    let mut output = vec![0.0; sample_count];
    let mut engine = Engine::new(instrument, spec.sample_rate as f32)?;
    let mut left = vec![0.0; spec.block_size.min(frame_count)];
    let mut right = vec![0.0; left.len()];
    let mut event_index = 0;
    let mut start = 0usize;
    while start < frame_count {
        let length = spec.block_size.min(frame_count - start);
        let end = start + length;
        let first_event = event_index;
        while event_index < events.len() && events[event_index].frame < end as u64 {
            event_index += 1;
        }
        let block_events = events[first_event..event_index]
            .iter()
            .map(|event| TimedEvent::new(event.frame as usize - start, event.event))
            .collect::<Vec<_>>();
        engine.render_block(&block_events, &mut left[..length], &mut right[..length])?;
        for index in 0..length {
            output[(start + index) * 2] = left[index];
            output[(start + index) * 2 + 1] = right[index];
        }
        start = end;
    }
    Ok(output)
}

pub fn write_wav(
    path: impl AsRef<Path>,
    instrument: &PreparedInstrument,
    spec: RenderSpec,
    events: &[RenderEvent],
) -> Result<(), OfflineError> {
    let samples = render(instrument, spec, events)?;
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 2,
            sample_rate: spec.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )?;
    for sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{PreparedSample, PreparedZone, SampleFrame};
    use crate::schema::{
        Adsr, InstrumentManifest, InstrumentMetadata, LoopMode, PlaybackMode, SampleMetadata,
        SampleRef, ZoneManifest,
    };

    fn instrument() -> PreparedInstrument {
        let frames = (0..256)
            .map(|index| SampleFrame {
                left: ((index as f32) * 0.017).sin() * 0.5,
                right: ((index as f32) * 0.023).sin() * 0.4,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let envelope = Adsr {
            attack_seconds: 0.001,
            decay_seconds: 0.002,
            sustain_level: 0.7,
            release_seconds: 0.003,
        };
        PreparedInstrument {
            manifest: InstrumentManifest {
                format_version: 1,
                instrument_id: "offline-fixture".into(),
                display_name: "Offline Fixture".into(),
                metadata: InstrumentMetadata {
                    author: "SHR tests".into(),
                    source: "Synthetic formula".into(),
                    licence: "MIT".into(),
                    attribution: "None".into(),
                    notes: "Test".into(),
                },
                max_polyphony: 2,
                zones: vec![ZoneManifest {
                    id: "zone-001".into(),
                    sample: SampleRef {
                        path: "samples/tone.wav".into(),
                        sha256: "0".repeat(64),
                        metadata: SampleMetadata {
                            sample_rate: 8_000,
                            channels: 2,
                            frames: 256,
                        },
                    },
                    key_low: 0,
                    key_high: 127,
                    velocity_low: 1,
                    velocity_high: 127,
                    root_note: 60,
                    coarse_transpose: 0,
                    fine_tune_cents: 0.0,
                    gain_db: 0.0,
                    pan: 0.0,
                    playback_mode: PlaybackMode::Gated,
                    loop_mode: LoopMode::None,
                    envelope,
                    sample_start_frame: 0,
                    sample_end_frame: None,
                }],
            },
            samples: vec![PreparedSample {
                frames,
                sample_rate: 8_000,
                original_channels: 2,
            }]
            .into_boxed_slice(),
            zones: vec![PreparedZone {
                sample_index: 0,
                key_low: 0,
                key_high: 127,
                velocity_low: 1,
                velocity_high: 127,
                root_note: 60,
                pitch_offset_cents: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
                playback_mode: PlaybackMode::Gated,
                loop_range: None,
                envelope,
                sample_start: 0,
                sample_end: 256,
            }]
            .into_boxed_slice(),
        }
    }

    #[test]
    fn rendering_is_identical_across_block_sizes() {
        let instrument = instrument();
        let events = [
            RenderEvent {
                frame: 3,
                event: Event::NoteOn {
                    note: 60,
                    velocity: 101,
                },
            },
            RenderEvent {
                frame: 79,
                event: Event::NoteOff { note: 60 },
            },
        ];
        let small = render(
            &instrument,
            RenderSpec {
                sample_rate: 8_000,
                frames: 160,
                block_size: 7,
            },
            &events,
        )
        .unwrap();
        let large = render(
            &instrument,
            RenderSpec {
                sample_rate: 8_000,
                frames: 160,
                block_size: 128,
            },
            &events,
        )
        .unwrap();
        assert_eq!(small, large);
        assert!(small.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn offline_wav_output_is_byte_identical_and_stereo_float() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.wav");
        let second = directory.path().join("second.wav");
        let spec = RenderSpec {
            sample_rate: 8_000,
            frames: 128,
            block_size: 31,
        };
        let events = [RenderEvent {
            frame: 0,
            event: Event::NoteOn {
                note: 60,
                velocity: 127,
            },
        }];
        write_wav(&first, &instrument(), spec, &events).unwrap();
        write_wav(&second, &instrument(), spec, &events).unwrap();
        assert_eq!(
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap()
        );
        let reader = hound::WavReader::open(first).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
        assert_eq!(reader.duration(), 128);
    }
}
