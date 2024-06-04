use derivative::Derivative;
use ensnare::{prelude::*, util::MidiUtils};
use ensnare_proc_macros::{Control, IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Derivative, IsEntity, Control, Metadata, Serialize, Deserialize)]
#[derivative(Default)]
#[entity(TransformsAudio)]
pub struct Arpeggiator {
    uid: Uid,
    last_beat: usize,
    is_playing: bool,
    should_play_low_note: bool,
    #[derivative(Default(value = "60"))]
    base_note: u8,
    note_we_are_playing: u8,
    time_range: TimeRange,
}
impl Serializable for Arpeggiator {}
impl HandlesMidi for Arpeggiator {
    fn handle_midi_message(
        &mut self,
        _channel: MidiChannel,
        message: MidiMessage,
        _midi_messages_fn: &mut MidiMessagesFn,
    ) {
        match message {
            #[allow(unused_variables)]
            MidiMessage::NoteOn { key, vel } => {
                self.base_note = key.into();
            }
            _ => {}
        }
    }
}
impl Generates<StereoSample> for Arpeggiator {}
impl Configurable for Arpeggiator {}
impl Displays for Arpeggiator {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.label(format!("Last beat: {}", self.last_beat))
    }
}
impl Controls for Arpeggiator {
    fn time_range(&self) -> Option<TimeRange> {
        Some(self.time_range.clone())
    }

    fn update_time_range(&mut self, time_range: &TimeRange) {
        self.time_range = time_range.clone();
    }

    // Play one whole note on every other beat, and alternate between two
    // pitches.
    fn work(&mut self, control_events_fn: &mut ControlEventsFn) {
        let latest_beat = self.time_range.0.end.total_beats();
        let beat_changed = latest_beat > self.last_beat;
        if beat_changed {
            self.last_beat = latest_beat;
        }
        if self.is_playing {
            if beat_changed {
                control_events_fn(WorkEvent::Midi(
                    MidiChannel::default(),
                    MidiUtils::new_note_off(self.note_we_are_playing, 127),
                ));
                self.is_playing = false;
                self.should_play_low_note = !self.should_play_low_note;
            }
        } else if beat_changed {
            self.note_we_are_playing = self.get_note_to_play();
            control_events_fn(WorkEvent::Midi(
                MidiChannel::default(),
                MidiUtils::new_note_on(self.note_we_are_playing, 127),
            ));
            self.is_playing = true;
        }
    }

    fn is_finished(&self) -> bool {
        true
    }

    fn play(&mut self) {}

    fn stop(&mut self) {}

    fn skip_to_start(&mut self) {}

    fn is_performing(&self) -> bool {
        false
    }
}

impl Arpeggiator {
    fn get_note_to_play(&mut self) -> u8 {
        self.base_note + if self.should_play_low_note { 0 } else { 7 }
    }
}
