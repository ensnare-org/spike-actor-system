use ensnare::prelude::*;
use ensnare_proc_macros::{IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(Controls, GeneratesStereoSample)]
pub struct Quietener {
    uid: Uid,
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
        input_sample * 0.5
    }
}
impl Serializable for Quietener {}
impl HandlesMidi for Quietener {}
impl Configurable for Quietener {}
impl Displays for Quietener {}
impl Controllable for Quietener {}
