use crate::{
    always::AlwaysSame,
    arp::Arpeggiator,
    busy::BusyWaiter,
    entity::{EntityAction, EntityActor, EntityRequest},
    quietener::Quietener,
    traits::ProvidesActorService,
};
use crossbeam_channel::{Select, Sender};
use eframe::egui::ahash::HashMap;
use ensnare::prelude::*;
use ensnare_toys::{ToyInstrument, ToySynth};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
pub enum TrackRequest {
    /// The track should handle an incoming MIDI message.
    Midi(MidiChannel, MidiMessage),
    /// Broken - meaningless
    Control(ControlIndex, ControlValue),
    /// The track should perform work for the given slice of time.
    Work(TimeRange),
    /// The track should generate a buffer of audio frames.
    NeedsAudio(usize),
    /// The [TrackActor] should exit.
    Quit,
    AddSourceTrack(TrackUid, Sender<TrackRequest>),
    RemoveSourceTrack(TrackUid),
}

#[derive(Debug)]
pub enum TrackAction {
    Midi(MidiChannel, MidiMessage),
    Control(Uid, ControlValue),
    Frames(Vec<StereoSample>),
}

#[derive(Debug)]
struct TrackActorStateMachine {
    state: TrackActorState,
    buffer: GenerationBuffer<StereoSample>,
    track: Arc<Mutex<Track>>,
    sender: Sender<TrackAction>,
}
impl TrackActorStateMachine {
    fn new_with(track: &Arc<Mutex<Track>>, sender: &Sender<TrackAction>) -> Self {
        Self {
            state: Default::default(),
            buffer: Default::default(),
            track: Arc::clone(&track),
            sender: sender.clone(),
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
                let _ = self.sender.send(TrackAction::Midi(channel, message));
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
        let _ = self.sender.send(TrackAction::Control(source_uid, value));
    }
    fn handle_track_action(&mut self, action: TrackAction) {
        match action {
            TrackAction::Midi(channel, message) => {
                let _ = self.sender.try_send(TrackAction::Midi(channel, message));
            }
            TrackAction::Control(source_uid, value) => {
                self.handle_control(source_uid, value);
            }
            TrackAction::Frames(frames) => {
                self.handle_incoming_frames(frames);
            }
        }
    }

    fn handle_incoming_frames(&mut self, frames: Vec<StereoSample>) {
        assert!(frames.len() <= 64);
        match &self.state {
            TrackActorState::Idle => panic!("We got frames when we weren't expecting any"),
            TrackActorState::AwaitingSources(_) => {
                // We got some audio from a track. Mix it
                // into the track buffer.
                self.buffer.merge(&frames);
                self.advance_state_awaiting_sources();
            }
            TrackActorState::AwaitingEffect(_) => {
                // An effect completed processing. Copy its results over the current buffer.
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
                    // We have. Now it's time to let the effects process what we have.
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
        let _ = self
            .sender
            .send(TrackAction::Frames(self.buffer.buffer().to_vec()));
    }

    fn handle_needs_audio(&mut self, count: usize) {
        assert!(
            matches!(self.state, TrackActorState::Idle),
            "{}: expected a clean slate",
            self.track.lock().unwrap().uid
        );
        self.buffer.resize(count);
        self.buffer.clear();

        // if we have source tracks, kick them off as well as any instruments
        let mut have_nonzero_sources = false;
        if let Ok(track) = self.track.lock() {
            let new_sources_count = track.source_tracks.len() + track.actors.len();
            self.state = TrackActorState::AwaitingSources(new_sources_count);
            for source in track.source_tracks.values() {
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

    /// Reports its actions to this channel, which is typically owned by a
    /// parent.
    action_sender: Sender<TrackAction>,

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
    fn action_sender(&self) -> &Sender<TrackAction> {
        &self.action_sender
    }

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
            action_sender: sender.clone(),
            entity_action_channel_pair,
            track_action_channel_pair: Default::default(),
            inner: Arc::new(Mutex::new(track)),
        };

        r.start_thread();

        r
    }

    fn start_thread(&mut self) {
        let input_receiver = self.request_channel_pair.receiver.clone();
        let track = Arc::clone(&self.inner);
        let entity_receiver = self.entity_action_channel_pair.receiver.clone();
        let track_receiver = self.track_action_channel_pair.receiver.clone();
        let mut state_machine = TrackActorStateMachine::new_with(&self.inner, &self.action_sender);

        std::thread::spawn(move || {
            let mut sel = Select::default();

            let input_index = sel.recv(&input_receiver);
            let entity_index = sel.recv(&entity_receiver);
            let track_index = sel.recv(&track_receiver);

            loop {
                let oper = sel.select();
                match oper.index() {
                    index if index == input_index => {
                        if let Ok(request) = oper.recv(&input_receiver) {
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
                                TrackRequest::AddSourceTrack(uid, sender) => {
                                    if let Ok(mut track) = track.lock() {
                                        track.source_tracks.insert(uid, sender);
                                    }
                                }
                                TrackRequest::RemoveSourceTrack(uid) => {
                                    if let Ok(mut track) = track.lock() {
                                        track.source_tracks.remove(&uid);
                                    }
                                }
                            }
                        }
                    }
                    index if index == entity_index => {
                        if let Ok(action) = oper.recv(&entity_receiver) {
                            state_machine.handle_entity_action(action);
                        }
                    }
                    index if index == track_index => {
                        if let Ok(action) = oper.recv(&track_receiver) {
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
    #[allow(dead_code)]
    uid: TrackUid,
    is_master_track: bool,
    entity_action_sender: Sender<EntityAction>,
    uid_factory: Arc<EntityUidFactory>,
    ordered_actor_uids: Vec<Uid>,
    actors: HashMap<Uid, EntityActor>,
    source_tracks: HashMap<TrackUid, Sender<TrackRequest>>,
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
            source_tracks: Default::default(),
        }
    }

    fn add_actor(&mut self, actor: EntityActor) {
        let uid = actor.uid();
        self.ordered_actor_uids.push(uid);
        self.actors.insert(uid, actor);
    }

    fn add_entity(&mut self, mut entity: impl EntityBounds + 'static) {
        entity.set_uid(self.uid_factory.mint_next());
        self.add_actor(EntityActor::new_with(entity, &self.entity_action_sender))
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
                        actor.ui(ui);
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
