use crate::{traits::ProvidesActorService, ATOMIC_ORDERING};
use crossbeam_channel::Sender;
use eframe::egui::{Color32, Frame, Margin, Stroke};
use ensnare::prelude::*;
use std::sync::{atomic::AtomicBool, Arc, Mutex};

#[derive(Debug)]
pub enum EntityRequest {
    /// The entity should handle this message (if it listens on this channel).
    /// As with [EntityRequest::Work], it can produce [EntityAction::Midi]
    /// and/or [EntityAction::Control].
    Midi(MidiChannel, MidiMessage),
    /// The entity should adjust the given control as specified.
    Control(ControlIndex, ControlValue),
    /// The entity should perform work for the given slice of time. During this
    /// time slice, it can produce any number of [EntityAction::Midi] and/or
    /// [EntityAction::Control].
    Work(TimeRange),
    /// The entity should produce the specified number of frames of audio via
    /// [EntityAction::Frames]. If it doesn't produce audio, it should produce a
    /// silent buffer.
    NeedsAudio(usize),
    /// The entity should transform the given buffer of audio via
    /// [EntityAction::Transformed]. If it doesn't transform audio, it should
    /// return the buffer unchanged.
    NeedsTransformation(Vec<StereoSample>),
    /// The entity should exit.
    Quit,
}

#[derive(Debug)]
pub enum EntityAction {
    /// The entity has emitted a MIDI message.
    Midi(Uid, MidiChannel, MidiMessage),
    /// The entity's signal has changed.
    Control(Uid, ControlValue),
    /// The entity has produced a buffer of audio.
    Frames(Vec<StereoSample>),
    /// The entity has transformed a buffer of audio.
    Transformed(Vec<StereoSample>),
}

#[derive(Debug)]
pub struct EntityActor {
    uid: Uid,
    request_channel_pair: ChannelPair<EntityRequest>,
    action_sender: Sender<EntityAction>,
    entity: Arc<Mutex<dyn EntityBounds>>,
    is_sound_active: Arc<AtomicBool>,
}
impl EntityActor {
    pub fn new_with(
        entity: impl EntityBounds + 'static,
        action_sender: &Sender<EntityAction>,
    ) -> Self {
        let uid = entity.uid();
        Self::new_with_wrapped(uid, Arc::new(Mutex::new(entity)), action_sender)
    }

    pub fn new_with_wrapped(
        uid: Uid,
        entity: Arc<Mutex<dyn EntityBounds>>,
        action_sender: &Sender<EntityAction>,
    ) -> Self {
        let r = Self {
            uid,
            request_channel_pair: Default::default(),
            action_sender: action_sender.clone(),
            entity,
            is_sound_active: Default::default(),
        };
        r.start_input_thread();
        r
    }

    fn start_input_thread(&self) {
        let receiver = self.request_channel_pair.receiver.clone();
        let sender = self.action_sender.clone();
        let entity = Arc::clone(&self.entity);
        let mut buffer = GenerationBuffer::<StereoSample>::default();
        let is_sound_active = Arc::clone(&self.is_sound_active);

        std::thread::spawn(move || {
            while let Ok(input) = receiver.recv() {
                match input {
                    EntityRequest::Midi(channel, message) => {
                        if let Ok(mut entity) = entity.lock() {
                            let uid = entity.uid();
                            entity.handle_midi_message(channel, message, &mut |c, m| {
                                let _ = sender.try_send(EntityAction::Midi(uid, c, m));
                            });
                        }
                    }
                    EntityRequest::Control(index, value) => {
                        entity
                            .lock()
                            .unwrap()
                            .control_set_param_by_index(index, value);
                    }
                    EntityRequest::NeedsAudio(count) => {
                        buffer.resize(count);
                        buffer.clear();
                        is_sound_active.store(
                            entity.lock().unwrap().generate(buffer.buffer_mut()),
                            ATOMIC_ORDERING,
                        );
                        let _ = sender.try_send(EntityAction::Frames(buffer.buffer().to_vec()));
                    }
                    EntityRequest::Quit => {
                        break;
                    }
                    EntityRequest::NeedsTransformation(frames) => {
                        let count = frames.len();
                        buffer.resize(count);
                        buffer.buffer_mut().copy_from_slice(&frames);
                        entity.lock().unwrap().transform(buffer.buffer_mut());
                        let _ =
                            sender.try_send(EntityAction::Transformed(buffer.buffer().to_vec()));
                    }
                    EntityRequest::Work(time_range) => {
                        if let Ok(mut entity) = entity.lock() {
                            let uid = entity.uid();
                            entity.update_time_range(&time_range);
                            entity.work(&mut |event| match event {
                                WorkEvent::Midi(channel, message) => {
                                    let _ =
                                        sender.try_send(EntityAction::Midi(uid, channel, message));
                                }
                                WorkEvent::MidiForTrack(_, _, _) => {
                                    todo!("This might be obsolete or not applicable here")
                                }
                                WorkEvent::Control(value) => {
                                    let _ = sender.try_send(EntityAction::Control(uid, value));
                                }
                            });
                        }
                    }
                }
            }
        });
    }

    pub fn send(&self, msg: EntityRequest) {
        let _ = self.request_channel_pair.sender.try_send(msg);
    }

    pub(crate) fn uid(&self) -> Uid {
        self.uid
    }

    pub(crate) fn is_sound_active(&self) -> bool {
        self.is_sound_active.load(ATOMIC_ORDERING)
    }
}
impl ProvidesActorService<EntityRequest, EntityAction> for EntityActor {
    fn action_sender(&self) -> &Sender<EntityAction> {
        &self.action_sender
    }

    fn sender(&self) -> &Sender<EntityRequest> {
        &self.request_channel_pair.sender
    }
}
impl Displays for EntityActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        self.entity.lock().unwrap().ui(ui)
    }
}
