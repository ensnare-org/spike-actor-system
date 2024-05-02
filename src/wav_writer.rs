use crate::traits::ProvidesService;
use anyhow::anyhow;
use ensnare::prelude::*;
use std::path::PathBuf;

#[derive(Debug)]
pub enum WavWriterInput {
    Reset(PathBuf, SampleRate, u16),
    Frames(Vec<StereoSample>),
    Quit,
}

#[derive(Debug)]
pub enum WavWriterEvent {
    Err(anyhow::Error),
}

#[derive(Debug)]
pub struct WavWriterService {
    input_channel_pair: ChannelPair<WavWriterInput>,
    event_channel_pair: ChannelPair<WavWriterEvent>,
}
impl Default for WavWriterService {
    fn default() -> Self {
        Self::new()
    }
}
impl WavWriterService {
    pub fn new() -> Self {
        let r = Self {
            input_channel_pair: Default::default(),
            event_channel_pair: Default::default(),
        };

        r.start_thread();
        r
    }

    fn start_thread(&self) {
        let receiver = self.input_channel_pair.receiver.clone();
        let sender = self.event_channel_pair.sender.clone();
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
                                channels: new_channel_count,
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
                                if !has_lead_in_ended {
                                    if f != StereoSample::SILENCE {
                                        has_lead_in_ended = true;
                                    }
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
        &self.event_channel_pair.receiver
    }

    fn sender(&self) -> &crossbeam_channel::Sender<WavWriterInput> {
        &self.input_channel_pair.sender
    }
}
