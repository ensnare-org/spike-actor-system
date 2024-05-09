use crate::{
    always::AlwaysSame,
    arp::Arpeggiator,
    busy::BusyWaiter,
    drone::DroneController,
    entity::{ControlAction, EntityAction, EntityActor, EntityRequest},
    midi::MidiAction,
    mixer::Mixer,
    quietener::Quietener,
    subscription::Subscription,
    traits::ProvidesActorService,
};
use anyhow::anyhow;
use crossbeam_channel::{Select, Sender};
use eframe::egui::{ComboBox, Frame, Margin};
use ensnare::prelude::*;
use ensnare_toys::{ToyInstrument, ToySynth};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
pub enum TrackRequest {
    /// Add a subscriber to our actions.
    Subscribe(Sender<TrackAction>),
    /// Remove a subscriber from our actions.
    Unsubscribe(Sender<TrackAction>),
    /// The track should handle an incoming MIDI message.
    Midi(MidiChannel, MidiMessage),
    /// The track should perform work for the given slice of time.
    Work(TimeRange),
    /// The track should generate a buffer of audio frames.
    NeedsAudio(usize),
    /// This track should consume the given track's output. All tracks,
    /// including the master track, accept sends. An aux track is one whose
    /// audio sources are only sends.
    AddSend(TrackUid, Sender<TrackRequest>),
    /// This track should stop consuming the given track's output.
    RemoveSend(TrackUid),
    /// The [TrackActor] should exit.
    Quit,
}

/// The [Track] has produced some work that the parent should look at.
#[derive(Debug, Clone)]
pub enum TrackAction {
    /// The [Track] has produced a MIDI message that should be considered for
    /// routing elsewhere. The track has already routed it appropriately within
    /// itself, and now it's telling anyone else interested.
    Midi(MidiChannel, MidiMessage),
    /// This track has produced a buffer of frames.
    Frames(TrackUid, Vec<StereoSample>),
}

#[derive(Debug)]
struct TrackActorStateMachine {
    track_uid: TrackUid,
    is_master_track: bool,
    state: TrackActorState,
    buffer: GenerationBuffer<StereoSample>,
    track: Arc<Mutex<Track>>,
    subscription: Subscription<TrackAction>,
}
impl TrackActorStateMachine {
    fn new_with(track: &Arc<Mutex<Track>>) -> Self {
        let (track_uid, is_master_track) = {
            let track = track.lock().unwrap();
            (track.uid, track.is_master_track)
        };

        Self {
            track_uid,
            is_master_track,
            state: Default::default(),
            buffer: Default::default(),
            track: Arc::clone(track),
            subscription: Default::default(),
        }
    }

    fn handle_entity_action(&mut self, action: EntityAction) {
        match action {
            EntityAction::Frames(frames) => {
                self.handle_incoming_frames(frames);
            }
            EntityAction::Transformed(frames) => {
                self.handle_incoming_frames(frames);
            }
        }
    }

    fn handle_midi_action(&mut self, action: MidiAction) {
        self.subscription
            .broadcast_mut(TrackAction::Midi(action.channel, action.message));
        // TODO: opportunity to use direct channels?
        if let Ok(track) = self.track.lock() {
            for actor in track
                .actors
                .values()
                .filter(|&a| a.uid() != action.source_uid)
            {
                actor.send(EntityRequest::Midi(action.channel, action.message));
            }
        }
    }

    fn handle_track_action(&mut self, action: TrackAction) {
        match action {
            TrackAction::Midi(channel, message) => {
                self.subscription
                    .broadcast_mut(TrackAction::Midi(channel, message));
            }
            TrackAction::Frames(track_uid, frames) => {
                if self.track.lock().unwrap().mixer.is_some() {
                    self.handle_incoming_track_frames(track_uid, frames);
                } else {
                    self.handle_incoming_frames(frames);
                }
            }
        }
    }

    fn handle_incoming_frames(&mut self, frames: Vec<StereoSample>) {
        assert!(frames.len() <= 64);
        match &self.state {
            TrackActorState::Idle => panic!("We got frames when we weren't expecting any"),
            TrackActorState::AwaitingSources(_) => {
                // We got some audio from someone. Mix it into the track buffer.
                self.buffer.merge(&frames);
                self.advance_state_awaiting_sources();
            }
            TrackActorState::AwaitingEffect(_) => {
                // An effect completed processing. Copy its results over the
                // current buffer.
                self.buffer.buffer_mut().copy_from_slice(&frames);
                self.advance_state_awaiting_effect();
            }
        }
    }

    fn handle_incoming_track_frames(&mut self, track_uid: TrackUid, frames: Vec<StereoSample>) {
        assert!(frames.len() <= 64);
        assert!(matches!(self.state, TrackActorState::AwaitingSources(..)));
        assert!(self.is_master_track);

        if let Ok(track) = self.track.lock() {
            if let Some(mixer) = track.mixer.as_ref() {
                mixer.mix(track_uid, &frames, self.buffer.buffer_mut());
            }
        }
        self.advance_state_awaiting_sources();
    }

    fn advance_state_awaiting_sources(&mut self) {
        match &self.state {
            TrackActorState::Idle => panic!("invalid starting state"),
            TrackActorState::AwaitingSources(count) => {
                // We got a frame. See if we've gotten all the ones we expect.
                if *count == 1 {
                    // We have. Now it's time to let the effects process what we
                    // have.
                    if let Ok(track) = self.track.lock() {
                        self.state = TrackActorState::AwaitingEffect(VecDeque::from(
                            track.ordered_actor_uids.clone(),
                        ));
                    }
                    self.advance_state_awaiting_effect();
                } else {
                    self.state = TrackActorState::AwaitingSources(count - 1);
                }
            }
            TrackActorState::AwaitingEffect(_) => {
                panic!("invalid state")
            }
        }
    }

    fn advance_state_awaiting_effect(&mut self) {
        if let TrackActorState::AwaitingEffect(uids) = &mut self.state {
            if let Some(uid) = uids.pop_front() {
                if let Ok(track) = self.track.lock() {
                    if let Some(actor) = track.actors.get(&uid) {
                        actor.send_request(EntityRequest::NeedsTransformation(
                            self.buffer.buffer().to_vec(),
                        ));
                    }
                }
            } else {
                // We're out of effects. Send what we have!
                self.issue_outgoing_frames_action();
            }
        } else {
            panic!("wrong state: {:?}", self.state);
        }
    }

    fn issue_outgoing_frames_action(&mut self) {
        self.state = TrackActorState::Idle;
        self.subscription.broadcast_mut(TrackAction::Frames(
            self.track_uid,
            self.buffer.buffer().to_vec(),
        ));
    }

    fn handle_needs_audio(&mut self, count: usize) {
        assert!(
            matches!(self.state, TrackActorState::Idle),
            "{}: expected a clean slate",
            self.track.lock().unwrap().uid
        );
        self.buffer.resize(count);
        self.buffer.clear();

        // if we have source tracks, start them. Same for instruments.
        let mut have_nonzero_sources = false;
        if let Ok(track) = self.track.lock() {
            let new_sources_count = track.send_tracks.len() + track.actors.len();
            self.state = TrackActorState::AwaitingSources(new_sources_count);
            for source in track.send_tracks.values() {
                let _ = source.try_send(TrackRequest::NeedsAudio(count));
            }
            for actor in track.actors.values() {
                actor.send(EntityRequest::NeedsAudio(count));
            }
            have_nonzero_sources = new_sources_count != 0;
        }

        // Did we have any sources in the first place?
        if !have_nonzero_sources {
            self.issue_outgoing_frames_action();
        } else {
            // Nothing to do now but wait for incoming Frames from our sources
        }
    }

    fn handle_subscribe(&mut self, sender: Sender<TrackAction>) {
        self.subscription.subscribe(&sender);
    }

    fn handle_unsubscribe(&mut self, sender: Sender<TrackAction>) {
        self.subscription.unsubscribe(&sender);
    }
}

#[derive(Default, Debug)]
enum TrackActorState {
    #[default]
    Idle,
    AwaitingSources(usize),
    AwaitingEffect(VecDeque<Uid>),
}

#[derive(Debug)]
pub struct TrackActor {
    /// Receives requests.
    request_channel_pair: ChannelPair<TrackRequest>,

    /// Receives child track actions.
    track_action_channel_pair: ChannelPair<TrackAction>,

    inner: Arc<Mutex<Track>>,
}
impl Displays for TrackActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        self.inner.lock().unwrap().ui(ui)
    }
}
impl ProvidesActorService<TrackRequest, TrackAction> for TrackActor {
    fn sender(&self) -> &Sender<TrackRequest> {
        &self.request_channel_pair.sender
    }

    fn action_sender(&self) -> &Sender<TrackAction> {
        &self.track_action_channel_pair.sender
    }
}
impl TrackActor {
    pub(crate) fn new_with(
        track_uid: TrackUid,
        is_master_track: bool,
        uid_factory: &Arc<EntityUidFactory>,
    ) -> Self {
        let entity_action_channel_pair: ChannelPair<EntityAction> = Default::default();
        let midi_action_channel_pair: ChannelPair<MidiAction> = Default::default();
        let control_action_channel_pair: ChannelPair<ControlAction> = Default::default();
        let action_subscription_senders = ActionSubscriptionSenders {
            entity: entity_action_channel_pair.sender.clone(),
            midi: midi_action_channel_pair.sender.clone(),
            control: control_action_channel_pair.sender.clone(),
        };

        let track = Track::new_with(
            track_uid,
            is_master_track,
            action_subscription_senders,
            uid_factory,
        );
        let mut r = Self {
            request_channel_pair: Default::default(),
            track_action_channel_pair: Default::default(),
            inner: Arc::new(Mutex::new(track)),
        };

        r.start_thread(
            entity_action_channel_pair,
            midi_action_channel_pair,
            control_action_channel_pair,
        );

        r
    }

    fn start_thread(
        &mut self,
        entity_action_channel_pair: ChannelPair<EntityAction>,
        midi_action_channel_pair: ChannelPair<MidiAction>,
        control_action_channel_pair: ChannelPair<ControlAction>,
    ) {
        let input_receiver = self.request_channel_pair.receiver.clone();
        let track = Arc::clone(&self.inner);
        let track_receiver = self.track_action_channel_pair.receiver.clone();
        let mut state_machine = TrackActorStateMachine::new_with(&self.inner);

        std::thread::spawn(move || {
            let mut sel = Select::default();

            let entity_receiver = entity_action_channel_pair.receiver.clone();
            let midi_receiver = midi_action_channel_pair.receiver.clone();
            let control_receiver = control_action_channel_pair.receiver.clone();

            let input_index = sel.recv(&input_receiver);
            let entity_index = sel.recv(&entity_receiver);
            let midi_index = sel.recv(&midi_receiver);
            let control_index = sel.recv(&control_receiver);
            let track_index = sel.recv(&track_receiver);

            loop {
                let operation = sel.select();
                match operation.index() {
                    index if index == input_index => {
                        if let Ok(request) = Self::recv_operation(operation, &input_receiver) {
                            match request {
                                TrackRequest::Midi(channel, message) => {
                                    if let Ok(mut track) = track.lock() {
                                        track
                                            .entity_request_subscription
                                            .broadcast_mut(EntityRequest::Midi(channel, message));
                                    }
                                }
                                TrackRequest::NeedsAudio(count) => {
                                    state_machine.handle_needs_audio(count);
                                }
                                TrackRequest::Quit => {
                                    if let Ok(mut track) = track.lock() {
                                        track
                                            .entity_request_subscription
                                            .broadcast_mut(EntityRequest::Quit);
                                    }
                                    break;
                                }
                                TrackRequest::Work(time_range) => {
                                    if let Ok(mut track) = track.lock() {
                                        track
                                            .entity_request_subscription
                                            .broadcast_mut(EntityRequest::Work(time_range.clone()));
                                    }
                                }
                                TrackRequest::AddSend(uid, sender) => {
                                    if let Ok(mut track) = track.lock() {
                                        track.send_tracks.insert(uid, sender);
                                        if let Some(mixer) = track.mixer.as_mut() {
                                            mixer.add_track(uid);
                                        }
                                    }
                                }
                                TrackRequest::RemoveSend(uid) => {
                                    if let Ok(mut track) = track.lock() {
                                        track.send_tracks.remove(&uid);
                                    }
                                }
                                TrackRequest::Subscribe(sender) => {
                                    state_machine.handle_subscribe(sender)
                                }
                                TrackRequest::Unsubscribe(sender) => {
                                    state_machine.handle_unsubscribe(sender)
                                }
                            }
                        }
                    }
                    index if index == entity_index => {
                        if let Ok(action) = Self::recv_operation(operation, &entity_receiver) {
                            state_machine.handle_entity_action(action);
                        }
                    }
                    index if index == midi_index => {
                        if let Ok(action) = Self::recv_operation(operation, &midi_receiver) {
                            state_machine.handle_midi_action(action)
                        }
                    }
                    index if index == control_index => {
                        if let Ok(_action) = Self::recv_operation(operation, &control_receiver) {
                            panic!("For now, Tracks shouldn't receive Control messages")
                        }
                    }
                    index if index == track_index => {
                        if let Ok(action) = Self::recv_operation(operation, &track_receiver) {
                            state_machine.handle_track_action(action);
                        }
                    }
                    _ => {
                        panic!("Unexpected select index")
                    }
                }
            }
        });
    }
}

#[derive(Debug)]
struct ControllableItem {
    name: String,
    uid: Uid,
    param: ControlIndex,
}

#[derive(Debug)]
struct ActionSubscriptionSenders {
    entity: Sender<EntityAction>,
    midi: Sender<MidiAction>,
    control: Sender<ControlAction>,
}

#[derive(Debug)]
struct Track {
    uid: TrackUid,
    is_master_track: bool,
    uid_factory: Arc<EntityUidFactory>,
    ordered_actor_uids: Vec<Uid>,
    actors: HashMap<Uid, EntityActor>,
    send_tracks: HashMap<TrackUid, Sender<TrackRequest>>,

    entity_request_subscription: Subscription<EntityRequest>,

    controllables: Vec<ControllableItem>,
    control_links: HashMap<Uid, Vec<ControlLink>>,

    mixer: Option<Mixer>,

    actor_subscription_senders: ActionSubscriptionSenders,
}
impl Track {
    fn new_with(
        uid: TrackUid,
        is_master_track: bool,
        actor_subscription_senders: ActionSubscriptionSenders,
        uid_factory: &Arc<EntityUidFactory>,
    ) -> Self {
        Self {
            uid,
            is_master_track,
            uid_factory: Arc::clone(uid_factory),
            ordered_actor_uids: Default::default(),
            actors: Default::default(),
            send_tracks: Default::default(),
            entity_request_subscription: Default::default(),
            controllables: vec![ControllableItem {
                name: "None".to_string(),
                uid: Uid::default(),
                param: ControlIndex(0),
            }],
            control_links: Default::default(),
            mixer: if is_master_track {
                Some(Mixer::default())
            } else {
                None
            },
            actor_subscription_senders,
        }
    }

    fn add_entity(&mut self, mut entity: impl EntityBounds + 'static) {
        entity.set_uid(self.uid_factory.mint_next());
        let actor = EntityActor::new_with(entity);
        self.add_actor(actor);
    }

    fn add_actor(&mut self, actor: EntityActor) {
        let uid = actor.uid();
        actor.send_request(EntityRequest::ActionSubscribe(
            self.actor_subscription_senders.entity.clone(),
        ));
        actor.send_request(EntityRequest::MidiSubscribe(
            self.actor_subscription_senders.midi.clone(),
        ));

        // Note: do not automatically subscribe Control, because that's
        // point-to-point

        if let Ok(entity) = actor.entity.lock() {
            for i in 0..entity.control_index_count() {
                self.controllables.push(ControllableItem {
                    name: format!(
                        "{}: {}",
                        entity.name(),
                        entity.control_name_for_index(i.into()).unwrap()
                    ),
                    uid: entity.uid(),
                    param: i.into(),
                })
            }
        }

        self.entity_request_subscription.subscribe(actor.sender());
        self.ordered_actor_uids.push(uid);
        self.actors.insert(uid, actor);
    }

    fn remove_actor(&mut self, uid: Uid) {
        if let Some(actor) = self.actors.get(&uid) {
            self.entity_request_subscription.unsubscribe(actor.sender());
            actor.send_request(EntityRequest::ActionUnsubscribe(
                self.actor_subscription_senders.entity.clone(),
            ));
            actor.send_request(EntityRequest::MidiUnsubscribe(
                self.actor_subscription_senders.midi.clone(),
            ));
            actor.send_request(EntityRequest::ControlUnsubscribe(
                self.actor_subscription_senders.control.clone(),
            ));
        }
        self.actors.remove(&uid);
        self.ordered_actor_uids.retain(|u| *u != uid);
        self.controllables.retain(|c| c.uid != uid);
    }

    fn link(
        &mut self,
        source_uid: Uid,
        target_uid: Uid,
        index: ControlIndex,
    ) -> anyhow::Result<()> {
        if let Some(source) = self.actors.get(&source_uid) {
            if let Some(target) = self.actors.get(&target_uid) {
                source.send_request(EntityRequest::ControlSubscribe(
                    target.control_sender().clone(),
                ));
                target.send_request(EntityRequest::ControlLinkAdd(source_uid, index));
                self.control_links
                    .entry(source_uid)
                    .or_default()
                    .push(ControlLink {
                        uid: target_uid,
                        param: index,
                    });
                return Ok(());
            }
        }
        Err(anyhow!("Couldn't find both {source_uid} and {target_uid}"))
    }

    fn unlink(&mut self, source_uid: Uid, target_uid: Uid, index: ControlIndex) {
        if let Some(source) = self.actors.get(&source_uid) {
            if let Some(target) = self.actors.get(&target_uid) {
                source.send_request(EntityRequest::ControlUnsubscribe(
                    target.control_sender().clone(),
                ));
                target.send_request(EntityRequest::ControlLinkRemove(source_uid, index));
                if let Some(links) = self.control_links.get_mut(&source_uid) {
                    links.retain(|link| link.uid != target_uid && link.param != index);
                }
            }
        }
    }
}

impl Displays for Track {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        let response = if self.is_master_track {
            ui.heading(format!("Master Track"))
        } else {
            ui.heading(format!("Track {}", self.uid))
        };
        ui.horizontal_wrapped(|ui| {
            if !self.is_master_track {
                if ui.button("Add Synth").clicked() {
                    self.add_entity(ToySynth::default());
                }
                if ui.button("Add ToyInstrument").clicked() {
                    self.add_entity(ToyInstrument::default());
                }
                if ui.button("Add Busy Waiter").clicked() {
                    self.add_entity(BusyWaiter::default());
                }
                if ui.button("Add 1.0").clicked() {
                    self.add_entity(AlwaysSame::new_with(1.0));
                }
                if ui.button("Add 0.5").clicked() {
                    self.add_entity(AlwaysSame::new_with(0.5));
                }
                if ui.button("Add -1.0").clicked() {
                    self.add_entity(AlwaysSame::new_with(-1.0));
                }
                if ui.button("Add Arpeggiator").clicked() {
                    self.add_entity(Arpeggiator::default());
                }
                if ui.button("Add Quietener").clicked() {
                    self.add_entity(Quietener::default());
                }
                if ui.button("Add Drone").clicked() {
                    self.add_entity(DroneController::default());
                }
                ui.end_row();
            }

            let mut actor_uid_to_remove = None;
            let mut link_to_add = None;
            let mut link_to_remove = None;
            for &uid in self.ordered_actor_uids.iter() {
                if let Some(actor) = self.actors.get_mut(&uid) {
                    ui.vertical(|ui| {
                        Frame::default()
                            .stroke(if actor.is_sound_active() {
                                ui.visuals().widgets.active.bg_stroke
                            } else {
                                ui.visuals().widgets.noninteractive.bg_stroke
                            })
                            .inner_margin(Margin::same(4.0))
                            .show(ui, |ui| {
                                actor.ui(ui);
                                ui.label("");
                                if ui.button("Remove").clicked() {
                                    actor_uid_to_remove = Some(uid);
                                }

                                if !self.controllables.is_empty() {
                                    let mut selected_index = 0;
                                    if ComboBox::new(ui.next_auto_id(), "Controls")
                                        .show_index(
                                            ui,
                                            &mut selected_index,
                                            self.controllables.len(),
                                            |i| self.controllables[i].name.clone(),
                                        )
                                        .changed()
                                        && selected_index != 0
                                    {
                                        let link = &self.controllables[selected_index];
                                        link_to_add = Some((
                                            uid,
                                            ControlLink {
                                                uid: link.uid,
                                                param: link.param,
                                            },
                                        ));
                                    };
                                }
                                if let Some(links) = self.control_links.get(&uid) {
                                    ui.label("This controls");
                                    for link in links {
                                        if ui
                                            .button(format!(
                                                "Uid #{}, Param #{}",
                                                link.uid, link.param
                                            ))
                                            .clicked()
                                        {
                                            link_to_remove = Some((uid, *link));
                                        }
                                    }
                                }
                            });
                    });
                }
            }
            if let Some(actor_uid_to_remove) = actor_uid_to_remove {
                if let Some(links) = self.control_links.get(&actor_uid_to_remove) {
                    let links = links.clone();
                    for link in links {
                        self.unlink(actor_uid_to_remove, link.uid, link.param);
                    }
                }

                let keys: Vec<Uid> = self.control_links.keys().map(|k| *k).collect();
                for source_uid in keys {
                    if let Some(links) = self.control_links.get(&source_uid) {
                        let links = links.clone();
                        for link in links {
                            if link.uid == actor_uid_to_remove {
                                self.unlink(source_uid, link.uid, link.param);
                            }
                        }
                    }
                }
                if let Some(actor) = self.actors.get(&actor_uid_to_remove) {
                    actor.send_request(EntityRequest::Quit);
                }
                self.remove_actor(actor_uid_to_remove);
            }
            if let Some((source_uid, control_link)) = link_to_add {
                let _ = self.link(source_uid, control_link.uid, control_link.param);
            }
            if let Some((source_uid, control_link)) = link_to_remove {
                self.unlink(source_uid, control_link.uid, control_link.param);
            }
            if let Some(mixer) = self.mixer.as_mut() {
                mixer.ui(ui);
            }
        });
        response
    }
}
