use crate::audio_feedback::{play_feedback_sound_blocking, SoundType};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::Settings;
use crate::text_utils::convert_chinese_variant;
use log::{debug, info};

pub struct TranscriptionResult {
    pub text: String,
    pub post_processed: Option<String>,
}

pub async fn perform_transcription(
    recording_manager: &AudioRecordingManager,
    transcription_manager: &TranscriptionManager,
    settings: &Settings,
    post_process: bool,
) -> Result<TranscriptionResult, String> {
    let start_time = std::time::Instant::now();

    play_feedback_sound_blocking(settings, SoundType::Stop);

    let samples = recording_manager
        .stop_recording("transcribe")
        .ok_or("No samples retrieved")?;

    info!(
        "Recording stopped, {} samples in {:?}",
        samples.len(),
        start_time.elapsed()
    );

    let transcription = transcription_manager
        .transcribe(samples.clone())
        .map_err(|e| format!("Transcription failed: {}", e))?;

    let lang = settings.selected_language();
    let final_text = convert_chinese_variant(&transcription, &lang);

    let post_processed = if post_process && settings.post_process_enabled() {
        post_process_transcription(settings, &final_text).await
    } else {
        None
    };

    Ok(TranscriptionResult {
        text: final_text,
        post_processed,
    })
}

async fn post_process_transcription(settings: &Settings, text: &str) -> Option<String> {
    let provider_id = settings.post_process_provider_id();
    let api_key = settings.post_process_api_keys().get(&provider_id)?.clone();

    if api_key.is_empty() {
        debug!("No API key for provider {}", provider_id);
        return None;
    }

    let prompts = settings.post_process_prompts();
    let selected_id = settings.post_process_selected_prompt_id();

    let prompt = if let Some(id) = selected_id {
        prompts.iter().find(|p| p.id == id)
    } else {
        prompts.first()
    };

    let prompt = match prompt {
        Some(p) => p.prompt.clone(),
        None => return None,
    };

    let prompt = prompt.replace("${output}", text);

    crate::llm_client::call_llm(settings, &prompt).await
}
