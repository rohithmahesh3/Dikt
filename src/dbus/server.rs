//! D-Bus server for IBus integration
//!
//! This module provides a D-Bus interface that allows the dikt-ibus engine
//! to control Dikt's transcription functionality.

use crate::global_shortcuts::{
    toggle_diagnostics_tuple, toggle_diagnostics_verbose_json, toggle_recent_events,
};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{PostProcessProvider, Settings};
use crate::text_utils::convert_chinese_variant;
use crate::utils::logging::read_recent_logs;
use crate::{audio_feedback::play_feedback_sound, audio_feedback::SoundType};
use log::{debug, error, info, warn};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::fdo;
use zbus::object_server::SignalContext;
use zbus::Connection;

const DIKT_BUS_NAME: &str = "io.dikt.Transcription";
const DIKT_OBJECT_PATH: &str = "/io/dikt/Transcription";

const MAX_PENDING_COMMIT_QUEUE: usize = 32;
const LIVE_PREEDIT_POLL_MS: u64 = 600;
const LIVE_PREEDIT_MIN_NEW_SAMPLES: usize = 3200;
const LIVE_PREEDIT_MIN_TOTAL_SAMPLES: usize = 8000;
const LIVE_PREEDIT_MAX_WINDOW_SAMPLES: usize = 16000 * 8;
const LIVE_PREEDIT_SNAPSHOT_WARN_EVERY: u64 = 10;
const SESSION_TTL_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, Debug)]
struct PendingCommit {
    session_id: u64,
    claim_token: String,
    text: String,
    created_ms: u64,
}

struct PendingCommitStore {
    inner: Mutex<VecDeque<PendingCommit>>,
    dropped_count: AtomicU64,
}

impl Default for PendingCommitStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(MAX_PENDING_COMMIT_QUEUE)),
            dropped_count: AtomicU64::new(0),
        }
    }
}

impl PendingCommitStore {
    fn store(&self, session_id: u64, claim_token: String, text: String) {
        if let Ok(mut queue) = self.inner.lock() {
            if queue.len() >= MAX_PENDING_COMMIT_QUEUE {
                let _ = queue.pop_front();
                self.dropped_count.fetch_add(1, Ordering::SeqCst);
            }
            queue.push_back(PendingCommit {
                session_id,
                claim_token,
                text,
                created_ms: now_millis(),
            });
        }
    }

    fn take_for_session(&self, session_id: u64, claim_token: &str) -> (bool, String) {
        let Ok(mut queue) = self.inner.lock() else {
            return (false, String::new());
        };
        if let Some(index) = queue
            .iter()
            .position(|entry| entry.session_id == session_id && entry.claim_token == claim_token)
        {
            return queue
                .remove(index)
                .map(|pending| (true, pending.text))
                .unwrap_or_else(|| (false, String::new()));
        }
        (false, String::new())
    }

    fn stats_json(&self) -> String {
        let dropped_count = self.dropped_count.load(Ordering::SeqCst);
        if let Ok(queue) = self.inner.lock() {
            let now = now_millis();
            let oldest_age_ms = queue
                .front()
                .map(|entry| now.saturating_sub(entry.created_ms))
                .unwrap_or(0);
            let targets = queue
                .iter()
                .fold(HashMap::<u64, u64>::new(), |mut acc, item| {
                    *acc.entry(item.session_id).or_insert(0) += 1;
                    acc
                });
            json!({
                "queue_len": queue.len(),
                "oldest_age_ms": oldest_age_ms,
                "dropped_count": dropped_count,
                "targets": targets,
            })
            .to_string()
        } else {
            json!({
                "queue_len": 0,
                "oldest_age_ms": 0,
                "dropped_count": dropped_count,
                "targets": {},
                "error": "lock_poisoned",
            })
            .to_string()
        }
    }
}

#[derive(Clone, Debug)]
struct LivePreeditEntry {
    revision: u64,
    visible: bool,
    text: String,
}

struct LivePreeditStore {
    inner: Mutex<HashMap<u64, LivePreeditEntry>>,
}

impl Default for LivePreeditStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl LivePreeditStore {
    fn set(&self, session_id: u64, revision: u64, text: String) {
        let Ok(mut entries) = self.inner.lock() else {
            return;
        };

        if let Some(existing) = entries.get(&session_id) {
            if existing.revision >= revision {
                return;
            }
        }

        info!(
            "set_live_preedit: session={}, rev={}, text_len={}",
            session_id,
            revision,
            text.len()
        );
        entries.insert(
            session_id,
            LivePreeditEntry {
                revision,
                visible: true,
                text,
            },
        );
    }

    fn clear(&self, session_id: u64, revision: u64) {
        let Ok(mut entries) = self.inner.lock() else {
            return;
        };

        if let Some(existing) = entries.get(&session_id) {
            if existing.revision >= revision {
                return;
            }
        }

        info!(
            "clear_live_preedit: session={}, rev={}",
            session_id, revision
        );
        entries.insert(
            session_id,
            LivePreeditEntry {
                revision,
                visible: false,
                text: String::new(),
            },
        );
    }

    fn get_for_session(&self, session_id: u64) -> (u64, bool, String) {
        let Ok(entries) = self.inner.lock() else {
            return (0, false, String::new());
        };

        entries
            .get(&session_id)
            .map(|entry| (entry.revision, entry.visible, entry.text.clone()))
            .unwrap_or((0, false, String::new()))
    }
}

#[derive(Clone, Debug)]
struct SessionStatusEntry {
    state: String,
    message: String,
    updated_ms: u64,
}

impl SessionStatusEntry {
    fn new(state: &str, message: &str) -> Self {
        Self {
            state: state.to_string(),
            message: message.to_string(),
            updated_ms: now_millis(),
        }
    }
}

/// Shared state for the D-Bus server and handlers
pub struct DiktState {
    pub selected_language: Mutex<String>,
    pub recording_manager: Arc<AudioRecordingManager>,
    pub transcription_manager: Arc<TranscriptionManager>,
    pub is_recording: AtomicBool,
    stopping_sessions: Mutex<HashSet<u64>>,
    session_counter: AtomicU64,
    claim_counter: AtomicU64,
    pending_commit: PendingCommitStore,
    live_preedit: LivePreeditStore,
    live_preedit_revision: AtomicU64,
    focused_engine_id: AtomicU64,
    focused_engine_last_change_ms: AtomicU64,
    session_bindings: Mutex<HashMap<u64, u64>>,
    session_claim_tokens: Mutex<HashMap<u64, String>>,
    session_statuses: Mutex<HashMap<u64, SessionStatusEntry>>,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
}

impl DiktState {
    pub fn new(
        recording_manager: Arc<AudioRecordingManager>,
        transcription_manager: Arc<TranscriptionManager>,
        selected_language: String,
        log_buffer: Arc<Mutex<VecDeque<String>>>,
    ) -> Self {
        Self {
            selected_language: Mutex::new(selected_language),
            recording_manager,
            transcription_manager,
            is_recording: AtomicBool::new(false),
            stopping_sessions: Mutex::new(HashSet::new()),
            session_counter: AtomicU64::new(1),
            claim_counter: AtomicU64::new(1),
            pending_commit: PendingCommitStore::default(),
            live_preedit: LivePreeditStore::default(),
            live_preedit_revision: AtomicU64::new(1),
            focused_engine_id: AtomicU64::new(0),
            focused_engine_last_change_ms: AtomicU64::new(now_millis()),
            session_bindings: Mutex::new(HashMap::new()),
            session_claim_tokens: Mutex::new(HashMap::new()),
            session_statuses: Mutex::new(HashMap::new()),
            log_buffer,
        }
    }

    fn next_session_id(&self) -> u64 {
        self.session_counter.fetch_add(1, Ordering::SeqCst)
    }

    fn recent_logs(&self, limit: usize) -> Vec<String> {
        read_recent_logs(&self.log_buffer, limit)
    }

    fn next_claim_token(&self, session_id: u64) -> String {
        let claim_nonce = self.claim_counter.fetch_add(1, Ordering::SeqCst);
        format!(
            "{:016x}{:016x}{:016x}",
            now_millis(),
            session_id,
            claim_nonce
        )
    }

    fn create_session(&self, target_engine_id: u64) -> (u64, String) {
        let session_id = self.next_session_id();
        let claim_token = self.next_claim_token(session_id);
        if let Ok(mut bindings) = self.session_bindings.lock() {
            bindings.insert(session_id, target_engine_id);
        }
        if let Ok(mut claims) = self.session_claim_tokens.lock() {
            claims.insert(session_id, claim_token.clone());
        }
        self.set_session_status(session_id, "created", "Session created");
        (session_id, claim_token)
    }

    fn session_binding(&self, session_id: u64) -> Option<u64> {
        self.session_bindings
            .lock()
            .ok()
            .and_then(|bindings| bindings.get(&session_id).copied())
    }

    fn session_claim_token(&self, session_id: u64) -> Option<String> {
        self.session_claim_tokens
            .lock()
            .ok()
            .and_then(|claims| claims.get(&session_id).cloned())
    }

    fn validate_session_claim(&self, session_id: u64, claim_token: &str) -> bool {
        self.session_claim_tokens
            .lock()
            .ok()
            .and_then(|claims| claims.get(&session_id).cloned())
            .is_some_and(|token| token == claim_token)
    }

    fn set_session_status(&self, session_id: u64, state: &str, message: &str) {
        if session_id == 0 {
            return;
        }
        if let Ok(mut statuses) = self.session_statuses.lock() {
            statuses.insert(session_id, SessionStatusEntry::new(state, message));
        }
    }

    fn session_status(&self, session_id: u64) -> Option<SessionStatusEntry> {
        self.session_statuses
            .lock()
            .ok()
            .and_then(|statuses| statuses.get(&session_id).cloned())
    }

    fn remove_session(&self, session_id: u64) {
        if let Ok(mut bindings) = self.session_bindings.lock() {
            bindings.remove(&session_id);
        }
        if let Ok(mut claims) = self.session_claim_tokens.lock() {
            claims.remove(&session_id);
        }
        if let Ok(mut statuses) = self.session_statuses.lock() {
            statuses.remove(&session_id);
        }
        self.clear_session_stopping(session_id);
    }

    fn cleanup_expired_sessions(&self) {
        let now = now_millis();
        let mut expired = Vec::new();
        if let Ok(statuses) = self.session_statuses.lock() {
            for (session_id, status) in statuses.iter() {
                let is_terminal = matches!(
                    status.state.as_str(),
                    "ready" | "failed" | "cancelled" | "committed"
                );
                if is_terminal && now.saturating_sub(status.updated_ms) > SESSION_TTL_MS {
                    expired.push(*session_id);
                }
            }
        }
        for session_id in expired {
            self.remove_session(session_id);
        }
    }

    fn active_session_for_engine(&self, engine_id: u64) -> (u64, String, bool) {
        if engine_id == 0 {
            return (0, String::new(), false);
        }
        self.cleanup_expired_sessions();
        let Ok(bindings) = self.session_bindings.lock() else {
            return (0, String::new(), false);
        };
        let Ok(claims) = self.session_claim_tokens.lock() else {
            return (0, String::new(), false);
        };
        let Ok(statuses) = self.session_statuses.lock() else {
            return (0, String::new(), false);
        };

        let mut best: Option<(u8, u64, u64, String, bool)> = None;
        for (session_id, bound_engine_id) in bindings.iter() {
            if *bound_engine_id != engine_id {
                continue;
            }
            let Some(status) = statuses.get(session_id) else {
                continue;
            };
            let Some(claim_token) = claims.get(session_id) else {
                continue;
            };
            let (priority, allow_preedit) = match status.state.as_str() {
                "recording" => (3, true),
                "finalizing" => (2, false),
                "ready" => (1, false),
                _ => (0, false),
            };
            if priority == 0 {
                continue;
            }
            let candidate = (
                priority,
                status.updated_ms,
                *session_id,
                claim_token.clone(),
                allow_preedit,
            );
            if let Some(current) = &best {
                if candidate.0 > current.0
                    || (candidate.0 == current.0 && candidate.1 > current.1)
                    || (candidate.0 == current.0
                        && candidate.1 == current.1
                        && candidate.2 > current.2)
                {
                    best = Some(candidate);
                }
            } else {
                best = Some(candidate);
            }
        }

        if let Some((_, _, session_id, claim_token, allow_preedit)) = best {
            (session_id, claim_token, allow_preedit)
        } else {
            (0, String::new(), false)
        }
    }

    fn store_pending_commit(&self, session_id: u64, text: String) {
        let Some(claim_token) = self.session_claim_token(session_id) else {
            warn!(
                "Dropping pending commit for unknown session {} (no claim token)",
                session_id
            );
            return;
        };
        self.pending_commit.store(session_id, claim_token, text);
    }

    fn take_pending_commit_for_session(
        &self,
        session_id: u64,
        claim_token: &str,
    ) -> (bool, String) {
        let result = self
            .pending_commit
            .take_for_session(session_id, claim_token);
        if result.0 {
            self.set_session_status(session_id, "committed", "Final commit delivered");
        }
        result
    }

    fn pending_commit_stats_json(&self) -> String {
        self.pending_commit.stats_json()
    }

    fn next_live_preedit_revision(&self) -> u64 {
        self.live_preedit_revision.fetch_add(1, Ordering::SeqCst)
    }

    fn set_live_preedit(&self, session_id: u64, revision: u64, text: String) {
        if session_id == 0 {
            return;
        }
        self.live_preedit.set(session_id, revision, text);
    }

    fn clear_live_preedit(&self, session_id: u64, revision: u64) {
        if session_id == 0 {
            return;
        }
        self.live_preedit.clear(session_id, revision);
    }

    fn get_live_preedit_for_session(
        &self,
        session_id: u64,
        claim_token: &str,
    ) -> (u64, bool, String) {
        if !self.validate_session_claim(session_id, claim_token) {
            return (0, false, String::new());
        }
        self.live_preedit.get_for_session(session_id)
    }

    fn set_focused_engine(&self, engine_id: u64, focused: bool) {
        let current = self.focused_engine_id.load(Ordering::SeqCst);
        let next = if focused {
            engine_id
        } else if current == engine_id {
            0
        } else {
            current
        };
        if next != current {
            self.focused_engine_id.store(next, Ordering::SeqCst);
            self.focused_engine_last_change_ms
                .store(now_millis(), Ordering::SeqCst);
        }
    }

    fn focused_engine_status(&self) -> (u64, u64) {
        (
            self.focused_engine_id.load(Ordering::SeqCst),
            self.focused_engine_last_change_ms.load(Ordering::SeqCst),
        )
    }

    fn mark_session_stopping(&self, session_id: u64) {
        if session_id == 0 {
            return;
        }
        if let Ok(mut sessions) = self.stopping_sessions.lock() {
            sessions.insert(session_id);
        }
    }

    fn clear_session_stopping(&self, session_id: u64) {
        if session_id == 0 {
            return;
        }
        if let Ok(mut sessions) = self.stopping_sessions.lock() {
            sessions.remove(&session_id);
        }
    }

    fn session_is_stopping(&self, session_id: u64) -> bool {
        if session_id == 0 {
            return false;
        }
        self.stopping_sessions
            .lock()
            .map(|sessions| sessions.contains(&session_id))
            .unwrap_or(false)
    }
}

/// D-Bus state for connection management
pub struct DiktDbusState {
    running: AtomicBool,
    connection: Mutex<Option<Connection>>,
}

impl Default for DiktDbusState {
    fn default() -> Self {
        Self::new()
    }
}

impl DiktDbusState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            connection: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// The D-Bus interface for Dikt transcription
struct DiktTranscription {
    state: Arc<DiktState>,
    dbus_state: Arc<DiktDbusState>,
}

#[zbus::interface(name = "io.dikt.Transcription")]
impl DiktTranscription {
    /// Start a recording session and bind commit routing to an engine id.
    async fn start_recording_session_for_target(
        &self,
        target_engine_id: u64,
    ) -> fdo::Result<(u64, String)> {
        self.state.cleanup_expired_sessions();
        if target_engine_id == 0 {
            return Err(fdo::Error::Failed(
                "Invalid target engine id 0 for session routing".to_string(),
            ));
        }
        let (session_id, claim_token) = self.state.create_session(target_engine_id);
        let binding_id = binding_id_for_session(session_id);
        self.state
            .set_session_status(session_id, "starting", "Starting recording");
        if let Err(e) = self.start_recording_internal(&binding_id, session_id).await {
            self.state.remove_session(session_id);
            return Err(e);
        }
        self.state
            .set_session_status(session_id, "recording", "Recording in progress");
        Ok((session_id, claim_token))
    }

    /// Stop a specific recording session; final text is delivered via pending commit path.
    async fn stop_recording_session(&self, session_id: u64) -> fdo::Result<bool> {
        self.stop_recording_internal(session_id).await
    }

    /// Cancel one recording session and clear live preview for that session.
    async fn cancel_recording_session(&self, session_id: u64) -> fdo::Result<bool> {
        self.state.cleanup_expired_sessions();
        if self.state.session_claim_token(session_id).is_none() {
            return Ok(false);
        }

        self.state
            .set_session_status(session_id, "cancelled", "Session cancelled");
        let revision = self.state.next_live_preedit_revision();
        self.state.clear_live_preedit(session_id, revision);
        self.state.clear_session_stopping(session_id);

        if self.state.is_recording.swap(false, Ordering::SeqCst) {
            self.state.recording_manager.cancel_recording();
            self.emit_recording_state_changed(false).await?;
        }

        Ok(true)
    }

    /// Get current state: (is_recording, has_model_selected)
    async fn get_state(&self) -> fdo::Result<(bool, bool)> {
        let is_recording = self.state.is_recording.load(Ordering::SeqCst);
        let has_model = self.state.transcription_manager.has_model_selected();

        Ok((is_recording, has_model))
    }

    /// Get global shortcut diagnostics tuple
    async fn get_toggle_diagnostics(
        &self,
    ) -> fdo::Result<(bool, String, String, String, u64, bool, bool, u64, u64, u64)> {
        Ok(toggle_diagnostics_tuple())
    }

    /// Get global shortcut diagnostics with verbose runtime fields.
    async fn get_toggle_diagnostics_verbose(&self) -> fdo::Result<String> {
        Ok(toggle_diagnostics_verbose_json())
    }

    /// Get recent global shortcut event lines.
    async fn get_toggle_recent_events(&self) -> fdo::Result<Vec<String>> {
        Ok(toggle_recent_events())
    }

    /// Atomically consume pending final text for a specific session claim.
    async fn take_pending_commit_for_session(
        &self,
        session_id: u64,
        claim_token: String,
    ) -> fdo::Result<(bool, String)> {
        Ok(self
            .state
            .take_pending_commit_for_session(session_id, claim_token.as_str()))
    }

    /// Get aggregate pending commit queue stats as JSON.
    async fn get_pending_commit_stats(&self) -> fdo::Result<String> {
        Ok(self.state.pending_commit_stats_json())
    }

    /// Read latest live preedit payload for a specific session claim.
    async fn get_live_preedit_for_session(
        &self,
        session_id: u64,
        claim_token: String,
    ) -> fdo::Result<(u64, bool, String)> {
        Ok(self
            .state
            .get_live_preedit_for_session(session_id, claim_token.as_str()))
    }

    /// Get latest known session bound to an engine id.
    async fn get_active_session_for_engine(
        &self,
        engine_id: u64,
    ) -> fdo::Result<(u64, String, bool)> {
        Ok(self.state.active_session_for_engine(engine_id))
    }

    /// Get current status of a session.
    async fn get_session_status(&self, session_id: u64) -> fdo::Result<(String, String, u64)> {
        self.state.cleanup_expired_sessions();
        if let Some(entry) = self.state.session_status(session_id) {
            Ok((entry.state, entry.message, entry.updated_ms))
        } else {
            Ok(("missing".to_string(), "Session not found".to_string(), 0))
        }
    }

    /// Report focused engine transitions from IBus callbacks.
    async fn set_focused_engine(&self, engine_id: u64, focused: bool) -> fdo::Result<()> {
        self.state.set_focused_engine(engine_id, focused);
        Ok(())
    }

    /// Read currently focused engine id and last change timestamp.
    async fn get_focused_engine(&self) -> fdo::Result<(u64, u64)> {
        Ok(self.state.focused_engine_status())
    }

    /// Get recent daemon log lines
    async fn get_recent_logs(&self) -> fdo::Result<Vec<String>> {
        Ok(self.state.recent_logs(400))
    }

    /// Get the currently selected language
    async fn get_language(&self) -> fdo::Result<String> {
        match self.state.selected_language.lock() {
            Ok(language) => Ok(language.clone()),
            Err(e) => {
                error!("GetLanguage failed: selected_language lock poisoned: {}", e);
                Err(fdo::Error::Failed(
                    "Internal state error (selected language unavailable)".to_string(),
                ))
            }
        }
    }

    /// Set the language for transcription
    async fn set_language(&self, language: String) -> fdo::Result<()> {
        match self.state.selected_language.lock() {
            Ok(mut selected_language) => {
                *selected_language = language.clone();
            }
            Err(e) => {
                error!("SetLanguage failed: selected_language lock poisoned: {}", e);
                return Err(fdo::Error::Failed(
                    "Internal state error (cannot update selected language)".to_string(),
                ));
            }
        }
        let settings = Settings::new();
        settings.set_selected_language(&language);
        self.state
            .transcription_manager
            .refresh_config_from_settings(&settings);
        Ok(())
    }

    /// Signal emitted when transcription is ready
    #[zbus(signal)]
    async fn transcription_ready(ctxt: &SignalContext<'_>, text: &str) -> zbus::Result<()>;

    /// Signal emitted when recording state changes
    #[zbus(signal)]
    async fn recording_state_changed(
        ctxt: &SignalContext<'_>,
        is_recording: bool,
    ) -> zbus::Result<()>;

    /// Signal emitted when an error occurs
    #[zbus(signal)]
    async fn error(ctxt: &SignalContext<'_>, message: &str) -> zbus::Result<()>;
}

struct PostProcessRequest {
    provider: PostProcessProvider,
    api_key: String,
    model: String,
    prompt_text: String,
}

fn build_post_process_request(text: &str) -> Option<PostProcessRequest> {
    let settings = Settings::new();
    if !settings.post_process_enabled() {
        return None;
    }

    let provider_id = settings.post_process_provider_id();
    let api_key = settings.post_process_api_keys().get(&provider_id)?.clone();
    if api_key.is_empty() {
        return None;
    }
    let model = settings.post_process_models().get(&provider_id)?.clone();
    if model.is_empty() {
        return None;
    }

    let prompts = settings.post_process_prompts();
    let selected_id = settings.post_process_selected_prompt_id();
    let prompt = if let Some(selected) = selected_id {
        prompts.iter().find(|p| p.id == selected)
    } else {
        prompts.first()
    }?;

    let base_url = settings
        .post_process_base_urls()
        .get(&provider_id)
        .cloned()
        .unwrap_or_else(|| match provider_id.as_str() {
            "openai" => "https://api.openai.com/v1".to_string(),
            "anthropic" => "https://api.anthropic.com/v1".to_string(),
            "openrouter" => "https://openrouter.ai/api/v1".to_string(),
            "groq" => "https://api.groq.com/openai/v1".to_string(),
            "cerebras" => "https://api.cerebras.ai/v1".to_string(),
            _ => "http://localhost:11434/v1".to_string(),
        });
    let provider = PostProcessProvider {
        id: provider_id.clone(),
        label: provider_id.clone(),
        base_url,
        allow_base_url_edit: provider_id == "custom",
    };

    let prompt_text = prompt.prompt.replace("${output}", text);
    Some(PostProcessRequest {
        provider,
        api_key,
        model,
        prompt_text,
    })
}

async fn post_process_transcription_if_enabled(text: &str) -> Option<String> {
    let request = build_post_process_request(text)?;
    let processed = crate::llm_client::send_chat_completion(
        &request.provider,
        request.api_key,
        &request.model,
        request.prompt_text,
    )
    .await
    .ok()
    .flatten()?;
    let trimmed = processed.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

impl DiktTranscription {
    fn new(state: Arc<DiktState>, dbus_state: Arc<DiktDbusState>) -> Self {
        Self { state, dbus_state }
    }

    async fn start_recording_internal(&self, binding_id: &str, session_id: u64) -> fdo::Result<()> {
        self.state.cleanup_expired_sessions();
        self.state.clear_session_stopping(session_id);
        debug!(
            "D-Bus: StartRecording called (binding='{}', session={})",
            binding_id, session_id
        );
        let start_time = Instant::now();

        if !self
            .state
            .transcription_manager
            .refresh_and_has_model_selected()
        {
            self.emit_error(
                "No model selected. Open Dikt preferences to download and select a model.",
            )
            .await?;
            self.state
                .set_session_status(session_id, "failed", "No model selected");
            return Err(fdo::Error::Failed("No model selected".to_string()));
        }

        self.state.transcription_manager.initiate_model_load();

        match self.state.recording_manager.try_start_recording(binding_id) {
            Ok(()) => {
                // Set is_recording BEFORE spawning worker to prevent race condition
                // where worker checks is_recording before it's set and exits immediately
                self.state.is_recording.store(true, Ordering::SeqCst);

                let rm = self.state.recording_manager.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    rm.apply_mute();
                });

                if Settings::new().experimental_enabled() {
                    let revision = self.state.next_live_preedit_revision();
                    self.state.clear_live_preedit(session_id, revision);
                    if let Some(target_engine_id) = self.state.session_binding(session_id) {
                        if target_engine_id != 0 {
                            spawn_live_preedit_worker(
                                self.state.clone(),
                                binding_id.to_string(),
                                session_id,
                                target_engine_id,
                            );
                        }
                    }
                }

                self.emit_recording_state_changed(true).await?;
                play_feedback_sound(&Settings::new(), SoundType::Start);
                info!("D-Bus: Recording started in {:?}", start_time.elapsed());
                Ok(())
            }
            Err(err) => {
                let detail = err.detail();
                let message = format!("Failed to start recording ({}): {}", err.code(), detail);
                error!(
                    "D-Bus: StartRecording failed (binding='{}', session={}): {}",
                    binding_id, session_id, message
                );
                self.state
                    .set_session_status(session_id, "failed", &message);
                self.emit_error(&message).await?;
                Err(fdo::Error::Failed(message))
            }
        }
    }

    async fn stop_recording_internal(&self, session_id: u64) -> fdo::Result<bool> {
        self.state.cleanup_expired_sessions();
        let Some(status) = self.state.session_status(session_id) else {
            return Ok(false);
        };

        if matches!(status.state.as_str(), "finalizing" | "ready" | "committed") {
            return Ok(true);
        }
        if matches!(status.state.as_str(), "failed" | "cancelled") {
            return Ok(false);
        }

        if self.state.session_binding(session_id).is_none() {
            self.state.set_session_status(
                session_id,
                "failed",
                "Cannot stop recording: no target binding",
            );
            return Ok(false);
        }

        let binding_id = binding_id_for_session(session_id);
        debug!(
            "D-Bus: StopRecordingSession called for binding '{}' (session={})",
            binding_id, session_id
        );

        self.state.mark_session_stopping(session_id);
        self.state
            .set_session_status(session_id, "finalizing", "Stopping recorder");

        if self.state.is_recording.swap(false, Ordering::SeqCst) {
            self.emit_recording_state_changed(false).await?;
        }

        play_feedback_sound(&Settings::new(), SoundType::Stop);
        self.state.recording_manager.remove_mute();

        let revision = self.state.next_live_preedit_revision();
        self.state.clear_live_preedit(session_id, revision);

        let Some(samples) = self.state.recording_manager.stop_recording(&binding_id) else {
            self.state.clear_session_stopping(session_id);
            self.state.set_session_status(
                session_id,
                "failed",
                "Stop requested for inactive recording session",
            );
            return Ok(false);
        };

        let worker = DiktTranscription::new(self.state.clone(), self.dbus_state.clone());
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(rt) => {
                    let finalize_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            rt.block_on(worker.finalize_stop_recording(session_id, samples))
                        }));
                    if finalize_result.is_err() {
                        error!(
                            "finalize_stop_recording(session={}) panicked; marking session failed",
                            session_id
                        );
                        worker.state.set_session_status(
                            session_id,
                            "failed",
                            "Internal transcription panic",
                        );
                        worker.state.clear_session_stopping(session_id);
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to create runtime for finalize_stop_recording(session={}): {}",
                        session_id, e
                    );
                    worker.state.set_session_status(
                        session_id,
                        "failed",
                        "Internal runtime initialization failed",
                    );
                    worker.state.clear_session_stopping(session_id);
                }
            }
        });
        Ok(true)
    }

    async fn finalize_stop_recording(&self, session_id: u64, samples: Vec<f32>) {
        let stop_time = Instant::now();
        if samples.is_empty() {
            self.state
                .set_session_status(session_id, "ready", "No speech detected");
            self.state.clear_session_stopping(session_id);
            let _ = self.emit_transcription_ready("").await;
            return;
        }

        debug!(
            "D-Bus: Stop finalized, {} samples captured for session {} in {:?}",
            samples.len(),
            session_id,
            stop_time.elapsed()
        );

        let transcription_time = Instant::now();
        match self.state.transcription_manager.transcribe(samples) {
            Ok(transcription) => {
                debug!(
                    "D-Bus: Transcription completed for session {} in {:?}",
                    session_id,
                    transcription_time.elapsed()
                );
                let lang = match self.state.selected_language.lock() {
                    Ok(selected_language) => selected_language.clone(),
                    Err(e) => {
                        error!(
                            "selected_language lock poisoned while finalizing session {}: {}",
                            session_id, e
                        );
                        Settings::new().selected_language()
                    }
                };
                let converted_text = convert_chinese_variant(&transcription, &lang);
                let output_text = match post_process_transcription_if_enabled(&converted_text).await
                {
                    Some(text) => text,
                    None => converted_text,
                };

                if !output_text.trim().is_empty() {
                    self.state
                        .store_pending_commit(session_id, output_text.clone());
                }
                self.state
                    .set_session_status(session_id, "ready", "Transcription ready");
                self.state.clear_session_stopping(session_id);

                if let Err(e) = self.emit_transcription_ready(&output_text).await {
                    error!(
                        "Failed to emit transcription_ready for session {}: {}",
                        session_id, e
                    );
                }
            }
            Err(err) => {
                let message = format!("Transcription failed: {}", err);
                error!("D-Bus: {}", message);
                self.state
                    .set_session_status(session_id, "failed", &message);
                self.state.clear_session_stopping(session_id);
                if let Err(e) = self.emit_error(&message).await {
                    error!(
                        "Failed to emit error signal for session {}: {}",
                        session_id, e
                    );
                }
            }
        }
    }

    async fn emit_transcription_ready(&self, text: &str) -> fdo::Result<()> {
        if let Some(conn) = self
            .dbus_state
            .connection
            .lock()
            .ok()
            .and_then(|c| c.clone())
        {
            let iface_ref = conn
                .object_server()
                .interface::<_, Self>(DIKT_OBJECT_PATH)
                .await;
            if let Ok(iface_ref) = iface_ref {
                if let Err(e) = Self::transcription_ready(iface_ref.signal_context(), text).await {
                    error!("Failed to emit TranscriptionReady signal: {}", e);
                }
            }
        }
        Ok(())
    }

    async fn emit_recording_state_changed(&self, is_recording: bool) -> fdo::Result<()> {
        if let Some(conn) = self
            .dbus_state
            .connection
            .lock()
            .ok()
            .and_then(|c| c.clone())
        {
            let iface_ref = conn
                .object_server()
                .interface::<_, Self>(DIKT_OBJECT_PATH)
                .await;
            if let Ok(iface_ref) = iface_ref {
                if let Err(e) =
                    Self::recording_state_changed(iface_ref.signal_context(), is_recording).await
                {
                    error!("Failed to emit RecordingStateChanged signal: {}", e);
                }
            }
        }
        Ok(())
    }

    async fn emit_error(&self, message: &str) -> fdo::Result<()> {
        if let Some(conn) = self
            .dbus_state
            .connection
            .lock()
            .ok()
            .and_then(|c| c.clone())
        {
            let iface_ref = conn
                .object_server()
                .interface::<_, Self>(DIKT_OBJECT_PATH)
                .await;
            if let Ok(iface_ref) = iface_ref {
                if let Err(e) = Self::error(iface_ref.signal_context(), message).await {
                    error!("Failed to emit Error signal: {}", e);
                }
            }
        }
        Ok(())
    }
}

fn spawn_live_preedit_worker(
    state: Arc<DiktState>,
    binding_id: String,
    session_id: u64,
    target_engine_id: u64,
) {
    std::thread::spawn(move || {
        let mut last_snapshot_len: usize = 0;
        let mut snapshot_failure_streak: u64 = 0;
        let mut published_text = String::new();
        let mut last_window_text = String::new();
        let mut accumulated_text = String::new();

        loop {
            if !Settings::new().experimental_enabled() {
                if !published_text.is_empty() {
                    let revision = state.next_live_preedit_revision();
                    state.clear_live_preedit(session_id, revision);
                    published_text.clear();
                }
                info!(
                    "Live preedit worker exiting: experimental features disabled during session {}",
                    session_id
                );
                break;
            }

            // During graceful stop for this session, keep polling until
            // stop_recording_internal clears preview and unmarks the session.
            let session_stopping = state.session_is_stopping(session_id);
            if !state.is_recording.load(Ordering::SeqCst) && !session_stopping {
                // Cancel path (not graceful stop) - exit and clear preview.
                info!(
                    "Live preedit worker exiting: cancel path (is_recording={}, session_stopping={})",
                    state.is_recording.load(Ordering::SeqCst),
                    session_stopping
                );
                break;
            }

            // During graceful stop, keep looping even after session-target removal.
            let current_target = state.session_binding(session_id);
            if !session_stopping && current_target != Some(target_engine_id) {
                info!(
                    "Live preedit worker exiting: session_target mismatch (session_id={}, expected={}, got={:?})",
                    session_id, target_engine_id, current_target
                );
                break;
            }

            std::thread::sleep(Duration::from_millis(LIVE_PREEDIT_POLL_MS));

            let Some(samples) = state
                .recording_manager
                .snapshot_recording_window(&binding_id, LIVE_PREEDIT_MAX_WINDOW_SAMPLES)
            else {
                // Snapshot failures can be transient under load; keep the last preview visible
                // and continue retrying while this session is still active.
                // Only break if NOT in graceful stop mode.
                if !state.session_is_stopping(session_id)
                    && (state.session_binding(session_id) != Some(target_engine_id)
                        || !state.is_recording.load(Ordering::SeqCst))
                {
                    info!(
                        "Live preedit worker exiting: snapshot failure path (is_recording={}, session_target={:?})",
                        state.is_recording.load(Ordering::SeqCst),
                        state.session_binding(session_id)
                    );
                    break;
                }
                snapshot_failure_streak = snapshot_failure_streak.saturating_add(1);
                if snapshot_failure_streak == 1
                    || snapshot_failure_streak.is_multiple_of(LIVE_PREEDIT_SNAPSHOT_WARN_EVERY)
                {
                    debug!(
                        "Live preedit snapshot unavailable for session {} (streak={}); retaining current preview",
                        session_id, snapshot_failure_streak
                    );
                }
                continue;
            };

            if snapshot_failure_streak > 0 {
                debug!(
                    "Live preedit snapshot recovered for session {} after {} transient misses",
                    session_id, snapshot_failure_streak
                );
                snapshot_failure_streak = 0;
            }

            if samples.len() < LIVE_PREEDIT_MIN_TOTAL_SAMPLES {
                continue;
            }

            if last_snapshot_len > 0
                && samples.len().saturating_sub(last_snapshot_len) < LIVE_PREEDIT_MIN_NEW_SAMPLES
            {
                continue;
            }
            last_snapshot_len = samples.len();

            let transcription = match state.transcription_manager.transcribe_for_live(samples) {
                Ok(text) => text,
                Err(err) => {
                    debug!(
                        "Live preedit transcription failed for session {}: {}",
                        session_id, err
                    );
                    continue;
                }
            };

            let lang = match state.selected_language.lock() {
                Ok(selected_language) => selected_language.clone(),
                Err(e) => {
                    error!(
                        "selected_language lock poisoned in live preedit worker (session={}): {}",
                        session_id, e
                    );
                    Settings::new().selected_language()
                }
            };
            let live_text = convert_chinese_variant(&transcription, &lang)
                .trim()
                .to_string();

            // During graceful stop, stop_recording_internal handles the clear.
            let post_transcribe_target = state.session_binding(session_id);
            if !state.session_is_stopping(session_id)
                && post_transcribe_target != Some(target_engine_id)
            {
                info!(
                    "Live preedit worker exiting: post-transcribe session_target mismatch (session_id={}, expected={}, got={:?})",
                    session_id, target_engine_id, post_transcribe_target
                );
                break;
            }

            if live_text.is_empty() {
                continue;
            }

            if accumulated_text.is_empty() {
                accumulated_text = live_text.clone();
            } else {
                accumulated_text =
                    merge_live_transcript(&accumulated_text, &last_window_text, &live_text);
            }
            last_window_text = live_text;

            if accumulated_text != published_text {
                let revision = state.next_live_preedit_revision();
                state.set_live_preedit(session_id, revision, accumulated_text.clone());
                published_text = accumulated_text.clone();
            }
        }

        // Only clear preview if NOT in graceful stop mode (i.e. cancelled).
        if !published_text.is_empty() && !state.session_is_stopping(session_id) {
            let revision = state.next_live_preedit_revision();
            state.clear_live_preedit(session_id, revision);
        }
    });
}

fn merge_live_transcript(accumulated: &str, prev_window: &str, next_window: &str) -> String {
    if accumulated.is_empty() || prev_window.is_empty() {
        return next_window.to_string();
    }
    if next_window.is_empty() || next_window == prev_window {
        return accumulated.to_string();
    }
    if let Some(base) = accumulated.strip_suffix(prev_window) {
        if next_window.starts_with(prev_window) {
            return format!("{}{}", base, next_window);
        }

        let lcp = common_prefix_chars(prev_window, next_window);
        let prev_len = prev_window.chars().count();
        let next_len = next_window.chars().count();
        if lcp >= 8 || (lcp * 2 >= prev_len.min(next_len) && lcp >= 3) {
            return format!("{}{}", base, next_window);
        }

        let overlap = longest_suffix_prefix_chars(prev_window, next_window);
        if overlap > 0 {
            let overlap_bytes = byte_index_at_char(next_window, overlap);
            return format!("{}{}", accumulated, &next_window[overlap_bytes..]);
        }
    }

    if accumulated.ends_with(next_window) {
        return accumulated.to_string();
    }

    format!("{}{}", accumulated, next_window)
}

fn common_prefix_chars(left: &str, right: &str) -> usize {
    left.chars()
        .zip(right.chars())
        .take_while(|(a, b)| a == b)
        .count()
}

fn longest_suffix_prefix_chars(left: &str, right: &str) -> usize {
    let left_bounds = char_boundaries(left);
    let right_bounds = char_boundaries(right);
    let max = left_bounds
        .len()
        .saturating_sub(1)
        .min(right_bounds.len().saturating_sub(1));
    for overlap_chars in (1..=max).rev() {
        let left_start = left_bounds[left_bounds.len() - 1 - overlap_chars];
        let right_end = right_bounds[overlap_chars];
        if left[left_start..] == right[..right_end] {
            return overlap_chars;
        }
    }
    0
}

fn byte_index_at_char(text: &str, char_idx: usize) -> usize {
    char_boundaries(text)
        .get(char_idx)
        .copied()
        .unwrap_or(text.len())
}

fn char_boundaries(text: &str) -> Vec<usize> {
    let mut bounds = text.char_indices().map(|(idx, _)| idx).collect::<Vec<_>>();
    bounds.push(text.len());
    bounds
}

fn binding_id_for_session(session_id: u64) -> String {
    format!("session-{}", session_id)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Start the D-Bus server
pub async fn start_dbus_server(state: Arc<DiktState>) -> Result<Arc<DiktDbusState>, String> {
    info!("Starting D-Bus server for IBus integration...");

    let dbus_state = Arc::new(DiktDbusState::new());

    let connection = Connection::session()
        .await
        .map_err(|e| format!("Failed to connect to session bus: {}", e))?;

    connection
        .request_name(DIKT_BUS_NAME)
        .await
        .map_err(|e| format!("Failed to request bus name: {}", e))?;

    let transcription = DiktTranscription::new(state, dbus_state.clone());

    connection
        .object_server()
        .at(DIKT_OBJECT_PATH, transcription)
        .await
        .map_err(|e| format!("Failed to register D-Bus object: {}", e))?;

    {
        let mut conn_guard = dbus_state
            .connection
            .lock()
            .map_err(|e| format!("Failed to lock connection: {}", e))?;
        *conn_guard = Some(connection);
    }

    dbus_state.running.store(true, Ordering::SeqCst);

    info!("D-Bus server started successfully on io.dikt.Transcription");
    Ok(dbus_state)
}

/// Stop the D-Bus server
pub async fn stop_dbus_server(dbus_state: &DiktDbusState) -> Result<(), String> {
    info!("Stopping D-Bus server...");

    if !dbus_state.is_running() {
        return Ok(());
    }

    {
        let mut conn_guard = dbus_state
            .connection
            .lock()
            .map_err(|e| format!("Failed to lock connection: {}", e))?;
        *conn_guard = None;
    }

    dbus_state.running.store(false, Ordering::SeqCst);

    info!("D-Bus server stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LivePreeditStore, PendingCommitStore};
    use std::time::Duration;

    #[test]
    fn pending_commit_store_take_for_session_claim_consumes_exact_match() {
        let store = PendingCommitStore::default();
        store.store(42, "claim-a".to_string(), "hello".to_string());
        store.store(43, "claim-b".to_string(), "world".to_string());

        let (ok_first, text_first) = store.take_for_session(42, "claim-a");
        assert!(ok_first);
        assert_eq!(text_first, "hello");

        let (ok_second, text_second) = store.take_for_session(43, "claim-b");
        assert!(ok_second);
        assert_eq!(text_second, "world");
    }

    #[test]
    fn pending_commit_store_rejects_wrong_claim() {
        let store = PendingCommitStore::default();
        store.store(61, "claim-ok".to_string(), "payload".to_string());

        let (ok, text) = store.take_for_session(61, "claim-wrong");
        assert!(!ok);
        assert!(text.is_empty());
        let (ok_again, text_again) = store.take_for_session(61, "claim-ok");
        assert!(ok_again);
        assert_eq!(text_again, "payload");
    }

    #[test]
    fn pending_commit_store_stats_reports_oldest_age() {
        let store = PendingCommitStore::default();
        store.store(99, "claim-99".to_string(), "payload".to_string());
        std::thread::sleep(Duration::from_millis(2));
        let parsed: serde_json::Value =
            serde_json::from_str(&store.stats_json()).expect("valid stats json");
        let queue_len = parsed
            .get("queue_len")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let oldest_age_ms = parsed
            .get("oldest_age_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        assert_eq!(queue_len, 1);
        assert!(oldest_age_ms > 0);
    }

    #[test]
    fn pending_commit_store_keeps_independent_queue_order() {
        let store = PendingCommitStore::default();
        store.store(10, "claim-10".to_string(), "first".to_string());
        store.store(11, "claim-11".to_string(), "second".to_string());
        store.store(12, "claim-12".to_string(), "third".to_string());

        let first = store.take_for_session(10, "claim-10");
        let second = store.take_for_session(11, "claim-11");
        let third = store.take_for_session(12, "claim-12");

        assert_eq!(first, (true, "first".to_string()));
        assert_eq!(second, (true, "second".to_string()));
        assert_eq!(third, (true, "third".to_string()));
    }

    #[test]
    fn live_preedit_store_tracks_latest_per_session() {
        let store = LivePreeditStore::default();
        store.set(42, 1, "alpha".to_string());
        store.set(42, 2, "bravo".to_string());

        let (revision, visible, text) = store.get_for_session(42);
        assert_eq!(revision, 2);
        assert!(visible);
        assert_eq!(text, "bravo");
    }

    #[test]
    fn live_preedit_store_keeps_session_isolation() {
        let store = LivePreeditStore::default();
        store.set(21, 7, "left".to_string());
        store.set(22, 9, "right".to_string());

        let left = store.get_for_session(21);
        let right = store.get_for_session(22);
        assert_eq!(left.0, 7);
        assert!(left.1);
        assert_eq!(left.2, "left");
        assert_eq!(right.0, 9);
        assert!(right.1);
        assert_eq!(right.2, "right");
    }

    #[test]
    fn live_preedit_store_clear_hides_entry() {
        let store = LivePreeditStore::default();
        store.set(101, 3, "hello".to_string());
        store.clear(101, 4);

        let (revision, visible, text) = store.get_for_session(101);
        assert_eq!(revision, 4);
        assert!(!visible);
        assert!(text.is_empty());
    }

    #[test]
    fn merge_live_transcript_appends_shifted_tail_without_losing_prefix() {
        let accumulated = "hello world";
        let prev = "hello world";
        let next = "world again";
        let merged = super::merge_live_transcript(accumulated, prev, next);
        assert_eq!(merged, "hello world again");
    }

    #[test]
    fn merge_live_transcript_replaces_tail_on_correction() {
        let accumulated = "hello wurld";
        let prev = "hello wurld";
        let next = "hello world";
        let merged = super::merge_live_transcript(accumulated, prev, next);
        assert_eq!(merged, "hello world");
    }
}
