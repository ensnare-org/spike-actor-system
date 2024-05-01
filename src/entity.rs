use crossbeam_channel::Sender;
use eframe::egui::{Color32, Frame, Margin, Stroke};
use ensnare::prelude::*;
use std::sync::{atomic::AtomicBool, Arc, Mutex};

use crate::{track::TrackInput, ATOMIC_ORDERING};

#[derive(Debug)]
pub enum EntityInput {
    Midi(MidiChannel, MidiMessage),
    Control(ControlIndex, ControlValue),
    NeedsAudio(usize),
    NeedsTransformation(Vec<StereoSample>),
    Frames(Vec<StereoSample>),
    Quit,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum EntityEvent {
    Midi(MidiChannel, MidiMessage),
    Control(ControlIndex, ControlValue),
    Quit,
}

#[derive(Debug)]
pub struct EntityActor {
    input_channel_pair: ChannelPair<EntityInput>,
    sender: Sender<TrackInput>,
    entity: Arc<Mutex<dyn EntityBounds>>,
    is_emitting_sound: Arc<AtomicBool>,
}
impl EntityActor {
    pub fn new_with(entity: impl EntityBounds + 'static, sender: &Sender<TrackInput>) -> Self {
        Self::new_with_wrapped(Arc::new(Mutex::new(entity)), sender)
    }

    pub fn new_with_wrapped(
        entity: Arc<Mutex<dyn EntityBounds>>,
        sender: &Sender<TrackInput>,
    ) -> Self {
        let r = Self {
            input_channel_pair: Default::default(),
            sender: sender.clone(),
            entity,
            is_emitting_sound: Default::default(),
        };
        r.start_input_thread();
        r
    }

    fn start_input_thread(&self) {
        let receiver = self.input_channel_pair.receiver.clone();
        let sender = self.sender.clone();
        let entity = Arc::clone(&self.entity);
        let mut buffer = GenerationBuffer::<StereoSample>::default();
        let is_emitting_sound = Arc::clone(&self.is_emitting_sound);

        std::thread::spawn(move || {
            while let Ok(input) = receiver.recv() {
                match input {
                    EntityInput::Midi(channel, message) => {
                        entity.lock().unwrap().handle_midi_message(
                            channel,
                            message,
                            &mut |c, m| {
                                let _ = sender.try_send(TrackInput::Midi(c, m));
                            },
                        );
                    }
                    EntityInput::Control(index, value) => {
                        entity
                            .lock()
                            .unwrap()
                            .control_set_param_by_index(index, value);
                    }
                    EntityInput::NeedsAudio(count) => {
                        buffer.resize(count);
                        buffer.clear();
                        entity.lock().unwrap().generate(buffer.buffer_mut());

                        is_emitting_sound.store(
                            buffer.buffer().iter().any(|&s| s != StereoSample::SILENCE),
                            ATOMIC_ORDERING,
                        );
                        let _ = sender.try_send(TrackInput::Frames(buffer.buffer().to_vec()));
                    }
                    EntityInput::Frames(_) => {
                        panic!("I generate audio; I don't aggregate it")
                    }
                    EntityInput::Quit => {
                        break;
                    }
                    EntityInput::NeedsTransformation(frames) => todo!(),
                }
            }
            eprintln!("EntityActor exit");
        });
    }

    pub fn send(&self, msg: EntityInput) {
        let _ = self.input_channel_pair.sender.try_send(msg);
    }
}
impl Displays for EntityActor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui) -> eframe::egui::Response {
        Frame::default()
            .stroke(if self.is_emitting_sound.load(ATOMIC_ORDERING) {
                Stroke::new(2.0, Color32::YELLOW)
            } else {
                Stroke::default()
            })
            .inner_margin(Margin::same(4.0))
            .show(ui, |ui| {
                let mut entity = self.entity.lock().unwrap();
                entity.ui(ui)
            })
            .inner
    }
}
