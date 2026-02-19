use std::{
    io::{Error, ErrorKind},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, SizedSample,
};

use crate::audio_toolkit::{
    audio::{AudioVisualiser, FrameResampler},
    constants,
    vad::{self, VadFrame},
    VoiceActivityDetector,
};

enum Cmd {
    Start,
    Stop(mpsc::Sender<Vec<f32>>),
    Snapshot(mpsc::Sender<Vec<f32>>),
    SnapshotWindow {
        max_samples: usize,
        reply_tx: mpsc::Sender<Vec<f32>>,
    },
    Shutdown,
}

enum WorkerInit {
    Ready,
    Failed(String),
}

pub struct AudioRecorder {
    device: Option<Device>,
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
}

impl AudioRecorder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(AudioRecorder {
            device: None,
            cmd_tx: None,
            worker_handle: None,
            vad: None,
            level_cb: None,
        })
    }

    pub fn with_vad(mut self, vad: Box<dyn VoiceActivityDetector>) -> Self {
        self.vad = Some(Arc::new(Mutex::new(vad)));
        self
    }

    pub fn with_level_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(Vec<f32>) + Send + Sync + 'static,
    {
        self.level_cb = Some(Arc::new(cb));
        self
    }

    pub fn open(&mut self, device: Option<Device>) -> Result<(), Box<dyn std::error::Error>> {
        if self.worker_handle.is_some() {
            return Ok(()); // already open
        }

        let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::channel::<WorkerInit>();

        let host = crate::audio_toolkit::get_cpal_host();
        let device = match device {
            Some(dev) => dev,
            None => host
                .default_input_device()
                .ok_or_else(|| Error::new(std::io::ErrorKind::NotFound, "No input device found"))?,
        };

        let thread_device = device.clone();
        let vad = self.vad.clone();
        // Move the optional level callback into the worker thread
        let level_cb = self.level_cb.clone();

        let worker = std::thread::spawn(move || {
            let config = match AudioRecorder::get_preferred_config(&thread_device) {
                Ok(config) => config,
                Err(e) => {
                    let _ = init_tx.send(WorkerInit::Failed(format!(
                        "Failed to fetch preferred config: {}",
                        e
                    )));
                    return;
                }
            };

            let sample_rate = config.sample_rate().0;
            let channels = config.channels() as usize;

            log::info!(
                "Using device: {:?}\nSample rate: {}\nChannels: {}\nFormat: {:?}",
                thread_device.name(),
                sample_rate,
                channels,
                config.sample_format()
            );

            let stream = match config.sample_format() {
                cpal::SampleFormat::U8 => {
                    AudioRecorder::build_stream::<u8>(&thread_device, &config, sample_tx, channels)
                }
                cpal::SampleFormat::I8 => {
                    AudioRecorder::build_stream::<i8>(&thread_device, &config, sample_tx, channels)
                }
                cpal::SampleFormat::I16 => {
                    AudioRecorder::build_stream::<i16>(&thread_device, &config, sample_tx, channels)
                }
                cpal::SampleFormat::I32 => {
                    AudioRecorder::build_stream::<i32>(&thread_device, &config, sample_tx, channels)
                }
                cpal::SampleFormat::F32 => {
                    AudioRecorder::build_stream::<f32>(&thread_device, &config, sample_tx, channels)
                }
                _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
            };

            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    let _ = init_tx.send(WorkerInit::Failed(format!(
                        "Failed to build input stream: {}",
                        e
                    )));
                    return;
                }
            };

            if let Err(e) = stream.play() {
                let _ = init_tx.send(WorkerInit::Failed(format!(
                    "Failed to start input stream: {}",
                    e
                )));
                return;
            }

            let _ = init_tx.send(WorkerInit::Ready);

            // keep the stream alive while we process samples
            run_consumer(sample_rate, vad, sample_rx, cmd_rx, level_cb);
            // stream is dropped here, after run_consumer returns
        });

        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(WorkerInit::Ready) => {
                self.device = Some(device);
                self.cmd_tx = Some(cmd_tx);
                self.worker_handle = Some(worker);
                Ok(())
            }
            Ok(WorkerInit::Failed(message)) => {
                let _ = worker.join();
                Err(Error::other(message).into())
            }
            Err(e) => {
                let _ = cmd_tx.send(Cmd::Shutdown);
                let _ = worker.join();
                Err(Error::new(
                    ErrorKind::TimedOut,
                    format!("Timed out waiting for recorder startup: {}", e),
                )
                .into())
            }
        }
    }

    pub fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        let tx = self.cmd_tx.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::NotConnected,
                "Recorder is not open; cannot start recording",
            )
        })?;
        tx.send(Cmd::Start)?;
        Ok(())
    }

    pub fn stop(&self) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let (resp_tx, resp_rx) = mpsc::channel();
        let tx = self.cmd_tx.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::NotConnected,
                "Recorder is not open; cannot stop recording",
            )
        })?;
        tx.send(Cmd::Stop(resp_tx))?;
        Ok(resp_rx.recv_timeout(Duration::from_secs(3)).map_err(|e| {
            Error::new(
                ErrorKind::TimedOut,
                format!("Timed out waiting for recorder stop: {}", e),
            )
        })?)
    }

    pub fn snapshot(&self) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let (resp_tx, resp_rx) = mpsc::channel();
        let tx = self.cmd_tx.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::NotConnected,
                "Recorder is not open; cannot snapshot recording",
            )
        })?;
        tx.send(Cmd::Snapshot(resp_tx))?;
        Ok(resp_rx
            .recv_timeout(Duration::from_millis(800))
            .map_err(|e| {
                Error::new(
                    ErrorKind::TimedOut,
                    format!("Timed out waiting for recorder snapshot: {}", e),
                )
            })?)
    }

    pub fn snapshot_window(
        &self,
        max_samples: usize,
    ) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let (resp_tx, resp_rx) = mpsc::channel();
        let tx = self.cmd_tx.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::NotConnected,
                "Recorder is not open; cannot snapshot recording window",
            )
        })?;
        tx.send(Cmd::SnapshotWindow {
            max_samples,
            reply_tx: resp_tx,
        })?;
        Ok(resp_rx
            .recv_timeout(Duration::from_millis(800))
            .map_err(|e| {
                Error::new(
                    ErrorKind::TimedOut,
                    format!("Timed out waiting for recorder snapshot window: {}", e),
                )
            })?)
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(h) = self.worker_handle.take() {
            let _ = h.join();
        }
        self.device = None;
        Ok(())
    }

    fn build_stream<T>(
        device: &cpal::Device,
        config: &cpal::SupportedStreamConfig,
        sample_tx: mpsc::Sender<Vec<f32>>,
        channels: usize,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: Sample + SizedSample + Send + 'static,
        f32: cpal::FromSample<T>,
    {
        let mut output_buffer = Vec::new();

        let stream_cb = move |data: &[T], _: &cpal::InputCallbackInfo| {
            output_buffer.clear();

            if channels == 1 {
                // Direct conversion without intermediate Vec
                output_buffer.extend(data.iter().map(|&sample| sample.to_sample::<f32>()));
            } else {
                // Convert to mono directly
                let frame_count = data.len() / channels;
                output_buffer.reserve(frame_count);

                for frame in data.chunks_exact(channels) {
                    let mono_sample = frame
                        .iter()
                        .map(|&sample| sample.to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32;
                    output_buffer.push(mono_sample);
                }
            }

            if sample_tx.send(output_buffer.clone()).is_err() {
                log::error!("Failed to send samples");
            }
        };

        device.build_input_stream(
            &config.clone().into(),
            stream_cb,
            |err| log::error!("Stream error: {}", err),
            None,
        )
    }

    fn get_preferred_config(
        device: &cpal::Device,
    ) -> Result<cpal::SupportedStreamConfig, Box<dyn std::error::Error>> {
        let supported_configs = device.supported_input_configs()?;
        let mut best_config: Option<cpal::SupportedStreamConfigRange> = None;

        // Try to find a config that supports 16kHz, prioritizing better formats
        for config_range in supported_configs {
            if config_range.min_sample_rate().0 <= constants::WHISPER_SAMPLE_RATE
                && config_range.max_sample_rate().0 >= constants::WHISPER_SAMPLE_RATE
            {
                match best_config {
                    None => best_config = Some(config_range),
                    Some(ref current) => {
                        // Prioritize F32 > I16 > I32 > others
                        let score = |fmt: cpal::SampleFormat| match fmt {
                            cpal::SampleFormat::F32 => 4,
                            cpal::SampleFormat::I16 => 3,
                            cpal::SampleFormat::I32 => 2,
                            _ => 1,
                        };

                        if score(config_range.sample_format()) > score(current.sample_format()) {
                            best_config = Some(config_range);
                        }
                    }
                }
            }
        }

        if let Some(config) = best_config {
            return Ok(config.with_sample_rate(cpal::SampleRate(constants::WHISPER_SAMPLE_RATE)));
        }

        // If no config supports 16kHz, fall back to default
        Ok(device.default_input_config()?)
    }
}

fn run_consumer(
    in_sample_rate: u32,
    vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    sample_rx: mpsc::Receiver<Vec<f32>>,
    cmd_rx: mpsc::Receiver<Cmd>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
) {
    let mut frame_resampler = FrameResampler::new(
        in_sample_rate as usize,
        constants::WHISPER_SAMPLE_RATE as usize,
        Duration::from_millis(30),
    );

    let mut processed_samples = Vec::<f32>::new();
    let mut recording = false;

    // ---------- spectrum visualisation setup ---------------------------- //
    const BUCKETS: usize = 16;
    const WINDOW_SIZE: usize = 512;
    let mut visualizer = AudioVisualiser::new(
        in_sample_rate,
        WINDOW_SIZE,
        BUCKETS,
        400.0,  // vocal_min_hz
        4000.0, // vocal_max_hz
    );

    fn handle_frame(
        samples: &[f32],
        recording: bool,
        vad: &Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
        out_buf: &mut Vec<f32>,
    ) {
        if !recording {
            return;
        }

        if let Some(vad_arc) = vad {
            let mut det = vad_arc.lock().unwrap();
            match det.push_frame(samples).unwrap_or(VadFrame::Speech(samples)) {
                VadFrame::Speech(buf) => out_buf.extend_from_slice(buf),
                VadFrame::Noise => {}
            }
        } else {
            out_buf.extend_from_slice(samples);
        }
    }

    fn process_cmd(
        cmd: Cmd,
        recording: &mut bool,
        vad: &Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
        visualizer: &mut AudioVisualiser,
        frame_resampler: &mut FrameResampler,
        processed_samples: &mut Vec<f32>,
    ) -> bool {
        match cmd {
            Cmd::Start => {
                processed_samples.clear();
                *recording = true;
                visualizer.reset();
                if let Some(v) = vad {
                    v.lock().unwrap().reset();
                }
                false
            }
            Cmd::Stop(reply_tx) => {
                *recording = false;
                frame_resampler
                    .finish(&mut |frame: &[f32]| handle_frame(frame, true, vad, processed_samples));
                let _ = reply_tx.send(std::mem::take(processed_samples));
                false
            }
            Cmd::Snapshot(reply_tx) => {
                let _ = reply_tx.send(processed_samples.clone());
                false
            }
            Cmd::SnapshotWindow {
                max_samples,
                reply_tx,
            } => {
                if max_samples == 0 || processed_samples.len() <= max_samples {
                    let _ = reply_tx.send(processed_samples.clone());
                } else {
                    let start = processed_samples.len().saturating_sub(max_samples);
                    let _ = reply_tx.send(processed_samples[start..].to_vec());
                }
                false
            }
            Cmd::Shutdown => true,
        }
    }

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            if process_cmd(
                cmd,
                &mut recording,
                &vad,
                &mut visualizer,
                &mut frame_resampler,
                &mut processed_samples,
            ) {
                return;
            }
        }

        let raw = match sample_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(raw) => raw,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        if let Some(buckets) = visualizer.feed(&raw) {
            if let Some(cb) = &level_cb {
                cb(buckets);
            }
        }

        frame_resampler.push(&raw, &mut |frame: &[f32]| {
            handle_frame(frame, recording, &vad, &mut processed_samples)
        });
    }
}
