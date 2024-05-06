use ensnare::prelude::*;
use ensnare_proc_macros::{Control, IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, IsEntity, Control, Metadata, Serialize, Deserialize)]
#[entity(Configurable, HandlesMidi, Serializable, TransformsAudio)]
pub struct DroneController {
    uid: Uid,

    #[serde(skip)]
    value: Normal,

    #[serde(skip)]
    last_value: Normal,

    #[serde(skip)]
    time_range: TimeRange,

    #[serde(skip)]
    oscillator: Oscillator,

    #[serde(skip)]
    oscillator_buffer: GenerationBuffer<BipolarNormal>,
}
impl Displays for DroneController {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.label(format!("Drone value: {:.4}", self.value.0))
    }
}
impl Controls for DroneController {
    fn time_range(&self) -> Option<TimeRange> {
        Some(self.time_range.clone())
    }

    fn update_time_range(&mut self, time_range: &TimeRange) {
        self.time_range = time_range.clone()
    }

    fn work(&mut self, control_events_fn: &mut ControlEventsFn) {
        // TODO: this is wrong, or at least imperfect, because work() happens
        // before generate(), so work() will always be a cycle late.
        if self.value != self.last_value {
            control_events_fn(WorkEvent::Control(self.value.into()));
            self.last_value = self.value;
        }
    }
}
impl Generates<StereoSample> for DroneController {
    fn generate(&mut self, values: &mut [StereoSample]) -> bool {
        self.oscillator_buffer.resize(values.len());
        self.oscillator
            .generate(self.oscillator_buffer.buffer_mut());
        if let Some(v) = self.oscillator_buffer.buffer().first() {
            self.value = (*v).into();
        }
        false
    }
}
impl Default for DroneController {
    fn default() -> Self {
        let mut oscillator = Oscillator::default();
        oscillator.set_frequency(1.0.into());
        Self {
            uid: Default::default(),
            value: Default::default(),
            last_value: Default::default(),
            time_range: Default::default(),
            oscillator,
            oscillator_buffer: Default::default(),
        }
    }
}
