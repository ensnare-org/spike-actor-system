use ensnare::prelude::*;
use ensnare_proc_macros::{IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(TransformsAudio)]
pub struct Arpeggiator {
    uid: Uid,
    last_beat: usize,
    is_playing: bool,
    should_play_low_note: bool,
    time_range: TimeRange,
}
impl Serializable for Arpeggiator {}
impl HandlesMidi for Arpeggiator {}
impl Generates<StereoSample> for Arpeggiator {}
impl Configurable for Arpeggiator {}
impl Displays for Arpeggiator {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.label(format!("Last beat: {}", self.last_beat))
    }
}
impl Controllable for Arpeggiator {}
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
                    MidiUtils::new_note_off(if self.should_play_low_note { 60 } else { 67 }, 127),
                ));
                self.is_playing = false;
                self.should_play_low_note = !self.should_play_low_note;
            }
        } else {
            if beat_changed {
                control_events_fn(WorkEvent::Midi(
                    MidiChannel::default(),
                    MidiUtils::new_note_on(if self.should_play_low_note { 60 } else { 67 }, 127),
                ));
                self.is_playing = true;
            }
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
