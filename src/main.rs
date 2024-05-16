use anyhow::anyhow;
use crossbeam_channel::{Receiver, Select, Sender};
use eframe::egui::{CentralPanel, ComboBox, Id, SidePanel};
use engine::{Engine, EngineService, EngineServiceEvent, EngineServiceInput};
use ensnare::prelude::*;
use ensnare::services::ProvidesService;
use std::{
    sync::{atomic::Ordering, Arc, Mutex, RwLock},
    time::Duration,
};

mod actions;
mod always;
mod arp;
mod busy;
mod drone;
mod engine;
mod entity;
mod mixer;
mod quietener;
mod subscription;
mod track;
mod traits;
mod wav_writer;

pub(crate) const ATOMIC_ORDERING: Ordering = Ordering::Relaxed;

#[derive(Debug)]
enum AppServiceInput {
    Quit,
    MidiInputPortSelected(MidiPortDescriptor),
    MidiOutputPortSelected(MidiPortDescriptor),
}

#[derive(Debug)]
enum AppServiceEvent {
    /// The service has started or restarted.
    Reset(Arc<Mutex<Engine>>),
    MidiInputsRefreshed(Vec<MidiPortDescriptor>),
    MidiOutputsRefreshed(Vec<MidiPortDescriptor>),
}

/// Manages all the services that the app uses.
#[derive(Debug)]
struct AppServiceManager {
    input_channel_pair: ChannelPair<AppServiceInput>,
    event_channel_pair: ChannelPair<AppServiceEvent>,

    // reason = "We need to keep a reference to the service or else it'll be dropped"
    #[allow(dead_code)]
    audio_service: AudioService,

    // reason = "We need to keep a reference to the service or else it'll be dropped"
    #[allow(dead_code)]
    midi_service: MidiService,
    // reason = "We need to keep a reference to the service or else it'll be dropped"
    #[allow(dead_code)]
    engine_service: EngineService,

    #[allow(dead_code)]
    midi_settings: Arc<RwLock<MidiSettings>>,
}
impl ProvidesService<AppServiceInput, AppServiceEvent> for AppServiceManager {
    fn receiver(&self) -> &Receiver<AppServiceEvent> {
        &self.event_channel_pair.receiver
    }

    fn sender(&self) -> &Sender<AppServiceInput> {
        &self.input_channel_pair.sender
    }
}
impl AppServiceManager {
    pub fn new() -> Self {
        let midi_settings = Arc::new(RwLock::new(MidiSettings::default()));
        let r = Self {
            audio_service: AudioService::default(),
            midi_service: MidiService::new_with(&midi_settings),
            engine_service: EngineService::default(),
            input_channel_pair: Default::default(),
            event_channel_pair: Default::default(),
            midi_settings,
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
        // is the set of channels that the app uses to talk with the service
        // manager, rather than being channels that the service manager uses to
        // aggregate other services.
        let service_manager_receiver = self.input_channel_pair.receiver.clone();
        let service_manager_sender = self.event_channel_pair.sender.clone();

        let audio_receiver = self.audio_service.receiver().clone();
        let audio_sender = self.audio_service.sender().clone();

        let _ = engine_sender.try_send(EngineServiceInput::SetAudioSender(
            self.audio_service.sender().clone(),
        ));

        std::thread::spawn(move || {
            let mut sel = Select::new();

            let audio_index = sel.recv(&audio_receiver);
            let service_manager_index = sel.recv(&service_manager_receiver);
            let midi_index = sel.recv(&midi_receiver);
            let engine_index = sel.recv(&engine_receiver);

            loop {
                let operation = sel.select();
                match operation.index() {
                    index if index == service_manager_index => {
                        if let Ok(input) =
                            Self::recv_operation(operation, &service_manager_receiver)
                        {
                            match input {
                                AppServiceInput::Quit => {
                                    println!("ServiceInput::Quit");
                                    let _ = audio_sender.try_send(AudioServiceInput::Quit);
                                    let _ = midi_sender.try_send(MidiInterfaceServiceInput::Quit);
                                    let _ = engine_sender.try_send(EngineServiceInput::Quit);
                                    break;
                                }
                                AppServiceInput::MidiInputPortSelected(port) => {
                                    let _ = midi_sender
                                        .try_send(MidiInterfaceServiceInput::SelectMidiInput(port));
                                }
                                AppServiceInput::MidiOutputPortSelected(port) => {
                                    let _ = midi_sender.try_send(
                                        MidiInterfaceServiceInput::SelectMidiOutput(port),
                                    );
                                }
                            }
                        }
                    }
                    index if index == audio_index => {
                        if let Ok(event) = Self::recv_operation(operation, &audio_receiver) {
                            match event {
                                AudioServiceEvent::Reset(new_sample_rate, new_channels) => {
                                    let _ = engine_sender.try_send(EngineServiceInput::Configure(
                                        new_sample_rate,
                                        new_channels,
                                    ));
                                }
                                AudioServiceEvent::FramesNeeded(count) => {
                                    let _ = engine_sender
                                        .try_send(EngineServiceInput::AudioQueueNeedsAudio(count));
                                }
                                AudioServiceEvent::Underrun => eprintln!("FYI underrun"),
                            }
                        }
                    }
                    index if index == midi_index => {
                        if let Ok(event) = Self::recv_operation(operation, &midi_receiver) {
                            match event {
                                MidiServiceEvent::Midi(channel, message) => {
                                    let _ = engine_sender
                                        .try_send(EngineServiceInput::Midi(channel, message));
                                }
                                MidiServiceEvent::MidiOut => {
                                    // TODO: blink activity.... (or get rid of this, because we sent it so we already know about it....)
                                }
                                MidiServiceEvent::InputPortsRefreshed(ports) => {
                                    let _ = service_manager_sender
                                        .try_send(AppServiceEvent::MidiInputsRefreshed(ports));
                                }
                                MidiServiceEvent::OutputPortsRefreshed(ports) => {
                                    let _ = service_manager_sender
                                        .try_send(AppServiceEvent::MidiOutputsRefreshed(ports));
                                }
                            }
                        }
                    }
                    index if index == engine_index => {
                        if let Ok(event) = Self::recv_operation(operation, &engine_receiver) {
                            match event {
                                EngineServiceEvent::Reset(new_o) => {
                                    let _ = service_manager_sender
                                        .try_send(AppServiceEvent::Reset(new_o));
                                }
                                EngineServiceEvent::Midi(channel, message) => {
                                    let _ = midi_sender.try_send(MidiInterfaceServiceInput::Midi(
                                        channel, message,
                                    ));
                                }
                            }
                        }
                    }
                    _ => panic!("ServiceManager: Unexpected select index"),
                }
            }
        });
    }
}

#[derive(Debug)]
struct ActorSystemApp {
    service_manager: AppServiceManager,
    engine: Option<Arc<Mutex<Engine>>>,
    midi_input_ports: Vec<MidiPortDescriptor>,
    midi_input_selected: usize,
    midi_output_ports: Vec<MidiPortDescriptor>,
    midi_output_selected: usize,
}
impl eframe::App for ActorSystemApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.service_manager.receiver().try_recv() {
            match event {
                AppServiceEvent::Reset(new_o) => self.engine = Some(new_o),
                AppServiceEvent::MidiInputsRefreshed(ports) => self.midi_input_ports = ports,
                AppServiceEvent::MidiOutputsRefreshed(ports) => self.midi_output_ports = ports,
            }
        }
        SidePanel::right(Id::new("right-panel")).show(ctx, |ui| {
            ui.heading("MIDI");
            if !self.midi_input_ports.is_empty()
                && ComboBox::new(ui.next_auto_id(), "MIDI Input")
                    .show_index(
                        ui,
                        &mut self.midi_input_selected,
                        self.midi_input_ports.len(),
                        |i| self.midi_input_ports[i].to_string(),
                    )
                    .changed()
            {
                self.service_manager
                    .send_input(AppServiceInput::MidiInputPortSelected(
                        self.midi_input_ports[self.midi_input_selected].clone(),
                    ));
            }

            if !self.midi_output_ports.is_empty()
                && ComboBox::new(ui.next_auto_id(), "MIDI Output")
                    .show_index(
                        ui,
                        &mut self.midi_output_selected,
                        self.midi_output_ports.len(),
                        |i| self.midi_output_ports[i].to_string(),
                    )
                    .changed()
            {
                self.service_manager
                    .send_input(AppServiceInput::MidiOutputPortSelected(
                        self.midi_output_ports[self.midi_output_selected].clone(),
                    ))
            }
        });
        CentralPanel::default().show(ctx, |ui| {
            if let Some(engine) = self.engine.as_ref() {
                if let Ok(mut engine) = engine.lock() {
                    engine.ui(ui);
                }
            }
        });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self
            .service_manager
            .sender()
            .try_send(AppServiceInput::Quit);
    }
}
impl ActorSystemApp {
    pub const NAME: &'static str = "ActorSystemApp";

    pub fn new() -> Self {
        Self {
            service_manager: AppServiceManager::new(),
            engine: Default::default(),
            midi_input_ports: Default::default(),
            midi_input_selected: Default::default(),
            midi_output_ports: Default::default(),
            midi_output_selected: Default::default(),
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
