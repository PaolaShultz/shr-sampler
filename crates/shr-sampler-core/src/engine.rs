use crate::package::{PreparedInstrument, PreparedLoop, PreparedZone, SampleFrame};
use crate::schema::{Adsr, PlaybackMode};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8 },
    Sustain { down: bool },
    AllNotesOff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimedEvent {
    pub sample_offset: usize,
    pub event: Event,
}

impl TimedEvent {
    pub const fn new(sample_offset: usize, event: Event) -> Self {
        Self {
            sample_offset,
            event,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum EngineError {
    #[error("sample rate must be finite and between 8,000 and 384,000 Hz")]
    InvalidSampleRate,
    #[error("left and right output buffers must have the same length")]
    BufferLengthMismatch,
    #[error("events must be ordered by sample offset")]
    EventsOutOfOrder,
    #[error("event sample offset lies outside the output block")]
    EventOutsideBlock,
}

#[derive(Clone, Copy, Debug)]
struct PreparedEnvelope {
    sustain: f32,
    attack_step: f32,
    decay_step: f32,
    release_samples: u32,
}

impl PreparedEnvelope {
    fn new(config: Adsr, sample_rate: f32) -> Self {
        let attack_samples = seconds_to_samples(config.attack_seconds, sample_rate);
        let decay_samples = seconds_to_samples(config.decay_seconds, sample_rate);
        Self {
            sustain: config.sustain_level,
            attack_step: if attack_samples == 0 {
                1.0
            } else {
                1.0 / attack_samples as f32
            },
            decay_step: if decay_samples == 0 {
                1.0
            } else {
                (1.0 - config.sustain_level) / decay_samples as f32
            },
            release_samples: seconds_to_samples(config.release_seconds, sample_rate),
        }
    }
}

fn seconds_to_samples(seconds: f32, sample_rate: f32) -> u32 {
    if seconds <= 0.0 {
        0
    } else {
        (seconds * sample_rate).round().clamp(1.0, u32::MAX as f32) as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvelopeStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy, Debug)]
struct EnvelopeState {
    stage: EnvelopeStage,
    level: f32,
    release_step: f32,
}

impl EnvelopeState {
    const fn idle() -> Self {
        Self {
            stage: EnvelopeStage::Idle,
            level: 0.0,
            release_step: 0.0,
        }
    }

    fn start(&mut self, config: PreparedEnvelope) {
        self.release_step = 0.0;
        if config.attack_step >= 1.0 {
            self.level = 1.0;
            if config.decay_step >= 1.0 - config.sustain {
                self.level = config.sustain;
                self.stage = EnvelopeStage::Sustain;
            } else {
                self.stage = EnvelopeStage::Decay;
            }
        } else {
            self.level = 0.0;
            self.stage = EnvelopeStage::Attack;
        }
    }

    fn release(&mut self, config: PreparedEnvelope) {
        if self.stage == EnvelopeStage::Idle || self.stage == EnvelopeStage::Release {
            return;
        }
        if config.release_samples == 0 || self.level <= 0.0 {
            self.reset();
        } else {
            self.release_step = self.level / config.release_samples as f32;
            self.stage = EnvelopeStage::Release;
        }
    }

    fn next(&mut self, config: PreparedEnvelope) -> f32 {
        match self.stage {
            EnvelopeStage::Idle => 0.0,
            EnvelopeStage::Attack => {
                self.level = (self.level + config.attack_step).min(1.0);
                if self.level >= 1.0 {
                    self.stage = if config.decay_step >= 1.0 - config.sustain {
                        self.level = config.sustain;
                        EnvelopeStage::Sustain
                    } else {
                        EnvelopeStage::Decay
                    };
                }
                self.level
            }
            EnvelopeStage::Decay => {
                self.level = (self.level - config.decay_step).max(config.sustain);
                if self.level <= config.sustain {
                    self.stage = EnvelopeStage::Sustain;
                }
                self.level
            }
            EnvelopeStage::Sustain => config.sustain,
            EnvelopeStage::Release => {
                self.level = (self.level - self.release_step).max(0.0);
                if self.level <= 0.0 {
                    self.reset();
                }
                self.level
            }
        }
    }

    fn reset(&mut self) {
        self.stage = EnvelopeStage::Idle;
        self.level = 0.0;
        self.release_step = 0.0;
    }

    fn is_releasing(&self) -> bool {
        self.stage == EnvelopeStage::Release
    }
}

#[derive(Clone, Copy, Debug)]
struct Voice {
    active: bool,
    note: u8,
    zone_index: usize,
    age: u64,
    position: f64,
    increment: f64,
    velocity_gain: f32,
    key_released: bool,
    envelope: EnvelopeState,
}

impl Voice {
    const fn idle() -> Self {
        Self {
            active: false,
            note: 0,
            zone_index: 0,
            age: 0,
            position: 0.0,
            increment: 0.0,
            velocity_gain: 0.0,
            key_released: false,
            envelope: EnvelopeState::idle(),
        }
    }

    fn stop(&mut self) {
        self.active = false;
        self.envelope.reset();
    }
}

#[derive(Clone, Copy, Debug)]
struct ZoneRuntime {
    envelope: PreparedEnvelope,
}

pub struct Engine<'a> {
    instrument: &'a PreparedInstrument,
    sample_rate: f32,
    zones: Vec<ZoneRuntime>,
    voices: Vec<Voice>,
    sustain_down: bool,
    note_age: u64,
}

impl<'a> Engine<'a> {
    pub fn new(instrument: &'a PreparedInstrument, sample_rate: f32) -> Result<Self, EngineError> {
        if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
            return Err(EngineError::InvalidSampleRate);
        }
        let zones = instrument
            .zones
            .iter()
            .map(|zone| ZoneRuntime {
                envelope: PreparedEnvelope::new(zone.envelope, sample_rate),
            })
            .collect();
        let voices = vec![Voice::idle(); instrument.manifest.max_polyphony];
        Ok(Self {
            instrument,
            sample_rate,
            zones,
            voices,
            sustain_down: false,
            note_age: 0,
        })
    }

    pub fn render_block(
        &mut self,
        events: &[TimedEvent],
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), EngineError> {
        if left.len() != right.len() {
            return Err(EngineError::BufferLengthMismatch);
        }
        if events
            .windows(2)
            .any(|pair| pair[0].sample_offset > pair[1].sample_offset)
        {
            return Err(EngineError::EventsOutOfOrder);
        }
        if events
            .last()
            .is_some_and(|event| event.sample_offset >= left.len())
        {
            return Err(EngineError::EventOutsideBlock);
        }

        left.fill(0.0);
        right.fill(0.0);
        let mut event_index = 0;
        for frame_index in 0..left.len() {
            while event_index < events.len() && events[event_index].sample_offset == frame_index {
                self.apply_event(events[event_index].event);
                event_index += 1;
            }
            let mut mixed_left = 0.0;
            let mut mixed_right = 0.0;
            for voice_index in 0..self.voices.len() {
                let (sample_left, sample_right) = self.render_voice(voice_index);
                mixed_left += sample_left;
                mixed_right += sample_right;
            }
            left[frame_index] = finite_or_zero(mixed_left);
            right[frame_index] = finite_or_zero(mixed_right);
        }
        Ok(())
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|voice| voice.active).count()
    }

    pub fn voice_capacity(&self) -> usize {
        self.voices.len()
    }

    /// Immediately clears every owned voice and releases sustain.
    ///
    /// This is the same bounded, allocation-free operation as an
    /// [`Event::AllNotesOff`] at a block boundary. Live hosts use it during
    /// callback-fault and owned-shutdown cleanup.
    pub fn all_notes_off(&mut self) {
        self.apply_event(Event::AllNotesOff);
    }

    fn apply_event(&mut self, event: Event) {
        match event {
            Event::NoteOn { note, velocity: 0 } => self.note_off(note),
            Event::NoteOn { note, velocity } => self.note_on(note, velocity),
            Event::NoteOff { note } => self.note_off(note),
            Event::Sustain { down } => self.set_sustain(down),
            Event::AllNotesOff => {
                self.sustain_down = false;
                for voice in &mut self.voices {
                    voice.stop();
                }
            }
        }
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        self.note_age = self.note_age.wrapping_add(1);
        let age = self.note_age;
        for zone_index in 0..self.instrument.zones.len() {
            let zone = &self.instrument.zones[zone_index];
            if !(zone.key_low..=zone.key_high).contains(&note)
                || !(zone.velocity_low..=zone.velocity_high).contains(&velocity)
            {
                continue;
            }
            let Some(voice_index) = self.voice_for_start(age) else {
                break;
            };
            let source_rate = self.instrument.samples[zone.sample_index].sample_rate as f64;
            let cents = f64::from(note) * 100.0 - f64::from(zone.root_note) * 100.0
                + f64::from(zone.pitch_offset_cents);
            let increment =
                source_rate / f64::from(self.sample_rate) * 2.0_f64.powf(cents / 1_200.0);
            let voice = &mut self.voices[voice_index];
            voice.active = increment.is_finite() && increment > 0.0;
            voice.note = note;
            voice.zone_index = zone_index;
            voice.age = age;
            voice.position = zone.sample_start as f64;
            voice.increment = increment;
            voice.velocity_gain = f32::from(velocity) / 127.0;
            voice.key_released = false;
            voice.envelope.start(self.zones[zone_index].envelope);
        }
    }

    fn voice_for_start(&self, new_age: u64) -> Option<usize> {
        self.voices
            .iter()
            .enumerate()
            .filter(|(_, voice)| !voice.active || voice.age != new_age)
            .min_by_key(|(index, voice)| {
                let class = if !voice.active {
                    0
                } else if voice.key_released || voice.envelope.is_releasing() {
                    1
                } else {
                    2
                };
                (class, voice.age, *index)
            })
            .map(|(index, _)| index)
    }

    fn note_off(&mut self, note: u8) {
        for voice in &mut self.voices {
            if !voice.active || voice.note != note {
                continue;
            }
            let zone = &self.instrument.zones[voice.zone_index];
            if zone.playback_mode == PlaybackMode::OneShot {
                continue;
            }
            voice.key_released = true;
            if !self.sustain_down {
                voice
                    .envelope
                    .release(self.zones[voice.zone_index].envelope);
                if voice.envelope.stage == EnvelopeStage::Idle {
                    voice.stop();
                }
            }
        }
    }

    fn set_sustain(&mut self, down: bool) {
        let was_down = self.sustain_down;
        self.sustain_down = down;
        if was_down && !down {
            for voice in &mut self.voices {
                if voice.active && voice.key_released {
                    voice
                        .envelope
                        .release(self.zones[voice.zone_index].envelope);
                    if voice.envelope.stage == EnvelopeStage::Idle {
                        voice.stop();
                    }
                }
            }
        }
    }

    fn render_voice(&mut self, voice_index: usize) -> (f32, f32) {
        let voice = &mut self.voices[voice_index];
        if !voice.active {
            return (0.0, 0.0);
        }
        let zone = &self.instrument.zones[voice.zone_index];
        let sample = &self.instrument.samples[zone.sample_index];
        let looping = zone.loop_range.is_some() && !voice.envelope.is_releasing();
        let frame = interpolate(sample.frames.as_ref(), zone, voice.position, looping);
        let envelope = voice.envelope.next(self.zones[voice.zone_index].envelope);
        let gain = envelope * voice.velocity_gain;
        let left = finite_or_zero(frame.left * zone.left_gain * gain);
        let right = finite_or_zero(frame.right * zone.right_gain * gain);

        voice.position += voice.increment;
        if let Some(loop_range) = zone.loop_range.filter(|_| looping) {
            if voice.position >= loop_range.end as f64 {
                let length = (loop_range.end - loop_range.start) as f64;
                voice.position =
                    loop_range.start as f64 + (voice.position - loop_range.start as f64) % length;
            }
        } else if voice.position >= zone.sample_end as f64 {
            voice.stop();
        }
        if voice.envelope.stage == EnvelopeStage::Idle {
            voice.stop();
        }
        (left, right)
    }

    #[cfg(test)]
    fn active_notes(&self) -> Vec<u8> {
        self.voices
            .iter()
            .filter(|voice| voice.active)
            .map(|voice| voice.note)
            .collect()
    }
}

fn interpolate(
    frames: &[SampleFrame],
    zone: &PreparedZone,
    position: f64,
    looping: bool,
) -> SampleFrame {
    let base = position.floor() as isize;
    let fraction = (position - base as f64) as f32;
    let a = frame_at(frames, zone, base - 1, looping);
    let b = frame_at(frames, zone, base, looping);
    let c = frame_at(frames, zone, base + 1, looping);
    let d = frame_at(frames, zone, base + 2, looping);
    SampleFrame {
        left: cubic(a.left, b.left, c.left, d.left, fraction),
        right: cubic(a.right, b.right, c.right, d.right, fraction),
    }
}

fn frame_at(
    frames: &[SampleFrame],
    zone: &PreparedZone,
    index: isize,
    looping: bool,
) -> SampleFrame {
    let mut index = index.clamp(zone.sample_start as isize, zone.sample_end as isize - 1);
    if looping
        && let Some(PreparedLoop { start, end }) = zone.loop_range
        && index >= start as isize
    {
        let length = end as isize - start as isize;
        index = start as isize + (index - start as isize).rem_euclid(length);
    }
    frames[index as usize]
}

#[inline]
fn cubic(a: f32, b: f32, c: f32, d: f32, x: f32) -> f32 {
    let c0 = b;
    let c1 = 0.5 * (c - a);
    let c2 = a - 2.5 * b + 2.0 * c - 0.5 * d;
    let c3 = 0.5 * (d - a) + 1.5 * (b - c);
    ((c3 * x + c2) * x + c1) * x + c0
}

#[inline]
fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{PreparedSample, PreparedZone};
    use crate::schema::{
        InstrumentManifest, InstrumentMetadata, LoopMode, SampleMetadata, SampleRef, ZoneManifest,
    };
    use assert_no_alloc::{AllocDisabler, assert_no_alloc};

    #[global_allocator]
    static ALLOCATOR: AllocDisabler = AllocDisabler;

    fn instrument(
        samples: Vec<Vec<SampleFrame>>,
        zones: Vec<PreparedZone>,
        polyphony: usize,
    ) -> PreparedInstrument {
        let prepared_samples = samples
            .into_iter()
            .map(|frames| PreparedSample {
                frames: frames.into_boxed_slice(),
                sample_rate: 8_000,
                original_channels: 2,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let manifest_zones = zones
            .iter()
            .enumerate()
            .map(|(index, zone)| ZoneManifest {
                id: format!("zone-{index:03}"),
                sample: SampleRef {
                    path: format!("samples/{index}.wav"),
                    sha256: "0".repeat(64),
                    metadata: SampleMetadata {
                        sample_rate: 8_000,
                        channels: 2,
                        frames: prepared_samples[zone.sample_index].frames.len() as u64,
                    },
                },
                key_low: zone.key_low,
                key_high: zone.key_high,
                velocity_low: zone.velocity_low,
                velocity_high: zone.velocity_high,
                root_note: zone.root_note,
                coarse_transpose: 0,
                fine_tune_cents: zone.pitch_offset_cents,
                gain_db: 0.0,
                pan: 0.0,
                playback_mode: zone.playback_mode,
                loop_mode: zone
                    .loop_range
                    .map_or(LoopMode::None, |range| LoopMode::Forward {
                        start_frame: range.start as u64,
                        end_frame: range.end as u64,
                    }),
                envelope: zone.envelope,
                sample_start_frame: zone.sample_start as u64,
                sample_end_frame: Some(zone.sample_end as u64),
            })
            .collect();
        PreparedInstrument {
            manifest: InstrumentManifest {
                format_version: 1,
                instrument_id: "engine-fixture".into(),
                display_name: "Engine Fixture".into(),
                metadata: InstrumentMetadata {
                    author: "SHR tests".into(),
                    source: "Synthetic".into(),
                    licence: "MIT".into(),
                    attribution: "None".into(),
                    notes: "Generated".into(),
                },
                max_polyphony: polyphony,
                zones: manifest_zones,
            },
            samples: prepared_samples,
            zones: zones.into_boxed_slice(),
        }
    }

    fn zone(sample_index: usize, sample_end: usize) -> PreparedZone {
        PreparedZone {
            sample_index,
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
            envelope: Adsr {
                attack_seconds: 0.0,
                decay_seconds: 0.0,
                sustain_level: 1.0,
                release_seconds: 0.0,
            },
            sample_start: 0,
            sample_end,
        }
    }

    fn ramp(length: usize) -> Vec<SampleFrame> {
        (0..length)
            .map(|index| SampleFrame {
                left: index as f32 / 100.0,
                right: -(index as f32) / 100.0,
            })
            .collect()
    }

    #[test]
    fn key_velocity_selection_root_pitch_and_transpose_are_exact() {
        let mut low = zone(0, 32);
        low.key_high = 60;
        low.velocity_high = 63;
        let mut high = zone(1, 32);
        high.key_low = 61;
        high.velocity_low = 64;
        let prepared = instrument(vec![ramp(32), ramp(32)], vec![low, high], 4);
        let mut engine = Engine::new(&prepared, 8_000.0).unwrap();
        let mut left = [0.0; 4];
        let mut right = [0.0; 4];
        engine
            .render_block(
                &[TimedEvent::new(
                    0,
                    Event::NoteOn {
                        note: 60,
                        velocity: 63,
                    },
                )],
                &mut left,
                &mut right,
            )
            .unwrap();
        let velocity = 63.0 / 127.0;
        assert!((left[1] - 0.01 * velocity).abs() < 1.0e-6);
        assert_eq!(engine.active_voice_count(), 1);

        let mut transposed = zone(0, 32);
        transposed.pitch_offset_cents = 1_200.0;
        let prepared = instrument(vec![ramp(32)], vec![transposed], 1);
        let mut engine = Engine::new(&prepared, 8_000.0).unwrap();
        engine
            .render_block(
                &[TimedEvent::new(
                    0,
                    Event::NoteOn {
                        note: 60,
                        velocity: 127,
                    },
                )],
                &mut left,
                &mut right,
            )
            .unwrap();
        assert!((left[1] - 0.02).abs() < 1.0e-6);
    }

    #[test]
    fn one_shot_ignores_note_off_while_gated_stops() {
        let samples = vec![vec![
            SampleFrame {
                left: 0.5,
                right: 0.5
            };
            16
        ]];
        let gated = instrument(samples.clone(), vec![zone(0, 16)], 1);
        let mut one_shot_zone = zone(0, 16);
        one_shot_zone.playback_mode = PlaybackMode::OneShot;
        let one_shot = instrument(samples, vec![one_shot_zone], 1);
        let events = [
            TimedEvent::new(
                0,
                Event::NoteOn {
                    note: 60,
                    velocity: 127,
                },
            ),
            TimedEvent::new(2, Event::NoteOff { note: 60 }),
        ];
        let mut gated_left = [0.0; 8];
        let mut gated_right = [0.0; 8];
        Engine::new(&gated, 8_000.0)
            .unwrap()
            .render_block(&events, &mut gated_left, &mut gated_right)
            .unwrap();
        let mut one_left = [0.0; 8];
        let mut one_right = [0.0; 8];
        Engine::new(&one_shot, 8_000.0)
            .unwrap()
            .render_block(&events, &mut one_left, &mut one_right)
            .unwrap();
        assert_eq!(&gated_left[2..], &[0.0; 6]);
        assert!(one_left[2..].iter().all(|sample| *sample == 0.5));
    }

    #[test]
    fn forward_loop_exits_on_release_and_sustain_defers_release() {
        let mut looping = zone(0, 8);
        looping.loop_range = Some(PreparedLoop { start: 2, end: 4 });
        looping.envelope.release_seconds = 1.0;
        let prepared = instrument(vec![ramp(8)], vec![looping], 1);
        let events = [
            TimedEvent::new(
                0,
                Event::NoteOn {
                    note: 60,
                    velocity: 127,
                },
            ),
            TimedEvent::new(3, Event::Sustain { down: true }),
            TimedEvent::new(4, Event::NoteOff { note: 60 }),
            TimedEvent::new(7, Event::Sustain { down: false }),
        ];
        let mut left = [0.0; 12];
        let mut right = [0.0; 12];
        Engine::new(&prepared, 8_000.0)
            .unwrap()
            .render_block(&events, &mut left, &mut right)
            .unwrap();
        assert_eq!(&left[..7], &[0.0, 0.01, 0.02, 0.03, 0.02, 0.03, 0.02]);
        assert!(left[7] > 0.029 && left[8] > 0.039);
    }

    #[test]
    fn adsr_attack_decay_and_sustain_are_sample_deterministic() {
        let mut enveloped = zone(0, 32);
        enveloped.envelope = Adsr {
            attack_seconds: 4.0 / 8_000.0,
            decay_seconds: 4.0 / 8_000.0,
            sustain_level: 0.5,
            release_seconds: 0.0,
        };
        let prepared = instrument(
            vec![vec![
                SampleFrame {
                    left: 1.0,
                    right: 1.0,
                };
                32
            ]],
            vec![enveloped],
            1,
        );
        let mut left = [0.0; 8];
        let mut right = [0.0; 8];
        Engine::new(&prepared, 8_000.0)
            .unwrap()
            .render_block(
                &[TimedEvent::new(
                    0,
                    Event::NoteOn {
                        note: 60,
                        velocity: 127,
                    },
                )],
                &mut left,
                &mut right,
            )
            .unwrap();
        assert_eq!(left, [0.25, 0.5, 0.75, 1.0, 0.875, 0.75, 0.625, 0.5]);
        assert_eq!(left, right);
    }

    #[test]
    fn voice_stealing_is_oldest_first_and_deterministic() {
        let prepared = instrument(vec![ramp(64)], vec![zone(0, 64)], 2);
        let events = [
            TimedEvent::new(
                0,
                Event::NoteOn {
                    note: 60,
                    velocity: 127,
                },
            ),
            TimedEvent::new(
                1,
                Event::NoteOn {
                    note: 61,
                    velocity: 127,
                },
            ),
            TimedEvent::new(
                2,
                Event::NoteOn {
                    note: 62,
                    velocity: 127,
                },
            ),
        ];
        let mut engine = Engine::new(&prepared, 8_000.0).unwrap();
        let mut left = [0.0; 4];
        let mut right = [0.0; 4];
        engine.render_block(&events, &mut left, &mut right).unwrap();
        let mut notes = engine.active_notes();
        notes.sort_unstable();
        assert_eq!(notes, vec![61, 62]);

        let mut again = Engine::new(&prepared, 8_000.0).unwrap();
        let mut again_left = [0.0; 4];
        let mut again_right = [0.0; 4];
        again
            .render_block(&events, &mut again_left, &mut again_right)
            .unwrap();
        assert_eq!(left, again_left);
        assert_eq!(right, again_right);
    }

    #[test]
    fn render_path_is_finite_exactly_silent_when_idle_and_allocation_free() {
        let prepared = instrument(vec![ramp(64)], vec![zone(0, 64)], 4);
        let mut engine = Engine::new(&prepared, 8_000.0).unwrap();
        let mut left = [1.0; 128];
        let mut right = [1.0; 128];
        assert_no_alloc(|| {
            engine.render_block(&[], &mut left, &mut right).unwrap();
        });
        assert_eq!(left, [0.0; 128]);
        assert_eq!(right, [0.0; 128]);

        let events = [TimedEvent::new(
            0,
            Event::NoteOn {
                note: 127,
                velocity: 127,
            },
        )];
        assert_no_alloc(|| {
            engine.render_block(&events, &mut left, &mut right).unwrap();
        });
        assert!(left.iter().all(|sample| sample.is_finite()));
        assert!(right.iter().all(|sample| sample.is_finite()));
        assert_eq!(engine.voice_capacity(), 4);
    }
}
