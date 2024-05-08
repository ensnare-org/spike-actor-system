use crate::{
    subscription::Subscription,
    track::{TrackAction, TrackActor, TrackRequest},
    traits::{ProvidesActorService, ProvidesService},
    wav_writer::{WavWriterInput, WavWriterService},
};
use crossbeam_channel::{Select, Sender};
use crossbeam_queue::ArrayQueue;
use delegate::delegate;
use ensnare::{
    orchestration::TrackUidFactory, prelude::*, traits::MidiNoteLabelMetadata, types::AudioQueue,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Communication from the client to [EngineService].
#[derive(Debug)]
pub enum EngineServiceInput {
    /// The audio interface's configuration changed.
    Reset(SampleRate, u16, AudioQueue),
    /// The midi interface received a MIDI message.
    Midi(MidiChannel, MidiMessage),
    /// The audio interface needs more audio.
    NeedsAudio(usize),
    #[allow(dead_code)]
    /// Start playing.
    Play,
    #[allow(dead_code)]
    /// Stop playing.
    Stop,
    /// The client would like the service to exit.
    Quit,
}

#[derive(Debug)]
pub enum EngineServiceEvent {
    /// The engine has started up or reset. Take the given parameters and save
    /// them.
    Reset(Arc<Mutex<Engine>>),
    /// The engine produced a MIDI message.
    Midi(MidiChannel, MidiMessage),
}

#[derive(Debug)]
pub struct EngineService {
    input_channel_pair: ChannelPair<EngineServiceInput>,
    event_channel_pair: ChannelPair<EngineServiceEvent>,
    track_action_channel_pair: ChannelPair<TrackAction>,

    engine: Arc<Mutex<Engine>>,
}
impl Default for EngineService {
    fn default() -> Self {
        Self::new()
    }
}
impl ProvidesService<EngineServiceInput, EngineServiceEvent> for EngineService {
    fn receiver(&self) -> &crossbeam_channel::Receiver<EngineServiceEvent> {
        &self.event_channel_pair.receiver
    }

    fn sender(&self) -> &Sender<EngineServiceInput> {
        &self.input_channel_pair.sender
    }
}
impl EngineService {
    pub fn new() -> Self {
        let track_action_channel_pair: ChannelPair<TrackAction> = Default::default();
        let mut engine = Engine::new();
        engine.subscribe(&track_action_channel_pair.sender);

        let r = Self {
            engine: Arc::new(Mutex::new(engine)),
            input_channel_pair: Default::default(),
            event_channel_pair: Default::default(),
            track_action_channel_pair,
        };

        r.start_thread();

        r
    }

    fn start_thread(&self) {
        let service_event_sender = self.event_channel_pair.sender.clone();

        let engine = Arc::clone(&self.engine);
        let _ = self
            .event_channel_pair
            .sender
            .try_send(EngineServiceEvent::Reset(Arc::clone(&self.engine)));
        let service_input_receiver = self.input_channel_pair.receiver.clone();
        let mut audio_queue: Option<Arc<ArrayQueue<StereoSample>>> = None;

        let writer_service = WavWriterService::new();

        let mut frames_requested = 0;

        let track_action_receiver = self.track_action_channel_pair.receiver.clone();

        std::thread::spawn(move || {
            let mut sel = Select::default();
            let service_index = sel.recv(&service_input_receiver);
            let track_index = sel.recv(&track_action_receiver);

            loop {
                let operation = sel.select();
                let mut start_generation = false;
                match operation.index() {
                    index if index == service_index => {
                        if let Ok(input) = Self::recv_operation(operation, &service_input_receiver)
                        {
                            match input {
                                EngineServiceInput::Reset(
                                    sample_rate,
                                    channel_count,
                                    new_audio_queue,
                                ) => {
                                    audio_queue = Some(new_audio_queue);
                                    engine.lock().unwrap().update_sample_rate(sample_rate);
                                    writer_service.send_input(WavWriterInput::Reset(
                                        PathBuf::from(format!(
                                            "/home/miket/out-{}-{}.wav",
                                            sample_rate.0, channel_count
                                        )),
                                        sample_rate,
                                        channel_count,
                                    ));
                                }
                                EngineServiceInput::Midi(channel, message) => engine
                                    .lock()
                                    .unwrap()
                                    .handle_midi_message(channel, message, &mut |_, _| panic!("This MIDI message should have been sent via channel, not callback.")),
                                EngineServiceInput::NeedsAudio(count) => {
                                    if frames_requested == 0 {
                                        start_generation = true;
                                    }
                                    frames_requested += count;
                                }
                                EngineServiceInput::Quit => {
                                    engine.lock().unwrap().request_quit();
                                    writer_service.send_input(WavWriterInput::Quit);
                                    break;
                                }
                                EngineServiceInput::Play => engine.lock().unwrap().play(),
                                EngineServiceInput::Stop => engine.lock().unwrap().stop(),
                            }
                        }
                    }
                    index if index == track_index => {
                        if let Ok(action) = Self::recv_operation(operation, &track_action_receiver)
                        {
                            match action {
                                TrackAction::Midi(channel, message) => {
                                    // TODO: is this the right point to
                                    // concentrate these messages? It seems
                                    // burdensome for the external MIDI service
                                    // to subscribe to every entity, but it
                                    // seems arbitrary for it to subscribe to
                                    // every track (maybe it's a feature to
                                    // switch on/off per track).
                                    let _ = service_event_sender
                                        .try_send(EngineServiceEvent::Midi(channel, message));
                                }
                                TrackAction::Frames(_track_uid, frames) => {
                                    // We don't care about track_uid because we
                                    // know that only the master track sends us
                                    // frames.

                                    let frames_len = frames.len();
                                    assert!(frames_len <= 64);
                                    let frames_len = frames.len();

                                    if let Some(queue) = audio_queue.as_ref() {
                                        for &frame in frames.iter() {
                                            if queue.force_push(frame).is_some() {
                                                eprintln!(
                                                    "FYI force_push queue len/cap {}/{}, frames {}",
                                                    queue.len(),
                                                    queue.capacity(),
                                                    frames_requested
                                                );
                                            }
                                        }
                                    }
                                    writer_service.send_input(WavWriterInput::Frames(frames));

                                    assert!(frames_len <= 64);
                                    if frames_requested > frames_len {
                                        // We still have work to do, so kick off
                                        // generation once again.
                                        frames_requested -= frames_len;
                                        start_generation = true;
                                    } else {
                                        // The case of (frames_requested <
                                        // frames_len) can happen because we
                                        // always generate 64 frames at once,
                                        // even if the request is for fewer than
                                        // that. This ends up adding as many as
                                        // 63 extra frames to the audio queue,
                                        // but we know we'll be needing it soon,
                                        // so it's OK.
                                        frames_requested = 0;
                                    }
                                }
                            }
                        }
                    }
                    _ => panic!(),
                }
                if start_generation {
                    engine
                        .lock()
                        .unwrap()
                        .start_generation(frames_requested.min(64));
                }
            }
        });
    }
}

#[derive(Debug)]
pub struct Engine {
    master_track: TrackActor,
    ordered_track_uids: Vec<TrackUid>,
    tracks: HashMap<TrackUid, TrackActor>,
    track_uid_factory: Arc<TrackUidFactory>,
    entity_uid_factory: Arc<EntityUidFactory>,

    track_subscription: Subscription<TrackRequest>,

    transport: Transport,
    c: Configurables,
}
impl Configurable for Engine {
    delegate! {
        to self.c {
            fn sample_rate(&self) -> SampleRate;
            fn tempo(&self) -> Tempo;
            fn time_signature(&self) -> TimeSignature;
        }
    }
    fn update_sample_rate(&mut self, sample_rate: SampleRate) {
        self.c.update_sample_rate(sample_rate);
    }
    fn update_tempo(&mut self, tempo: Tempo) {
        self.c.update_tempo(tempo);
    }
    fn update_time_signature(&mut self, time_signature: TimeSignature) {
        self.c.update_time_signature(time_signature);
    }
}
impl HandlesMidi for Engine {
    fn handle_midi_message(
        &mut self,
        channel: MidiChannel,
        message: MidiMessage,
        _midi_messages_fn: &mut MidiMessagesFn,
    ) {
        self.track_subscription
            .broadcast(TrackRequest::Midi(channel, message));
    }

    fn midi_note_label_metadata(&self) -> Option<MidiNoteLabelMetadata> {
        None
    }
}
impl Controls for Engine {
    delegate! {
        to self.transport {
            fn time_range(&self) -> Option<TimeRange>;
            fn update_time_range(&mut self, time_range: &TimeRange);
            fn work(&mut self, control_events_fn: &mut ControlEventsFn);
            fn is_finished(&self) -> bool;
            fn play(&mut self);
            fn skip_to_start(&mut self);
            fn is_performing(&self) -> bool;
        }
    }

    fn stop(&mut self) {
        self.transport.stop();
        self.track_subscription.broadcast(TrackRequest::Midi(
            MidiChannel::default(),
            MidiMessage::Controller {
                controller: 123.into(),
                value: 0.into(),
            },
        ));
    }
}
impl Engine {
    fn new() -> Self {
        let entity_uid_factory: Arc<EntityUidFactory> = Default::default();
        let master_track = TrackActor::new_with(TrackUid::default(), true, &entity_uid_factory);
        let master_track_request = master_track.sender().clone();

        let mut r = Self {
            master_track,
            ordered_track_uids: Default::default(),
            tracks: Default::default(),
            track_uid_factory: Default::default(),
            entity_uid_factory,
            track_subscription: Default::default(),
            transport: Default::default(),
            c: Default::default(),
        };
        r.track_subscription.subscribe(&master_track_request);
        r
    }

    fn subscribe(&mut self, sender: &Sender<TrackAction>) {
        // The master track produces all the actions that we would publish, so
        // we delegate the subscription request to it.
        self.master_track
            .send_request(TrackRequest::Subscribe(sender.clone()));
    }

    fn start_generation(&mut self, count: usize) {
        // Figure out the time slice for this batch of frames.
        let time_range = self.transport.advance(count);

        // Ask tracks to do their time-based work.
        self.track_subscription
            .broadcast(TrackRequest::Work(time_range));

        // Ask master track for next buffer of frames.
        self.master_track
            .send_request(TrackRequest::NeedsAudio(count));
    }

    fn create_track(&mut self) -> anyhow::Result<TrackUid> {
        let track_uid = self.track_uid_factory.mint_next();
        let is_master_track = false;

        let track_actor =
            TrackActor::new_with(track_uid, is_master_track, &self.entity_uid_factory);
        track_actor.send_request(TrackRequest::Subscribe(
            self.master_track.action_sender().clone(),
        ));

        self.master_track.send_request(TrackRequest::AddSend(
            track_uid,
            track_actor.sender().clone(),
        ));

        self.track_subscription.subscribe(track_actor.sender());
        self.ordered_track_uids.push(track_uid);
        self.tracks.insert(track_uid, track_actor);

        Ok(track_uid)
    }

    fn delete_track(&mut self, uid: TrackUid) {
        self.master_track
            .send_request(TrackRequest::RemoveSend(uid));
        if let Some(track_actor) = self.tracks.get(&uid) {
            track_actor.send_request(TrackRequest::Unsubscribe(
                self.master_track.action_sender().clone(),
            ));
            track_actor.send_request(TrackRequest::Quit);
        }
        self.ordered_track_uids.retain(|t| *t != uid);
        self.tracks.remove(&uid);
    }

    fn request_quit(&mut self) {
        self.track_subscription.broadcast(TrackRequest::Quit);
    }
}
impl Displays for Engine {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.horizontal_top(|ui| {
            if ui.button("Play").clicked() {
                self.play();
            }
            if ui.button("Stop").clicked() {
                self.stop();
            }
            if ui.button("Add track").clicked() {
                let _ = self.create_track();
            }
        });
        let response = ui.separator();

        let mut track_index_to_delete = None;

        for &track_uid in self.ordered_track_uids.iter() {
            if let Some(track) = self.tracks.get_mut(&track_uid) {
                track.ui(ui);

                if ui.button(format!("Delete Track {}", track_uid)).clicked() {
                    track_index_to_delete = Some(track_uid);
                }
            }
        }
        ui.separator();
        self.master_track.ui(ui);

        if let Some(uid) = track_index_to_delete {
            self.delete_track(uid);
        }

        response
    }
}
