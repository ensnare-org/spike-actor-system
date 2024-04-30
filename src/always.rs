use ensnare::prelude::*;
use ensnare_proc_macros::{IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(Controls, TransformsAudio)]
pub struct AlwaysSame {
    uid: Uid,
    value: f64,
}
impl Serializable for AlwaysSame {}
impl HandlesMidi for AlwaysSame {}
impl Generates<StereoSample> for AlwaysSame {
    fn generate(&mut self, values: &mut [StereoSample]) {
        values.fill(StereoSample::from(self.value));
    }
}
impl Configurable for AlwaysSame {}
impl Displays for AlwaysSame {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.label(format!("My value: {:0.2}", self.value))
    }

    fn set_action(&mut self, action: DisplaysAction) {}

    fn take_action(&mut self) -> Option<DisplaysAction> {
        None
    }

    fn set_view_range(&mut self, view_range: &ViewRange) {}
}
impl Controllable for AlwaysSame {}
impl AlwaysSame {
    pub fn new_with(value: f64) -> Self {
        Self {
            uid: Default::default(),
            value,
        }
    }
}
