use ensnare::{
    midi::{MidiChannel, MidiMessage},
    types::Uid,
};

/// This actor has emitted a MIDI message.
#[derive(Debug, Clone)]
pub struct MidiAction {
    pub(crate) source_uid: Uid,
    pub(crate) channel: MidiChannel,
    pub(crate) message: MidiMessage,
}
