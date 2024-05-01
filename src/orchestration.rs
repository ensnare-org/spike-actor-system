use crate::{
    track::{TrackActor, TrackInput},
    traits::ProvidesService,
    wav_writer::{WavWriterInput, WavWriterService},
    ATOMIC_ORDERING,
};
use crossbeam_channel::{Select, Sender};
use crossbeam_queue::ArrayQueue;
use delegate::delegate;
use eframe::egui::ahash::HashMap;
use ensnare::{
    orchestration::TrackUidFactory, prelude::*, traits::MidiNoteLabelMetadata, types::AudioQueue,
};
use std::{
    path::PathBuf,
    sync::{atomic::AtomicUsize, Arc, Mutex},
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
    /// The client would like the service to exit.
    Quit,
}

#[derive(Debug)]
pub enum EngineServiceEvent {
    NewOrchestratress(Arc<Mutex<Engine>>),
}

/// Events that [Engine] reports to its parent.
#[derive(Debug)]
pub enum EngineEvent {
    Frames(Vec<StereoSample>),
}

#[derive(Debug)]
pub struct EngineService {
    service_input_channel_pair: ChannelPair<EngineServiceInput>,
    service_event_channel_pair: ChannelPair<EngineServiceEvent>,
    engine_event_channel_pair: ChannelPair<EngineEvent>,

    engine: Arc<Mutex<Engine>>,
}
impl Default for EngineService {
    fn default() -> Self {
        Self::new()
    }
}
impl ProvidesService<EngineServiceInput, EngineServiceEvent> for EngineService {
    fn receiver(&self) -> &crossbeam_channel::Receiver<EngineServiceEvent> {
        &self.service_event_channel_pair.receiver
    }

    fn sender(&self) -> &Sender<EngineServiceInput> {
        &self.service_input_channel_pair.sender
    }
}
impl EngineService {
    pub fn new() -> Self {
        let engine_event_channel_pair: ChannelPair<EngineEvent> = Default::default();
        let r = Self {
            engine: Arc::new(Mutex::new(Engine::new_with_sender(
                &engine_event_channel_pair.sender,
            ))),
            service_input_channel_pair: Default::default(),
            service_event_channel_pair: Default::default(),
            engine_event_channel_pair,
        };

        r.start_thread();

        r
    }

    fn start_thread(&self) {
        let engine = Arc::clone(&self.engine);
        let _ =
            self.service_event_channel_pair
                .sender
                .try_send(EngineServiceEvent::NewOrchestratress(Arc::clone(
                    &self.engine,
                )));
        let service_input_receiver = self.service_input_channel_pair.receiver.clone();
        let mut audio_queue: Option<Arc<ArrayQueue<StereoSample>>> = None;

        let writer_service = WavWriterService::new();

        let mut frames_requested = 0;

        let engine_event_receiver = self.engine_event_channel_pair.receiver.clone();

        std::thread::spawn(move || {
            let mut sel = Select::default();
            let service_index = sel.recv(&service_input_receiver);
            let engine_index = sel.recv(&engine_event_receiver);
            loop {
                let oper = sel.select();
                let mut start_generation = false;
                match oper.index() {
                    index if index == service_index => {
                        if let Ok(input) = oper.recv(&service_input_receiver) {
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
                                    .handle_midi_message(channel, message, &mut |_, _| panic!()),
                                EngineServiceInput::NeedsAudio(count) => {
                                    if frames_requested == 0 {
                                        start_generation = true;
                                    }
                                    frames_requested += count;
                                }
                                EngineServiceInput::Quit => {
                                    writer_service.send_input(WavWriterInput::Quit);
                                    break;
                                }
                            }
                        }
                    }
                    index if index == engine_index => {
                        if let Ok(input) = oper.recv(&engine_event_receiver) {
                            match input {
                                EngineEvent::Frames(frames) => {
                                    let frames_len = frames.len();

                                    if let Some(queue) = audio_queue.as_ref() {
                                        for &frame in frames.iter() {
                                            let push_result = queue.force_push(frame);
                                            assert!(
                                                push_result.is_none(),
                                                "queue len/cap {}/{}, frames {}",
                                                queue.len(),
                                                queue.capacity(),
                                                frames_requested
                                            );
                                        }
                                    }
                                    writer_service.send_input(WavWriterInput::Frames(frames));

                                    assert!(frames_len <= 64);
                                    if frames_requested > frames_len {
                                        // We still have work to do, so kick off generation once again.
                                        frames_requested -= frames_len;
                                        start_generation = true;
                                    } else {
                                        // The case of (frames_requested < frames_len) can
                                        // happen because we always generate 64 frames at
                                        // once, even if the request is for fewer than that.
                                        // This ends up adding as many as 63 extra frames to
                                        // the audio queue, but we know we'll be needing it
                                        // soon, so it's OK.
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

/// Messages from [Engine] children to the engine.
#[derive(Debug)]
pub enum EngineInput {
    // Reset(SampleRate, u16, AudioQueue),
    Midi(MidiChannel, MidiMessage),
    Control(ControlIndex, ControlValue),
    // NeedsAudio(usize),
    Frames(Vec<StereoSample>),
}

#[derive(Debug)]
pub struct Engine {
    children_input_channel_pair: ChannelPair<EngineInput>,
    parent_sender: Sender<EngineEvent>,
    c: Configurables,
    buffer: Arc<Mutex<GenerationBuffer<StereoSample>>>,

    ordered_track_uids: Vec<TrackUid>,
    tracks: HashMap<TrackUid, TrackActor>,
    track_uid_factory: TrackUidFactory,
    master_track_uid: TrackUid,

    track_pending_count: Arc<AtomicUsize>,
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
        for track in self.tracks.values_mut() {
            track.send(TrackInput::Midi(channel, message));
        }
    }

    fn midi_note_label_metadata(&self) -> Option<MidiNoteLabelMetadata> {
        None
    }
}
impl Engine {
    pub fn new_with_sender(parent_sender: &Sender<EngineEvent>) -> Self {
        let track_uid_factory = TrackUidFactory::default();
        let master_track_uid = track_uid_factory.mint_next();

        let mut r = Self {
            children_input_channel_pair: Default::default(),
            parent_sender: parent_sender.clone(),
            c: Default::default(),
            buffer: Default::default(),
            ordered_track_uids: Default::default(),
            tracks: Default::default(),
            track_uid_factory,
            master_track_uid,
            track_pending_count: Default::default(),
        };

        r.create_track_with_uid(r.master_track_uid).unwrap();

        r.start_thread();
        r
    }

    fn start_thread(&self) {
        let receiver = self.children_input_channel_pair.receiver.clone();
        let parent_sender = self.parent_sender.clone();
        let track_pending_count = Arc::clone(&self.track_pending_count);
        let buffer = Arc::clone(&self.buffer);

        std::thread::spawn(move || loop {
            if let Ok(input) = receiver.recv() {
                match input {
                    EngineInput::Midi(_, _) => todo!(),
                    EngineInput::Control(index, value) => {
                        todo!();
                    }
                    EngineInput::Frames(frames) => {
                        let frames_len = frames.len();
                        assert!(frames_len <= 64);

                        buffer.lock().unwrap().merge(&frames);

                        // Are we on the last track?
                        if track_pending_count.fetch_sub(1, ATOMIC_ORDERING) == 1 {
                            // We've completed a buffer. Send it!
                            let _ = parent_sender.send(EngineEvent::Frames(
                                buffer.lock().unwrap().buffer().to_vec(),
                            ));
                        }
                    }
                }
            }
        });
    }

    pub fn start_generation(&mut self, count: usize) {
        println!("start {count}");
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.resize(count);
            buffer.clear();
        }
        if self.ordered_track_uids.is_empty() {
            let _ = self.parent_sender.send(EngineEvent::Frames(
                self.buffer.lock().unwrap().buffer().to_vec(),
            ));
        } else {
            let prior_number = self
                .track_pending_count
                .fetch_add(self.ordered_track_uids.len(), ATOMIC_ORDERING);
            assert_eq!(
                prior_number, 0,
                "we shouldn't start generation while another is pending"
            );
            for track_uid in self.ordered_track_uids.iter() {
                if let Some(track) = self.tracks.get(track_uid) {
                    track.send(TrackInput::NeedsAudio(count));
                }
            }
        }
    }

    fn create_track_with_uid(&mut self, track_uid: TrackUid) -> anyhow::Result<TrackUid> {
        let track_actor = TrackActor::new_with(track_uid, &self.children_input_channel_pair.sender);
        self.ordered_track_uids.push(track_uid.clone());
        self.tracks.insert(track_uid, track_actor);
        Ok(track_uid)
    }

    fn create_track(&mut self) -> anyhow::Result<TrackUid> {
        self.create_track_with_uid(self.track_uid_factory.mint_next())
    }

    fn delete_track(&mut self, uid: TrackUid) {
        self.ordered_track_uids.retain(|t| *t != uid);
        self.tracks.remove(&uid);
    }
}
impl Displays for Engine {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        let mut track_index_to_delete = None;

        for &track_uid in self.ordered_track_uids.iter() {
            if let Some(track) = self.tracks.get_mut(&track_uid) {
                track.ui(ui);

                if track_uid != self.master_track_uid {
                    if ui.button(format!("Delete Track {}", track_uid)).clicked() {
                        track_index_to_delete = Some(track_uid);
                    };
                }
            }
        }

        if let Some(uid) = track_index_to_delete {
            self.delete_track(uid);
        }
        ui.separator();
        if ui.button("Add track").clicked() {
            let _ = self.create_track();
        }

        ui.separator();

        ui.separator();
        ui.label("Coming soon!")
    }
}
