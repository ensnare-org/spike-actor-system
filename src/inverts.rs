use ensnare::prelude::*;
use ensnare_proc_macros::{IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(Controls, GeneratesStereoSample)]
pub struct AlwaysSame {
    uid: Uid,
}
impl TransformsAudio for AlwaysSame {
    fn transform(&mut self, samples: &mut [StereoSample]) {
        for sample in samples {
            *sample = StereoSample(
                self.transform_channel(0, sample.0),
                self.transform_channel(1, sample.1),
            )
        }
    }

    fn transform_channel(&mut self, _channel: usize, input_sample: Sample) -> Sample {
        -input_sample
    }
}
impl Serializable for AlwaysSame {}
impl HandlesMidi for AlwaysSame {}
impl Configurable for AlwaysSame {}
impl Displays for AlwaysSame {}
impl Controllable for AlwaysSame {}
