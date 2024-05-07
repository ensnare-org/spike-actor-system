use crate::{subscription::Subscription, traits::ProvidesActorService, ATOMIC_ORDERING};
use crossbeam_channel::{Select, Sender};
use ensnare::prelude::*;
use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

#[derive(Debug, Clone)]
pub enum EntityRequest {
    /// Connect a MIDI receiver to this entity's MIDI output.
    MidiSubscribe(Sender<EntityAction>),
    #[allow(dead_code)]
    /// Disconnect a MIDI receiver from this entity's MIDI output.
    MidiUnsubscribe(Sender<EntityAction>),
    /// Link another entity to this entity's control output.
    ControlSubscribe(Sender<EntityAction>),
    #[allow(dead_code)]
    /// Disconnect another entity from this entity's control output.
    ControlUnsubscribe(Sender<EntityAction>),
    /// Link this entity's controllable parameter to the specified source entity.
    ControlLinkAdd(Uid, ControlIndex),
    /// Unlink this entity's controllable parameter from the specified source entity.
    ControlLinkRemove(Uid, ControlIndex),
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
    /// Incoming requests to this entity.
    request_channel_pair: ChannelPair<EntityRequest>,

    /// Receipient of this entity's actions.
    action_channel_pair: ChannelPair<EntityAction>,

    /// A cached copy of entity's [Uid].
    uid: Uid,

    /// The wrapped entity.
    pub(crate) entity: Arc<Mutex<dyn EntityBounds>>,

    /// Have we just emitted sound? Used for GUI activity indicators.
    is_sound_active: Arc<AtomicBool>,
}
impl EntityActor {
    pub(crate) fn new_with(entity: impl EntityBounds + 'static) -> Self {
        let uid = entity.uid();
        Self::new_with_wrapped(uid, Arc::new(Mutex::new(entity)))
    }

    pub(crate) fn new_with_wrapped(uid: Uid, entity: Arc<Mutex<dyn EntityBounds>>) -> Self {
        let r = Self {
            request_channel_pair: Default::default(),
            action_channel_pair: Default::default(),
            uid,
            entity,
            is_sound_active: Default::default(),
        };
        r.start_input_thread();
        r
    }

    fn start_input_thread(&self) {
        let request_receiver = self.request_channel_pair.receiver.clone();
        let mut midi_subscription: Subscription<EntityAction> = Default::default();
        let mut control_links: Subscription<EntityAction> = Default::default();
        let mut source_uid_to_control_indexes: HashMap<Uid, Vec<ControlIndex>> = Default::default();
        let entity = Arc::clone(&self.entity);
        let mut buffer = GenerationBuffer::<StereoSample>::default();
        let is_sound_active = Arc::clone(&self.is_sound_active);
        let action_receiver = self.action_channel_pair.receiver.clone();

        std::thread::spawn(move || {
            let mut sel = Select::default();
            let request_index = sel.recv(&request_receiver);
            let action_index = sel.recv(&action_receiver);

            loop {
                let operation = sel.select();
                match operation.index() {
                    index if index == request_index => {
                        if let Ok(request) = Self::recv_operation(operation, &request_receiver) {
                            match request {
                                EntityRequest::Midi(channel, message) => {
                                    Self::handle_midi(
                                        &entity,
                                        channel,
                                        message,
                                        &midi_subscription,
                                    );
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
                                    let is_active =
                                        entity.lock().unwrap().generate(buffer.buffer_mut());
                                    is_sound_active.store(is_active, ATOMIC_ORDERING);
                                    midi_subscription
                                        .broadcast(EntityAction::Frames(buffer.buffer().to_vec()));
                                }
                                EntityRequest::Quit => {
                                    break;
                                }
                                EntityRequest::NeedsTransformation(frames) => {
                                    let count = frames.len();
                                    buffer.resize(count);
                                    buffer.buffer_mut().copy_from_slice(&frames);
                                    entity.lock().unwrap().transform(buffer.buffer_mut());
                                    midi_subscription.broadcast(EntityAction::Transformed(
                                        buffer.buffer().to_vec(),
                                    ));
                                }
                                EntityRequest::Work(time_range) => {
                                    if let Ok(mut entity) = entity.lock() {
                                        let uid = entity.uid();
                                        entity.update_time_range(&time_range);
                                        entity.work(&mut |event| match event {
                                            WorkEvent::Midi(channel, message) => {
                                                midi_subscription.broadcast(EntityAction::Midi(
                                                    uid, channel, message,
                                                ));
                                            }
                                            WorkEvent::MidiForTrack(_, _, _) => {
                                                todo!(
                                                    "This might be obsolete or not applicable here"
                                                )
                                            }
                                            WorkEvent::Control(value) => {
                                                control_links
                                                    .broadcast(EntityAction::Control(uid, value));
                                            }
                                        });
                                    }
                                }
                                EntityRequest::MidiSubscribe(sender) => {
                                    midi_subscription.subscribe(&sender)
                                }
                                EntityRequest::MidiUnsubscribe(sender) => {
                                    midi_subscription.unsubscribe(&sender)
                                }
                                EntityRequest::ControlSubscribe(sender) => {
                                    control_links.subscribe(&sender)
                                }
                                EntityRequest::ControlUnsubscribe(sender) => {
                                    control_links.unsubscribe(&sender)
                                }
                                EntityRequest::ControlLinkAdd(uid, index) => {
                                    source_uid_to_control_indexes
                                        .entry(uid)
                                        .or_default()
                                        .push(index)
                                }
                                EntityRequest::ControlLinkRemove(uid, index) => {
                                    if let Some(indexes) =
                                        source_uid_to_control_indexes.get_mut(&uid)
                                    {
                                        indexes.retain(|&i| i != index)
                                    }
                                }
                            }
                        }
                    }
                    index if index == action_index => {
                        if let Ok(action) = Self::recv_operation(operation, &action_receiver) {
                            match action {
                                EntityAction::Midi(_uid, channel, message) => {
                                    Self::handle_midi(&entity, channel, message, &midi_subscription)
                                }
                                EntityAction::Control(uid, value) => {
                                    if let Some(indexes) = source_uid_to_control_indexes.get(&uid) {
                                        if let Ok(mut entity) = entity.lock() {
                                            for &index in indexes {
                                                entity.control_set_param_by_index(index, value)
                                            }
                                        }
                                    }
                                }
                                EntityAction::Frames(_) => panic!("this shouldn't happen"),
                                EntityAction::Transformed(_) => panic!("this shouldn't happen"),
                            }
                        }
                    }
                    _ => {
                        panic!("Unexpected select index")
                    }
                }
            }
        });
    }

    pub(crate) fn send(&self, msg: EntityRequest) {
        let _ = self.request_channel_pair.sender.try_send(msg);
    }

    pub(crate) fn uid(&self) -> Uid {
        self.uid
    }

    pub(crate) fn is_sound_active(&self) -> bool {
        self.is_sound_active.load(ATOMIC_ORDERING)
    }

    fn handle_midi(
        entity: &Arc<Mutex<dyn EntityBounds>>,
        channel: MidiChannel,
        message: MidiMessage,
        subscription: &Subscription<EntityAction>,
    ) {
        if let Ok(mut entity) = entity.lock() {
            let uid = entity.uid();
            entity.handle_midi_message(channel, message, &mut |c, m| {
                subscription.broadcast(EntityAction::Midi(uid, c, m));
            });
        }
    }
}

impl ProvidesActorService<EntityRequest, EntityAction> for EntityActor {
    fn sender(&self) -> &Sender<EntityRequest> {
        &self.request_channel_pair.sender
    }

    fn action_sender(&self) -> &Sender<EntityAction> {
        &self.action_channel_pair.sender
    }
}
impl Displays for EntityActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        self.entity.lock().unwrap().ui(ui)
    }
}
