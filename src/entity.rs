use ensnare_v1::prelude::*;
use crate::{
    actions::{AudioAction, ControlAction, MidiAction},
    subscription::Subscription,
    traits::ProvidesActorService,
    ATOMIC_ORDERING,
};
use crossbeam_channel::{Select, Sender};
use ensnare::{prelude::*, types::CrossbeamChannel};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

#[derive(Debug, Clone)]
pub enum EntityRequest {
    /// Connect a receiver to this entity's audio output.
    ActionSubscribe(Sender<AudioAction>),
    /// Disconnect a receiver from this entity's audio output.
    ActionUnsubscribe(Sender<AudioAction>),
    /// Connect a MIDI receiver to this entity's MIDI output.
    MidiSubscribe(Sender<MidiAction>),
    /// Disconnect a MIDI receiver from this entity's MIDI output.
    MidiUnsubscribe(Sender<MidiAction>),
    /// Link another entity to this entity's control output.
    ControlSubscribe(Sender<ControlAction>),
    /// Disconnect another entity from this entity's control output.
    ControlUnsubscribe(Sender<ControlAction>),
    /// Link this entity's controllable parameter to the specified source entity.
    ControlLinkAdd(Uid, ControlIndex),
    /// Unlink this entity's controllable parameter from the specified source entity.
    ControlLinkRemove(Uid, ControlIndex),
    /// The entity should handle this message (if it listens on this channel).
    /// As with [EntityRequest::Work], it can produce [MidiAction] and/or
    /// [ControlAction].
    Midi(MidiChannel, MidiMessage),
    /// The entity should adjust the given control as specified.
    Control(ControlIndex, ControlValue),
    /// The entity should perform work for the given slice of time. During this
    /// time slice, it can produce any number of [MidiAction] and/or
    /// [ControlAction].
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
pub struct EntityActor {
    /// Incoming requests to this entity.
    requests: CrossbeamChannel<EntityRequest>,

    /// This entity's audio subscriptions (actions from other entities).
    audio_actions: CrossbeamChannel<AudioAction>,

    /// Control receiver channel.
    control_actions: CrossbeamChannel<ControlAction>,

    /// A cached copy of entity's [Uid].
    uid: Uid,

    /// The wrapped entity.
    pub(crate) entity: Arc<Mutex<dyn Entity>>,

    /// Have we just emitted sound? Used for GUI activity indicators.
    is_sound_active: Arc<AtomicBool>,
}
impl EntityActor {
    pub(crate) fn new_with(entity: impl Entity + 'static) -> Self {
        let uid = entity.uid();
        Self::new_with_wrapped(uid, Arc::new(Mutex::new(entity)))
    }

    pub(crate) fn new_with_wrapped(uid: Uid, entity: Arc<Mutex<dyn Entity>>) -> Self {
        let r = Self {
            requests: Default::default(),
            audio_actions: Default::default(),
            control_actions: Default::default(),
            uid,
            entity,
            is_sound_active: Default::default(),
        };
        r.start_input_thread();
        r
    }

    fn start_input_thread(&self) {
        let request_receiver = self.requests.receiver.clone();
        let mut audio_subscription: Subscription<AudioAction> = Default::default();
        let mut midi_subscription: Subscription<MidiAction> = Default::default();
        let mut control_subscription: Subscription<ControlAction> = Default::default();
        let mut source_uid_to_control_indexes: HashMap<Uid, Vec<ControlIndex>> = Default::default();
        let entity = Arc::clone(&self.entity);
        let mut buffer = GenerationBuffer::<StereoSample>::default();
        let is_sound_active = Arc::clone(&self.is_sound_active);
        let action_receiver = self.audio_actions.receiver.clone();
        let control_receiver = self.control_actions.receiver.clone();
        let uid = self.uid;

        std::thread::spawn(move || {
            let midi_channel_pair: CrossbeamChannel<MidiAction> = Default::default();
            let midi_receiver = midi_channel_pair.receiver.clone();

            let mut sel = Select::default();
            let request_index = sel.recv(&request_receiver);
            let action_index = sel.recv(&action_receiver);
            let midi_index = sel.recv(&midi_receiver);
            let control_index = sel.recv(&control_receiver);

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
                                        &mut midi_subscription,
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
                                    audio_subscription.broadcast_mut(AudioAction {
                                        source_uid: uid,
                                        frames: buffer.buffer().into(),
                                    });
                                }
                                EntityRequest::Quit => {
                                    break;
                                }
                                EntityRequest::NeedsTransformation(frames) => {
                                    let count = frames.len();
                                    buffer.resize(count);
                                    buffer.buffer_mut().copy_from_slice(&frames);
                                    entity.lock().unwrap().transform(buffer.buffer_mut());
                                    audio_subscription.broadcast_mut(AudioAction {
                                        source_uid: uid,
                                        frames: buffer.buffer().into(),
                                    });
                                }
                                EntityRequest::Work(time_range) => {
                                    if let Ok(mut entity) = entity.lock() {
                                        entity.update_time_range(&time_range);
                                        entity.work(&mut |event| match event {
                                            WorkEvent::Midi(channel, message) => {
                                                midi_subscription.broadcast_mut(MidiAction {
                                                    source_uid: uid,
                                                    channel,
                                                    message,
                                                });
                                            }
                                            WorkEvent::MidiForTrack(_, _, _) => {
                                                todo!(
                                                    "This might be obsolete or not applicable here"
                                                )
                                            }
                                            WorkEvent::Control(value) => {
                                                control_subscription.broadcast_mut(ControlAction {
                                                    source_uid: uid,
                                                    value,
                                                });
                                            }
                                        });
                                    }
                                }
                                EntityRequest::ActionSubscribe(sender) => {
                                    audio_subscription.subscribe(&sender);
                                }
                                EntityRequest::ActionUnsubscribe(sender) => {
                                    audio_subscription.unsubscribe(&sender);
                                }
                                EntityRequest::MidiSubscribe(sender) => {
                                    midi_subscription.subscribe(&sender)
                                }
                                EntityRequest::MidiUnsubscribe(sender) => {
                                    midi_subscription.unsubscribe(&sender)
                                }
                                EntityRequest::ControlSubscribe(sender) => {
                                    control_subscription.subscribe(&sender)
                                }
                                EntityRequest::ControlUnsubscribe(sender) => {
                                    control_subscription.unsubscribe(&sender)
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
                        if let Ok(_action) = Self::recv_operation(operation, &action_receiver) {
                            panic!("this shouldn't happen")
                        }
                    }
                    index if index == midi_index => {
                        if let Ok(action) = Self::recv_operation(operation, &midi_receiver) {
                            Self::handle_midi(
                                &entity,
                                action.channel,
                                action.message,
                                &mut midi_subscription,
                            )
                        }
                    }
                    index if index == control_index => {
                        if let Ok(action) = Self::recv_operation(operation, &control_receiver) {
                            if let Some(indexes) =
                                source_uid_to_control_indexes.get(&action.source_uid)
                            {
                                if let Ok(mut entity) = entity.lock() {
                                    for &index in indexes {
                                        entity.control_set_param_by_index(index, action.value)
                                    }
                                }
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
        let _ = self.requests.sender.try_send(msg);
    }

    pub(crate) fn uid(&self) -> Uid {
        self.uid
    }

    pub(crate) fn is_sound_active(&self) -> bool {
        self.is_sound_active.load(ATOMIC_ORDERING)
    }

    fn handle_midi(
        entity: &Arc<Mutex<dyn Entity>>,
        channel: MidiChannel,
        message: MidiMessage,
        subscription: &mut Subscription<MidiAction>,
    ) {
        if let Ok(mut entity) = entity.lock() {
            let uid = entity.uid();
            entity.handle_midi_message(channel, message, &mut |c, m| {
                subscription.broadcast_mut(MidiAction {
                    source_uid: uid,
                    channel: c,
                    message: m,
                });
            });
        }
    }

    pub(crate) fn control_sender(&self) -> &Sender<ControlAction> {
        &self.control_actions.sender
    }
}

impl ProvidesActorService<EntityRequest, AudioAction> for EntityActor {
    fn sender(&self) -> &Sender<EntityRequest> {
        &self.requests.sender
    }
}
impl Displays for EntityActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        self.entity.lock().unwrap().ui(ui)
    }
}
