use crate::{
    always::AlwaysSame,
    arp::Arpeggiator,
    busy::BusyWaiter,
    entity::{EntityAction, EntityActor, EntityRequest},
    traits::ProvidesActorService,
    ATOMIC_ORDERING,
};
use crossbeam_channel::{Select, Sender};
use ensnare::prelude::*;
use ensnare_toys::{ToyInstrument, ToySynth};
use std::sync::{atomic::AtomicUsize, Arc, Mutex};

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
    /// The track should process this audio buffer (effects -- this isn't right)
    NeedsTransformation(Vec<StereoSample>),
    /// The [TrackActor] should exit.
    Quit,
}

#[derive(Debug)]
pub enum TrackAction {
    Midi(MidiChannel, MidiMessage),
    Control(Uid, ControlValue),
    Frames(Vec<StereoSample>),
}

#[derive(Debug)]
pub struct TrackActor {
    request_channel_pair: ChannelPair<TrackRequest>,
    action_sender: Sender<TrackAction>,
    entity_action_channel_pair: ChannelPair<EntityAction>,

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
        sender: &Sender<TrackAction>,
        uid_factory: &Arc<EntityUidFactory>,
    ) -> Self {
        let entity_action_channel_pair = ChannelPair::<EntityAction>::default();
        let track = Track::new_with(track_uid, &entity_action_channel_pair.sender, uid_factory);
        let mut r = Self {
            request_channel_pair: Default::default(),
            action_sender: sender.clone(),
            entity_action_channel_pair,
            inner: Arc::new(Mutex::new(track)),
        };

        r.start_thread();

        r
    }

    fn start_thread(&mut self) {
        let input_receiver = self.request_channel_pair.receiver.clone();
        let sender = self.action_sender.clone();
        let mut buffer = GenerationBuffer::<StereoSample>::default();
        let pending_instrument_count = AtomicUsize::default();
        let track = Arc::clone(&self.inner);
        let entity_receiver = self.entity_action_channel_pair.receiver.clone();

        std::thread::spawn(move || {
            let mut sel = Select::default();

            let input_index = sel.recv(&input_receiver);
            let entity_index = sel.recv(&entity_receiver);
            loop {
                let oper = sel.select();
                match oper.index() {
                    index if index == input_index => {
                        if let Ok(input) = oper.recv(&input_receiver) {
                            match input {
                                TrackRequest::Midi(channel, message) => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.iter() {
                                            let _ =
                                                actor.send(EntityRequest::Midi(channel, message));
                                        }
                                    }
                                }
                                TrackRequest::Control(index, value) => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.iter() {
                                            let _ =
                                                actor.send(EntityRequest::Control(index, value));
                                        }
                                    }
                                }
                                TrackRequest::NeedsAudio(count) => {
                                    buffer.resize(count);
                                    buffer.clear();
                                    if let Ok(track) = track.lock() {
                                        assert_eq!(
                                            pending_instrument_count.load(ATOMIC_ORDERING),
                                            0
                                        );

                                        if track.actors.is_empty() {
                                            // TODO: consolidate this with the Frames
                                            // case, because I'm sure this will grow in
                                            // complexity, and we'll end up having to
                                            // maintain two copies.
                                            let _ = sender.send(TrackAction::Frames(
                                                buffer.buffer().to_vec(),
                                            ));
                                        } else {
                                            pending_instrument_count
                                                .fetch_add(track.actors.len(), ATOMIC_ORDERING);
                                            for actor in track.actors.iter() {
                                                let _ =
                                                    actor.send(EntityRequest::NeedsAudio(count));
                                            }
                                        }
                                    }
                                }
                                TrackRequest::NeedsTransformation(_) => todo!(),
                                TrackRequest::Quit => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.iter() {
                                            let _ = actor.send(EntityRequest::Quit);
                                        }
                                    }
                                    break;
                                }
                                TrackRequest::Work(time_range) => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.iter() {
                                            let _ =
                                                actor.send(EntityRequest::Work(time_range.clone()));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    index if index == entity_index => {
                        if let Ok(action) = oper.recv(&entity_receiver) {
                            match action {
                                EntityAction::Midi(uid, channel, message) => {
                                    if let Ok(track) = track.lock() {
                                        for actor in track.actors.iter().filter(|&a| a.uid() != uid)
                                        {
                                            let _ =
                                                actor.send(EntityRequest::Midi(channel, message));
                                        }
                                    }
                                    let _ = sender.send(TrackAction::Midi(channel, message));
                                }
                                EntityAction::Control(source_uid, value) => {
                                    let _ = sender.send(TrackAction::Control(source_uid, value));
                                }
                                EntityAction::Frames(frames) => {
                                    assert!(frames.len() <= 64);
                                    // We got some audio from an instrument. Mix it into
                                    // the track buffer.
                                    buffer.merge(&frames);
                                    // Then see if we've gotten all the ones we expect.
                                    if pending_instrument_count.fetch_sub(1, ATOMIC_ORDERING) == 1 {
                                        // We have. Send our completed track to the owner.
                                        let _ = sender
                                            .send(TrackAction::Frames(buffer.buffer().to_vec()));
                                    }
                                }
                                EntityAction::Transformed(frames) => {
                                    assert!(frames.len() <= 64);
                                    // We got audio from an effect. It replaces the track buffer.
                                    buffer.buffer_mut().copy_from_slice(&frames);
                                }
                            }
                        }
                    }
                    _ => {
                        panic!()
                    }
                }
            }
        });
    }
}

#[derive(Debug)]
struct Track {
    #[allow(dead_code)]
    uid: TrackUid,
    entity_action_sender: Sender<EntityAction>,
    uid_factory: Arc<EntityUidFactory>,
    actors: Vec<EntityActor>,
}
impl Track {
    fn new_with(
        uid: TrackUid,
        entity_action_sender: &Sender<EntityAction>,
        uid_factory: &Arc<EntityUidFactory>,
    ) -> Self {
        Self {
            uid,
            entity_action_sender: entity_action_sender.clone(),
            uid_factory: Arc::clone(uid_factory),
            actors: Default::default(),
        }
    }

    fn add_actor(&mut self, actor: EntityActor) {
        self.actors.push(actor);
    }

    fn add_entity(&mut self, mut entity: impl EntityBounds + 'static) {
        entity.set_uid(self.uid_factory.mint_next());
        self.add_actor(EntityActor::new_with(entity, &self.entity_action_sender))
    }
}

impl Displays for Track {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.horizontal_top(|ui| {
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
            let mut actor_to_remove = None;
            for (i, actor) in self.actors.iter_mut().enumerate() {
                ui.vertical(|ui| {
                    actor.ui(ui);
                    if ui.button("Remove").clicked() {
                        actor_to_remove = Some(i);
                    }
                });
            }
            if let Some(actor_to_remove) = actor_to_remove {
                self.actors.remove(actor_to_remove);
            }
        });
        ui.label("Coming soon!")
    }
}
