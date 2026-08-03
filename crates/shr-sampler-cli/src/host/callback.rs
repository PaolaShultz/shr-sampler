use super::queue::Consumer;
use shr_sampler_core::{Engine, Event, PreparedInstrument, TimedEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

pub const MAX_EVENTS_PER_PERIOD: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HostFault {
    JackShutdown = 1,
    MidiInput = 2,
    CallbackBuffers = 3,
    EngineRender = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledEvent {
    pub cycle: u64,
    pub sample_offset: usize,
    pub event: Event,
}

struct SignalState {
    fault: AtomicU8,
    panic_request: AtomicU64,
    panic_acknowledged: AtomicU64,
    queue_recoveries: AtomicU64,
    period_limit_hits: AtomicU64,
    last_period_events: AtomicUsize,
}

#[derive(Clone)]
pub struct HostSignals {
    stop: Arc<AtomicBool>,
    state: Arc<SignalState>,
}

impl HostSignals {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            state: Arc::new(SignalState {
                fault: AtomicU8::new(0),
                panic_request: AtomicU64::new(0),
                panic_acknowledged: AtomicU64::new(0),
                queue_recoveries: AtomicU64::new(0),
                period_limit_hits: AtomicU64::new(0),
                last_period_events: AtomicUsize::new(0),
            }),
        }
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn request_all_notes_off(&self) -> u64 {
        self.state.panic_request.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn panic_acknowledged(&self) -> u64 {
        self.state.panic_acknowledged.load(Ordering::Acquire)
    }

    pub fn fault(&self) -> Option<HostFault> {
        match self.state.fault.load(Ordering::Acquire) {
            1 => Some(HostFault::JackShutdown),
            2 => Some(HostFault::MidiInput),
            3 => Some(HostFault::CallbackBuffers),
            4 => Some(HostFault::EngineRender),
            _ => None,
        }
    }

    pub fn queue_recoveries(&self) -> u64 {
        self.state.queue_recoveries.load(Ordering::Relaxed)
    }

    pub fn period_limit_hits(&self) -> u64 {
        self.state.period_limit_hits.load(Ordering::Relaxed)
    }

    pub fn last_period_events(&self) -> usize {
        self.state.last_period_events.load(Ordering::Relaxed)
    }

    pub(crate) fn publish_fault(&self, fault: HostFault) {
        let _ =
            self.state
                .fault
                .compare_exchange(0, fault as u8, Ordering::AcqRel, Ordering::Acquire);
        self.request_stop();
    }
}

impl Default for HostSignals {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct CallbackAdapter<'a> {
    engine: Engine<'a>,
    queue: Consumer<ScheduledEvent>,
    pending: Option<ScheduledEvent>,
    events: [TimedEvent; MAX_EVENTS_PER_PERIOD],
    cycle: Arc<AtomicU64>,
    signals: HostSignals,
    observed_panic_request: u64,
}

impl<'a> CallbackAdapter<'a> {
    pub fn new(
        instrument: &'a PreparedInstrument,
        sample_rate: f32,
        queue: Consumer<ScheduledEvent>,
        cycle: Arc<AtomicU64>,
        signals: HostSignals,
    ) -> Result<Self, String> {
        let engine = Engine::new(instrument, sample_rate).map_err(|error| error.to_string())?;
        Ok(Self {
            engine,
            queue,
            pending: None,
            events: [TimedEvent::new(0, Event::AllNotesOff); MAX_EVENTS_PER_PERIOD],
            cycle,
            signals,
            observed_panic_request: 0,
        })
    }

    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if left.len() != right.len() {
            left.fill(0.0);
            right.fill(0.0);
            self.engine.all_notes_off();
            self.signals.publish_fault(HostFault::CallbackBuffers);
            return;
        }
        if left.is_empty() {
            return;
        }

        let cycle = self.cycle.fetch_add(1, Ordering::AcqRel) + 1;
        self.apply_recovery_requests();

        let mut count = 0;
        while count < MAX_EVENTS_PER_PERIOD {
            let Some(scheduled) = self.pending.take().or_else(|| self.queue.pop()) else {
                break;
            };
            if scheduled.cycle > cycle {
                self.pending = Some(scheduled);
                break;
            }
            let event =
                TimedEvent::new(period_offset(scheduled, cycle, left.len()), scheduled.event);
            insert_ordered(&mut self.events, &mut count, event);
        }
        self.signals
            .state
            .last_period_events
            .store(count, Ordering::Relaxed);
        if count == MAX_EVENTS_PER_PERIOD && (self.pending.is_some() || !self.queue.is_empty()) {
            self.signals
                .state
                .period_limit_hits
                .fetch_add(1, Ordering::Relaxed);
        }
        render_or_silence(
            &mut self.engine,
            &self.events[..count],
            left,
            right,
            &self.signals,
        );
    }

    fn apply_recovery_requests(&mut self) {
        let panic_request = self.signals.state.panic_request.load(Ordering::Acquire);
        if panic_request != self.observed_panic_request {
            self.pending = None;
            self.queue.discard_all();
            self.engine.all_notes_off();
            self.observed_panic_request = panic_request;
            self.signals
                .state
                .panic_acknowledged
                .store(panic_request, Ordering::Release);
        }
        if self.queue.take_overflow_recovery() {
            self.pending = None;
            self.queue.discard_all();
            self.engine.all_notes_off();
            self.signals
                .state
                .queue_recoveries
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn force_all_notes_off(&mut self) {
        self.pending = None;
        self.queue.discard_all();
        self.engine.all_notes_off();
    }
}

fn insert_ordered(
    events: &mut [TimedEvent; MAX_EVENTS_PER_PERIOD],
    count: &mut usize,
    event: TimedEvent,
) {
    let mut insert = *count;
    while insert > 0 && events[insert - 1].sample_offset > event.sample_offset {
        events[insert] = events[insert - 1];
        insert -= 1;
    }
    events[insert] = event;
    *count += 1;
}

fn render_or_silence(
    engine: &mut Engine<'_>,
    events: &[TimedEvent],
    left: &mut [f32],
    right: &mut [f32],
    signals: &HostSignals,
) -> bool {
    if engine.render_block(events, left, right).is_ok() {
        true
    } else {
        left.fill(0.0);
        right.fill(0.0);
        engine.all_notes_off();
        signals.publish_fault(HostFault::EngineRender);
        false
    }
}

pub(crate) fn period_offset(scheduled: ScheduledEvent, current_cycle: u64, frames: usize) -> usize {
    if scheduled.cycle < current_cycle {
        0
    } else {
        scheduled.sample_offset.min(frames.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::queue::channel;
    use assert_no_alloc::{AllocDisabler, assert_no_alloc};
    use hound::{SampleFormat, WavSpec, WavWriter};
    use sha2::{Digest, Sha256};
    use shr_sampler_core::{
        Adsr, InstrumentManifest, InstrumentMetadata, LoopMode, PlaybackMode, SampleMetadata,
        SampleRef, ZoneManifest, load_package,
    };
    use std::fs;
    use std::path::Path;

    #[global_allocator]
    static ALLOCATOR: AllocDisabler = AllocDisabler;

    fn prepared_fixture(directory: &Path) -> PreparedInstrument {
        let package = directory.join("callback.shrinst");
        let samples = package.join("samples");
        fs::create_dir_all(&samples).unwrap();
        let wav = samples.join("tone.wav");
        let mut writer = WavWriter::create(
            &wav,
            WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            },
        )
        .unwrap();
        for index in 0..512 {
            writer
                .write_sample(((index % 64) as i16 - 32) * 500)
                .unwrap();
        }
        writer.finalize().unwrap();
        let bytes = fs::read(&wav).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let manifest = InstrumentManifest {
            format_version: 1,
            instrument_id: "callback-fixture".into(),
            display_name: "Callback Fixture".into(),
            metadata: InstrumentMetadata {
                author: "SHR tests".into(),
                source: "Project-authored synthetic waveform".into(),
                licence: "MIT".into(),
                attribution: "None".into(),
                notes: "Host callback test".into(),
            },
            max_polyphony: 4,
            zones: vec![ZoneManifest {
                id: "zone-001".into(),
                sample: SampleRef {
                    path: "samples/tone.wav".into(),
                    sha256: hash,
                    metadata: SampleMetadata {
                        sample_rate: 8_000,
                        channels: 1,
                        frames: 512,
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
                envelope: Adsr {
                    attack_seconds: 0.0,
                    decay_seconds: 0.0,
                    sustain_level: 1.0,
                    release_seconds: 0.1,
                },
                sample_start_frame: 0,
                sample_end_frame: None,
            }],
        };
        fs::write(
            package.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        load_package(package).unwrap()
    }

    #[test]
    fn event_ordering_is_stable_and_offsets_are_clamped() {
        let mut events = [TimedEvent::new(0, Event::AllNotesOff); MAX_EVENTS_PER_PERIOD];
        let mut count = 0;
        insert_ordered(
            &mut events,
            &mut count,
            TimedEvent::new(17, Event::NoteOff { note: 1 }),
        );
        insert_ordered(
            &mut events,
            &mut count,
            TimedEvent::new(3, Event::NoteOff { note: 2 }),
        );
        insert_ordered(
            &mut events,
            &mut count,
            TimedEvent::new(17, Event::NoteOff { note: 3 }),
        );
        assert_eq!(
            events[..count]
                .iter()
                .map(|event| event.sample_offset)
                .collect::<Vec<_>>(),
            vec![3, 17, 17]
        );
        assert_eq!(events[1].event, Event::NoteOff { note: 1 });
        assert_eq!(events[2].event, Event::NoteOff { note: 3 });

        let scheduled = ScheduledEvent {
            cycle: 4,
            sample_offset: 99,
            event: Event::AllNotesOff,
        };
        assert_eq!(period_offset(scheduled, 4, 64), 63);
        assert_eq!(period_offset(scheduled, 5, 64), 0);
    }

    #[test]
    fn adapter_and_engine_are_allocation_free_and_cap_events_per_period() {
        let directory = tempfile::tempdir().unwrap();
        let instrument = prepared_fixture(directory.path());
        let (producer, consumer) = channel();
        for note in 0..300 {
            assert!(producer.push(ScheduledEvent {
                cycle: 1,
                sample_offset: note,
                event: Event::NoteOff {
                    note: (note % 128) as u8,
                },
            }));
        }
        let signals = HostSignals::new();
        let mut adapter = CallbackAdapter::new(
            &instrument,
            8_000.0,
            consumer,
            Arc::new(AtomicU64::new(0)),
            signals.clone(),
        )
        .unwrap();
        let mut left = [1.0; 128];
        let mut right = [1.0; 128];
        assert_no_alloc(|| adapter.process(&mut left, &mut right));
        assert_eq!(signals.last_period_events(), MAX_EVENTS_PER_PERIOD);
        assert_eq!(signals.period_limit_hits(), 1);
        assert_no_alloc(|| adapter.process(&mut left, &mut right));
        assert_eq!(signals.last_period_events(), 44);
        assert!(left.iter().all(|sample| sample.is_finite()));
        assert!(right.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn queue_overflow_forces_one_bounded_panic_recovery_before_new_events() {
        let directory = tempfile::tempdir().unwrap();
        let instrument = prepared_fixture(directory.path());
        let (producer, consumer) = channel();
        for _ in 0..crate::host::EVENT_QUEUE_CAPACITY {
            assert!(producer.push(ScheduledEvent {
                cycle: 1,
                sample_offset: 0,
                event: Event::NoteOn {
                    note: 60,
                    velocity: 127,
                },
            }));
        }
        assert!(!producer.push(ScheduledEvent {
            cycle: 1,
            sample_offset: 0,
            event: Event::NoteOff { note: 60 },
        }));
        let signals = HostSignals::new();
        let mut adapter = CallbackAdapter::new(
            &instrument,
            8_000.0,
            consumer,
            Arc::new(AtomicU64::new(0)),
            signals.clone(),
        )
        .unwrap();
        let mut left = [1.0; 32];
        let mut right = [1.0; 32];
        assert_no_alloc(|| adapter.process(&mut left, &mut right));
        assert_eq!(signals.queue_recoveries(), 1);
        assert_eq!(signals.last_period_events(), 0);
        assert_eq!(adapter.engine.active_voice_count(), 0);
        assert_eq!(left, [0.0; 32]);
        assert_eq!(right, [0.0; 32]);

        assert!(producer.push(ScheduledEvent {
            cycle: 2,
            sample_offset: 4,
            event: Event::NoteOn {
                note: 60,
                velocity: 127,
            },
        }));
        adapter.process(&mut left, &mut right);
        assert_eq!(signals.queue_recoveries(), 1);
        assert_eq!(signals.last_period_events(), 1);
        assert!(adapter.engine.active_voice_count() > 0);
    }

    #[test]
    fn render_failure_writes_exact_stereo_silence_and_publishes_shutdown_fault() {
        let directory = tempfile::tempdir().unwrap();
        let instrument = prepared_fixture(directory.path());
        let mut engine = Engine::new(&instrument, 8_000.0).unwrap();
        let signals = HostSignals::new();
        let mut left = [0.5; 16];
        let mut right = [-0.5; 16];
        let invalid = [TimedEvent::new(16, Event::AllNotesOff)];
        assert!(!render_or_silence(
            &mut engine,
            &invalid,
            &mut left,
            &mut right,
            &signals,
        ));
        assert_eq!(left, [0.0; 16]);
        assert_eq!(right, [0.0; 16]);
        assert_eq!(signals.fault(), Some(HostFault::EngineRender));
        assert!(signals.stop_requested());
        assert_eq!(engine.active_voice_count(), 0);
    }

    #[test]
    fn shutdown_request_is_acknowledged_and_clears_owned_voices() {
        let directory = tempfile::tempdir().unwrap();
        let instrument = prepared_fixture(directory.path());
        let (producer, consumer) = channel();
        producer.push(ScheduledEvent {
            cycle: 1,
            sample_offset: 0,
            event: Event::NoteOn {
                note: 60,
                velocity: 127,
            },
        });
        let signals = HostSignals::new();
        let mut adapter = CallbackAdapter::new(
            &instrument,
            8_000.0,
            consumer,
            Arc::new(AtomicU64::new(0)),
            signals.clone(),
        )
        .unwrap();
        let mut left = [0.0; 32];
        let mut right = [0.0; 32];
        adapter.process(&mut left, &mut right);
        assert!(adapter.engine.active_voice_count() > 0);
        let request = signals.request_all_notes_off();
        adapter.process(&mut left, &mut right);
        assert_eq!(signals.panic_acknowledged(), request);
        assert_eq!(adapter.engine.active_voice_count(), 0);
        assert_eq!(left, [0.0; 32]);
        assert_eq!(right, [0.0; 32]);
    }
}
