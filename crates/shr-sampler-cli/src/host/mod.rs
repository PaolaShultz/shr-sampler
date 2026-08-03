mod callback;
mod jack;
pub mod midi;
pub mod queue;

pub use callback::{HostFault, HostSignals, MAX_EVENTS_PER_PERIOD, ScheduledEvent};
pub use jack::{JACK_OUTPUT_LEFT, JACK_OUTPUT_RIGHT};
pub use queue::EVENT_QUEUE_CAPACITY;

use self::jack::JackHost;
use self::queue::channel;
use alsa::Direction;
use alsa::seq::{PortCap, PortType, Seq};
use shr_sampler_core::PreparedInstrument;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

pub const ALSA_INPUT_PORT: &str = "input";
pub const MAX_CLIENT_NAME_BYTES: usize = 63;

pub fn validate_client_name(client_name: &str) -> Result<(), String> {
    if client_name.trim().is_empty()
        || client_name.len() > MAX_CLIENT_NAME_BYTES
        || client_name.chars().any(char::is_control)
        || client_name.contains('\0')
    {
        return Err(format!(
            "--client-name must be 1..={MAX_CLIENT_NAME_BYTES} non-control UTF-8 bytes"
        ));
    }
    Ok(())
}

pub fn run(client_name: &str, instrument: &PreparedInstrument) -> Result<(), String> {
    validate_client_name(client_name)?;
    let signals = HostSignals::new();
    signal_hook::flag::register(signal_hook::consts::SIGINT, signals.stop_flag())
        .map_err(|error| format!("register SIGINT handler: {error}"))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, signals.stop_flag())
        .map_err(|error| format!("register SIGTERM handler: {error}"))?;

    let (producer, consumer) = channel::<ScheduledEvent>();
    let (mut jack, timing) = JackHost::open(client_name, instrument, consumer, signals.clone())?;

    // This is ALSA Sequencer MIDI capture only. No ALSA PCM/audio handle is
    // opened anywhere in this process.
    let seq = Seq::open(None, Some(Direction::Capture), true)
        .map_err(|error| format!("open ALSA Sequencer MIDI input: {error}"))?;
    seq.set_client_name(
        &CString::new(client_name).map_err(|_| "ALSA client name contains NUL".to_string())?,
    )
    .map_err(|error| format!("set ALSA Sequencer client name: {error}"))?;
    seq.create_simple_port(
        &CString::new(ALSA_INPUT_PORT).expect("static ALSA port name"),
        PortCap::WRITE | PortCap::SUBS_WRITE,
        PortType::MIDI_GENERIC | PortType::APPLICATION,
    )
    .map_err(|error| format!("create ALSA Sequencer MIDI input port: {error}"))?;

    let midi_start = Arc::new(AtomicBool::new(false));
    let midi_start_thread = midi_start.clone();
    let midi_signals = signals.clone();
    let midi_thread = thread::Builder::new()
        .name("shr-sampler-midi".into())
        .spawn(move || {
            while !midi_start_thread.load(Ordering::Acquire) && !midi_signals.stop_requested() {
                thread::sleep(Duration::from_millis(1));
            }
            let mut input = seq.input();
            while !midi_signals.stop_requested() {
                match input.event_input_pending(true) {
                    Ok(pending) if pending > 0 => match input.event_input() {
                        Ok(event) => {
                            if let Some(event) = midi::translate_alsa(&event) {
                                producer.push(timing.schedule(event));
                            }
                        }
                        Err(_) => midi_signals.publish_fault(HostFault::MidiInput),
                    },
                    Ok(_) => thread::sleep(Duration::from_millis(1)),
                    Err(_) => midi_signals.publish_fault(HostFault::MidiInput),
                }
            }
            producer.overflow_count()
        })
        .map_err(|error| format!("start ALSA MIDI input thread: {error}"))?;

    if let Err(error) = jack.activate() {
        signals.request_stop();
        midi_start.store(true, Ordering::Release);
        let _ = midi_thread.join();
        jack.shutdown_owned();
        return Err(error);
    }
    midi_start.store(true, Ordering::Release);

    let mut last_recoveries = 0;
    let mut last_limit_hits = 0;
    while !signals.stop_requested() {
        thread::sleep(Duration::from_millis(50));
        let recoveries = signals.queue_recoveries();
        if recoveries != last_recoveries {
            eprintln!("SHR Sampler MIDI queue recovery count: {recoveries}");
            last_recoveries = recoveries;
        }
        let limit_hits = signals.period_limit_hits();
        if limit_hits != last_limit_hits {
            eprintln!("SHR Sampler callback event-cap deferral count: {limit_hits}");
            last_limit_hits = limit_hits;
        }
    }

    let queue_overflow = midi_thread
        .join()
        .map_err(|_| "ALSA MIDI input thread panicked".to_string())?;
    jack.shutdown_owned();
    if queue_overflow > 0 {
        eprintln!("SHR Sampler MIDI queue overflow count: {queue_overflow}");
    }
    match signals.fault() {
        Some(HostFault::JackShutdown) => Err("JACK shut down while SHR Sampler was active".into()),
        Some(HostFault::MidiInput) => {
            Err("ALSA Sequencer MIDI input failed while SHR Sampler was active".into())
        }
        Some(HostFault::CallbackBuffers) => {
            Err("JACK callback could not acquire exact stereo output buffers".into())
        }
        Some(HostFault::EngineRender) => {
            Err("sampler engine render failed inside the JACK callback".into())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_public_names_and_client_validation_are_exact() {
        assert_eq!(JACK_OUTPUT_LEFT, "out_l");
        assert_eq!(JACK_OUTPUT_RIGHT, "out_r");
        assert_eq!(ALSA_INPUT_PORT, "input");
        assert!(validate_client_name("shr-sampler").is_ok());
        assert!(validate_client_name("").is_err());
        assert!(validate_client_name("   ").is_err());
        assert!(validate_client_name("bad\nname").is_err());
        assert!(validate_client_name(&"x".repeat(MAX_CLIENT_NAME_BYTES + 1)).is_err());
    }
}
