use eframe::egui::{Color32, Frame, Slider, Stroke};
use ensnare::{
    orchestration::TrackUid,
    traits::Displays,
    types::{Normal, StereoSample},
};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct MixerParamSet {
    level: Normal,
    muted: bool,
    relative_level: f64,
}

#[derive(Debug, Default)]
pub struct Mixer {
    track_uids: Vec<TrackUid>,
    track_param_sets: HashMap<TrackUid, MixerParamSet>,
}
impl Mixer {
    pub(crate) fn add_track(&mut self, track_uid: TrackUid) {
        self.track_uids.push(track_uid);
        self.track_param_sets.insert(track_uid, Default::default());
        self.recalc_relative_levels();
    }

    pub(crate) fn mix(
        &self,
        track_uid: TrackUid,
        source: &[StereoSample],
        dest: &mut [StereoSample],
    ) {
        if let Some(param_set) = self.track_param_sets.get(&track_uid) {
            if !param_set.muted && param_set.level != Normal::minimum() {
                for (src, dst) in source.iter().zip(dest.iter_mut()) {
                    *dst += *src * param_set.relative_level;
                }
            }
        }
    }

    fn recalc_relative_levels(&mut self) {
        let total_level: f64 = self
            .track_param_sets
            .values()
            .map(|param_set| param_set.level.0)
            .sum();
        if total_level > 0.0 {
            self.track_param_sets
                .values_mut()
                .for_each(|param_set| param_set.relative_level = param_set.level.0 / total_level);
        } else {
            // We know we won't be looking at relative_level because each level
            // is zero, so mix will skip it.
        }
    }
}
impl Displays for Mixer {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.horizontal_top(|ui| {
            let mut needs_level_recalc = false;
            for track_uid in self.track_uids.iter() {
                if let Some(param_set) = self.track_param_sets.get_mut(&track_uid) {
                    Frame::default()
                        .stroke(Stroke::new(0.2, Color32::YELLOW))
                        .show(ui, |ui| {
                            ui.set_width(64.0);
                            ui.set_height(192.0);
                            ui.vertical_centered(|ui| {
                                let mut level_f64 = param_set.level.0;
                                if ui
                                    .add(Slider::new(&mut level_f64, Normal::range()).vertical())
                                    .changed()
                                {
                                    param_set.level.set(level_f64);
                                    needs_level_recalc = true;
                                }

                                ui.checkbox(&mut param_set.muted, "Mute");
                            });
                        });
                }
            }
            if needs_level_recalc {
                self.recalc_relative_levels();
            }
        })
        .response
    }
}
