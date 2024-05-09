use ensnare::{
    automation::ControlValue,
    midi::{MidiChannel, MidiMessage},
    types::{StereoSample, Uid},
};

/// The actor has produced a buffer of audio.
#[derive(Debug, Clone)]
pub struct AudioAction {
    pub(crate) source_uid: Uid,
    pub(crate) frames: Vec<StereoSample>,
}

/// This actor has produced a MIDI message.
#[derive(Debug, Clone)]
pub struct MidiAction {
    pub(crate) source_uid: Uid,
    pub(crate) channel: MidiChannel,
    pub(crate) message: MidiMessage,
}

/// The entity's signal has changed.
#[derive(Debug, Clone)]
pub struct ControlAction {
    pub(crate) source_uid: Uid,
    pub(crate) value: ControlValue,
}
