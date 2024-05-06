use crate::{subscription::Subscription, traits::ProvidesActorService, ATOMIC_ORDERING};
use crossbeam_channel::Sender;
use ensnare::prelude::*;
use std::sync::{atomic::AtomicBool, Arc, Mutex};

#[derive(Debug)]
pub enum EntityRequest {
    /// Add a subscriber to our actions.
    Subscribe(Sender<EntityAction>),
    #[allow(dead_code)]
    /// Remove a subscriber from our actions.
    Unsubscribe(Sender<EntityAction>),
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

#[derive(Debug, Clone)]
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
    entity: Arc<Mutex<dyn EntityBounds>>,
    is_sound_active: Arc<AtomicBool>,
}
impl EntityActor {
    #[allow(dead_code)]
    pub fn new_with(entity: impl EntityBounds + 'static) -> Self {
        let uid = entity.uid();
        Self::new_with_wrapped(uid, Arc::new(Mutex::new(entity)))
    }

    pub fn new_and_subscribe(
        entity: impl EntityBounds + 'static,
        sender: &Sender<EntityAction>,
    ) -> Self {
        let uid = entity.uid();
        let actor = Self::new_with_wrapped(uid, Arc::new(Mutex::new(entity)));
        actor.send_request(EntityRequest::Subscribe(sender.clone()));
        actor
    }

    pub fn new_with_wrapped(uid: Uid, entity: Arc<Mutex<dyn EntityBounds>>) -> Self {
        let r = Self {
            uid,
            request_channel_pair: Default::default(),
            entity,
            is_sound_active: Default::default(),
        };
        r.start_input_thread();
        r
    }

    fn start_input_thread(&self) {
        let receiver = self.request_channel_pair.receiver.clone();
        let mut subscription: Subscription<EntityAction> = Default::default();
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
                                subscription.broadcast(EntityAction::Midi(uid, c, m));
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
                        subscription.broadcast(EntityAction::Frames(buffer.buffer().to_vec()));
                    }
                    EntityRequest::Quit => {
                        break;
                    }
                    EntityRequest::NeedsTransformation(frames) => {
                        let count = frames.len();
                        buffer.resize(count);
                        buffer.buffer_mut().copy_from_slice(&frames);
                        entity.lock().unwrap().transform(buffer.buffer_mut());
                        subscription.broadcast(EntityAction::Transformed(buffer.buffer().to_vec()));
                    }
                    EntityRequest::Work(time_range) => {
                        if let Ok(mut entity) = entity.lock() {
                            let uid = entity.uid();
                            entity.update_time_range(&time_range);
                            entity.work(&mut |event| match event {
                                WorkEvent::Midi(channel, message) => {
                                    subscription
                                        .broadcast(EntityAction::Midi(uid, channel, message));
                                }
                                WorkEvent::MidiForTrack(_, _, _) => {
                                    todo!("This might be obsolete or not applicable here")
                                }
                                WorkEvent::Control(value) => {
                                    subscription.broadcast(EntityAction::Control(uid, value));
                                }
                            });
                        }
                    }
                    EntityRequest::Subscribe(sender) => {
                        subscription.subscribe(&sender);
                    }
                    EntityRequest::Unsubscribe(sender) => {
                        subscription.unsubscribe(&sender);
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
    fn sender(&self) -> &Sender<EntityRequest> {
        &self.request_channel_pair.sender
    }
}
impl Displays for EntityActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        self.entity.lock().unwrap().ui(ui)
    }
}
