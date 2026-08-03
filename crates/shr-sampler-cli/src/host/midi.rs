use shr_sampler_core::Event;

pub const SUSTAIN_CC: u8 = 64;
pub const ALL_SOUND_OFF_CC: u8 = 120;
pub const ALL_NOTES_OFF_CC: u8 = 123;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiMessage {
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8 },
    ControlChange { controller: u8, value: u8 },
    Reset,
}

pub fn translate_message(message: MidiMessage) -> Option<Event> {
    match message {
        MidiMessage::NoteOn { note, velocity: 0 } => Some(Event::NoteOff { note }),
        MidiMessage::NoteOn { note, velocity } => Some(Event::NoteOn { note, velocity }),
        MidiMessage::NoteOff { note } => Some(Event::NoteOff { note }),
        MidiMessage::ControlChange {
            controller: SUSTAIN_CC,
            value,
        } => Some(Event::Sustain { down: value >= 64 }),
        MidiMessage::ControlChange {
            controller: ALL_SOUND_OFF_CC | ALL_NOTES_OFF_CC,
            ..
        }
        | MidiMessage::Reset => Some(Event::AllNotesOff),
        MidiMessage::ControlChange { .. } => None,
    }
}

pub(crate) fn translate_alsa(event: &alsa::seq::Event<'_>) -> Option<Event> {
    use alsa::seq::{EvCtrl, EvNote, EventType};

    let message = match event.get_type() {
        EventType::Noteon => {
            let note = event.get_data::<EvNote>()?;
            MidiMessage::NoteOn {
                note: note.note,
                velocity: note.velocity,
            }
        }
        EventType::Noteoff => {
            let note = event.get_data::<EvNote>()?;
            MidiMessage::NoteOff { note: note.note }
        }
        EventType::Controller => {
            let control = event.get_data::<EvCtrl>()?;
            MidiMessage::ControlChange {
                controller: u8::try_from(control.param).ok()?,
                value: u8::try_from(control.value.clamp(0, 127)).ok()?,
            }
        }
        EventType::Reset => MidiMessage::Reset,
        _ => return None,
    };
    translate_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_note_on_note_off_and_velocity_zero() {
        assert_eq!(
            translate_message(MidiMessage::NoteOn {
                note: 64,
                velocity: 100,
            }),
            Some(Event::NoteOn {
                note: 64,
                velocity: 100,
            })
        );
        assert_eq!(
            translate_message(MidiMessage::NoteOff { note: 64 }),
            Some(Event::NoteOff { note: 64 })
        );
        assert_eq!(
            translate_message(MidiMessage::NoteOn {
                note: 64,
                velocity: 0,
            }),
            Some(Event::NoteOff { note: 64 })
        );
    }

    #[test]
    fn sustain_threshold_and_panic_controllers_are_exact() {
        for (value, down) in [(0, false), (63, false), (64, true), (127, true)] {
            assert_eq!(
                translate_message(MidiMessage::ControlChange {
                    controller: SUSTAIN_CC,
                    value,
                }),
                Some(Event::Sustain { down })
            );
        }
        for controller in [ALL_SOUND_OFF_CC, ALL_NOTES_OFF_CC] {
            assert_eq!(
                translate_message(MidiMessage::ControlChange {
                    controller,
                    value: 0,
                }),
                Some(Event::AllNotesOff)
            );
        }
        assert_eq!(
            translate_message(MidiMessage::Reset),
            Some(Event::AllNotesOff)
        );
        assert_eq!(
            translate_message(MidiMessage::ControlChange {
                controller: 1,
                value: 127,
            }),
            None
        );
    }
}
