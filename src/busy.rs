use ensnare::{prelude::*, util::Rng};
use ensnare_proc_macros::{IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(Controls, TransformsAudio)]
pub struct BusyWaiter {
    uid: Uid,
    #[serde(skip)]
    rng: Rng,
}
impl Serializable for BusyWaiter {}
impl HandlesMidi for BusyWaiter {}
impl Generates<StereoSample> for BusyWaiter {
    fn generate(&mut self, values: &mut [StereoSample]) {
        let aaa: usize = (0..=100000).sum();
        println!("{aaa}");
        for v in values.iter_mut() {
            *v = StereoSample::from(self.rng.rand_float());
        }
    }
}
impl Configurable for BusyWaiter {}
impl Displays for BusyWaiter {}
impl Controllable for BusyWaiter {}
