use ensnare::prelude::*;
use ensnare_proc_macros::{Control, IsEntity, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Control, IsEntity, Metadata, Serialize, Deserialize)]
#[entity(Controls, TransformsAudio)]
pub struct BusyWaiter {
    uid: Uid,
}
impl Serializable for BusyWaiter {}
impl HandlesMidi for BusyWaiter {}
impl Generates<StereoSample> for BusyWaiter {
    fn generate(&mut self, _values: &mut [StereoSample]) -> bool {
        let aaa: usize = (0..=100000).sum();
        println!("{aaa}"); // Don't remove this; it keeps the work from being optimized away
        true
    }
}
impl Configurable for BusyWaiter {}
impl Displays for BusyWaiter {}
