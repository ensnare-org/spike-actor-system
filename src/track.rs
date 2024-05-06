use crate::{
    always::AlwaysSame,
    arp::Arpeggiator,
    busy::BusyWaiter,
    entity::{EntityAction, EntityActor, EntityRequest},
    quietener::Quietener,
    subscription::Subscription,
    traits::ProvidesActorService,
};
use crossbeam_channel::{Select, Sender};
use eframe::egui::{Color32, Frame, Margin, Stroke};
use ensnare::prelude::*;
use ensnare_toys::{ToyInstrument, ToySynth};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

#[derive(Debug)]
pub enum TrackRequest {
    /// Add a subscriber to our actions.
    Subscribe(Sender<TrackAction>),
    #[allow(dead_code)]
    /// Remove a subscriber from our actions.
    Unsubscribe(Sender<TrackAction>),
    /// The track should handle an incoming MIDI message.
    Midi(MidiChannel, MidiMessage),
    #[allow(dead_code)]
    /// Broken - meaningless
    Control(ControlIndex, ControlValue),
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
    /// itself.
    ///
    /// TODO: replace with channels!
    Midi(MidiChannel, MidiMessage),
    /// The [Entity] with the given [Uid] has produced the given signal that
    /// should be handled. The track has already routed it appropriately within
    /// itself (TODO - really? Doesn't that mean each track is keeping a table
    /// of subscribers, in addition to the global one?).
    ///
    /// TODO: replace with channels!
    Control(Uid, ControlValue),
    /// This track has produced a buffer of frames.
    Frames(TrackUid, Vec<StereoSample>),
}

#[derive(Debug)]
struct TrackActorStateMachine {
    track_uid: TrackUid,
    state: TrackActorState,
    buffer: GenerationBuffer<StereoSample>,
    track: Arc<Mutex<Track>>,
    subscription: Subscription<TrackAction>,
}
impl TrackActorStateMachine {
    fn new_with(track: &Arc<Mutex<Track>>) -> Self {
        let track_uid = track.lock().unwrap().uid;
        Self {
            track_uid,
            state: Default::default(),
            buffer: Default::default(),
            track: Arc::clone(&track),
            subscription: Default::default(),
        }
    }
    fn handle_entity_action(&mut self, action: EntityAction) {
        match action {
            EntityAction::Midi(uid, channel, message) => {
                if let Ok(track) = self.track.lock() {
                    for actor in track.actors.values().filter(|&a| a.uid() != uid) {
                        let _ = actor.send(EntityRequest::Midi(channel, message));
                    }
                }
                self.subscription
                    .broadcast(TrackAction::Midi(channel, message));
            }
            EntityAction::Control(source_uid, value) => {
                self.handle_control(source_uid, value);
            }
            EntityAction::Frames(frames) => {
                self.handle_incoming_frames(frames);
            }
            EntityAction::Transformed(frames) => {
                self.handle_incoming_frames(frames);
            }
        }
    }

    fn handle_control(&mut self, source_uid: Uid, value: ControlValue) {
        self.subscription
            .broadcast(TrackAction::Control(source_uid, value));
    }

    fn handle_track_action(&mut self, action: TrackAction) {
        match action {
            TrackAction::Midi(channel, message) => {
                self.subscription
                    .broadcast(TrackAction::Midi(channel, message));
            }
            TrackAction::Control(source_uid, value) => {
                self.handle_control(source_uid, value);
            }
            TrackAction::Frames(_track_uid, frames) => {
                // TODO: if we're receiving this, then we might be a parent of a
                // track that is waiting as an aux track for this frame. Think
                // about how to represent that (and should the master track be
                // using the same method?).

                //                dddddddo this MAYBE WE GET THIS FOR FREE

                self.handle_incoming_frames(frames);
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
        self.subscription.broadcast(TrackAction::Frames(
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

    /// Receives entity actions.
    entity_action_channel_pair: ChannelPair<EntityAction>,

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
}
impl TrackActor {
    pub fn new_with(
        track_uid: TrackUid,
        is_master_track: bool,
        sender: &Sender<TrackAction>,
        uid_factory: &Arc<EntityUidFactory>,
    ) -> Self {
        let entity_action_channel_pair = ChannelPair::<EntityAction>::default();
        let track = Track::new_with(
            track_uid,
            is_master_track,
            &entity_action_channel_pair.sender,
            uid_factory,
        );
        let mut r = Self {
            request_channel_pair: Default::default(),
            entity_action_channel_pair,
            track_action_channel_pair: Default::default(),
            inner: Arc::new(Mutex::new(track)),
        };

        r.send_request(TrackRequest::Subscribe(sender.clone()));
        r.start_thread();

        r
    }

    fn start_thread(&mut self) {
        let input_receiver = self.request_channel_pair.receiver.clone();
        let track = Arc::clone(&self.inner);
        let entity_receiver = self.entity_action_channel_pair.receiver.clone();
        let track_receiver = self.track_action_channel_pair.receiver.clone();
        let mut state_machine = TrackActorStateMachine::new_with(&self.inner);

        std::thread::spawn(move || {
            let mut sel = Select::default();

            let input_index = sel.recv(&input_receiver);
            let entity_index = sel.recv(&entity_receiver);
            let track_index = sel.recv(&track_receiver);

            loop {
                let operation = sel.select();
                match operation.index() {
                    index if index == input_index => {
                        if let Ok(request) = Self::recv_operation(operation, &input_receiver) {
                            match request {
                                TrackRequest::Midi(channel, message) => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.values() {
                                            let _ =
                                                actor.send(EntityRequest::Midi(channel, message));
                                        }
                                    }
                                }
                                TrackRequest::Control(index, value) => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.values() {
                                            let _ =
                                                actor.send(EntityRequest::Control(index, value));
                                        }
                                    }
                                }
                                TrackRequest::NeedsAudio(count) => {
                                    state_machine.handle_needs_audio(count);
                                }
                                TrackRequest::Quit => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.values() {
                                            let _ = actor.send(EntityRequest::Quit);
                                        }
                                    }
                                    break;
                                }
                                TrackRequest::Work(time_range) => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.values() {
                                            let _ =
                                                actor.send(EntityRequest::Work(time_range.clone()));
                                        }
                                    }
                                }
                                TrackRequest::AddSend(uid, sender) => {
                                    if let Ok(mut track) = track.lock() {
                                        track.send_tracks.insert(uid, sender);
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
                    index if index == track_index => {
                        if let Ok(action) = Self::recv_operation(operation, &track_receiver) {
                            state_machine.handle_track_action(action);
                        }
                    }
                    _ => {
                        panic!()
                    }
                }
            }
        });
    }

    pub fn child_track_action_sender(&self) -> &Sender<TrackAction> {
        &self.track_action_channel_pair.sender
    }
}

#[derive(Debug)]
struct Track {
    uid: TrackUid,
    is_master_track: bool,
    entity_action_sender: Sender<EntityAction>,
    uid_factory: Arc<EntityUidFactory>,
    ordered_actor_uids: Vec<Uid>,
    actors: HashMap<Uid, EntityActor>,
    send_tracks: HashMap<TrackUid, Sender<TrackRequest>>,
}
impl Track {
    fn new_with(
        uid: TrackUid,
        is_master_track: bool,
        entity_action_sender: &Sender<EntityAction>,
        uid_factory: &Arc<EntityUidFactory>,
    ) -> Self {
        Self {
            uid,
            is_master_track,
            entity_action_sender: entity_action_sender.clone(),
            uid_factory: Arc::clone(uid_factory),
            ordered_actor_uids: Default::default(),
            actors: Default::default(),
            send_tracks: Default::default(),
        }
    }

    fn add_actor(&mut self, actor: EntityActor) {
        let uid = actor.uid();
        self.ordered_actor_uids.push(uid);
        self.actors.insert(uid, actor);
    }

    fn add_entity(&mut self, mut entity: impl EntityBounds + 'static) {
        entity.set_uid(self.uid_factory.mint_next());
        let actor = EntityActor::new_and_subscribe(entity, &self.entity_action_sender);
        self.add_actor(actor);
    }
}

impl Displays for Track {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.heading(format!("Track {}", self.uid));
        ui.horizontal_top(|ui| {
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
            }
            let mut actor_uid_to_remove = None;
            for &uid in self.ordered_actor_uids.iter() {
                if let Some(actor) = self.actors.get_mut(&uid) {
                    ui.vertical(|ui| {
                        Frame::default()
                            .stroke(if actor.is_sound_active() {
                                Stroke::new(2.0, Color32::YELLOW)
                            } else {
                                Stroke::default()
                            })
                            .inner_margin(Margin::same(4.0))
                            .show(ui, |ui| actor.ui(ui));

                        if ui.button("Remove").clicked() {
                            actor_uid_to_remove = Some(uid);
                        }
                    });
                }
            }
            if let Some(actor_uid_to_remove) = actor_uid_to_remove {
                self.actors.remove(&actor_uid_to_remove);
                self.ordered_actor_uids
                    .retain(|uid| *uid != actor_uid_to_remove);
            }
        });
        ui.label("Coming soon!")
    }
}
