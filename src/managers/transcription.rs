use crate::audio_toolkit::{apply_custom_words, filter_transcription_output};
use crate::managers::model::{EngineType, ModelManager};
use crate::settings::{ModelUnloadTimeout, Settings};
use anyhow::Result;
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};
use transcribe_rs::{
    engines::{
        moonshine::{ModelVariant, MoonshineEngine, MoonshineModelParams},
        parakeet::{ParakeetEngine, ParakeetModelParams},
        sense_voice::{SenseVoiceEngine, SenseVoiceModelParams},
        whisper::{WhisperEngine, WhisperInferenceParams},
    },
    TranscriptionEngine,
};

enum LoadedEngine {
    Whisper(WhisperEngine),
    Parakeet(ParakeetEngine),
    Moonshine(MoonshineEngine),
    SenseVoice(SenseVoiceEngine),
}

const LOAD_RETRY_COOLDOWN_MS: u64 = 3000;

struct ModelLoadFailure {
    model_id: String,
    message: String,
    at_ms: u64,
}

#[derive(Clone)]
pub struct TranscriptionConfig {
    pub model_unload_timeout: ModelUnloadTimeout,
    pub selected_language: String,
    pub translate_to_english: bool,
    pub custom_words: Vec<String>,
    pub word_correction_threshold: f64,
}

impl TranscriptionConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            model_unload_timeout: settings.model_unload_timeout(),
            selected_language: settings.selected_language(),
            translate_to_english: settings.translate_to_english(),
            custom_words: settings.custom_words(),
            word_correction_threshold: settings.word_correction_threshold(),
        }
    }
}

struct SharedState {
    engine: Mutex<Option<LoadedEngine>>,
    config: Mutex<TranscriptionConfig>,
    current_model_id: Mutex<Option<String>>,
    last_activity: AtomicU64,
    is_loading: Mutex<bool>,
    loading_condvar: Condvar,
    last_load_failure: Mutex<Option<ModelLoadFailure>>,
}

pub struct TranscriptionManager {
    shared: Arc<SharedState>,
    model_manager: Arc<ModelManager>,
    shutdown_signal: Arc<AtomicBool>,
    watcher_handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl TranscriptionManager {
    pub fn new(model_manager: Arc<ModelManager>) -> Result<Self> {
        let settings = Settings::new();
        let config = TranscriptionConfig::from_settings(&settings);
        let _unload_timeout = config.model_unload_timeout;

        let shared = Arc::new(SharedState {
            engine: Mutex::new(None),
            config: Mutex::new(config),
            current_model_id: Mutex::new(None),
            last_activity: AtomicU64::new(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            ),
            is_loading: Mutex::new(false),
            loading_condvar: Condvar::new(),
            last_load_failure: Mutex::new(None),
        });

        let shutdown_signal = Arc::new(AtomicBool::new(false));

        {
            let shared_clone = shared.clone();
            let shutdown_signal_clone = shutdown_signal.clone();
            let handle = thread::spawn(move || {
                while !shutdown_signal_clone.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(10));

                    if shutdown_signal_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    let config = shared_clone.config.lock().unwrap();
                    let timeout = config.model_unload_timeout;
                    drop(config);

                    let timeout_seconds = timeout.to_seconds();

                    if let Some(limit_seconds) = timeout_seconds {
                        if limit_seconds == 0 {
                            continue; // Handled by maybe_unload_immediately()
                        }

                        let last = shared_clone.last_activity.load(Ordering::Relaxed);
                        let now_ms = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;

                        if now_ms.saturating_sub(last) > limit_seconds * 1000 {
                            let mut engine = shared_clone.engine.lock().unwrap();
                            if engine.is_some() {
                                debug!("Unloading model due to inactivity");
                                *engine = None;
                                drop(engine);
                                *shared_clone.current_model_id.lock().unwrap() = None;
                            }
                        }
                    }
                }
                debug!("Idle watcher thread shutting down");
            });

            let manager = Self {
                shared,
                model_manager,
                shutdown_signal,
                watcher_handle: Mutex::new(Some(handle)),
            };

            Ok(manager)
        }
    }

    pub fn is_model_loaded(&self) -> bool {
        let engine = self.shared.engine.lock().unwrap();
        engine.is_some()
    }

    /// Returns true if a model is selected in settings AND downloaded to disk.
    /// This is distinct from `is_model_loaded()` which checks if the engine is
    /// currently loaded in memory (it may have been unloaded by the idle timeout).
    pub fn has_model_selected(&self) -> bool {
        let selected = self.model_manager.get_current_model();
        if selected.is_empty() {
            return false;
        }
        self.model_manager
            .get_model_info(&selected)
            .map(|m| m.is_downloaded)
            .unwrap_or(false)
    }

    /// Refreshes model download status from filesystem and then checks if a model is selected.
    /// This should be used before critical operations (like recording) to ensure
    /// the daemon sees models that were downloaded by other processes (e.g., the UI).
    pub fn refresh_and_has_model_selected(&self) -> bool {
        if let Err(e) = self.model_manager.refresh_download_status() {
            warn!("Failed to refresh model download status: {}", e);
        }
        self.has_model_selected()
    }

    pub fn unload_model(&self) -> Result<()> {
        debug!("Unloading model");

        {
            let mut engine = self.shared.engine.lock().unwrap();
            if let Some(ref mut loaded_engine) = *engine {
                match loaded_engine {
                    LoadedEngine::Whisper(ref mut e) => e.unload_model(),
                    LoadedEngine::Parakeet(ref mut e) => e.unload_model(),
                    LoadedEngine::Moonshine(ref mut e) => e.unload_model(),
                    LoadedEngine::SenseVoice(ref mut e) => e.unload_model(),
                }
            }
            *engine = None;
        }
        {
            let mut current_model = self.shared.current_model_id.lock().unwrap();
            *current_model = None;
        }

        debug!("Model unloaded");
        Ok(())
    }

    pub fn maybe_unload_immediately(&self, context: &str) {
        let config = self.shared.config.lock().unwrap();
        if config.model_unload_timeout == ModelUnloadTimeout::Immediately && self.is_model_loaded()
        {
            info!("Immediately unloading model after {}", context);
            drop(config);
            let _ = self.unload_model();
        }
    }

    pub fn load_model(&self, model_id: &str) -> Result<()> {
        debug!("Loading model: {}", model_id);

        let model_info = self
            .model_manager
            .get_model_info(model_id)
            .ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        if !model_info.is_downloaded {
            return Err(anyhow::anyhow!("Model not downloaded"));
        }

        let model_path = self
            .model_manager
            .get_model_path(model_id)
            .ok_or_else(|| anyhow::anyhow!("Model path not found"))?;

        let loaded_engine = match model_info.engine_type {
            EngineType::Whisper => {
                let mut engine = WhisperEngine::new();
                engine
                    .load_model(&model_path)
                    .map_err(|e| anyhow::anyhow!("Failed to load Whisper model: {}", e))?;
                LoadedEngine::Whisper(engine)
            }
            EngineType::Parakeet => {
                let mut engine = ParakeetEngine::new();
                engine
                    .load_model_with_params(&model_path, ParakeetModelParams::int8())
                    .map_err(|e| anyhow::anyhow!("Failed to load Parakeet model: {}", e))?;
                LoadedEngine::Parakeet(engine)
            }
            EngineType::Moonshine => {
                let mut engine = MoonshineEngine::new();
                engine
                    .load_model_with_params(
                        &model_path,
                        MoonshineModelParams::variant(ModelVariant::Base),
                    )
                    .map_err(|e| anyhow::anyhow!("Failed to load Moonshine model: {}", e))?;
                LoadedEngine::Moonshine(engine)
            }
            EngineType::SenseVoice => {
                let mut engine = SenseVoiceEngine::new();
                engine
                    .load_model_with_params(&model_path, SenseVoiceModelParams::int8())
                    .map_err(|e| anyhow::anyhow!("Failed to load SenseVoice model: {}", e))?;
                LoadedEngine::SenseVoice(engine)
            }
        };

        {
            let mut engine = self.shared.engine.lock().unwrap();
            *engine = Some(loaded_engine);
        }
        {
            let mut current_model = self.shared.current_model_id.lock().unwrap();
            *current_model = Some(model_id.to_string());
        }

        info!("Model {} loaded successfully", model_id);
        Ok(())
    }

    pub fn initiate_model_load(&self) {
        if self.is_model_loaded() {
            return;
        }

        let selected_model = self.model_manager.get_current_model();
        if selected_model.is_empty() {
            warn!("No model selected");
            return;
        }

        if self.should_throttle_load_attempt(&selected_model) {
            debug!(
                "Skipping immediate retry for model {} due to recent load failure",
                selected_model
            );
            return;
        }

        let mut is_loading = self.shared.is_loading.lock().unwrap();
        if *is_loading {
            return;
        }
        *is_loading = true;
        let shared = self.shared.clone();
        let model_manager = self.model_manager.clone();
        drop(is_loading);

        thread::spawn(move || {
            let model_info = model_manager.get_model_info(&selected_model);
            if model_info.is_none() || !model_info.as_ref().unwrap().is_downloaded {
                let message = format!("Model not found or not downloaded: {}", selected_model);
                error!("{}", message);
                Self::set_load_failure(&shared, &selected_model, message);
                let mut is_loading = shared.is_loading.lock().unwrap();
                *is_loading = false;
                shared.loading_condvar.notify_all();
                return;
            }

            let model_path = model_manager.get_model_path(&selected_model);
            if model_path.is_none() {
                let message = format!("Model path not found: {}", selected_model);
                error!("{}", message);
                Self::set_load_failure(&shared, &selected_model, message);
                let mut is_loading = shared.is_loading.lock().unwrap();
                *is_loading = false;
                shared.loading_condvar.notify_all();
                return;
            }

            let model_path = model_path.unwrap();
            let model_info = model_info.unwrap();

            let load_result: Result<LoadedEngine> = match model_info.engine_type {
                EngineType::Whisper => {
                    let mut engine = WhisperEngine::new();
                    if let Err(e) = engine.load_model(&model_path) {
                        Err(anyhow::anyhow!("Failed to load Whisper model: {}", e))
                    } else {
                        Ok(LoadedEngine::Whisper(engine))
                    }
                }
                EngineType::Parakeet => {
                    let mut engine = ParakeetEngine::new();
                    if let Err(e) =
                        engine.load_model_with_params(&model_path, ParakeetModelParams::int8())
                    {
                        Err(anyhow::anyhow!("Failed to load Parakeet model: {}", e))
                    } else {
                        Ok(LoadedEngine::Parakeet(engine))
                    }
                }
                EngineType::Moonshine => {
                    let mut engine = MoonshineEngine::new();
                    if let Err(e) = engine.load_model_with_params(
                        &model_path,
                        MoonshineModelParams::variant(ModelVariant::Base),
                    ) {
                        Err(anyhow::anyhow!("Failed to load Moonshine model: {}", e))
                    } else {
                        Ok(LoadedEngine::Moonshine(engine))
                    }
                }
                EngineType::SenseVoice => {
                    let mut engine = SenseVoiceEngine::new();
                    if let Err(e) =
                        engine.load_model_with_params(&model_path, SenseVoiceModelParams::int8())
                    {
                        Err(anyhow::anyhow!("Failed to load SenseVoice model: {}", e))
                    } else {
                        Ok(LoadedEngine::SenseVoice(engine))
                    }
                }
            };

            match load_result {
                Ok(loaded_engine) => {
                    *shared.engine.lock().unwrap() = Some(loaded_engine);
                    *shared.current_model_id.lock().unwrap() = Some(selected_model.clone());
                    Self::clear_load_failure(&shared, &selected_model);
                    info!("Model {} loaded successfully", selected_model);
                }
                Err(e) => {
                    error!("{}", e);
                    Self::set_load_failure(&shared, &selected_model, e.to_string());
                }
            }

            let mut is_loading = shared.is_loading.lock().unwrap();
            *is_loading = false;
            shared.loading_condvar.notify_all();
        });
    }

    fn transcribe_internal(
        &self,
        samples: Vec<f32>,
        allow_immediate_unload: bool,
    ) -> Result<String> {
        self.update_activity();

        let model_id = {
            let current = self.shared.current_model_id.lock().unwrap();
            current.clone()
        };

        if model_id.is_none() || !self.is_model_loaded() {
            self.initiate_model_load();

            let mut is_loading = self.shared.is_loading.lock().unwrap();
            while *is_loading {
                is_loading = self.shared.loading_condvar.wait(is_loading).unwrap();
            }
        }

        let mut engine = self.shared.engine.lock().unwrap();
        if engine.is_none() {
            drop(engine);
            if let Some(message) = self.selected_model_failure_message() {
                return Err(anyhow::anyhow!("No engine loaded: {}", message));
            }
            return Err(anyhow::anyhow!("No engine loaded"));
        }
        let loaded_engine = engine.as_mut().unwrap();

        let (language, translate, custom_words, threshold) = {
            let config = self.shared.config.lock().unwrap();
            (
                config.selected_language.clone(),
                config.translate_to_english,
                config.custom_words.clone(),
                config.word_correction_threshold,
            )
        };

        let result = match loaded_engine {
            LoadedEngine::Whisper(e) => {
                let mut params = WhisperInferenceParams::default();
                if language != "auto" {
                    params.language = Some(language.clone());
                }
                params.translate = translate;
                e.transcribe_samples(samples.clone(), Some(params))
                    .map_err(|e| anyhow::anyhow!("Whisper transcription failed: {}", e))
            }
            LoadedEngine::Parakeet(e) => e
                .transcribe_samples(samples.clone(), None)
                .map_err(|e| anyhow::anyhow!("Parakeet transcription failed: {}", e)),
            LoadedEngine::Moonshine(e) => e
                .transcribe_samples(samples.clone(), None)
                .map_err(|e| anyhow::anyhow!("Moonshine transcription failed: {}", e)),
            LoadedEngine::SenseVoice(e) => e
                .transcribe_samples(samples, None)
                .map_err(|e| anyhow::anyhow!("SenseVoice transcription failed: {}", e)),
        };

        drop(engine);

        let transcription_result = result?;
        let mut text = transcription_result.text;

        if !custom_words.is_empty() {
            text = apply_custom_words(&text, &custom_words, threshold);
        }

        text = filter_transcription_output(&text);

        if allow_immediate_unload {
            self.maybe_unload_immediately("transcription");
        }

        Ok(text)
    }

    pub fn transcribe(&self, samples: Vec<f32>) -> Result<String> {
        self.transcribe_internal(samples, true)
    }

    pub fn transcribe_for_live(&self, samples: Vec<f32>) -> Result<String> {
        self.transcribe_internal(samples, false)
    }

    pub fn refresh_config_from_settings(&self, settings: &Settings) {
        let updated = TranscriptionConfig::from_settings(settings);
        let mut config = self.shared.config.lock().unwrap();
        *config = updated;
    }

    pub fn get_model_load_status(&self) -> (bool, bool, Option<String>) {
        let is_loading = *self.shared.is_loading.lock().unwrap();
        let is_loaded = self.is_model_loaded();
        let current_model = self.shared.current_model_id.lock().unwrap().clone();
        (is_loading, is_loaded, current_model)
    }

    fn update_activity(&self) {
        let now = Self::now_ms();
        self.shared.last_activity.store(now, Ordering::Relaxed);
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn set_load_failure(shared: &Arc<SharedState>, model_id: &str, message: String) {
        let mut failure = shared.last_load_failure.lock().unwrap();
        *failure = Some(ModelLoadFailure {
            model_id: model_id.to_string(),
            message,
            at_ms: Self::now_ms(),
        });
    }

    fn clear_load_failure(shared: &Arc<SharedState>, model_id: &str) {
        let mut failure = shared.last_load_failure.lock().unwrap();
        if failure
            .as_ref()
            .map(|f| f.model_id == model_id)
            .unwrap_or(false)
        {
            *failure = None;
        }
    }

    fn should_throttle_load_attempt(&self, model_id: &str) -> bool {
        let failure = self.shared.last_load_failure.lock().unwrap();
        if let Some(failure) = failure.as_ref() {
            if failure.model_id != model_id {
                return false;
            }
            let elapsed = Self::now_ms().saturating_sub(failure.at_ms);
            return elapsed < LOAD_RETRY_COOLDOWN_MS;
        }
        false
    }

    fn selected_model_failure_message(&self) -> Option<String> {
        let selected_model = self.model_manager.get_current_model();
        if selected_model.is_empty() {
            return None;
        }

        let failure = self.shared.last_load_failure.lock().unwrap();
        failure.as_ref().and_then(|failure| {
            if failure.model_id == selected_model {
                Some(format!(
                    "failed to load model {}: {}",
                    selected_model, failure.message
                ))
            } else {
                None
            }
        })
    }
}

impl Drop for TranscriptionManager {
    fn drop(&mut self) {
        self.shutdown_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = self.watcher_handle.lock().unwrap().take() {
            if let Err(err) = handle.join() {
                warn!("Transcription watcher thread join failed: {:?}", err);
            }
        }
    }
}
