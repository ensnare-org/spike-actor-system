//! The app spins up required services, each of which creates channels for
//! communicating inputs and events with the app.
//!
//! The app's UI thread also looks like a service, except that it handles events
//! only as part of the egui update() code, which means that there will be
//! latency on average in handling those events. But that should be OK, because
//! the only events sent to it should be restricted to UI events.
//!
//! Specific services
//!
//! - Audio
//!     - accepts config changes
//!     - reports that the audio buffer needs data
//!     - provides a queue for clients to supply data
//!     - (future) handles audio input
//! - MIDI
//!     - accepts config changes
//!     - reports changes (e.g., a device was added)
//!     - reports MIDI data arriving on interface inputs (should this be a
//!       queue?)
//!     - handles MIDI data outgoing to interface outputs (should this be a
//!       queue?)
//! - Engine
//!     - accepts config changes
//!     - reports events (outgoing MIDI messages, generated audio)
//!     - takes an optional audio queue where it pushes generated audio
//!     - can be interacted with directly (via Arc<Mutex>) for fast egui code
//!
//! Then each audio device is an actor, which I think is identical to a service
//! in the sense that it has input/event channels and the ability to do the egui
//! direct-interaction thing.
//!
//! The audio flow works like this:
//!
//! 1. cpal calls us on the callback
//! 2. We provide the audio data it needs from the buffer
//! 3. Just before the callback returns, we decide that we're under the buffer's
//!    low-water mark, and we send NeedsAudio to the engine. *** worried about
//!    reentrancy if this callback returns and the next one happens before the
//!    prior NeedsAudio is fulfilled ***
//! 4. The engine receives a NeedsAudio. It calculates what needs to be done to
//!    satisfy that request, which at a high level is (1) identify generators of
//!    audio, (2) chain them to effects, (3) run the sends, (4) mix down to
//!    final, and (5) push the final onto the audio queue.
//!
//! Breaking down step 4:
//!
//! - each track has one or more audio sources (instrument). Create a buffer for
//!   the track, issue generate requests to each, and as they produce their
//!   results, mix them into the buffer. When everyone has finished, the audio
//!   source is done and then it's time for effects.
//! - As a buffer finishes with its sources, ask each effect to do its
//!   processing, and as it returns, hand it to the next effect or indicate that
//!   the track's buffer is done.
//! - Handle sends - each track buffer might be the source of a send, so as a
//!   track finishes, mix it into the send buffer(s) and then let them decide
//!   whether it's time to kick off their processing. This is pretty much
//!   identical to regular tracks, except that they don't get their source until
//!   later. (I think this can all be expressed as a dependency graph.)
//! - When send tracks are done processing, mark those track buffers as done.
//! - When all tracks (normal + send) are done, mix down to final and broadcast
//!   that the track's buffer is complete.
//!
//! - Final buffer depends on tracks
//! - Track depends on last effect
//! - Last effect depends on prior effect
//! - First effect depends on instrument buffer -OR- sends
//! - Instrument buffer depends on each instrument
//!
//! 1. Start with master track.
//! 2. Notice that master track depends on all other tracks.
//! 3. Kick off all other tracks.
//!
//! 1. Create an empty buffer for the track
//! 2. Associate a count of instruments (OR SENDS) with the buffer
//! 3. Issue generate to each instrument in track
//! 4. When a buffer comes back, mix it into [track]'s buffer
//! 5. Decrement count. If it's done, then it's time to move on to effects.
//! 6. Create effects chain: a list of Uids of each effect.
//! 7. As each effect returns, look up [Uid] in the list, send its results to
//!    next, or terminate.
//! 8. If the effects are done, then the track is done.
//! 9. If the track is a source of an aux track, treat it like an instrument --
//!    step 4.
//! 10. The master track is one giant aux track. So it has an instrument count,
//!     effects, etc.
//! 11. If the master track is done, broadcast its buffer.

use anyhow::anyhow;
use crossbeam_channel::{Receiver, Select, Sender};
use crossbeam_queue::ArrayQueue;
use delegate::delegate;
use eframe::egui::{ahash::HashMap, CentralPanel};
use ensnare::{orchestration::TrackUidFactory, prelude::*, traits::MidiNoteLabelMetadata};
use ensnare_toys::{ToyInstrument, ToySynth};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex, RwLock,
};

const ATOMIC_ORDERING: Ordering = Ordering::Relaxed;

#[derive(Debug)]
enum Input {
    Midi(MidiChannel, MidiMessage),
    Control(ControlIndex, ControlValue),
    NeedsAudio(usize),
    Frames(Vec<StereoSample>),
    Quit,
}

#[derive(Debug)]
enum Event {
    Midi(MidiChannel, MidiMessage),
    Control,
    Quit,
}

#[derive(Debug)]
struct EntityActor {
    input_channel_pair: ChannelPair<Input>,
    sender: Sender<Input>,
    entity: Arc<Mutex<dyn EntityBounds>>,
}
impl EntityActor {
    fn new_with(entity: impl EntityBounds + 'static, sender: &Sender<Input>) -> Self {
        Self::new_with_wrapped(Arc::new(Mutex::new(entity)), sender)
    }

    fn new_with_wrapped(entity: Arc<Mutex<dyn EntityBounds>>, sender: &Sender<Input>) -> Self {
        let r = Self {
            input_channel_pair: Default::default(),
            sender: sender.clone(),
            entity,
        };
        r.start_input_thread();
        r
    }

    fn start_input_thread(&self) {
        let receiver = self.input_channel_pair.receiver.clone();
        let sender = self.sender.clone();
        let entity = Arc::clone(&self.entity);
        let mut buffer = GenerationBuffer::<StereoSample>::default();

        std::thread::spawn(move || {
            while let Ok(input) = receiver.recv() {
                match input {
                    Input::Midi(channel, message) => {
                        entity.lock().unwrap().handle_midi_message(
                            channel,
                            message,
                            &mut |c, m| {
                                let _ = sender.try_send(Input::Midi(c, m));
                            },
                        );
                    }
                    Input::Control(index, value) => {
                        entity
                            .lock()
                            .unwrap()
                            .control_set_param_by_index(index, value);
                    }
                    Input::NeedsAudio(count) => {
                        buffer.resize(count);
                        buffer.clear();
                        entity.lock().unwrap().generate(buffer.buffer_mut());
                        let _ = sender.try_send(Input::Frames(buffer.buffer().to_vec()));
                    }
                    Input::Frames(count) => {
                        panic!("I generate audio; I don't aggregate it")
                    }
                    Input::Quit => {
                        let _ = sender.try_send(Input::Quit); // TODO: this will create a flood of Quits. Who cares?
                    }
                }
            }
            eprintln!("EntityActor exit");
        });
    }

    fn send(&self, msg: Input) {
        let _ = self.input_channel_pair.sender.try_send(msg);
    }
}
impl Displays for EntityActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        self.entity.lock().unwrap().ui(ui)
    }

    fn set_action(&mut self, action: DisplaysAction) {}

    fn take_action(&mut self) -> Option<DisplaysAction> {
        None
    }

    fn set_view_range(&mut self, view_range: &ViewRange) {}
}

#[derive(Debug)]
struct TrackActor {
    input_channel_pair: ChannelPair<Input>,
    sender: Sender<Input>,

    inner: Arc<Mutex<Track>>,
}
impl TrackActor {
    fn new_with(track_uid: TrackUid, sender: &Sender<Input>) -> Self {
        let input_channel_pair = ChannelPair::<Input>::default();
        let track = Track::new_with(track_uid, &input_channel_pair.sender);

        // track.add_actor();

        // let mut instrument_2 = ToyInstrument::default();
        // instrument_2.handle_midi_message(
        //     MidiChannel::default(),
        //     MidiUtils::new_note_on(64, 127),
        //     &mut |c, m| {},
        // );
        // track.actors.push(EntityActor::new_with(
        //     instrument_2,
        //     &input_channel_pair.sender,
        // ));

        // let mut instrument_3 = ToyInstrument::default();
        // instrument_3.handle_midi_message(
        //     MidiChannel::default(),
        //     MidiUtils::new_note_on(67, 127),
        //     &mut |c, m| {},
        // );
        // track.actors.push(EntityActor::new_with(
        //     instrument_3,
        //     &input_channel_pair.sender,
        // ));

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
                        Input::Midi(channel, message) => {
                            if let Ok(track) = track.lock() {
                                for actor in track.actors.iter() {
                                    let _ = actor.send(Input::Midi(channel, message));
                                }
                            }
                        }
                        Input::Control(_, _) => todo!(),
                        Input::Frames(frames) => {
                            assert!(frames.len() <= 64);
                            // We got some audio from an instrument. Mix it into
                            // the track buffer.
                            buffer.merge(&frames);
                            // Then see if we've gotten all the ones we expect.
                            if pending_instrument_count.fetch_sub(1, ATOMIC_ORDERING) == 1 {
                                // We have. Send our completed track to the owner.
                                let _ = sender.send(Input::Frames(buffer.buffer().to_vec()));
                            }
                        }
                        Input::Quit => todo!(),
                        Input::NeedsAudio(count) => {
                            buffer.resize(count);
                            buffer.clear();
                            if let Ok(mut track) = track.lock() {
                                assert_eq!(pending_instrument_count.load(ATOMIC_ORDERING), 0);

                                if track.actors.is_empty() {
                                    // TODO: consolidate this with the Frames
                                    // case, because I'm sure this will grow in
                                    // complexity, and we'll end up having to
                                    // maintain two copies.
                                    let _ = sender.send(Input::Frames(buffer.buffer().to_vec()));
                                } else {
                                    pending_instrument_count
                                        .fetch_add(track.actors.len(), ATOMIC_ORDERING);
                                    for actor in track.actors.iter_mut() {
                                        let _ = actor.send(Input::NeedsAudio(count));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    fn send(&self, input: Input) {
        let _ = self.input_channel_pair.sender.send(input);
    }
}

#[derive(Debug)]
struct Track {
    uid: TrackUid,
    sender: Sender<Input>,
    actors: Vec<EntityActor>,
}
impl Track {
    fn new_with(uid: TrackUid, sender: &Sender<Input>) -> Self {
        Self {
            uid,
            sender: sender.clone(),
            actors: Default::default(),
        }
    }

    fn add_actor(&mut self) {
        self.actors
            .push(EntityActor::new_with(ToySynth::default(), &self.sender));
    }
}

impl Displays for Track {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        ui.horizontal_top(|ui| {
            for actor in self.actors.iter_mut() {
                actor.ui(ui);
            }
            if ui.button("Add").clicked() {
                self.add_actor();
            }
        });
        ui.label("Coming soon!")
    }

    fn set_action(&mut self, action: DisplaysAction) {}

    fn take_action(&mut self) -> Option<DisplaysAction> {
        None
    }

    fn set_view_range(&mut self, view_range: &ViewRange) {}
}

#[derive(Debug)]
struct Orchestratress {
    input_channel_pair: ChannelPair<Input>,
    input_sender: Sender<EngineServiceInput>,
    c: Configurables,
    buffer: Arc<Mutex<GenerationBuffer<StereoSample>>>,

    ordered_track_uids: Vec<TrackUid>,
    tracks: HashMap<TrackUid, TrackActor>,
    track_uid_factory: TrackUidFactory,
    master_track_uid: TrackUid,

    track_pending_count: Arc<AtomicUsize>,
}
impl Configurable for Orchestratress {
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
impl HandlesMidi for Orchestratress {
    fn handle_midi_message(
        &mut self,
        channel: MidiChannel,
        message: MidiMessage,
        _midi_messages_fn: &mut MidiMessagesFn,
    ) {
        for track in self.tracks.values_mut() {
            track.send(Input::Midi(channel, message));
        }
    }

    fn midi_note_label_metadata(&self) -> Option<MidiNoteLabelMetadata> {
        None
    }
}
impl Orchestratress {
    fn new_with_sender(sender: &Sender<EngineServiceInput>) -> Self {
        let track_uid_factory = TrackUidFactory::default();
        let master_track_uid = track_uid_factory.mint_next();

        let mut r = Self {
            input_channel_pair: Default::default(),
            input_sender: sender.clone(),
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
        let receiver = self.input_channel_pair.receiver.clone();
        let sender = self.input_sender.clone();
        let track_pending_count = Arc::clone(&self.track_pending_count);
        let buffer = Arc::clone(&self.buffer);

        std::thread::spawn(move || loop {
            if let Ok(input) = receiver.recv() {
                match input {
                    Input::Midi(_, _) => todo!(),
                    Input::Control(_, _) => todo!(),
                    Input::Frames(frames) => {
                        let frames_len = frames.len();
                        assert!(frames_len <= 64);

                        buffer.lock().unwrap().merge(&frames);

                        // Are we on the last track?
                        if track_pending_count.fetch_sub(1, ATOMIC_ORDERING) == 1 {
                            // We've completed a buffer. Send it!
                            let _ = sender.send(EngineServiceInput::Frames(
                                buffer.lock().unwrap().buffer().to_vec(),
                            ));
                        }
                    }
                    Input::Quit => todo!(),
                    Input::NeedsAudio(count) => {
                        panic!("this is only downstream from parent, not upstream from children")
                    }
                }
            }
        });
    }

    fn start_generation(&mut self, count: usize) {
        println!("start {count}");
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.resize(count);
            buffer.clear();
        }
        if self.ordered_track_uids.is_empty() {
            let _ = self.input_sender.send(EngineServiceInput::Frames(
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
                    track.send(Input::NeedsAudio(count));
                }
            }
        }
    }

    fn create_track_with_uid(&mut self, track_uid: TrackUid) -> anyhow::Result<TrackUid> {
        let track_actor = TrackActor::new_with(track_uid, &self.input_channel_pair.sender);
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
impl Displays for Orchestratress {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        let mut track_index_to_delete = None;

        for &track_uid in self.ordered_track_uids.iter() {
            if let Some(track) = self.tracks.get_mut(&track_uid) {
                track.inner.lock().unwrap().ui(ui);

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
            self.create_track();
        }

        ui.separator();

        ui.separator();
        ui.label("Coming soon!")
    }

    fn set_action(&mut self, action: DisplaysAction) {}

    fn take_action(&mut self) -> Option<DisplaysAction> {
        None
    }

    fn set_view_range(&mut self, view_range: &ViewRange) {}
}

#[derive(Debug)]
enum EngineServiceInput {
    Quit,
    Reset(SampleRate, u16, Arc<ArrayQueue<StereoSample>>),
    NeedsAudio(usize),
    Frames(Vec<StereoSample>),
}

#[derive(Debug)]
enum EngineServiceEvent {
    Quit,
}

#[derive(Debug)]
struct EngineService {
    input_channel_pair: ChannelPair<EngineServiceInput>,
    event_channel_pair: ChannelPair<EngineServiceEvent>,

    orchestratress: Arc<Mutex<Orchestratress>>,
}
impl EngineService {
    fn new_with(
        orchestratress: &Arc<Mutex<Orchestratress>>,
        input_channel_pair: ChannelPair<EngineServiceInput>,
    ) -> Self {
        let r = Self {
            input_channel_pair,
            event_channel_pair: Default::default(),
            orchestratress: Arc::clone(&orchestratress),
        };

        r.start_thread();

        r
    }

    fn start_thread(&self) {
        let o = Arc::clone(&self.orchestratress);
        let receiver = self.input_channel_pair.receiver.clone();
        let mut audio_queue: Option<Arc<ArrayQueue<StereoSample>>> = None;

        let o_sender = o.lock().unwrap().input_channel_pair.sender.clone();

        let mut frames_requested = 0;

        std::thread::spawn(move || loop {
            if let Ok(input) = receiver.recv() {
                let mut start_generation = false;
                match input {
                    EngineServiceInput::Quit => break,
                    EngineServiceInput::NeedsAudio(count) => {
                        if frames_requested == 0 {
                            start_generation = true;
                        }
                        frames_requested += count;
                    }
                    EngineServiceInput::Frames(frames) => {
                        let frames_len = frames.len();

                        if let Some(queue) = audio_queue.as_ref() {
                            for (i, &frame) in frames.iter().enumerate() {
                                // assert_eq!(
                                //     frame,
                                //     StereoSample::from(i as f64 / 64.0),
                                //     "failed at frame #{}",
                                //     frame_counter
                                // );
                                // counter += 1;
                                // if counter == 100 {
                                //     counter = 0;
                                // }
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
                    EngineServiceInput::Reset(sample_rate, _channels, new_audio_queue) => {
                        audio_queue = Some(new_audio_queue);
                        o.lock().unwrap().update_sample_rate(sample_rate);
                    }
                }

                if start_generation {
                    o.lock().unwrap().start_generation(frames_requested.min(64));
                }
            }
        });
    }

    fn receiver(&self) -> &Receiver<EngineServiceEvent> {
        &self.event_channel_pair.receiver
    }

    fn sender(&self) -> &Sender<EngineServiceInput> {
        &self.input_channel_pair.sender
    }
}

#[derive(Debug)]
struct ActorSystemApp {
    orchestratress: Arc<Mutex<Orchestratress>>,
}
impl eframe::App for ActorSystemApp {
    fn update(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        CentralPanel::default().show(ctx, |ui| {
            self.orchestratress.lock().unwrap().ui(ui);
        });
    }
}
impl ActorSystemApp {
    pub const NAME: &'static str = "ActorSystemApp";

    pub fn new() -> Self {
        let engine_input_channel_pair = ChannelPair::<EngineServiceInput>::default();
        let r = Self {
            orchestratress: Arc::new(Mutex::new(Orchestratress::new_with_sender(
                &engine_input_channel_pair.sender,
            ))),
        };

        r.start_thread(engine_input_channel_pair);

        r
    }

    fn start_thread(&self, engine_input_channel_pair: ChannelPair<EngineServiceInput>) {
        let o = Arc::clone(&self.orchestratress);

        std::thread::spawn(move || {
            let mut sel = Select::new();

            let audio_service = AudioService::default();
            let audio_receiver = audio_service.receiver().clone();
            let audio_index = sel.recv(&audio_receiver);

            let midi_settings = Arc::new(RwLock::new(MidiSettings::default()));
            let midi_service = MidiService::new_with(&midi_settings);
            let midi_receiver = midi_service.receiver().clone();
            let midi_index = sel.recv(&midi_receiver);
            let midi_sender = midi_service.sender().clone();

            let engine_service = EngineService::new_with(&o, engine_input_channel_pair);
            let engine_receiver = engine_service.receiver().clone();
            let engine_sender = engine_service.sender().clone();
            let engine_index = sel.recv(&engine_receiver);

            loop {
                let operation = sel.select();
                match operation.index() {
                    index if index == audio_index => {
                        if let Ok(event) = operation.recv(&audio_receiver) {
                            match event {
                                AudioServiceEvent::Reset(
                                    new_sample_rate,
                                    new_channels,
                                    new_audio_queue,
                                ) => {
                                    // for _ in 0..new_sample_rate.0 {
                                    //     let _ = new_audio_queue.push(StereoSample::default());
                                    // }
                                    let _ = engine_sender.send(EngineServiceInput::Reset(
                                        new_sample_rate,
                                        new_channels,
                                        new_audio_queue,
                                    ));
                                }
                                AudioServiceEvent::NeedsAudio(count) => {
                                    println!("NeedsAudio({count})");
                                    let _ =
                                        engine_sender.send(EngineServiceInput::NeedsAudio(count));
                                }
                                AudioServiceEvent::Underrun => println!("FYI underrun"),
                            }
                        }
                    }
                    index if index == midi_index => {
                        if let Ok(event) = operation.recv(&midi_receiver) {
                            println!("{event:?}");
                            match event {
                                MidiServiceEvent::Midi(channel, message) => {
                                    o.lock().unwrap().handle_midi_message(channel, message, &mut |c, m| {
                                        panic!("If this gets called, it means that Orchestratress wanted to broadcast a MIDI message by callback, but it should be using the channel");
                                    })
                                }
                                MidiServiceEvent::MidiOut => todo!("blink the activity indicator"),
                                MidiServiceEvent::InputPortsRefreshed(ports) => {eprintln!("inputs: {ports:?}"); 
                                let _ = midi_sender.send(MidiInterfaceServiceInput::SelectMidiInput(ports[1].clone()));
                            },
                                MidiServiceEvent::OutputPortsRefreshed(ports) => {eprintln!("outputs: {ports:?}");
                                // let _ = midi_sender.send(MidiInterfaceServiceInput::SelectMidiOutput(ports[2].clone()));
                                },
                            }
                        }
                    }
                    index if index == engine_index => {
                        if let Ok(event) = operation.recv(&engine_receiver) {
                            println!("{event:?}");
                            match event {
                                EngineServiceEvent::Quit => todo!(),
                            }
                        }
                    }
                    _ => panic!(),
                }
            }
        });
    }
}

fn main() -> anyhow::Result<()> {
    const APP_NAME: &str = ActorSystemApp::NAME;

    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size(eframe::epaint::vec2(1280.0, 720.0))
            .to_owned(),
        vsync: true,
        centered: true,
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        APP_NAME,
        options,
        Box::new(|_cc| Box::new(ActorSystemApp::new())),
    ) {
        return Err(anyhow!("eframe::run_native failed: {:?}", e));
    }

    Ok(())
}
