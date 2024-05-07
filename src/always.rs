use ensnare::prelude::*;
use ensnare_proc_macros::{Control, IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Control, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(Controls, TransformsAudio)]
pub struct AlwaysSame {
    uid: Uid,
    value: f64,
}
impl Serializable for AlwaysSame {}
impl HandlesMidi for AlwaysSame {}
impl Generates<StereoSample> for AlwaysSame {
    fn generate(&mut self, values: &mut [StereoSample]) -> bool {
        values.fill(StereoSample::from(self.value));
        self.value != 0.0
    }
}
impl Configurable for AlwaysSame {}
impl Displays for AlwaysSame {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.label(format!("My value: {:0.2}", self.value))
    }
}
impl AlwaysSame {
    pub fn new_with(value: f64) -> Self {
        Self {
            uid: Default::default(),
            value,
        }
    }
}
