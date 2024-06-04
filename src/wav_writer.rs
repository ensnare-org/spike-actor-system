use ensnare_v1::prelude::*;
use anyhow::anyhow;
use ensnare::{prelude::*, traits::ProvidesService, types::CrossbeamChannel};
use ensnare_services::prelude::*;
use std::path::PathBuf;

#[derive(Debug)]
pub enum WavWriterInput {
    Reset(PathBuf, SampleRate, u8),
    Frames(Vec<StereoSample>),
    Quit,
}

#[derive(Debug)]
pub enum WavWriterEvent {
    Err(anyhow::Error),
}

#[derive(Debug)]
pub struct WavWriterService {
    inputs: CrossbeamChannel<WavWriterInput>,
    events: CrossbeamChannel<WavWriterEvent>,
}
impl Default for WavWriterService {
    fn default() -> Self {
        Self::new()
    }
}
impl WavWriterService {
    pub fn new() -> Self {
        let r = Self {
            inputs: Default::default(),
            events: Default::default(),
        };

        r.start_thread();
        r
    }

    fn start_thread(&self) {
        let receiver = self.inputs.receiver.clone();
        let sender = self.events.sender.clone();
        let mut writer = None;

        // Nice touch: don't write to the file until our first non-silent sample.
        let mut has_lead_in_ended = false;

        std::thread::spawn(move || {
            while let Ok(input) = receiver.recv() {
                match input {
                    WavWriterInput::Reset(path_buf, new_sample_rate, new_channel_count) => {
                        has_lead_in_ended = false;
                        match hound::WavWriter::create(
                            path_buf.as_os_str(),
                            hound::WavSpec {
                                channels: new_channel_count as u16,
                                sample_rate: new_sample_rate.0 as u32,
                                bits_per_sample: 32,
                                sample_format: hound::SampleFormat::Float,
                            },
                        ) {
                            Ok(ww) => {
                                writer = Some(ww);
                            }
                            Err(e) => {
                                writer = None;
                                let _ = sender.try_send(WavWriterEvent::Err(anyhow!(
                                    "Error while creating file: {:?}",
                                    e
                                )));
                            }
                        }
                    }
                    WavWriterInput::Frames(frames) => {
                        if let Some(writer) = writer.as_mut() {
                            frames.iter().for_each(|&f| {
                                if !has_lead_in_ended && f != StereoSample::SILENCE {
                                    has_lead_in_ended = true;
                                }
                                if has_lead_in_ended {
                                    let _ = writer.write_sample(f.0 .0 as f32);
                                    let _ = writer.write_sample(f.1 .0 as f32);
                                }
                            })
                        }
                    }
                    WavWriterInput::Quit => {
                        if let Some(writer) = writer {
                            let _ = writer.finalize();
                        }
                        break;
                    }
                }
            }
        });
    }
}
impl ProvidesService<WavWriterInput, WavWriterEvent> for WavWriterService {
    fn receiver(&self) -> &crossbeam_channel::Receiver<WavWriterEvent> {
        &self.events.receiver
    }

    fn sender(&self) -> &crossbeam_channel::Sender<WavWriterInput> {
        &self.inputs.sender
    }
}
