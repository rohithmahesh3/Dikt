use crate::settings::{Settings, SoundTheme};
use log::{debug, error, warn};
use rodio::{OutputStream, Sink};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::thread;

pub enum SoundType {
    Start,
    Stop,
}

fn get_sound_path(settings: &Settings, sound_type: SoundType) -> PathBuf {
    let filename = match (settings.sound_theme(), sound_type) {
        (SoundTheme::Custom, SoundType::Start) => "custom_start.wav",
        (SoundTheme::Custom, SoundType::Stop) => "custom_stop.wav",
        (SoundTheme::Pop, SoundType::Start) => "pop_start.wav",
        (SoundTheme::Pop, SoundType::Stop) => "pop_stop.wav",
        (SoundTheme::Marimba, SoundType::Start) => "marimba_start.wav",
        (SoundTheme::Marimba, SoundType::Stop) => "marimba_stop.wav",
    };

    if settings.sound_theme() == SoundTheme::Custom {
        let data_dir = std::env::var("XDG_DATA_HOME")
            .map(|p| PathBuf::from(p).join("dikt").join("sounds"))
            .unwrap_or_else(|_| PathBuf::from("/usr/share/dikt/sounds"));
        return data_dir.join(filename);
    }

    let system_path = PathBuf::from("/usr/share/dikt/sounds").join(filename);
    if system_path.exists() {
        return system_path;
    }

    PathBuf::from("resources").join(filename)
}

pub fn play_feedback_sound(settings: &Settings, sound_type: SoundType) {
    if !settings.audio_feedback() {
        return;
    }
    let path = get_sound_path(settings, sound_type);
    let volume = settings.audio_feedback_volume();
    let output_device = settings.selected_output_device();
    thread::spawn(move || {
        if let Err(e) = play_audio_file(&path, volume, output_device.as_deref()) {
            error!("Failed to play sound '{}': {}", path.display(), e);
        }
    });
}

pub fn play_feedback_sound_blocking(settings: &Settings, sound_type: SoundType) {
    if !settings.audio_feedback() {
        return;
    }
    let path = get_sound_path(settings, sound_type);
    if let Err(e) = play_audio_file(
        &path,
        settings.audio_feedback_volume(),
        settings.selected_output_device().as_deref(),
    ) {
        error!("Failed to play sound '{}': {}", path.display(), e);
    }
}

pub fn play_test_sound(settings: &Settings, sound_type: SoundType) {
    let path = get_sound_path(settings, sound_type);
    if let Err(e) = play_audio_file(
        &path,
        settings.audio_feedback_volume(),
        settings.selected_output_device().as_deref(),
    ) {
        error!("Failed to play sound '{}': {}", path.display(), e);
    }
}

fn play_audio_file(
    path: &Path,
    volume: f32,
    output_device_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("Playing audio file: {}", path.display());

    let (_stream, stream_handle) = if let Some(device_name) = output_device_name {
        match find_output_device_by_name(device_name)
            .and_then(|device| OutputStream::try_from_device(&device).ok())
        {
            Some(stream) => stream,
            None => {
                warn!(
                    "Selected output device '{}' not available, falling back to default output",
                    device_name
                );
                OutputStream::try_default()?
            }
        }
    } else {
        OutputStream::try_default()?
    };

    let file = File::open(path)?;
    let buf_reader = BufReader::new(file);
    let source = rodio::Decoder::new(buf_reader)?;

    let sink = Sink::try_new(&stream_handle)?;
    sink.append(source);
    sink.set_volume(volume);
    sink.sleep_until_end();

    Ok(())
}

fn find_output_device_by_name(device_name: &str) -> Option<rodio::cpal::Device> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};

    let host = rodio::cpal::default_host();
    let mut devices = host.output_devices().ok()?;
    devices.find(|device| {
        device
            .name()
            .map(|name| name == device_name)
            .unwrap_or(false)
    })
}
