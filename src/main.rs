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
//!
//! Terminology
//!
//! - Input: A message sent from a client to a service that tells the service to
//!   do something or informs it that something happened.
//! - Event: A message broadcast by a service to its client(s) saying that
//!   something happened.
//! - Request: A message sent from an actor's owner to ask the actor to do something.
//! - Action: A message sent by an actor to inform its owner of completed work.

use crate::orchestration::EngineServiceEvent;
use anyhow::anyhow;
use crossbeam_channel::{Receiver, Select, Sender};
use eframe::egui::CentralPanel;
use ensnare::prelude::*;
use orchestration::{Engine, EngineService, EngineServiceInput};
use std::{
    sync::{atomic::Ordering, Arc, Mutex, RwLock},
    time::Duration,
};
use traits::ProvidesService;

mod always;
mod busy;
mod entity;
mod orchestration;
mod track;
mod traits;
mod wav_writer;

pub(crate) const ATOMIC_ORDERING: Ordering = Ordering::Relaxed;

#[derive(Debug)]
enum ServiceInput {
    Quit,
}

#[derive(Debug)]
enum ServiceEvent {
    NewOrchestratress(Arc<Mutex<Engine>>),
}

/// Manages all the services that the app uses.
#[derive(Debug)]
struct ServiceManager {
    input_channel_pair: ChannelPair<ServiceInput>,
    event_channel_pair: ChannelPair<ServiceEvent>,

    #[allow(dead_code)]
    midi_service: MidiService,
    #[allow(dead_code)]
    engine_service: EngineService,
}
impl ServiceManager {
    pub fn new() -> Self {
        let r = Self {
            midi_service: MidiService::new_with(&Arc::new(RwLock::new(MidiSettings::default()))),
            engine_service: EngineService::default(),
            input_channel_pair: Default::default(),
            event_channel_pair: Default::default(),
        };
        r.start_thread();
        r
    }

    fn start_thread(&self) {
        let midi_receiver = self.midi_service.receiver().clone();
        let midi_sender = self.midi_service.sender().clone();

        let engine_receiver = self.engine_service.receiver().clone();
        let engine_sender = self.engine_service.sender().clone();

        // This one is backwards (receiver of input, sender of event) because it
        // is the set of channels that others use to talk with the service
        // manager, rather than being channels that the service manager uses to
        // aggregate other services.
        let sm_receiver = self.input_channel_pair.receiver.clone();
        let sm_sender = self.event_channel_pair.sender.clone();

        std::thread::spawn(move || {
            let mut sel = Select::new();

            // AudioService is special because it isn't Send, so we have to
            // create it on the same thread that we use to communicate with it.
            // See https://github.com/RustAudio/cpal/issues/818.
            let audio_service = AudioService::default();
            let audio_receiver = audio_service.receiver().clone();
            let audio_sender = audio_service.sender().clone();
            let audio_index = sel.recv(&audio_receiver);

            let ui_index = sel.recv(&sm_receiver);
            let midi_index = sel.recv(&midi_receiver);
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
                                    let _ = engine_sender.try_send(EngineServiceInput::Reset(
                                        new_sample_rate,
                                        new_channels,
                                        new_audio_queue,
                                    ));
                                }
                                AudioServiceEvent::NeedsAudio(count) => {
                                    let _ = engine_sender
                                        .try_send(EngineServiceInput::NeedsAudio(count));
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
                                    let _ = engine_sender
                                        .try_send(EngineServiceInput::Midi(channel, message));
                                }
                                MidiServiceEvent::MidiOut => todo!("blink the activity indicator"),
                                MidiServiceEvent::InputPortsRefreshed(ports) => {
                                    eprintln!("inputs: {ports:?}");
                                    let _ = midi_sender.try_send(
                                        MidiInterfaceServiceInput::SelectMidiInput(
                                            ports[1].clone(),
                                        ),
                                    );
                                }
                                MidiServiceEvent::OutputPortsRefreshed(ports) => {
                                    eprintln!("outputs: {ports:?}");
                                    // let _ = midi_sender.send(MidiInterfaceServiceInput::SelectMidiOutput(ports[2].clone()));
                                }
                            }
                        }
                    }
                    index if index == engine_index => {
                        if let Ok(event) = operation.recv(&engine_receiver) {
                            println!("{event:?}");
                            match event {
                                EngineServiceEvent::NewOrchestratress(new_o) => {
                                    let _ =
                                        sm_sender.try_send(ServiceEvent::NewOrchestratress(new_o));
                                }
                                EngineServiceEvent::Midi(channel, message) => {
                                    let _ = midi_sender
                                        .send(MidiInterfaceServiceInput::Midi(channel, message));
                                }
                            }
                        }
                    }
                    index if index == ui_index => {
                        if let Ok(input) = operation.recv(&sm_receiver) {
                            println!("{input:?}");
                            match input {
                                ServiceInput::Quit => {
                                    let _ = audio_sender.try_send(AudioServiceInput::Quit);
                                    let _ = midi_sender.try_send(MidiInterfaceServiceInput::Quit);
                                    let _ = engine_sender.try_send(EngineServiceInput::Quit);
                                }
                            }
                            break;
                        }
                    }
                    _ => panic!(),
                }
            }
        });
    }

    fn receiver(&self) -> &Receiver<ServiceEvent> {
        &self.event_channel_pair.receiver
    }

    fn sender(&self) -> &Sender<ServiceInput> {
        &self.input_channel_pair.sender
    }
}

#[derive(Debug)]
struct ActorSystemApp {
    service_manager: ServiceManager,
    orchestratress: Option<Arc<Mutex<Engine>>>,
}
impl eframe::App for ActorSystemApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        loop {
            if let Ok(event) = self.service_manager.receiver().try_recv() {
                match event {
                    ServiceEvent::NewOrchestratress(new_o) => self.orchestratress = Some(new_o),
                }
            } else {
                break;
            }
        }
        CentralPanel::default().show(ctx, |ui| {
            if let Some(orchestratress) = self.orchestratress.as_ref() {
                orchestratress.lock().unwrap().ui(ui);
            }
        });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.service_manager.sender().try_send(ServiceInput::Quit);
    }
}
impl ActorSystemApp {
    pub const NAME: &'static str = "ActorSystemApp";

    pub fn new() -> Self {
        Self {
            service_manager: ServiceManager::new(),
            orchestratress: None,
        }
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
