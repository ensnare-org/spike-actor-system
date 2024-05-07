use eframe::egui::DragValue;
use ensnare::prelude::*;
use ensnare_proc_macros::{Control, IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Control, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(Controls, GeneratesStereoSample)]
pub struct Quietener {
    uid: Uid,
    #[control]
    quiet_factor: Normal,
}
impl TransformsAudio for Quietener {
    fn transform(&mut self, samples: &mut [StereoSample]) {
        for sample in samples {
            *sample = StereoSample(
                self.transform_channel(0, sample.0),
                self.transform_channel(1, sample.1),
            )
        }
    }

    fn transform_channel(&mut self, _channel: usize, input_sample: Sample) -> Sample {
        input_sample * self.quiet_factor
    }
}
impl Serializable for Quietener {}
impl HandlesMidi for Quietener {}
impl Configurable for Quietener {}
impl Displays for Quietener {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        let mut v = self.quiet_factor.0;
        let response = ui.add(
            DragValue::new(&mut v)
                .prefix("Quiet level: ")
                .fixed_decimals(2)
                .speed(0.01)
                .clamp_range(0.0..=1.0),
        );
        if response.changed() {
            self.quiet_factor.set(v);
        }
        response
    }
}
impl Quietener {
    pub fn set_quiet_factor(&mut self, quiet_factor: Normal) {
        self.quiet_factor = quiet_factor;
    }
}
