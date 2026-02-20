use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use evdev::{Device, EventType, InputEventKind};
use log::{debug, error, info, warn};
use notify_rust::Notification;
use serde_json::json;
use tokio::sync::mpsc;

use crate::ibus_control::{get_current_engine, is_dikt_engine, switch_to_dikt_engine_verified};
use crate::key_mapping::{
    gdk_keyval_to_evdev, is_modifier_key, modifiers_from_held_keys, EvdevKeybinding, MOD_ALT,
    MOD_CTRL, MOD_SHIFT, MOD_SUPER,
};
use crate::settings::Settings;
use crate::utils::launch::open_dikt_ui;

const DIKT_BUS_NAME: &str = "io.dikt.Transcription";
const DIKT_OBJECT_PATH: &str = "/io/dikt/Transcription";
const DIKT_INTERFACE: &str = "io.dikt.Transcription";

const START_RECORDING_ARM_DELAY_MS: u64 = 120;
const STOP_RECORDING_TIMEOUT_MS: u64 = 20_000;
const ENGINE_SWITCH_VERIFY_TIMEOUT_MS: u64 = 350;
const FOCUSED_ENGINE_VERIFY_TIMEOUT_MS: u64 = 700;
const FOCUSED_ENGINE_VERIFY_POLL_MS: u64 = 20;
const TOGGLE_PRESS_DEBOUNCE_MS: u64 = 90;
const SETTINGS_POLL_INTERVAL_MS: u64 = 350;
const FAILURE_NOTIFICATION_COOLDOWN_MS: u64 = 8_000;
const TOGGLE_EVENT_HISTORY_LIMIT: usize = 60;

static TOGGLE_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
static HEALTH_STATE: OnceLock<Mutex<ToggleRuntimeHealth>> = OnceLock::new();
static TOGGLE_RECENT_EVENTS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
static FORCE_REBIND_REQUESTED: AtomicBool = AtomicBool::new(false);

fn is_greeter_session_from(user: Option<&str>, session_class: Option<&str>) -> bool {
    let is_greeter_user = user
        .map(str::trim)
        .map(|u| u.eq_ignore_ascii_case("gdm") || u.eq_ignore_ascii_case("gdm-greeter"))
        .unwrap_or(false);
    let is_greeter_class = session_class
        .map(str::trim)
        .map(|class| class.eq_ignore_ascii_case("greeter"))
        .unwrap_or(false);
    is_greeter_user || is_greeter_class
}

pub fn is_restricted_session_context() -> bool {
    let user = std::env::var("USER").ok();
    let session_class = std::env::var("XDG_SESSION_CLASS").ok();
    is_greeter_session_from(user.as_deref(), session_class.as_deref())
}

// ── TOGGLE state machine ──────────────────────────────────────────────────

#[derive(Debug)]
enum ToggleState {
    Idle,
    Pending {
        toggle_session_id: u64,
    },
    Recording {
        toggle_session_id: u64,
        daemon_session_id: u64,
        claim_token: String,
    },
    Stopping {
        toggle_session_id: u64,
        daemon_session_id: u64,
    },
}

enum InternalEvent {
    StartRecording {
        toggle_session_id: u64,
        result: std::result::Result<(u64, String), String>,
    },
    StopRecording {
        toggle_session_id: u64,
        result: StopRecordingOutcome,
    },
}

enum StopRecordingOutcome {
    Acknowledged,
    Finalizing { reason: String, timed_out: bool },
    Failed(String),
}

enum StopRecordingCallError {
    TimedOut,
    Disconnected,
    Failed(String),
}

// ── Health diagnostics ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ToggleRuntimeHealth {
    healthy: bool,
    component: String,
    code: String,
    message: String,
    last_success_ms: u64,
    listener_session_ok: bool,
    shortcut_bound: bool,
    bind_fail_count: u64,
    press_while_dikt_count: u64,
    stop_timeout_fallback_count: u64,
    last_notification_ms: u64,
    current_state: String,
    shortcut_description: String,
    last_start_failure_code: String,
    last_start_failure_message: String,
    last_start_failure_ms: u64,
    last_stop_failure_message: String,
    last_stop_failure_ms: u64,
    pending_commit_session_id: u64,
    pending_commit_mark_ms: u64,
    focused_engine_id: u64,
    engine_last_change_ms: u64,
    last_switch_attempt_ms: u64,
    last_switch_confirm_latency_ms: u64,
    last_switch_failure_message: String,
    last_dbus_error: String,
    last_dbus_error_ms: u64,
}

impl Default for ToggleRuntimeHealth {
    fn default() -> Self {
        Self {
            healthy: false,
            component: "global_shortcuts".to_string(),
            code: "not_initialized".to_string(),
            message: "Global dictation shortcut listener not initialized yet".to_string(),
            last_success_ms: 0,
            listener_session_ok: false,
            shortcut_bound: false,
            bind_fail_count: 0,
            press_while_dikt_count: 0,
            stop_timeout_fallback_count: 0,
            last_notification_ms: 0,
            current_state: "idle".to_string(),
            shortcut_description: String::new(),
            last_start_failure_code: String::new(),
            last_start_failure_message: String::new(),
            last_start_failure_ms: 0,
            last_stop_failure_message: String::new(),
            last_stop_failure_ms: 0,
            pending_commit_session_id: 0,
            pending_commit_mark_ms: 0,
            focused_engine_id: 0,
            engine_last_change_ms: 0,
            last_switch_attempt_ms: 0,
            last_switch_confirm_latency_ms: 0,
            last_switch_failure_message: String::new(),
            last_dbus_error: String::new(),
            last_dbus_error_ms: 0,
        }
    }
}

fn health_state() -> &'static Mutex<ToggleRuntimeHealth> {
    HEALTH_STATE.get_or_init(|| Mutex::new(ToggleRuntimeHealth::default()))
}

fn toggle_recent_events_state() -> &'static Mutex<VecDeque<String>> {
    TOGGLE_RECENT_EVENTS
        .get_or_init(|| Mutex::new(VecDeque::with_capacity(TOGGLE_EVENT_HISTORY_LIMIT)))
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn push_toggle_event(event: impl Into<String>) {
    let line = format!("{} {}", now_millis(), event.into());
    if let Ok(mut events) = toggle_recent_events_state().lock() {
        events.push_back(line);
        while events.len() > TOGGLE_EVENT_HISTORY_LIMIT {
            let _ = events.pop_front();
        }
    }
}

pub fn toggle_recent_events() -> Vec<String> {
    toggle_recent_events_state()
        .lock()
        .map(|events| events.iter().cloned().collect())
        .unwrap_or_default()
}

fn mark_health_success(message: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.healthy = true;
        health.component = "global_shortcuts".to_string();
        health.code = "ok".to_string();
        health.message = message.to_string();
        health.last_success_ms = now_millis();
        health.listener_session_ok = true;
        health.shortcut_bound = true;
    }
}

fn mark_toggle_state(state: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.current_state = state.to_string();
    }
}

fn mark_health_error(code: &str, message: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.healthy = false;
        health.component = "global_shortcuts".to_string();
        health.code = code.to_string();
        health.message = message.to_string();
        if code.starts_with("evdev_") {
            health.listener_session_ok = false;
            if code.contains("bind") || code.contains("permission") {
                health.shortcut_bound = false;
                health.bind_fail_count = health.bind_fail_count.saturating_add(1);
            }
        }
    }
}

fn mark_shortcut_description(description: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.shortcut_description = description.to_string();
    }
}

fn mark_start_failure(code: &str, message: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.last_start_failure_code = code.to_string();
        health.last_start_failure_message = message.to_string();
        health.last_start_failure_ms = now_millis();
    }
}

fn clear_start_failure() {
    if let Ok(mut health) = health_state().lock() {
        health.last_start_failure_code.clear();
        health.last_start_failure_message.clear();
        health.last_start_failure_ms = 0;
    }
}

fn mark_stop_failure(message: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.last_stop_failure_message = message.to_string();
        health.last_stop_failure_ms = now_millis();
    }
}

fn clear_stop_failure() {
    if let Ok(mut health) = health_state().lock() {
        health.last_stop_failure_message.clear();
        health.last_stop_failure_ms = 0;
    }
}

fn mark_pending_commit(session_id: u64) {
    if let Ok(mut health) = health_state().lock() {
        health.pending_commit_session_id = session_id;
        health.pending_commit_mark_ms = now_millis();
    }
}

fn clear_pending_commit() {
    if let Ok(mut health) = health_state().lock() {
        health.pending_commit_session_id = 0;
        health.pending_commit_mark_ms = 0;
    }
}

fn mark_focused_engine_status(engine_id: u64, last_change_ms: u64) {
    if let Ok(mut health) = health_state().lock() {
        health.focused_engine_id = engine_id;
        health.engine_last_change_ms = last_change_ms;
    }
}

fn mark_switch_attempt() {
    if let Ok(mut health) = health_state().lock() {
        health.last_switch_attempt_ms = now_millis();
    }
}

fn mark_switch_confirm(latency_ms: u64) {
    if let Ok(mut health) = health_state().lock() {
        health.last_switch_confirm_latency_ms = latency_ms;
        health.last_switch_failure_message.clear();
    }
}

fn mark_switch_failure(message: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.last_switch_failure_message = message.to_string();
    }
}

fn mark_dbus_error(method: &str, message: &str) {
    if let Ok(mut health) = health_state().lock() {
        health.last_dbus_error = format!("{}: {}", method, message);
        health.last_dbus_error_ms = now_millis();
    }
}

fn bump_press_while_dikt() {
    if let Ok(mut health) = health_state().lock() {
        health.press_while_dikt_count = health.press_while_dikt_count.saturating_add(1);
    }
}

fn bump_stop_timeout_fallback() {
    if let Ok(mut health) = health_state().lock() {
        health.stop_timeout_fallback_count = health.stop_timeout_fallback_count.saturating_add(1);
    }
}

pub fn toggle_diagnostics_tuple() -> (bool, String, String, String, u64, bool, bool, u64, u64, u64)
{
    if let Ok(health) = health_state().lock() {
        (
            health.healthy,
            health.component.clone(),
            health.code.clone(),
            health.message.clone(),
            health.last_success_ms,
            health.listener_session_ok,
            health.shortcut_bound,
            health.bind_fail_count,
            health.press_while_dikt_count,
            health.stop_timeout_fallback_count,
        )
    } else {
        (
            false,
            "global_shortcuts".to_string(),
            "lock_poisoned".to_string(),
            "Failed to read TOGGLE diagnostics".to_string(),
            0,
            false,
            false,
            0,
            0,
            0,
        )
    }
}

pub fn toggle_diagnostics_verbose_json() -> String {
    if let Ok(health) = health_state().lock() {
        let pending_commit_age_ms = if health.pending_commit_session_id == 0 {
            0
        } else {
            now_millis().saturating_sub(health.pending_commit_mark_ms)
        };
        json!({
            "healthy": health.healthy,
            "component": health.component,
            "code": health.code,
            "message": health.message,
            "last_success_ms": health.last_success_ms,
            "listener_session_ok": health.listener_session_ok,
            "shortcut_bound": health.shortcut_bound,
            "bind_fail_count": health.bind_fail_count,
            "press_while_dikt_count": health.press_while_dikt_count,
            "stop_timeout_fallback_count": health.stop_timeout_fallback_count,
            "current_state": health.current_state,
            "shortcut_description": health.shortcut_description,
            "last_start_failure_code": health.last_start_failure_code,
            "last_start_failure_message": health.last_start_failure_message,
            "last_start_failure_ms": health.last_start_failure_ms,
            "last_stop_failure_message": health.last_stop_failure_message,
            "last_stop_failure_ms": health.last_stop_failure_ms,
            "pending_commit_session_id": health.pending_commit_session_id,
            "pending_commit_age_ms": pending_commit_age_ms,
            "engine_active": health.focused_engine_id != 0,
            "focused_engine_id": health.focused_engine_id,
            "engine_last_change_ms": health.engine_last_change_ms,
            "last_switch_attempt_ms": health.last_switch_attempt_ms,
            "last_switch_confirm_latency_ms": health.last_switch_confirm_latency_ms,
            "last_switch_failure_message": health.last_switch_failure_message,
            "last_dbus_error": health.last_dbus_error,
            "last_dbus_error_ms": health.last_dbus_error_ms,
            "recent_event_count": toggle_recent_events().len(),
        })
        .to_string()
    } else {
        json!({
            "healthy": false,
            "component": "global_shortcuts",
            "code": "lock_poisoned",
            "message": "Failed to read TOGGLE diagnostics",
            "last_success_ms": 0,
            "listener_session_ok": false,
            "shortcut_bound": false,
            "bind_fail_count": 0,
            "press_while_dikt_count": 0,
            "stop_timeout_fallback_count": 0,
            "current_state": "unknown",
            "shortcut_description": "",
            "last_start_failure_code": "",
            "last_start_failure_message": "Failed to read TOGGLE diagnostics",
            "last_start_failure_ms": 0,
            "last_stop_failure_message": "",
            "last_stop_failure_ms": 0,
            "pending_commit_session_id": 0,
            "pending_commit_age_ms": 0,
            "engine_active": false,
            "focused_engine_id": 0,
            "engine_last_change_ms": 0,
            "last_switch_attempt_ms": 0,
            "last_switch_confirm_latency_ms": 0,
            "last_switch_failure_message": "",
            "last_dbus_error": "health_state lock poisoned",
            "last_dbus_error_ms": 0,
            "recent_event_count": 0,
        })
        .to_string()
    }
}

// ── Public entry points ────────────────────────────────────────────────

pub fn start_global_shortcuts_listener() {
    let initial_config = ShortcutConfig::from_settings(&Settings::new());
    mark_health_error(
        "initializing",
        "Starting global dictation shortcut listener",
    );
    mark_toggle_state("initializing");
    push_toggle_event("listener: initializing");

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                error!("Failed to create runtime for global shortcuts: {}", e);
                mark_health_error(
                    "runtime_init_failed",
                    &format!("Failed to create runtime for global shortcuts: {}", e),
                );
                push_toggle_event(format!("listener: runtime init failed: {}", e));
                return;
            }
        };

        runtime.block_on(async move {
            run_evdev_listener_loop(initial_config).await;
        });
    });
}

pub fn request_shortcut_listener_rebind() {
    FORCE_REBIND_REQUESTED.store(true, Ordering::SeqCst);
}

/// Called from the UI "Authorize Now" button (legacy path).
/// With evdev this is no longer needed — included only for API compatibility.
pub fn authorize_shortcut_interactively_from_ui() -> Result<String> {
    let config = ShortcutConfig::from_settings(&Settings::new());
    let _keybinding = config.resolve().ok_or_else(|| {
        anyhow!(
            "Cannot resolve keybinding for keyval {:#x} + modifiers {:#x}",
            config.keyval,
            config.modifiers
        )
    })?;
    let description = config.human_description();

    // Try opening a keyboard device to validate permissions
    match find_keyboard_devices() {
        Ok(devices) if !devices.is_empty() => {
            request_shortcut_listener_rebind();
            Ok(format!(
                "evdev: {} keyboard(s) accessible, shortcut {} ready",
                devices.len(),
                description
            ))
        }
        Ok(_) => Err(anyhow!(
            "No keyboard devices found in /dev/input/. Is the input group set up?"
        )),
        Err(e) => Err(anyhow!("Cannot access keyboard devices: {}", e)),
    }
}

// ── evdev listener loop ────────────────────────────────────────────────

async fn run_evdev_listener_loop(mut active_config: ShortcutConfig) {
    loop {
        let keybinding = match active_config.resolve() {
            Some(kb) => kb,
            None => {
                let msg = format!(
                    "Unsupported dictation shortcut: keyval {:#x}",
                    active_config.keyval
                );
                mark_health_error("invalid_shortcut", &msg);
                notify_toggle_failure(
                    "Invalid dictation shortcut",
                    "Set a supported shortcut in Dikt preferences.",
                );
                // Wait before retrying
                sleep_until_retry_or_rebind(5_000).await;
                active_config = ShortcutConfig::from_settings(&Settings::new());
                continue;
            }
        };

        match run_evdev_session(&active_config, &keybinding).await {
            Ok(()) => {
                // Session ended normally (settings changed, rebind requested)
                info!("evdev session ended normally, restarting");
            }
            Err(e) => {
                warn!("evdev session error: {}", e);
                let code = if e.to_string().contains("Permission denied")
                    || e.to_string().contains("permission")
                {
                    "evdev_permission_denied"
                } else {
                    "evdev_session_error"
                };
                mark_health_error(code, &e.to_string());
                notify_toggle_failure(
                    "Global dictation shortcut is unavailable",
                    &format!("Keyboard input error: {}", e),
                );
            }
        }

        active_config = ShortcutConfig::from_settings(&Settings::new());
        sleep_until_retry_or_rebind(2_000).await;
    }
}

async fn run_evdev_session(
    active_config: &ShortcutConfig,
    keybinding: &EvdevKeybinding,
) -> Result<()> {
    let devices = find_keyboard_devices()?;
    if devices.is_empty() {
        return Err(anyhow!(
            "No keyboard devices found. Check /dev/input/ permissions."
        ));
    }

    let description = active_config.human_description();
    let n_devices = devices.len();
    mark_shortcut_description(&description);
    mark_health_success(&format!(
        "Listening on {} keyboard(s) for {}",
        n_devices, description
    ));
    mark_toggle_state("idle");
    info!(
        "evdev: listening on {} keyboard device(s) for TOGGLE shortcut {}",
        n_devices, description
    );

    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<InternalEvent>();
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<KeyEvent>();

    // Spawn a reader task for each keyboard device
    let mut reader_handles = Vec::new();
    for device_path in &devices {
        let path = device_path.clone();
        let tx = key_tx.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = read_device_events(path.clone(), tx).await {
                warn!("evdev reader for {:?} ended: {}", path, e);
            }
        });
        reader_handles.push(handle);
    }
    // Drop the original sender so the channel closes when all reader tasks end
    drop(key_tx);

    let mut toggle_state = ToggleState::Idle;
    let mut config_poll = tokio::time::interval(Duration::from_millis(SETTINGS_POLL_INTERVAL_MS));
    let mut held_modifiers: HashSet<u16> = HashSet::new();
    let mut last_shortcut_press_ms = 0_u64;

    let loop_result = loop {
        tokio::select! {
            _ = config_poll.tick() => {
                let new_config = ShortcutConfig::from_settings(&Settings::new());
                if new_config != *active_config {
                    info!("Toggle dictation settings changed, restarting evdev session");
                    break Ok(());
                }
                if FORCE_REBIND_REQUESTED.swap(false, Ordering::SeqCst) {
                    info!("Force rebind requested, restarting evdev session");
                    break Ok(());
                }
            }
            maybe_key = key_rx.recv() => {
                let Some(event) = maybe_key else {
                    // All reader tasks exited — keyboard disconnected?
                    break Err(anyhow!(
                        "All keyboard device readers disconnected"
                    ));
                };

                match event {
                    KeyEvent::Press(code) => {
                        if is_modifier_key(code) {
                            held_modifiers.insert(code);
                        } else if code == keybinding.key_code {
                            let current_mods = modifiers_from_held_keys(&held_modifiers);
                            if current_mods == keybinding.modifiers {
                                let now_ms = now_millis();
                                if now_ms.saturating_sub(last_shortcut_press_ms)
                                    < TOGGLE_PRESS_DEBOUNCE_MS
                                {
                                    push_toggle_event(format!(
                                        "toggle:shortcut press ignored by debounce ({} ms)",
                                        TOGGLE_PRESS_DEBOUNCE_MS
                                    ));
                                    continue;
                                }
                                last_shortcut_press_ms = now_ms;
                                on_global_pressed(&mut toggle_state, &internal_tx);
                            }
                        }
                    }
                    KeyEvent::Release(code) => {
                        if is_modifier_key(code) {
                            held_modifiers.remove(&code);
                        }
                    }
                }
            }
            maybe_internal = internal_rx.recv() => {
                let Some(internal) = maybe_internal else {
                    break Err(anyhow!("Internal global shortcut channel closed"));
                };
                handle_internal_event(&mut toggle_state, internal);
            }
        }
    };

    cleanup_state(&mut toggle_state);

    // Cancel all reader tasks
    for handle in reader_handles {
        handle.abort();
    }

    loop_result
}

// ── evdev device management ────────────────────────────────────────────

#[derive(Debug)]
enum KeyEvent {
    Press(u16),
    Release(u16),
}

fn find_keyboard_devices() -> Result<Vec<PathBuf>> {
    let mut keyboards = Vec::new();

    let input_dir = std::fs::read_dir("/dev/input").map_err(|e| {
        anyhow!(
            "Cannot read /dev/input: {}. You may need to add your user to the 'input' group.",
            e
        )
    })?;

    for entry in input_dir.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with("event") {
            continue;
        }

        match Device::open(&path) {
            Ok(device) => {
                // Check if this device has keyboard capabilities (EV_KEY with key codes)
                if device.supported_events().contains(EventType::KEY) {
                    let supported_keys = device.supported_keys();
                    let has_keyboard_keys = supported_keys
                        .map(|keys| {
                            // A real keyboard has letter keys
                            keys.contains(evdev::Key::KEY_A)
                                && keys.contains(evdev::Key::KEY_Z)
                                && keys.contains(evdev::Key::KEY_SPACE)
                        })
                        .unwrap_or(false);

                    if has_keyboard_keys {
                        let dev_name = device.name().unwrap_or("unknown");
                        info!("evdev: found keyboard device {:?} ({})", path, dev_name);
                        keyboards.push(path);
                    }
                }
            }
            Err(e) => {
                debug!("evdev: cannot open {:?}: {}", path, e);
            }
        }
    }

    Ok(keyboards)
}

async fn read_device_events(path: PathBuf, tx: mpsc::UnboundedSender<KeyEvent>) -> Result<()> {
    let device = Device::open(&path).map_err(|e| anyhow!("Failed to open {:?}: {}", path, e))?;
    let mut stream = device
        .into_event_stream()
        .map_err(|e| anyhow!("Failed to create event stream for {:?}: {}", path, e))?;

    loop {
        let event = stream
            .next_event()
            .await
            .map_err(|e| anyhow!("Event read error on {:?}: {}", path, e))?;

        if let InputEventKind::Key(key) = event.kind() {
            let code = key.code();
            match event.value() {
                1 => {
                    // Key press
                    if tx.send(KeyEvent::Press(code)).is_err() {
                        break;
                    }
                }
                0 => {
                    // Key release
                    if tx.send(KeyEvent::Release(code)).is_err() {
                        break;
                    }
                }
                2 => {
                    // Key repeat — ignore for TOGGLE
                }
                _ => {}
            }
        }
    }

    Ok(())
}

// ── TOGGLE toggle handlers ─────────────────────────────────────────────────

fn on_global_pressed(
    toggle_state: &mut ToggleState,
    internal_tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    match toggle_state {
        ToggleState::Idle => start_toggle_recording(toggle_state, internal_tx),
        ToggleState::Pending { toggle_session_id } => {
            push_toggle_event(format!(
                "toggle:{} toggle ignored while start transition is pending",
                toggle_session_id
            ));
            debug!(
                "[toggle:{}] Ignoring toggle while start is pending",
                toggle_session_id
            );
        }
        ToggleState::Recording {
            toggle_session_id,
            daemon_session_id,
            claim_token,
        } => {
            let current_session = *toggle_session_id;
            let daemon_session = *daemon_session_id;
            let stop_claim_token = claim_token.clone();
            info!(
                "[toggle:{}] Toggle pressed; waiting for StopRecordingSession({})",
                current_session, daemon_session
            );
            push_toggle_event(format!(
                "toggle:{} toggle stop requested; stopping daemon session {}",
                current_session, daemon_session
            ));
            spawn_stop_recording(
                current_session,
                daemon_session,
                stop_claim_token.clone(),
                internal_tx.clone(),
            );
            *toggle_state = ToggleState::Stopping {
                toggle_session_id: current_session,
                daemon_session_id: daemon_session,
            };
            mark_toggle_state("stopping");
        }
        ToggleState::Stopping {
            toggle_session_id, ..
        } => {
            push_toggle_event(format!(
                "toggle:{} toggle ignored while stop transition is pending",
                toggle_session_id
            ));
            debug!(
                "[toggle:{}] Ignoring toggle while stop is pending",
                toggle_session_id
            );
        }
    }
}

fn start_toggle_recording(
    toggle_state: &mut ToggleState,
    internal_tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    debug_assert!(matches!(toggle_state, ToggleState::Idle));

    let current_engine = match get_current_engine() {
        Ok(engine) => Some(engine),
        Err(e) => {
            warn!(
                "Global TOGGLE press could not read IBus engine (continuing with verified switch): {}",
                e
            );
            push_toggle_event(format!(
                "toggle:read-current-engine failed (non-fatal): {}",
                e
            ));
            None
        }
    };

    let toggle_session_id = next_toggle_session_id();
    push_toggle_event(format!("toggle:{} pressed", toggle_session_id));

    if current_engine
        .as_ref()
        .is_some_and(|engine| is_dikt_engine(engine))
    {
        mark_switch_confirm(0);
        push_toggle_event(format!(
            "toggle:{} already on dikt source",
            toggle_session_id
        ));
        bump_press_while_dikt();
        push_toggle_event(format!(
            "toggle:{} pressed while dikt already active",
            toggle_session_id
        ));
    } else {
        mark_switch_attempt();
        let current_engine_label = current_engine.as_deref().unwrap_or("<unknown>");
        push_toggle_event(format!(
            "toggle:{} switch requested from {}",
            toggle_session_id, current_engine_label
        ));
        let switch_started = Instant::now();
        let switched_engine = match switch_to_dikt_engine_verified(ENGINE_SWITCH_VERIFY_TIMEOUT_MS)
        {
            Ok(engine) => engine,
            Err(e) => {
                warn!(
                    "[toggle:{}] Failed to switch input source to Dikt on press: {}",
                    toggle_session_id, e
                );
                mark_switch_failure(&e.to_string());
                mark_health_error("ibus_switch_to_dikt_failed", &e.to_string());
                notify_toggle_failure(
                    "Cannot start recording",
                    "Failed to switch input source to Dikt (not confirmed active).",
                );
                push_toggle_event(format!(
                    "toggle:{} failed to switch to dikt engine: {}",
                    toggle_session_id, e
                ));
                return;
            }
        };
        mark_switch_confirm(switch_started.elapsed().as_millis() as u64);
        push_toggle_event(format!(
            "toggle:{} switch confirmed to {} ({} ms)",
            toggle_session_id,
            switched_engine,
            switch_started.elapsed().as_millis()
        ));
        info!(
            "[toggle:{}] Pressed; switched to Dikt source '{}' from '{}'",
            toggle_session_id, switched_engine, current_engine_label
        );
    }

    let target_engine_id = match wait_for_focused_engine(
        Duration::from_millis(FOCUSED_ENGINE_VERIFY_TIMEOUT_MS),
        Duration::from_millis(FOCUSED_ENGINE_VERIFY_POLL_MS),
    ) {
        Ok((engine_id, last_change_ms)) => {
            push_toggle_event(format!(
                "toggle:{} focused engine confirmed id={} (last_change_ms={})",
                toggle_session_id, engine_id, last_change_ms
            ));
            engine_id
        }
        Err(e) => {
            warn!(
                "[toggle:{}] Dikt engine did not become focused in the target context: {}",
                toggle_session_id, e
            );
            mark_start_failure("focused_engine_unavailable", &e);
            mark_health_error("focused_engine_unavailable", &e);
            notify_toggle_failure(
                "Cannot start recording",
                "Dikt input source is not focused in the target text field.",
            );
            push_toggle_event(format!(
                "toggle:{} blocked start because focused engine is unavailable: {}",
                toggle_session_id, e
            ));
            return;
        }
    };

    spawn_start_recording(toggle_session_id, target_engine_id, internal_tx.clone());
    *toggle_state = ToggleState::Pending { toggle_session_id };
    mark_toggle_state("pending");
    clear_pending_commit();
}

fn handle_internal_event(toggle_state: &mut ToggleState, internal: InternalEvent) {
    match internal {
        InternalEvent::StartRecording {
            toggle_session_id,
            result,
        } => {
            on_start_recording_result(toggle_state, toggle_session_id, result);
        }
        InternalEvent::StopRecording {
            toggle_session_id,
            result,
        } => {
            on_stop_recording_result(toggle_state, toggle_session_id, result);
        }
    }
}

fn on_start_recording_result(
    toggle_state: &mut ToggleState,
    toggle_session_id: u64,
    result: std::result::Result<(u64, String), String>,
) {
    match toggle_state {
        ToggleState::Pending {
            toggle_session_id: active_session,
        } if *active_session == toggle_session_id => match result {
            Ok((daemon_session_id, claim_token)) => {
                clear_start_failure();
                clear_stop_failure();
                info!(
                    "[toggle:{}] Recording started with daemon session {}",
                    toggle_session_id, daemon_session_id
                );
                *toggle_state = ToggleState::Recording {
                    toggle_session_id,
                    daemon_session_id,
                    claim_token,
                };
                mark_toggle_state("recording");
                push_toggle_event(format!(
                    "toggle:{} started daemon session {}",
                    toggle_session_id, daemon_session_id
                ));
            }
            Err(err) => {
                warn!(
                    "[toggle:{}] Failed to start recording: {}",
                    toggle_session_id, err
                );
                let failure_code = extract_start_failure_code(&err);
                mark_start_failure(&failure_code, &err);
                mark_health_error("start_recording_failed", &err);
                notify_toggle_failure(
                    "Cannot start recording",
                    &format!(
                        "Toggle dictation start failed ({})",
                        extract_start_failure_code(&err)
                    ),
                );
                push_toggle_event(format!(
                    "toggle:{} start failed: {}",
                    toggle_session_id, err
                ));
                *toggle_state = ToggleState::Idle;
                mark_toggle_state("idle");
                clear_pending_commit();
            }
        },
        _ => {
            if let Ok((daemon_session_id, _claim_token)) = result {
                warn!(
                    "[toggle:{}] Received stale start success, cancelling recording to avoid orphan state",
                    toggle_session_id
                );
                spawn_cancel_recording(daemon_session_id, "stale start success");
                push_toggle_event(format!(
                    "toggle:{} stale start success cancelled",
                    toggle_session_id
                ));
            } else {
                debug!(
                    "[toggle:{}] Ignoring stale start failure for inactive session",
                    toggle_session_id
                );
                push_toggle_event(format!(
                    "toggle:{} stale start failure ignored",
                    toggle_session_id
                ));
            }
        }
    }
}

fn on_stop_recording_result(
    toggle_state: &mut ToggleState,
    toggle_session_id: u64,
    result: StopRecordingOutcome,
) {
    match toggle_state {
        ToggleState::Stopping {
            toggle_session_id: active_session,
            daemon_session_id,
        } if *active_session == toggle_session_id => {
            match result {
                StopRecordingOutcome::Acknowledged => {
                    info!(
                        "[toggle:{}] StopRecordingSession({}) acknowledged",
                        toggle_session_id, daemon_session_id
                    );
                    clear_stop_failure();
                    mark_pending_commit(*daemon_session_id);
                    push_toggle_event(format!(
                        "toggle:{} stop acknowledged for daemon session {}",
                        toggle_session_id, daemon_session_id
                    ));
                    push_toggle_event(format!(
                        "toggle:{} stop-complete for session {}; commit is delivered by engine-side pending commit listener",
                        toggle_session_id, daemon_session_id
                    ));
                    *toggle_state = ToggleState::Idle;
                    mark_toggle_state("idle");
                    return;
                }
                StopRecordingOutcome::Finalizing { reason, timed_out } => {
                    if timed_out {
                        bump_stop_timeout_fallback();
                    }
                    clear_stop_failure();
                    mark_pending_commit(*daemon_session_id);
                    info!(
                        "[toggle:{}] StopRecordingSession({}) finalizing asynchronously: {}",
                        toggle_session_id, daemon_session_id, reason
                    );
                    push_toggle_event(format!(
                        "toggle:{} stop finalized asynchronously for daemon session {}: {}",
                        toggle_session_id, daemon_session_id, reason
                    ));
                    *toggle_state = ToggleState::Idle;
                    mark_toggle_state("idle");
                    return;
                }
                StopRecordingOutcome::Failed(err) => {
                    warn!(
                        "[toggle:{}] StopRecordingSession({}) failed: {}",
                        toggle_session_id, daemon_session_id, err
                    );
                    mark_health_error("stop_recording_failed", &err);
                    mark_stop_failure(&err);
                    push_toggle_event(format!(
                        "toggle:{} stop failed for daemon session {}: {}",
                        toggle_session_id, daemon_session_id, err
                    ));
                }
            }

            clear_pending_commit();
            *toggle_state = ToggleState::Idle;
            mark_toggle_state("idle");
            push_toggle_event(format!(
                "toggle:{} stop result handled (failure path)",
                toggle_session_id
            ));
        }
        _ => {
            debug!(
                "[toggle:{}] Ignoring stale stop result for inactive session",
                toggle_session_id
            );
            push_toggle_event(format!(
                "toggle:{} stale stop result ignored",
                toggle_session_id
            ));
        }
    }
}

fn cleanup_state(toggle_state: &mut ToggleState) {
    match toggle_state {
        ToggleState::Idle => {}
        ToggleState::Pending { .. } => {
            // Start request is still in flight and no daemon session id is known yet.
        }
        ToggleState::Recording {
            toggle_session_id: _,
            daemon_session_id,
            claim_token: _,
        } => {
            let sid = *daemon_session_id;
            spawn_cancel_recording(sid, "cleanup");
        }
        ToggleState::Stopping {
            toggle_session_id: _,
            daemon_session_id,
        } => {
            let sid = *daemon_session_id;
            spawn_cancel_recording(sid, "cleanup after stop pending");
        }
    }

    *toggle_state = ToggleState::Idle;
    mark_toggle_state("idle");
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn spawn_start_recording(
    toggle_session_id: u64,
    target_engine_id: u64,
    tx: mpsc::UnboundedSender<InternalEvent>,
) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(START_RECORDING_ARM_DELAY_MS));
        let result = call_dikt_start_recording_session_for_target(target_engine_id);
        let _ = tx.send(InternalEvent::StartRecording {
            toggle_session_id,
            result,
        });
    });
}

fn spawn_stop_recording(
    toggle_session_id: u64,
    daemon_session_id: u64,
    _claim_token: String,
    tx: mpsc::UnboundedSender<InternalEvent>,
) {
    std::thread::spawn(move || {
        let result = match call_dikt_stop_recording_session_with_timeout(
            daemon_session_id,
            Duration::from_millis(STOP_RECORDING_TIMEOUT_MS),
        ) {
            Ok(true) => StopRecordingOutcome::Acknowledged,
            Ok(false) => {
                let cancel_result = call_dikt_cancel_recording_session(daemon_session_id);
                StopRecordingOutcome::Failed(match cancel_result {
                    Ok(()) => {
                        "StopRecordingSession returned false; fallback CancelRecordingSession succeeded".to_string()
                    }
                    Err(cancel_err) => format!(
                        "StopRecordingSession returned false; fallback CancelRecordingSession failed: {}",
                        cancel_err
                    ),
                })
            }
            Err(stop_err) => {
                let is_recording = call_dikt_get_state()
                    .map(|(active, _)| active)
                    .unwrap_or(true);

                if !is_recording {
                    let timed_out = matches!(stop_err, StopRecordingCallError::TimedOut);
                    let reason = match stop_err {
                        StopRecordingCallError::TimedOut => format!(
                            "Stop call timed out after {} ms, daemon reports recording stopped; waiting for final commit",
                            STOP_RECORDING_TIMEOUT_MS
                        ),
                        StopRecordingCallError::Disconnected => "Stop call worker disconnected, daemon reports recording stopped; waiting for final commit".to_string(),
                        StopRecordingCallError::Failed(err) => format!(
                            "Stop call returned error ('{}'), daemon reports recording stopped; waiting for final commit",
                            err
                        ),
                    };
                    StopRecordingOutcome::Finalizing { reason, timed_out }
                } else {
                    let stop_detail = match stop_err {
                        StopRecordingCallError::TimedOut => format!(
                            "StopRecordingSession call timed out after {} ms",
                            STOP_RECORDING_TIMEOUT_MS
                        ),
                        StopRecordingCallError::Disconnected => {
                            "StopRecordingSession call worker disconnected before returning"
                                .to_string()
                        }
                        StopRecordingCallError::Failed(err) => err,
                    };
                    let cancel_result = call_dikt_cancel_recording_session(daemon_session_id);
                    StopRecordingOutcome::Failed(match cancel_result {
                        Ok(()) => format!(
                            "{}; fallback CancelRecordingSession({}) succeeded",
                            stop_detail, daemon_session_id
                        ),
                        Err(cancel_err) => {
                            format!(
                                "{}; fallback CancelRecordingSession({}) failed: {}",
                                stop_detail, daemon_session_id, cancel_err
                            )
                        }
                    })
                }
            }
        };
        let _ = tx.send(InternalEvent::StopRecording {
            toggle_session_id,
            result,
        });
    });
}

fn spawn_cancel_recording(session_id: u64, reason: &'static str) {
    std::thread::spawn(
        move || match call_dikt_cancel_recording_session(session_id) {
            Ok(()) => {
                info!("[toggle:{}] Cancelled recording ({})", session_id, reason);
            }
            Err(e) => {
                warn!(
                    "[toggle:{}] Failed to cancel recording ({}): {}",
                    session_id, reason, e
                );
            }
        },
    );
}

fn call_dikt_cancel_recording_session(session_id: u64) -> std::result::Result<(), String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| {
        let msg = format!("Failed to open session bus: {}", e);
        mark_dbus_error("CancelRecordingSession", &msg);
        msg
    })?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "CancelRecordingSession",
            &(session_id,),
        )
        .map_err(|e| {
            let msg = format!("CancelRecordingSession call failed: {}", e);
            mark_dbus_error("CancelRecordingSession", &msg);
            msg
        })?;
    let cancelled = reply.body().deserialize::<bool>().map_err(|e| {
        let msg = format!("CancelRecordingSession decode failed: {}", e);
        mark_dbus_error("CancelRecordingSession", &msg);
        msg
    })?;
    if !cancelled {
        return Err(format!(
            "CancelRecordingSession returned false for session {}",
            session_id
        ));
    }
    Ok(())
}

fn call_dikt_start_recording_session_for_target(
    target_engine_id: u64,
) -> std::result::Result<(u64, String), String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| {
        let msg = format!("Failed to open session bus: {}", e);
        mark_dbus_error("StartRecordingSessionForTarget", &msg);
        msg
    })?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "StartRecordingSessionForTarget",
            &(target_engine_id,),
        )
        .map_err(|e| {
            let msg = format!("StartRecordingSessionForTarget call failed: {}", e);
            mark_dbus_error("StartRecordingSessionForTarget", &msg);
            msg
        })?;
    reply.body().deserialize::<(u64, String)>().map_err(|e| {
        let msg = format!("StartRecordingSessionForTarget decode failed: {}", e);
        mark_dbus_error("StartRecordingSessionForTarget", &msg);
        msg
    })
}

fn call_dikt_get_focused_engine() -> std::result::Result<(u64, u64), String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| {
        let msg = format!("Failed to open session bus: {}", e);
        mark_dbus_error("GetFocusedEngine", &msg);
        msg
    })?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetFocusedEngine",
            &(),
        )
        .map_err(|e| {
            let msg = format!("GetFocusedEngine call failed: {}", e);
            mark_dbus_error("GetFocusedEngine", &msg);
            msg
        })?;
    reply.body().deserialize::<(u64, u64)>().map_err(|e| {
        let msg = format!("GetFocusedEngine decode failed: {}", e);
        mark_dbus_error("GetFocusedEngine", &msg);
        msg
    })
}

fn call_dikt_get_state() -> std::result::Result<(bool, bool), String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| {
        let msg = format!("Failed to open session bus: {}", e);
        mark_dbus_error("GetState", &msg);
        msg
    })?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetState",
            &(),
        )
        .map_err(|e| {
            let msg = format!("GetState call failed: {}", e);
            mark_dbus_error("GetState", &msg);
            msg
        })?;
    reply.body().deserialize::<(bool, bool)>().map_err(|e| {
        let msg = format!("GetState decode failed: {}", e);
        mark_dbus_error("GetState", &msg);
        msg
    })
}

fn wait_for_focused_engine(
    timeout: Duration,
    poll_interval: Duration,
) -> std::result::Result<(u64, u64), String> {
    let start = Instant::now();
    let mut last_focused_engine_id = 0_u64;
    let mut last_change_ms = 0_u64;
    let last_error = loop {
        let error_text = match call_dikt_get_focused_engine() {
            Ok((engine_id, change_ms)) => {
                last_focused_engine_id = engine_id;
                last_change_ms = change_ms;
                mark_focused_engine_status(engine_id, change_ms);
                if engine_id != 0 {
                    return Ok((engine_id, change_ms));
                }
                format!(
                    "Focused Dikt engine is unavailable (last_change_ms={})",
                    change_ms
                )
            }
            Err(e) => e,
        };

        if start.elapsed() >= timeout {
            break error_text;
        }
        std::thread::sleep(poll_interval);
    };

    mark_focused_engine_status(0, last_change_ms);
    Err(format!(
        "Dikt engine did not report a focused context within {} ms (last_focused_engine_id={} last_change_ms={} last_error='{}')",
        timeout.as_millis(),
        last_focused_engine_id,
        last_change_ms,
        last_error
    ))
}

fn call_dikt_stop_recording_session(session_id: u64) -> std::result::Result<bool, String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| {
        let msg = format!("Failed to open session bus: {}", e);
        mark_dbus_error("StopRecordingSession", &msg);
        msg
    })?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "StopRecordingSession",
            &(session_id,),
        )
        .map_err(|e| {
            let msg = format!("StopRecordingSession call failed: {}", e);
            mark_dbus_error("StopRecordingSession", &msg);
            msg
        })?;
    reply.body().deserialize::<bool>().map_err(|e| {
        let msg = format!("StopRecordingSession decode failed: {}", e);
        mark_dbus_error("StopRecordingSession", &msg);
        msg
    })
}

fn call_dikt_stop_recording_session_with_timeout(
    session_id: u64,
    timeout: Duration,
) -> std::result::Result<bool, StopRecordingCallError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(call_dikt_stop_recording_session(session_id));
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => result.map_err(StopRecordingCallError::Failed),
        Err(RecvTimeoutError::Timeout) => {
            let msg = format!(
                "StopRecordingSession call timed out after {} ms",
                timeout.as_millis()
            );
            mark_dbus_error("StopRecordingSession", &msg);
            Err(StopRecordingCallError::TimedOut)
        }
        Err(RecvTimeoutError::Disconnected) => {
            let msg = "StopRecordingSession call worker disconnected before returning".to_string();
            mark_dbus_error("StopRecordingSession", &msg);
            Err(StopRecordingCallError::Disconnected)
        }
    }
}

fn extract_start_failure_code(err: &str) -> String {
    let needle = "Failed to start recording (";
    if let Some(start) = err.find(needle) {
        let code_start = start + needle.len();
        if let Some(end_rel) = err[code_start..].find("):") {
            return err[code_start..code_start + end_rel].trim().to_string();
        }
    }
    "start_recording_failed".to_string()
}

fn notify_toggle_failure(summary: &str, body: &str) {
    if is_restricted_session_context() {
        return;
    }

    let now = now_millis();
    if let Ok(mut health) = health_state().lock() {
        if now.saturating_sub(health.last_notification_ms) < FAILURE_NOTIFICATION_COOLDOWN_MS {
            return;
        }
        health.last_notification_ms = now;
    }

    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let notification = Notification::new()
            .summary(&summary)
            .body(&body)
            .action("default", "Show Diagnostics")
            .show();
        if let Ok(handle) = notification {
            handle.wait_for_action(|action| {
                if action == "default" || action == "clicked" {
                    if let Err(e) = open_dikt_ui(None) {
                        error!("Failed to open Dikt diagnostics UI: {}", e);
                    }
                }
            });
        }
    });
}

fn next_toggle_session_id() -> u64 {
    TOGGLE_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::is_greeter_session_from;

    #[test]
    fn greeter_user_is_restricted() {
        assert!(is_greeter_session_from(Some("gdm"), Some("user")));
        assert!(is_greeter_session_from(Some("gdm-greeter"), Some("user")));
    }

    #[test]
    fn greeter_session_class_is_restricted() {
        assert!(is_greeter_session_from(Some("testuser"), Some("greeter")));
        assert!(is_greeter_session_from(Some("testuser"), Some("Greeter")));
    }

    #[test]
    fn normal_user_session_is_not_restricted() {
        assert!(!is_greeter_session_from(Some("testuser"), Some("user")));
    }
}

// ── Shortcut config ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShortcutConfig {
    keyval: u32,
    modifiers: u32,
}

impl ShortcutConfig {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            keyval: normalize_keyval(settings.dictation_shortcut_keyval()),
            modifiers: settings.dictation_shortcut_modifiers(),
        }
    }

    /// Resolve to an evdev keybinding.
    fn resolve(&self) -> Option<EvdevKeybinding> {
        crate::key_mapping::resolve_keybinding(self.keyval, self.modifiers)
    }

    /// Human-readable description of the shortcut.
    fn human_description(&self) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.modifiers & MOD_CTRL != 0 {
            parts.push("Ctrl");
        }
        if self.modifiers & MOD_ALT != 0 {
            parts.push("Alt");
        }
        if self.modifiers & MOD_SHIFT != 0 {
            parts.push("Shift");
        }
        if self.modifiers & MOD_SUPER != 0 {
            parts.push("Super");
        }

        let key_name = gdk_keyval_to_evdev(self.keyval)
            .map(|code| format!("{:?}", evdev::Key(code)))
            .unwrap_or_else(|| format!("keyval_{:#x}", self.keyval));

        parts.push(&key_name);
        // Need to collect since key_name is a local
        let parts_owned: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
        parts_owned.join("+")
    }
}

fn normalize_keyval(keyval: u32) -> u32 {
    if (b'A' as u32..=b'Z' as u32).contains(&keyval) {
        keyval + (b'a' - b'A') as u32
    } else {
        keyval
    }
}

async fn sleep_until_retry_or_rebind(total_delay_ms: u64) {
    let mut remaining = total_delay_ms;
    while remaining > 0 {
        if FORCE_REBIND_REQUESTED.swap(false, Ordering::SeqCst) {
            return;
        }
        let step = remaining.min(200);
        tokio::time::sleep(Duration::from_millis(step)).await;
        remaining -= step;
    }
}
