use crate::{
    always::AlwaysSame,
    busy::BusyWaiter,
    entity::{EntityActor, EntityInput},
    orchestration::EngineInput,
    ATOMIC_ORDERING,
};
use crossbeam_channel::Sender;
use ensnare::prelude::*;
use ensnare_toys::{ToyInstrument, ToySynth};
use std::sync::{atomic::AtomicUsize, Arc, Mutex};

#[derive(Debug)]
pub enum TrackInput {
    Midi(MidiChannel, MidiMessage),
    Control(ControlIndex, ControlValue),
    NeedsAudio(usize),
    NeedsTransformation(Vec<StereoSample>),
    Frames(Vec<StereoSample>),
    Quit,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum TrackEvent {
    Midi(MidiChannel, MidiMessage),
    Control(ControlIndex, ControlValue),
    Quit,
}

#[derive(Debug)]
pub struct TrackActor {
    input_channel_pair: ChannelPair<TrackInput>,
    sender: Sender<EngineInput>,

    inner: Arc<Mutex<Track>>,
}
impl Displays for TrackActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        self.inner.lock().unwrap().ui(ui)
    }
}
impl TrackActor {
    pub fn new_with(track_uid: TrackUid, sender: &Sender<EngineInput>) -> Self {
        let input_channel_pair = ChannelPair::<TrackInput>::default();
        let track = Track::new_with(track_uid, &input_channel_pair.sender);

        let mut r = Self {
            input_channel_pair,
            sender: sender.clone(),
            inner: Arc::new(Mutex::new(track)),
        };

        r.start_thread();

        r
    }

    fn start_thread(&mut self) {
        let receiver = self.input_channel_pair.receiver.clone();
        let sender = self.sender.clone();
        let mut buffer = GenerationBuffer::<StereoSample>::default();
        let pending_instrument_count = AtomicUsize::default();
        let track = Arc::clone(&self.inner);

        std::thread::spawn(move || {
            loop {
                if let Ok(input) = receiver.recv() {
                    match input {
                        TrackInput::Midi(channel, message) => {
                            if let Ok(track) = track.lock() {
                                for actor in track.actors.iter() {
                                    let _ = actor.send(EntityInput::Midi(channel, message));
                                }
                            }
                        }
                        TrackInput::Control(index, value) => {
                            if let Ok(track) = track.lock() {
                                for actor in track.actors.iter() {
                                    let _ = actor.send(EntityInput::Control(index, value));
                                }
                            }
                        }
                        TrackInput::Frames(frames) => {
                            assert!(frames.len() <= 64);
                            // We got some audio from an instrument. Mix it into
                            // the track buffer.
                            buffer.merge(&frames);
                            // Then see if we've gotten all the ones we expect.
                            if pending_instrument_count.fetch_sub(1, ATOMIC_ORDERING) == 1 {
                                // We have. Send our completed track to the owner.
                                let _ = sender.send(EngineInput::Frames(buffer.buffer().to_vec()));
                            }
                        }
                        TrackInput::NeedsAudio(count) => {
                            buffer.resize(count);
                            buffer.clear();
                            if let Ok(mut track) = track.lock() {
                                assert_eq!(pending_instrument_count.load(ATOMIC_ORDERING), 0);

                                if track.actors.is_empty() {
                                    // TODO: consolidate this with the Frames
                                    // case, because I'm sure this will grow in
                                    // complexity, and we'll end up having to
                                    // maintain two copies.
                                    let _ =
                                        sender.send(EngineInput::Frames(buffer.buffer().to_vec()));
                                } else {
                                    pending_instrument_count
                                        .fetch_add(track.actors.len(), ATOMIC_ORDERING);
                                    for actor in track.actors.iter_mut() {
                                        let _ = actor.send(EntityInput::NeedsAudio(count));
                                    }
                                }
                            }
                        }
                        TrackInput::NeedsTransformation(_) => todo!(),
                        TrackInput::Quit => break,
                    }
                }
            }
        });
    }

    pub fn send(&self, input: TrackInput) {
        let _ = self.input_channel_pair.sender.send(input);
    }
}

#[derive(Debug)]
struct Track {
    #[allow(dead_code)]
    uid: TrackUid,
    sender: Sender<TrackInput>,
    actors: Vec<EntityActor>,
}
impl Track {
    fn new_with(uid: TrackUid, sender: &Sender<TrackInput>) -> Self {
        Self {
            uid,
            sender: sender.clone(),
            actors: Default::default(),
        }
    }

    fn add_actor(&mut self, entity: impl EntityBounds + 'static) {
        self.actors
            .push(EntityActor::new_with(entity, &self.sender));
    }
}

impl Displays for Track {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.horizontal_top(|ui| {
            if ui.button("Add Synth").clicked() {
                self.add_actor(ToySynth::default());
            }
            if ui.button("Add ToyInstrument").clicked() {
                self.add_actor(ToyInstrument::default());
            }
            if ui.button("Add Busy Waiter").clicked() {
                self.add_actor(BusyWaiter::default());
            }
            if ui.button("Add 1.0").clicked() {
                self.add_actor(AlwaysSame::new_with(1.0));
            }
            if ui.button("Add 0.5").clicked() {
                self.add_actor(AlwaysSame::new_with(0.5));
            }
            if ui.button("Add -1.0").clicked() {
                self.add_actor(AlwaysSame::new_with(-1.0));
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
