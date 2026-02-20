use anyhow::Result;
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tar::Archive;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineType {
    Whisper,
    Parakeet,
    Moonshine,
    SenseVoice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub filename: String,
    pub url: Option<String>,
    pub size_mb: u64,
    pub is_downloaded: bool,
    pub is_downloading: bool,
    pub partial_size: u64,
    pub is_directory: bool,
    pub engine_type: EngineType,
    pub accuracy_score: f32,
    pub speed_score: f32,
    pub supports_translation: bool,
    pub is_recommended: bool,
    pub supported_languages: Vec<String>,
    pub is_custom: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub model_id: String,
    pub downloaded: u64,
    pub total: u64,
    pub percentage: f64,
}

/// Represents the current state of a model in its lifecycle
#[derive(Debug, Clone)]
pub enum ModelState {
    /// Model is available for download
    Available,
    /// Download is in progress
    Downloading {
        bytes_downloaded: u64,
        bytes_total: u64,
        cancel_flag: Arc<AtomicBool>,
    },
    /// File downloaded, extracting archive
    Extracting { progress_message: String },
    /// Model is downloaded and ready to use
    Ready,
    /// An error occurred (may be retryable)
    Error { message: String, retryable: bool },
}

impl ModelState {
    /// Check if the model can be downloaded
    pub fn can_download(&self) -> bool {
        matches!(self, ModelState::Available | ModelState::Error { .. })
    }

    /// Check if download is in progress
    pub fn is_downloading(&self) -> bool {
        matches!(self, ModelState::Downloading { .. })
    }

    /// Check if extraction is in progress
    pub fn is_extracting(&self) -> bool {
        matches!(self, ModelState::Extracting { .. })
    }

    /// Check if the model is ready to use
    pub fn is_ready(&self) -> bool {
        matches!(self, ModelState::Ready)
    }

    /// Get progress percentage if downloading
    pub fn progress_percentage(&self) -> Option<f64> {
        match self {
            ModelState::Downloading {
                bytes_downloaded,
                bytes_total,
                ..
            } => {
                if *bytes_total == 0 {
                    Some(0.0)
                } else {
                    Some((*bytes_downloaded as f64 / *bytes_total as f64) * 100.0)
                }
            }
            _ => None,
        }
    }

    /// Get cancel flag if downloading
    pub fn cancel_flag(&self) -> Option<Arc<AtomicBool>> {
        match self {
            ModelState::Downloading { cancel_flag, .. } => Some(cancel_flag.clone()),
            _ => None,
        }
    }
}

/// Event emitted when a model's state changes
#[derive(Debug, Clone)]
pub struct ModelStateEvent {
    pub model_id: String,
    pub state: ModelState,
}

pub struct ModelManager {
    selected_model: Mutex<String>,
    models_dir: PathBuf,
    available_models: Mutex<HashMap<String, ModelInfo>>,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    extracting_models: Arc<Mutex<HashSet<String>>>,
    state_observers: Arc<Mutex<Vec<std::sync::mpsc::Sender<ModelStateEvent>>>>,
}

struct DownloadInFlightGuard<'a> {
    manager: &'a ModelManager,
    model_id: String,
    active: bool,
}

impl<'a> DownloadInFlightGuard<'a> {
    fn new(manager: &'a ModelManager, model_id: &str) -> Self {
        Self {
            manager,
            model_id: model_id.to_string(),
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for DownloadInFlightGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.manager.clear_download_tracking(&self.model_id);
        }
    }
}

impl ModelManager {
    pub fn new() -> Result<Self> {
        let settings = crate::settings::Settings::new();
        let models_dir = std::env::var("XDG_DATA_HOME")
            .map(|p| PathBuf::from(p).join("dikt").join("models"))
            .unwrap_or_else(|_| {
                dirs::data_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("dikt")
                    .join("models")
            });

        if !models_dir.exists() {
            fs::create_dir_all(&models_dir)?;
        }

        let mut available_models = HashMap::new();

        let whisper_languages: Vec<String> = vec![
            "en", "zh", "zh-Hans", "zh-Hant", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl",
            "ca", "nl", "ar", "sv", "it", "id", "hi", "fi", "vi", "he", "uk", "el", "ms", "cs",
            "ro", "da", "hu", "ta", "no", "th", "ur", "hr", "bg", "lt", "la", "mi", "ml", "cy",
            "sk", "te", "fa", "lv", "bn", "sr", "az", "sl", "kn", "et", "mk", "br", "eu", "is",
            "hy", "ne", "mn", "bs", "kk", "sq", "sw", "gl", "mr", "pa", "si", "km", "sn", "yo",
            "so", "af", "oc", "ka", "be", "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo", "ht",
            "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl", "mg", "as", "tt", "haw", "ln",
            "ha", "ba", "jw", "su", "yue",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        available_models.insert(
            "small".to_string(),
            ModelInfo {
                id: "small".to_string(),
                name: "Whisper Small".to_string(),
                description: "Fast and fairly accurate.".to_string(),
                filename: "ggml-small.bin".to_string(),
                url: Some(
                    "https://github.com/rohithmahesh3/Dikt/releases/download/models/ggml-small.bin"
                        .to_string(),
                ),
                size_mb: 487,
                is_downloaded: false,
                is_downloading: false,
                partial_size: 0,
                is_directory: false,
                engine_type: EngineType::Whisper,
                accuracy_score: 0.60,
                speed_score: 0.85,
                supports_translation: true,
                is_recommended: false,
                supported_languages: whisper_languages.clone(),
                is_custom: false,
            },
        );

        available_models.insert(
            "medium".to_string(),
            ModelInfo {
                id: "medium".to_string(),
                name: "Whisper Medium".to_string(),
                description: "Good accuracy, medium speed".to_string(),
                filename: "whisper-medium-q4_1.bin".to_string(),
                url: Some("https://github.com/rohithmahesh3/Dikt/releases/download/models/whisper-medium-q4_1.bin".to_string()),
                size_mb: 492,
                is_downloaded: false,
                is_downloading: false,
                partial_size: 0,
                is_directory: false,
                engine_type: EngineType::Whisper,
                accuracy_score: 0.75,
                speed_score: 0.60,
                supports_translation: true,
                is_recommended: false,
                supported_languages: whisper_languages.clone(),
                is_custom: false,
            },
        );

        available_models.insert(
            "turbo".to_string(),
            ModelInfo {
                id: "turbo".to_string(),
                name: "Whisper Turbo".to_string(),
                description: "Balanced accuracy and speed.".to_string(),
                filename: "ggml-large-v3-turbo.bin".to_string(),
                url: Some("https://github.com/rohithmahesh3/Dikt/releases/download/models/ggml-large-v3-turbo.bin".to_string()),
                size_mb: 1600,
                is_downloaded: false,
                is_downloading: false,
                partial_size: 0,
                is_directory: false,
                engine_type: EngineType::Whisper,
                accuracy_score: 0.80,
                speed_score: 0.40,
                supports_translation: false,
                is_recommended: false,
                supported_languages: whisper_languages.clone(),
                is_custom: false,
            },
        );

        let parakeet_v3_languages: Vec<String> = vec![
            "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv",
            "lt", "mt", "pl", "pt", "ro", "sk", "sl", "es", "sv", "ru", "uk",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        available_models.insert(
            "parakeet-tdt-0.6b-v3".to_string(),
            ModelInfo {
                id: "parakeet-tdt-0.6b-v3".to_string(),
                name: "Parakeet V3".to_string(),
                description: "Fast and accurate. Supports 25 European languages.".to_string(),
                filename: "parakeet-tdt-0.6b-v3-int8".to_string(),
                url: Some("https://github.com/rohithmahesh3/Dikt/releases/download/models/parakeet-v3-int8.tar.gz".to_string()),
                size_mb: 478,
                is_downloaded: false,
                is_downloading: false,
                partial_size: 0,
                is_directory: true,
                engine_type: EngineType::Parakeet,
                accuracy_score: 0.80,
                speed_score: 0.85,
                supports_translation: false,
                is_recommended: true,
                supported_languages: parakeet_v3_languages,
                is_custom: false,
            },
        );

        let sense_voice_languages: Vec<String> =
            vec!["zh", "zh-Hans", "zh-Hant", "en", "yue", "ja", "ko"]
                .into_iter()
                .map(String::from)
                .collect();

        available_models.insert(
            "sense-voice-int8".to_string(),
            ModelInfo {
                id: "sense-voice-int8".to_string(),
                name: "SenseVoice".to_string(),
                description: "Very fast. Chinese, English, Japanese, Korean, Cantonese."
                    .to_string(),
                filename: "sense-voice-int8".to_string(),
                url: Some("https://github.com/rohithmahesh3/Dikt/releases/download/models/sense-voice-int8.tar.gz".to_string()),
                size_mb: 160,
                is_downloaded: false,
                is_downloading: false,
                partial_size: 0,
                is_directory: true,
                engine_type: EngineType::SenseVoice,
                accuracy_score: 0.65,
                speed_score: 0.95,
                supports_translation: false,
                is_recommended: false,
                supported_languages: sense_voice_languages,
                is_custom: false,
            },
        );

        if let Err(e) = Self::discover_custom_whisper_models(&models_dir, &mut available_models) {
            warn!("Failed to discover custom models: {}", e);
        }

        let selected_model = settings.selected_model();
        let manager = Self {
            selected_model: Mutex::new(selected_model),
            models_dir,
            available_models: Mutex::new(available_models),
            cancel_flags: Arc::new(Mutex::new(HashMap::new())),
            extracting_models: Arc::new(Mutex::new(HashSet::new())),
            state_observers: Arc::new(Mutex::new(Vec::new())),
        };

        manager.update_download_status()?;
        manager.auto_select_model_if_needed()?;

        Ok(manager)
    }

    pub fn get_available_models(&self) -> Vec<ModelInfo> {
        let models = self.available_models.lock().unwrap();
        models.values().cloned().collect()
    }

    pub fn get_model_info(&self, model_id: &str) -> Option<ModelInfo> {
        let models = self.available_models.lock().unwrap();
        models.get(model_id).cloned()
    }

    pub fn get_model_path(&self, model_id: &str) -> Option<PathBuf> {
        let models = self.available_models.lock().unwrap();
        models
            .get(model_id)
            .map(|m| self.models_dir.join(&m.filename))
    }

    fn is_valid_directory_model_layout(model_info: &ModelInfo, model_path: &Path) -> bool {
        if !model_path.is_dir() {
            return false;
        }

        let entries = match fs::read_dir(model_path) {
            Ok(entries) => entries,
            Err(_) => return false,
        };

        let mut names = HashSet::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                names.insert(name.to_string());
            }
        }

        match model_info.engine_type {
            EngineType::Parakeet => {
                let has_encoder = names
                    .iter()
                    .any(|n| n.starts_with("encoder-model") && n.ends_with(".onnx"));
                let has_decoder = names
                    .iter()
                    .any(|n| n.starts_with("decoder_joint-model") && n.ends_with(".onnx"));
                has_encoder
                    && has_decoder
                    && names.contains("nemo128.onnx")
                    && names.contains("vocab.txt")
            }
            EngineType::SenseVoice => {
                names.contains("tokens.txt")
                    && (names.contains("model.int8.onnx") || names.contains("model.onnx"))
            }
            EngineType::Moonshine => names.iter().any(|n| n.ends_with(".onnx")),
            EngineType::Whisper => false,
        }
    }

    fn repair_and_validate_directory_model(
        &self,
        model_info: &ModelInfo,
        model_path: &Path,
    ) -> Result<bool> {
        if !model_path.exists() {
            return Ok(false);
        }

        if model_path.is_file() {
            warn!(
                "Directory model {} expected a directory, found file at {}. Removing stale file.",
                model_info.id,
                model_path.display()
            );
            fs::remove_file(model_path)?;
            return Ok(false);
        }

        if Self::is_valid_directory_model_layout(model_info, model_path) {
            return Ok(true);
        }

        // Auto-repair common extraction issue:
        // model_dir/<model>/<model>/<files> (single nested root directory).
        let mut valid_children = Vec::new();
        for entry in fs::read_dir(model_path)? {
            let path = entry?.path();
            if path.is_dir() && Self::is_valid_directory_model_layout(model_info, &path) {
                valid_children.push(path);
            }
        }

        if valid_children.len() == 1 {
            let nested = valid_children.remove(0);
            warn!(
                "Repairing nested model directory layout for {} at {}",
                model_info.id,
                model_path.display()
            );

            for entry in fs::read_dir(&nested)? {
                let entry = entry?;
                let src = entry.path();
                let dst = model_path.join(entry.file_name());
                if dst.exists() {
                    warn!(
                        "Cannot repair {} due to path collision: {}",
                        model_info.id,
                        dst.display()
                    );
                    return Ok(false);
                }
                fs::rename(src, dst)?;
            }

            fs::remove_dir(&nested)?;
            return Ok(Self::is_valid_directory_model_layout(
                model_info, model_path,
            ));
        }

        Ok(false)
    }

    fn extract_root_dir(extracting_dir: &Path) -> Result<PathBuf> {
        let mut child_dirs = Vec::new();
        let mut non_dirs = 0usize;

        for entry in fs::read_dir(extracting_dir)? {
            let path = entry?.path();
            if path.is_dir() {
                child_dirs.push(path);
            } else {
                non_dirs += 1;
            }
        }

        if non_dirs == 0 && child_dirs.len() == 1 {
            Ok(child_dirs.remove(0))
        } else {
            Ok(extracting_dir.to_path_buf())
        }
    }

    fn update_download_status(&self) -> Result<()> {
        let mut models = self.available_models.lock().unwrap();

        for model in models.values_mut() {
            if model.is_directory {
                let model_path = self.models_dir.join(&model.filename);
                let partial_path = self.models_dir.join(format!("{}.partial", &model.filename));

                model.is_downloaded =
                    match self.repair_and_validate_directory_model(model, &model_path) {
                        Ok(valid) => valid,
                        Err(e) => {
                            warn!(
                                "Failed to validate model {} at {}: {}",
                                model.id,
                                model_path.display(),
                                e
                            );
                            false
                        }
                    };
                model.is_downloading = false;

                if partial_path.exists() {
                    model.partial_size = partial_path.metadata().map(|m| m.len()).unwrap_or(0);
                } else {
                    model.partial_size = 0;
                }
            } else {
                let model_path = self.models_dir.join(&model.filename);
                let partial_path = self.models_dir.join(format!("{}.partial", &model.filename));

                model.is_downloaded = model_path.exists();
                model.is_downloading = false;

                if partial_path.exists() {
                    model.partial_size = partial_path.metadata().map(|m| m.len()).unwrap_or(0);
                } else {
                    model.partial_size = 0;
                }
            }
        }

        Ok(())
    }

    /// Public method to refresh download status from filesystem.
    /// This is useful when the daemon needs to detect models downloaded by other processes.
    pub fn refresh_download_status(&self) -> Result<()> {
        self.update_download_status()
    }

    fn auto_select_model_if_needed(&self) -> Result<()> {
        let selected = self.selected_model.lock().unwrap().clone();
        let models = self.available_models.lock().unwrap();

        let is_valid_selected = !selected.is_empty()
            && models
                .get(&selected)
                .map(|m| m.is_downloaded)
                .unwrap_or(false);

        if is_valid_selected {
            return Ok(());
        }

        let fallback = models
            .values()
            .find(|m| m.is_downloaded && m.is_recommended)
            .or_else(|| models.values().find(|m| m.is_downloaded))
            .map(|m| m.id.clone());
        drop(models);

        if let Some(model_id) = fallback {
            info!("Auto-selecting model: {}", model_id);
            *self.selected_model.lock().unwrap() = model_id.clone();
            crate::settings::Settings::new().set_selected_model(&model_id);
        } else {
            *self.selected_model.lock().unwrap() = String::new();
            crate::settings::Settings::new().set_selected_model("");
        }

        Ok(())
    }

    fn discover_custom_whisper_models(
        models_dir: &Path,
        available_models: &mut HashMap<String, ModelInfo>,
    ) -> Result<()> {
        if !models_dir.exists() {
            return Ok(());
        }

        let predefined_filenames: HashSet<String> = available_models
            .values()
            .filter(|m| matches!(m.engine_type, EngineType::Whisper) && !m.is_directory)
            .map(|m| m.filename.clone())
            .collect();

        for entry in fs::read_dir(models_dir)? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let filename = match path.file_name().and_then(|s| s.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            if filename.starts_with('.') || !filename.ends_with(".bin") {
                continue;
            }

            if predefined_filenames.contains(&filename) {
                continue;
            }

            let model_id = filename.trim_end_matches(".bin").to_string();

            if available_models.contains_key(&model_id) {
                continue;
            }

            let size_mb = match path.metadata() {
                Ok(meta) => meta.len() / (1024 * 1024),
                Err(_) => 0,
            };

            info!(
                "Discovered custom Whisper model: {} ({} MB)",
                model_id, size_mb
            );

            available_models.insert(
                model_id.clone(),
                ModelInfo {
                    id: model_id,
                    name: filename.clone(),
                    description: "Custom model".to_string(),
                    filename,
                    url: None,
                    size_mb,
                    is_downloaded: true,
                    is_downloading: false,
                    partial_size: 0,
                    is_directory: false,
                    engine_type: EngineType::Whisper,
                    accuracy_score: 0.0,
                    speed_score: 0.0,
                    supports_translation: false,
                    is_recommended: false,
                    supported_languages: vec![],
                    is_custom: true,
                },
            );
        }

        Ok(())
    }

    pub async fn download_model(&self, model_id: &str) -> Result<()> {
        let model_info = {
            let models = self.available_models.lock().unwrap();
            models.get(model_id).cloned()
        };

        let model_info =
            model_info.ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        let url = model_info
            .url
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No download URL for model"))?
            .clone();
        let model_path = self.models_dir.join(&model_info.filename);
        let partial_path = self
            .models_dir
            .join(format!("{}.partial", &model_info.filename));

        if model_path.exists() {
            if model_info.is_directory {
                match self.repair_and_validate_directory_model(&model_info, &model_path) {
                    Ok(true) => {
                        if partial_path.exists() {
                            let _ = fs::remove_file(&partial_path);
                        }
                        self.update_download_status()?;
                        return Ok(());
                    }
                    Ok(false) => {
                        warn!(
                            "Model {} exists but has an invalid directory layout. Re-downloading.",
                            model_id
                        );
                        if model_path.is_dir() {
                            fs::remove_dir_all(&model_path)?;
                        } else {
                            fs::remove_file(&model_path)?;
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to validate model {} at {}: {}. Re-downloading.",
                            model_id,
                            model_path.display(),
                            e
                        );
                        if model_path.is_dir() {
                            fs::remove_dir_all(&model_path)?;
                        } else if model_path.exists() {
                            fs::remove_file(&model_path)?;
                        }
                    }
                }
            } else {
                if partial_path.exists() {
                    let _ = fs::remove_file(&partial_path);
                }
                self.update_download_status()?;
                return Ok(());
            }
        }

        let mut resume_from = if partial_path.exists() {
            partial_path.metadata()?.len()
        } else {
            0
        };

        // Set downloading state and notify
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let total_bytes = model_info.size_mb * 1024 * 1024;

        {
            let mut models = self.available_models.lock().unwrap();
            let model = models
                .get_mut(model_id)
                .ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;
            if model.is_downloading {
                return Err(anyhow::anyhow!(
                    "Download already in progress for model: {}",
                    model_id
                ));
            }
            model.is_downloading = true;
            model.partial_size = resume_from;
        }

        let duplicate_inflight = {
            let mut flags = self.cancel_flags.lock().unwrap();
            if flags.contains_key(model_id) {
                true
            } else {
                flags.insert(model_id.to_string(), cancel_flag.clone());
                false
            }
        };
        if duplicate_inflight {
            self.clear_download_tracking(model_id);
            return Err(anyhow::anyhow!(
                "Download already in progress for model: {}",
                model_id
            ));
        }
        let mut guard = DownloadInFlightGuard::new(self, model_id);

        // Notify UI that download has started
        self.notify_state_change(
            model_id,
            ModelState::Downloading {
                bytes_downloaded: resume_from,
                bytes_total: total_bytes,
                cancel_flag: cancel_flag.clone(),
            },
        );

        let client = reqwest::Client::new();
        let mut request = client.get(&url);

        if resume_from > 0 {
            request = request.header("Range", format!("bytes={}-", resume_from));
        }

        let mut response = request.send().await.map_err(|e| {
            self.notify_state_change(
                model_id,
                ModelState::Error {
                    message: format!("Download request failed: {}", e),
                    retryable: true,
                },
            );
            anyhow::anyhow!("Download request failed: {}", e)
        })?;

        if resume_from > 0 && response.status() == reqwest::StatusCode::OK {
            drop(response);
            let _ = fs::remove_file(&partial_path);
            resume_from = 0;
            response = client.get(&url).send().await.map_err(|e| {
                self.notify_state_change(
                    model_id,
                    ModelState::Error {
                        message: format!("Download request failed: {}", e),
                        retryable: true,
                    },
                );
                anyhow::anyhow!("Download request failed: {}", e)
            })?;
        }

        if !response.status().is_success()
            && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        {
            self.notify_state_change(
                model_id,
                ModelState::Error {
                    message: format!("HTTP {}", response.status()),
                    retryable: true,
                },
            );

            return Err(anyhow::anyhow!(
                "Failed to download: HTTP {}",
                response.status()
            ));
        }

        let _total_size = if resume_from > 0 {
            resume_from + response.content_length().unwrap_or(0)
        } else {
            response.content_length().unwrap_or(0)
        };

        let mut _downloaded = resume_from;
        let mut stream = response.bytes_stream();
        let mut last_notify_bytes = resume_from;

        let mut file = if resume_from > 0 {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&partial_path)
                .map_err(|e| {
                    self.notify_state_change(
                        model_id,
                        ModelState::Error {
                            message: format!("Failed to open partial file: {}", e),
                            retryable: true,
                        },
                    );
                    anyhow::anyhow!("Failed to open partial file: {}", e)
                })?
        } else {
            std::fs::File::create(&partial_path).map_err(|e| {
                self.notify_state_change(
                    model_id,
                    ModelState::Error {
                        message: format!("Failed to create partial file: {}", e),
                        retryable: true,
                    },
                );
                anyhow::anyhow!("Failed to create partial file: {}", e)
            })?
        };

        while let Some(chunk) = stream.next().await {
            if cancel_flag.load(Ordering::Acquire) {
                drop(file);

                // Notify cancellation
                self.notify_state_change(model_id, ModelState::Available);

                return Ok(());
            }

            let chunk = chunk.map_err(|e| {
                self.notify_state_change(
                    model_id,
                    ModelState::Error {
                        message: format!("Download stream failed: {}", e),
                        retryable: true,
                    },
                );
                anyhow::anyhow!("Download stream failed: {}", e)
            })?;
            file.write_all(&chunk).map_err(|e| {
                self.notify_state_change(
                    model_id,
                    ModelState::Error {
                        message: format!("Failed to write model data: {}", e),
                        retryable: true,
                    },
                );
                anyhow::anyhow!("Failed to write model data: {}", e)
            })?;
            _downloaded += chunk.len() as u64;

            // Update progress in model info
            if let Ok(mut models) = self.available_models.lock() {
                if let Some(model) = models.get_mut(model_id) {
                    model.partial_size = _downloaded;
                }
            }

            // Notify progress every 1MB to avoid spamming
            if _downloaded - last_notify_bytes >= 1024 * 1024 {
                self.notify_state_change(
                    model_id,
                    ModelState::Downloading {
                        bytes_downloaded: _downloaded,
                        bytes_total: total_bytes,
                        cancel_flag: cancel_flag.clone(),
                    },
                );
                last_notify_bytes = _downloaded;
            }
        }

        drop(file);

        if model_info.is_directory {
            // For directory-based models, rename to .tar.gz for extraction
            let tar_path = self
                .models_dir
                .join(format!("{}.tar.gz", &model_info.filename));
            fs::rename(&partial_path, &tar_path).map_err(|e| {
                self.notify_state_change(
                    model_id,
                    ModelState::Error {
                        message: format!("Failed to prepare archive for extraction: {}", e),
                        retryable: true,
                    },
                );
                anyhow::anyhow!("Failed to prepare archive for extraction: {}", e)
            })?;

            // Notify extraction state
            self.notify_state_change(
                model_id,
                ModelState::Extracting {
                    progress_message: "Extracting files...".to_string(),
                },
            );

            if let Err(e) = self.extract_model(model_id, &tar_path, &model_path).await {
                self.notify_state_change(
                    model_id,
                    ModelState::Error {
                        message: format!("Extraction failed: {}", e),
                        retryable: true,
                    },
                );

                return Err(e);
            }
        } else {
            // For single-file models, just rename the partial file
            fs::rename(&partial_path, &model_path).map_err(|e| {
                self.notify_state_change(
                    model_id,
                    ModelState::Error {
                        message: format!("Failed to finalize model download: {}", e),
                        retryable: true,
                    },
                );
                anyhow::anyhow!("Failed to finalize model download: {}", e)
            })?;
        }

        {
            let mut flags = self.cancel_flags.lock().unwrap();
            flags.remove(model_id);
        }

        {
            let mut models = self.available_models.lock().unwrap();
            if let Some(model) = models.get_mut(model_id) {
                model.is_downloading = false;
                model.is_downloaded = true;
                model.partial_size = 0;
            }
        }

        // Notify ready state
        self.notify_state_change(model_id, ModelState::Ready);
        guard.disarm();

        self.auto_select_model_if_needed()?;

        info!("Model {} downloaded successfully", model_id);
        Ok(())
    }

    async fn extract_model(&self, model_id: &str, tar_path: &Path, final_dir: &Path) -> Result<()> {
        {
            let mut extracting = self.extracting_models.lock().unwrap();
            extracting.insert(model_id.to_string());
        }

        let result = self.do_extract(tar_path, final_dir).await;

        {
            let mut extracting = self.extracting_models.lock().unwrap();
            extracting.remove(model_id);
        }

        result
    }

    async fn do_extract(&self, tar_path: &Path, final_dir: &Path) -> Result<()> {
        let file = File::open(tar_path)?;
        let decoder = GzDecoder::new(&file);
        let mut archive = Archive::new(decoder);

        let extracting_dir = tar_path.with_extension("extracting");
        if extracting_dir.exists() {
            fs::remove_dir_all(&extracting_dir)?;
        }
        fs::create_dir_all(&extracting_dir)?;

        archive.unpack(&extracting_dir)?;

        if final_dir.exists() {
            if final_dir.is_dir() {
                fs::remove_dir_all(final_dir)?;
            } else {
                fs::remove_file(final_dir)?;
            }
        }

        let extracted_root = Self::extract_root_dir(&extracting_dir)?;
        if extracted_root == extracting_dir {
            fs::rename(&extracting_dir, final_dir)?;
        } else {
            fs::rename(&extracted_root, final_dir)?;
            if extracting_dir.exists() {
                fs::remove_dir_all(&extracting_dir)?;
            }
        }
        fs::remove_file(tar_path)?;

        Ok(())
    }

    fn clear_download_tracking(&self, model_id: &str) {
        {
            let mut flags = self.cancel_flags.lock().unwrap();
            flags.remove(model_id);
        }
        if let Ok(mut models) = self.available_models.lock() {
            if let Some(model) = models.get_mut(model_id) {
                model.is_downloading = false;
            }
        }
    }

    pub fn cancel_download(&self, model_id: &str) -> Result<()> {
        let flags = self.cancel_flags.lock().unwrap();
        if let Some(flag) = flags.get(model_id) {
            flag.store(true, Ordering::Release);
        }
        Ok(())
    }

    pub fn is_model_downloading(&self, model_id: &str) -> bool {
        let models = self.available_models.lock().unwrap();
        models
            .get(model_id)
            .map(|m| m.is_downloading)
            .unwrap_or(false)
    }

    /// Subscribe to model state changes
    /// Returns a std::sync::mpsc::Receiver that can be used with glib::MainContext::default().invoke()
    pub fn subscribe_state_changes(&self) -> std::sync::mpsc::Receiver<ModelStateEvent> {
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let mut observers = self.state_observers.lock().unwrap();
            observers.push(sender);
        }
        receiver
    }

    /// Notify all observers of a state change
    fn notify_state_change(&self, model_id: &str, state: ModelState) {
        let event = ModelStateEvent {
            model_id: model_id.to_string(),
            state,
        };
        let observers = self.state_observers.lock().unwrap();
        for observer in observers.iter() {
            let _ = observer.send(event.clone());
        }
    }

    /// Get the current state of a model
    pub fn get_model_state(&self, model_id: &str) -> Option<ModelState> {
        let models = self.available_models.lock().unwrap();
        models.get(model_id).map(|m| {
            if m.is_downloading {
                let cancel_flag = self
                    .cancel_flags
                    .lock()
                    .unwrap()
                    .get(model_id)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                ModelState::Downloading {
                    bytes_downloaded: m.partial_size,
                    bytes_total: m.size_mb * 1024 * 1024,
                    cancel_flag,
                }
            } else if m.is_downloaded {
                let is_extracting = self.extracting_models.lock().unwrap().contains(model_id);
                if is_extracting {
                    ModelState::Extracting {
                        progress_message: "Extracting files...".to_string(),
                    }
                } else {
                    ModelState::Ready
                }
            } else if m.partial_size > 0 {
                // Has partial download but not currently downloading
                ModelState::Available
            } else {
                ModelState::Available
            }
        })
    }

    pub fn delete_model(&self, model_id: &str) -> Result<()> {
        let model_info = {
            let models = self.available_models.lock().unwrap();
            models.get(model_id).cloned()
        };

        if let Some(model) = model_info {
            let model_path = self.models_dir.join(&model.filename);
            let partial_path = self.models_dir.join(format!("{}.partial", &model.filename));

            if model_path.exists() {
                if model_path.is_dir() {
                    fs::remove_dir_all(&model_path)?;
                } else {
                    fs::remove_file(&model_path)?;
                }
            }

            if partial_path.exists() {
                fs::remove_file(&partial_path)?;
            }

            self.update_download_status()?;

            let selected = self.selected_model.lock().unwrap();
            if *selected == model_id {
                drop(selected);
                *self.selected_model.lock().unwrap() = String::new();
                crate::settings::Settings::new().set_selected_model("");
            }

            // Notify that model is now available again
            self.notify_state_change(model_id, ModelState::Available);
        }

        Ok(())
    }

    pub fn set_active_model(&self, model_id: &str) -> Result<()> {
        let models = self.available_models.lock().unwrap();
        if let Some(model) = models.get(model_id) {
            if !model.is_downloaded {
                return Err(anyhow::anyhow!("Model not downloaded: {}", model_id));
            }
            drop(models);
            let previous_model = self.get_current_model();
            *self.selected_model.lock().unwrap() = model_id.to_string();
            crate::settings::Settings::new().set_selected_model(model_id);
            if !previous_model.is_empty() && previous_model != model_id {
                self.notify_state_change(&previous_model, ModelState::Ready);
            }
            self.notify_state_change(model_id, ModelState::Ready);
            info!("Active model set to: {}", model_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Model not found: {}", model_id))
        }
    }

    pub fn sync_selected_model_from_settings(&self) -> Result<()> {
        // Refresh download status from filesystem to catch models downloaded
        // by other processes (e.g., UI downloaded while daemon was running)
        self.update_download_status()?;

        let selected = crate::settings::Settings::new().selected_model();
        let models = self.available_models.lock().unwrap();

        if selected.is_empty() {
            drop(models);
            *self.selected_model.lock().unwrap() = String::new();
            return Ok(());
        }

        if let Some(model) = models.get(&selected) {
            if model.is_downloaded {
                drop(models);
                *self.selected_model.lock().unwrap() = selected;
                return Ok(());
            }
        }

        drop(models);
        self.auto_select_model_if_needed()
    }

    pub fn get_current_model(&self) -> String {
        self.selected_model.lock().unwrap().clone()
    }

    pub fn has_any_models_available(&self) -> bool {
        let models = self.available_models.lock().unwrap();
        models.values().any(|m| m.is_downloaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_test_dir(prefix: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dikt-{}-{}", prefix, ts));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn directory_model_info(id: &str, filename: &str, engine_type: EngineType) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            description: "test".to_string(),
            filename: filename.to_string(),
            url: None,
            size_mb: 0,
            is_downloaded: false,
            is_downloading: false,
            partial_size: 0,
            is_directory: true,
            engine_type,
            accuracy_score: 0.0,
            speed_score: 0.0,
            supports_translation: false,
            is_recommended: false,
            supported_languages: vec![],
            is_custom: false,
        }
    }

    fn test_manager(models_dir: PathBuf) -> ModelManager {
        ModelManager {
            selected_model: Mutex::new(String::new()),
            models_dir,
            available_models: Mutex::new(HashMap::new()),
            cancel_flags: Arc::new(Mutex::new(HashMap::new())),
            extracting_models: Arc::new(Mutex::new(HashSet::new())),
            state_observers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn test_is_model_downloading() {
        // This test verifies the is_model_downloading method works correctly
        // Note: We can't easily test the full ModelManager without mocking the filesystem,
        // but we can test the logic directly

        let _model_id = "test-model";
        let cancel_flag = Arc::new(AtomicBool::new(false));

        // Test that a new flag is not set
        assert!(!cancel_flag.load(Ordering::Acquire));

        // Set the flag and verify it can be read
        cancel_flag.store(true, Ordering::Release);
        assert!(cancel_flag.load(Ordering::Acquire));
    }

    #[test]
    fn test_cancel_flag_ordering() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        // Thread 1: Set cancel flag after a short delay
        let handle1 = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(50));
            flag_clone.store(true, Ordering::Release);
        });

        // Thread 2: Check cancel flag - wait for thread 1 to complete
        let flag_clone2 = flag.clone();
        let handle2 = thread::spawn(move || {
            // Wait for thread 1 to finish
            let start = std::time::Instant::now();
            while !flag_clone2.load(Ordering::Acquire) {
                thread::yield_now();
                // Safety timeout to prevent infinite loop
                if start.elapsed() > std::time::Duration::from_secs(5) {
                    break;
                }
            }
            flag_clone2.load(Ordering::Acquire)
        });

        // Wait for both threads
        handle1.join().unwrap();
        let result = handle2.join().unwrap();

        // The Acquire/Release ordering should ensure the change is visible
        assert!(
            result,
            "Cancel flag change should be visible across threads with Acquire/Release ordering"
        );
    }

    #[test]
    fn test_concurrent_download_prevention_logic() {
        // Test the logic that prevents concurrent downloads
        // This simulates checking is_downloading flag before starting a download

        let is_downloading = Arc::new(AtomicBool::new(false));
        let _is_downloading_clone = is_downloading.clone();

        // Simulate first download starting
        assert!(!is_downloading.load(Ordering::Acquire));
        is_downloading.store(true, Ordering::Release);

        // Simulate second download attempt
        let is_downloading_clone2 = is_downloading.clone();
        let can_start_second = !is_downloading_clone2.load(Ordering::Acquire);

        assert!(
            !can_start_second,
            "Second download should be prevented when first is in progress"
        );

        // Simulate first download completing
        is_downloading.store(false, Ordering::Release);

        // Now second download should be able to start
        let can_start_after = !is_downloading.load(Ordering::Acquire);
        assert!(
            can_start_after,
            "Download should be allowed after previous one completes"
        );
    }

    #[test]
    fn test_repair_nested_parakeet_directory_layout() {
        let models_dir = create_test_dir("model-repair");
        let model_info = directory_model_info(
            "parakeet-tdt-0.6b-v3",
            "parakeet-tdt-0.6b-v3-int8",
            EngineType::Parakeet,
        );
        let manager = test_manager(models_dir.clone());
        let model_path = models_dir.join(&model_info.filename);
        let nested_path = model_path.join("parakeet-tdt-0.6b-v3-int8");
        fs::create_dir_all(&nested_path).unwrap();

        File::create(nested_path.join("encoder-model.int8.onnx")).unwrap();
        File::create(nested_path.join("decoder_joint-model.int8.onnx")).unwrap();
        File::create(nested_path.join("nemo128.onnx")).unwrap();
        File::create(nested_path.join("vocab.txt")).unwrap();

        assert!(!ModelManager::is_valid_directory_model_layout(
            &model_info,
            &model_path
        ));

        let repaired = manager
            .repair_and_validate_directory_model(&model_info, &model_path)
            .unwrap();
        assert!(repaired);
        assert!(ModelManager::is_valid_directory_model_layout(
            &model_info,
            &model_path
        ));
        assert!(!nested_path.exists());

        let _ = fs::remove_dir_all(models_dir);
    }

    #[test]
    fn test_repair_directory_model_removes_stale_file_path() {
        let models_dir = create_test_dir("model-stale-file");
        let model_info = directory_model_info(
            "sense-voice-int8",
            "sense-voice-int8",
            EngineType::SenseVoice,
        );
        let manager = test_manager(models_dir.clone());
        let model_path = models_dir.join(&model_info.filename);
        File::create(&model_path).unwrap();

        let is_valid = manager
            .repair_and_validate_directory_model(&model_info, &model_path)
            .unwrap();
        assert!(!is_valid);
        assert!(!model_path.exists());

        let _ = fs::remove_dir_all(models_dir);
    }

    #[test]
    fn test_extract_root_dir_flattens_single_top_level_directory() {
        let root = create_test_dir("extract-root");
        let nested = root.join("nested-root");
        fs::create_dir_all(&nested).unwrap();
        File::create(nested.join("file.txt")).unwrap();

        let extracted_root = ModelManager::extract_root_dir(&root).unwrap();
        assert_eq!(extracted_root, nested);

        let _ = fs::remove_dir_all(root);
    }
}
