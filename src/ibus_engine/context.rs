use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ibus_sys::{g_object_ref, g_object_unref, gboolean, gpointer, guint, IBusEngine};
use log::{debug, error, info, warn};
use notify_rust::Notification;
use zbus::blocking::Connection;

use crate::utils::launch::open_dikt_ui;

/// Owned reference to IBusEngine used by the command timer.
/// We hold an explicit GObject ref while the engine is active to prevent
/// use-after-free if callbacks race with engine teardown.
#[derive(Debug)]
struct EngineRef {
    ptr: *mut IBusEngine,
    engine_id: u64,
}

unsafe impl Send for EngineRef {}

impl EngineRef {
    fn new(ptr: *mut IBusEngine, engine_id: u64) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        unsafe {
            g_object_ref(ptr as gpointer);
        }
        Some(Self { ptr, engine_id })
    }
}

impl Drop for EngineRef {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                g_object_unref(self.ptr as gpointer);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct SessionClaim {
    session_id: u64,
    claim_token: String,
}

const DIKT_BUS_NAME: &str = "io.dikt.Transcription";
const DIKT_OBJECT_PATH: &str = "/io/dikt/Transcription";
const DIKT_INTERFACE: &str = "io.dikt.Transcription";
const PENDING_COMMIT_POLL_MS: u64 = 60;
const PENDING_COMMIT_FAILURE_RECONNECT_THRESHOLD: u64 = 5;
const LIVE_PREEDIT_POLL_TICKS: u64 = 4;
const LIVE_PREEDIT_REFRESH_TICKS: u64 = 5;
const COMMAND_POLL_INTERVAL_MS: u32 = 60;
const DISABLE_PENDING_COMMIT_TIMEOUT_MS: u64 = 80;

/// Commands that can be sent from background threads to be processed on the main thread.
/// Engine pointers never cross thread boundaries - only engine IDs are used.
#[derive(Debug, Clone)]
enum EngineCommand {
    UpdatePreedit {
        engine_id: u64,
        text: String,
        cursor_pos: u32,
    },
    HidePreedit {
        engine_id: u64,
    },
    CommitText {
        engine_id: u64,
        text: String,
    },
}

/// Shared command queue accessible from both threads.
/// Background thread pushes commands, timer callback on main thread processes them.
struct CommandQueue {
    commands: Vec<EngineCommand>,
}

static COMMAND_QUEUE: OnceLock<Mutex<CommandQueue>> = OnceLock::new();

fn get_command_queue() -> &'static Mutex<CommandQueue> {
    COMMAND_QUEUE.get_or_init(|| {
        Mutex::new(CommandQueue {
            commands: Vec::new(),
        })
    })
}

/// Current engine pointer and ID, only accessed from main thread via timer callback.
/// Set in enable(), cleared in disable().
static CURRENT_ENGINE: Mutex<Option<EngineRef>> = Mutex::new(None);

/// Ensures timer is only started once.
static TIMER_STARTED: AtomicBool = AtomicBool::new(false);

/// Timer callback that processes pending commands on the main thread.
/// This is a simple extern "C" function - no Rust closure trampoline that could crash.
unsafe extern "C" fn process_commands_callback(_data: gpointer) -> gboolean {
    // Get commands from queue
    let commands: Vec<EngineCommand> = {
        let mut queue = match get_command_queue().lock() {
            Ok(q) => q,
            Err(_) => return 1, // G_SOURCE_CONTINUE
        };
        std::mem::take(&mut queue.commands)
    };

    // Get current engine
    let engine_guard = match CURRENT_ENGINE.lock() {
        Ok(g) => g,
        Err(_) => return 1, // G_SOURCE_CONTINUE
    };

    if let Some(engine_ref) = engine_guard.as_ref() {
        let engine_ptr = engine_ref.ptr;
        let current_engine_id = engine_ref.engine_id;
        for cmd in commands {
            match cmd {
                EngineCommand::UpdatePreedit {
                    engine_id,
                    text,
                    cursor_pos,
                } => {
                    if engine_id == current_engine_id && !engine_ptr.is_null() {
                        debug!(
                            "Timer: UpdatePreedit engine_id={}, text_len={}",
                            engine_id,
                            text.len()
                        );
                        update_preedit_text(engine_ptr, &text, cursor_pos);
                    }
                }
                EngineCommand::HidePreedit { engine_id } => {
                    if engine_id == current_engine_id && !engine_ptr.is_null() {
                        debug!("Timer: HidePreedit engine_id={}", engine_id);
                        hide_preedit_text(engine_ptr);
                    }
                }
                EngineCommand::CommitText { engine_id, text } => {
                    if engine_id == current_engine_id && !engine_ptr.is_null() {
                        debug!(
                            "Timer: CommitText engine_id={}, text_len={}",
                            engine_id,
                            text.len()
                        );
                        hide_preedit_text(engine_ptr);
                        commit_text_to_engine(engine_ptr, &text);
                    }
                }
            }
        }
    }

    1 // G_SOURCE_CONTINUE - keep timer running
}

/// Start the command processing timer. Only starts once per process lifetime.
fn ensure_timer_started() {
    if !TIMER_STARTED.swap(true, Ordering::SeqCst) {
        unsafe {
            glib::ffi::g_timeout_add(
                COMMAND_POLL_INTERVAL_MS,
                Some(process_commands_callback),
                std::ptr::null_mut(),
            );
        }
        info!(
            "Command processing timer started ({}ms interval)",
            COMMAND_POLL_INTERVAL_MS
        );
    }
}

/// Helper to send a command from background thread
fn send_command(cmd: EngineCommand) {
    if let Ok(mut queue) = get_command_queue().lock() {
        queue.commands.push(cmd);
    }
}

fn drain_engine_commands_for_disable(engine: *mut IBusEngine, engine_id: u64) -> usize {
    if engine.is_null() {
        return 0;
    }

    let pending = match get_command_queue().lock() {
        Ok(mut queue) => std::mem::take(&mut queue.commands),
        Err(_) => return 0,
    };

    let mut remaining = Vec::new();
    let mut commits = Vec::new();
    let mut hide_requested = false;

    for cmd in pending {
        match cmd {
            EngineCommand::UpdatePreedit {
                engine_id: cmd_engine_id,
                ..
            } if cmd_engine_id == engine_id => {
                // Disable path intentionally drops stale preedit updates.
            }
            EngineCommand::HidePreedit {
                engine_id: cmd_engine_id,
            } if cmd_engine_id == engine_id => {
                hide_requested = true;
            }
            EngineCommand::CommitText {
                engine_id: cmd_engine_id,
                text,
            } if cmd_engine_id == engine_id => {
                commits.push(text);
                hide_requested = true;
            }
            _ => remaining.push(cmd),
        }
    }

    if let Ok(mut queue) = get_command_queue().lock() {
        if queue.commands.is_empty() {
            queue.commands = remaining;
        } else {
            remaining.append(&mut queue.commands);
            queue.commands = remaining;
        }
    }

    if hide_requested {
        hide_preedit_text(engine);
    }

    for text in &commits {
        commit_text_to_engine(engine, text);
    }

    commits.len()
}

pub struct DiktContext {
    connection: Option<Connection>,
    is_focused: bool,
    is_enabled: bool,
    notification_shown: bool,
    pending_commit_cancel: Option<Arc<AtomicBool>>,
    current_engine_id: Option<u64>,
    last_session_claim: Arc<Mutex<Option<SessionClaim>>>,
}

impl DiktContext {
    pub fn new() -> Self {
        Self {
            connection: None,
            is_focused: false,
            is_enabled: false,
            notification_shown: false,
            pending_commit_cancel: None,
            current_engine_id: None,
            last_session_claim: Arc::new(Mutex::new(None)),
        }
    }

    fn try_connect(&mut self) -> bool {
        if self.connection.is_some() {
            return true;
        }

        match Connection::session() {
            Ok(conn) => {
                self.connection = Some(conn);
                info!("Connected to D-Bus session bus");
                true
            }
            Err(e) => {
                error!("Failed to connect to D-Bus: {}", e);
                false
            }
        }
    }

    pub fn focus_in(&mut self, _engine: *mut IBusEngine) {
        info!("IBus focus_in: engine={:?}", _engine);
        self.is_focused = true;
        self.set_focused_engine_state(_engine, true);
    }

    pub fn focus_out(&mut self, engine: *mut IBusEngine) {
        info!("IBus focus_out: engine={:?}", engine);
        self.is_focused = false;
        hide_preedit_text(engine);
        self.set_focused_engine_state(engine, false);
    }

    pub fn reset(&mut self, _engine: *mut IBusEngine) {
        debug!("Reset");
    }

    pub fn enable(&mut self, engine: *mut IBusEngine) {
        debug!("Engine enabled");
        self.is_enabled = true;

        let engine_id = engine as u64;
        self.current_engine_id = Some(engine_id);

        // Store current engine in static for timer callback access
        if let Ok(mut current) = CURRENT_ENGINE.lock() {
            *current = EngineRef::new(engine, engine_id);
        } else {
            warn!("Failed to store active engine reference: lock poisoned");
        }

        // Ensure command processing timer is running
        ensure_timer_started();

        if self.connection.is_none() && !self.try_connect() {
            return;
        }

        self.set_focused_engine_state(engine, self.is_focused);
        self.ensure_pending_commit_listener(engine_id);

        if !self.notification_shown {
            self.notification_shown = true;
            std::thread::spawn(|| {
                let conn = match Connection::session() {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!("Failed to open D-Bus session for GetState: {}", e);
                        DiktContext::show_service_notification();
                        return;
                    }
                };

                match conn.call_method(
                    Some(DIKT_BUS_NAME),
                    DIKT_OBJECT_PATH,
                    Some(DIKT_INTERFACE),
                    "GetState",
                    &(),
                ) {
                    Ok(reply) => {
                        if let Ok((_, has_model)) = reply.body().deserialize::<(bool, bool)>() {
                            if !has_model {
                                DiktContext::show_model_notification();
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to get state from daemon: {}", e);
                        DiktContext::show_service_notification();
                    }
                }
            });
        }
    }

    fn ensure_pending_commit_listener(&mut self, engine_id: u64) {
        if self.pending_commit_cancel.is_some() {
            if self.current_engine_id == Some(engine_id) {
                return;
            }
            self.stop_pending_commit_listener();
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let last_session_claim = self.last_session_claim.clone();

        self.pending_commit_cancel = Some(cancel.clone());

        // Note: Engine pointer NEVER crosses thread boundaries.
        // We only pass the engine_id, and commands are sent via the command queue.
        // The main thread processes commands via the timer callback and safely
        // accesses the engine pointer there.

        std::thread::spawn(move || {
            let mut conn = match Connection::session() {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to create pending commit DBus connection: {}", e);
                    return;
                }
            };
            let mut failure_streak: u64 = 0;
            let mut poll_tick: u64 = 0;
            let mut live_preedit_supported = true;
            let mut last_live_revision: u64 = 0;
            let mut last_live_visible = false;
            let mut last_live_text = String::new();
            let mut live_refresh_tick: u64 = 0;
            let mut active_session_id: u64 = 0;
            let mut active_claim_token = String::new();

            while !cancel.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(PENDING_COMMIT_POLL_MS));
                if cancel.load(Ordering::SeqCst) {
                    break;
                }

                poll_tick = poll_tick.wrapping_add(1);

                let active_reply = conn.call_method(
                    Some(DIKT_BUS_NAME),
                    DIKT_OBJECT_PATH,
                    Some(DIKT_INTERFACE),
                    "GetActiveSessionForEngine",
                    &(engine_id,),
                );

                let (next_session_id, next_claim_token, next_allow_preedit) = match active_reply {
                    Ok(reply) => match reply.body().deserialize::<(u64, String, bool)>() {
                        Ok(payload) => payload,
                        Err(_) => {
                            warn!("GetActiveSessionForEngine returned an invalid payload");
                            continue;
                        }
                    },
                    Err(e) => {
                        failure_streak = failure_streak.saturating_add(1);
                        if failure_streak == 1 || failure_streak.is_multiple_of(10) {
                            warn!(
                                "GetActiveSessionForEngine call failed (streak={}): {}",
                                failure_streak, e
                            );
                        }
                        if failure_streak >= PENDING_COMMIT_FAILURE_RECONNECT_THRESHOLD {
                            match Connection::session() {
                                Ok(new_conn) => {
                                    warn!(
                                        "Reconnected pending commit listener DBus session after {} failures",
                                        failure_streak
                                    );
                                    conn = new_conn;
                                    failure_streak = 0;
                                }
                                Err(reconnect_err) => {
                                    warn!(
                                        "Pending commit listener reconnect failed after {} errors: {}",
                                        failure_streak, reconnect_err
                                    );
                                }
                            }
                        }
                        continue;
                    }
                };

                if failure_streak > 0 {
                    info!(
                        "Pending commit listener recovered after {} consecutive errors",
                        failure_streak
                    );
                    failure_streak = 0;
                }

                if next_session_id != active_session_id || next_claim_token != active_claim_token {
                    if last_live_visible {
                        send_command(EngineCommand::HidePreedit { engine_id });
                        last_live_visible = false;
                    }
                    last_live_revision = 0;
                    last_live_text.clear();
                    live_refresh_tick = 0;
                }

                active_session_id = next_session_id;
                active_claim_token = next_claim_token;

                if let Ok(mut guard) = last_session_claim.lock() {
                    *guard = if active_session_id != 0 && !active_claim_token.is_empty() {
                        Some(SessionClaim {
                            session_id: active_session_id,
                            claim_token: active_claim_token.clone(),
                        })
                    } else {
                        None
                    };
                }

                if active_session_id == 0 || active_claim_token.is_empty() {
                    continue;
                }

                if live_preedit_supported
                    && next_allow_preedit
                    && poll_tick.is_multiple_of(LIVE_PREEDIT_POLL_TICKS)
                {
                    match conn.call_method(
                        Some(DIKT_BUS_NAME),
                        DIKT_OBJECT_PATH,
                        Some(DIKT_INTERFACE),
                        "GetLivePreeditForSession",
                        &(active_session_id, active_claim_token.clone()),
                    ) {
                        Ok(live_reply) => {
                            match live_reply.body().deserialize::<(u64, bool, String)>() {
                                Ok((revision, visible, text)) => {
                                    let preedit_text = text.trim().to_string();
                                    let should_show = visible && !preedit_text.is_empty();
                                    let should_apply = should_show
                                        && (revision > last_live_revision
                                            || !last_live_visible
                                            || preedit_text != last_live_text
                                            || live_refresh_tick >= LIVE_PREEDIT_REFRESH_TICKS);
                                    let should_hide = !should_show
                                        && (last_live_visible || revision > last_live_revision);

                                    if cancel.load(Ordering::SeqCst) {
                                        break;
                                    }

                                    if should_apply {
                                        let text_len = preedit_text.chars().count() as u32;
                                        send_command(EngineCommand::UpdatePreedit {
                                            engine_id,
                                            text: preedit_text.clone(),
                                            cursor_pos: text_len,
                                        });
                                        live_refresh_tick = 0;
                                    } else if should_hide {
                                        send_command(EngineCommand::HidePreedit { engine_id });
                                        live_refresh_tick = 0;
                                    } else {
                                        live_refresh_tick = live_refresh_tick.saturating_add(1);
                                    }

                                    last_live_revision = last_live_revision.max(revision);
                                    last_live_visible = should_show;
                                    if should_show {
                                        last_live_text = preedit_text;
                                    } else {
                                        last_live_text.clear();
                                    }
                                }
                                Err(_) => {
                                    warn!("GetLivePreeditForSession returned an invalid payload");
                                }
                            }
                        }
                        Err(e) => {
                            let detail = e.to_string();
                            if detail.contains("UnknownMethod") {
                                warn!(
                                    "GetLivePreeditForSession unavailable; disabling live preedit polling"
                                );
                                live_preedit_supported = false;
                            } else if poll_tick == 1 || poll_tick.is_multiple_of(50) {
                                warn!("GetLivePreeditForSession call failed: {}", detail);
                            }
                        }
                    }
                } else if !next_allow_preedit && last_live_visible {
                    send_command(EngineCommand::HidePreedit { engine_id });
                    last_live_visible = false;
                    last_live_text.clear();
                    live_refresh_tick = 0;
                }

                let reply = conn.call_method(
                    Some(DIKT_BUS_NAME),
                    DIKT_OBJECT_PATH,
                    Some(DIKT_INTERFACE),
                    "TakePendingCommitForSession",
                    &(active_session_id, active_claim_token.clone()),
                );

                let reply = match reply {
                    Ok(reply) => reply,
                    Err(e) => {
                        failure_streak = failure_streak.saturating_add(1);
                        if failure_streak == 1 || failure_streak.is_multiple_of(10) {
                            warn!(
                                "TakePendingCommitForSession call failed (streak={}): {}",
                                failure_streak, e
                            );
                        }
                        if failure_streak >= PENDING_COMMIT_FAILURE_RECONNECT_THRESHOLD {
                            match Connection::session() {
                                Ok(new_conn) => {
                                    warn!(
                                        "Reconnected pending commit listener DBus session after {} failures",
                                        failure_streak
                                    );
                                    conn = new_conn;
                                    failure_streak = 0;
                                }
                                Err(reconnect_err) => {
                                    warn!(
                                        "Pending commit listener reconnect failed after {} errors: {}",
                                        failure_streak, reconnect_err
                                    );
                                }
                            }
                        }
                        continue;
                    }
                };
                if failure_streak > 0 {
                    info!(
                        "Pending commit listener recovered after {} consecutive errors",
                        failure_streak
                    );
                    failure_streak = 0;
                }
                let Ok((has_text, text)) = reply.body().deserialize::<(bool, String)>() else {
                    warn!("TakePendingCommitForSession returned an invalid payload");
                    continue;
                };

                if !has_text {
                    continue;
                }

                let final_text = text.trim().to_string();
                if final_text.is_empty() {
                    continue;
                }

                if cancel.load(Ordering::SeqCst) {
                    break;
                }

                info!(
                    "Pending commit ready: session={}, text_len={}",
                    active_session_id,
                    final_text.len()
                );

                send_command(EngineCommand::CommitText {
                    engine_id,
                    text: final_text,
                });
            }
        });
    }

    fn stop_pending_commit_listener(&mut self) {
        if let Some(cancel) = self.pending_commit_cancel.take() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    fn set_focused_engine_state(&mut self, engine: *mut IBusEngine, focused: bool) {
        if engine.is_null() {
            return;
        }
        let engine_id = engine as usize as u64;
        std::thread::spawn(move || {
            let conn = match Connection::session() {
                Ok(conn) => conn,
                Err(e) => {
                    warn!(
                        "SetFocusedEngine(engine_id={}, focused={}) failed to open session bus: {}",
                        engine_id, focused, e
                    );
                    return;
                }
            };

            if let Err(e) = conn.call_method(
                Some(DIKT_BUS_NAME),
                DIKT_OBJECT_PATH,
                Some(DIKT_INTERFACE),
                "SetFocusedEngine",
                &(engine_id, focused),
            ) {
                warn!(
                    "SetFocusedEngine(engine_id={}, focused={}) failed: {}",
                    engine_id, focused, e
                );
            }
        });
    }

    fn show_model_notification() {
        debug!("Showing model notification");

        std::thread::spawn(|| {
            let notification = Notification::new()
                .summary("Dikt Speech-to-Text")
                .body("No speech model configured. Click to open preferences.")
                .timeout(notify_rust::Timeout::Never)
                .action("default", "Open Preferences")
                .show();

            match notification {
                Ok(handle) => {
                    handle.wait_for_action(|action| {
                        if action == "default" || action == "clicked" {
                            info!("Notification clicked, opening Dikt GUI");
                            if let Err(e) = open_dikt_ui(None) {
                                error!("Failed to spawn dikt: {}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to show notification: {}", e);
                }
            }
        });
    }

    fn show_service_notification() {
        debug!("Showing service notification");

        std::thread::spawn(|| {
            let notification = Notification::new()
                .summary("Dikt Speech-to-Text")
                .body("Dikt service is not running. Click to open preferences and start it.")
                .timeout(notify_rust::Timeout::Never)
                .action("default", "Open Preferences")
                .show();

            match notification {
                Ok(handle) => {
                    handle.wait_for_action(|action| {
                        if action == "default" || action == "clicked" {
                            info!("Service notification clicked, opening Dikt GUI");
                            if let Err(e) = open_dikt_ui(None) {
                                error!("Failed to spawn dikt: {}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to show notification: {}", e);
                }
            }
        });
    }

    pub fn disable(&mut self, engine: *mut IBusEngine) {
        debug!("Engine disabled");

        let engine_id = engine as usize as u64;

        self.set_focused_engine_state(engine, false);
        self.stop_pending_commit_listener();
        let queued_commits = drain_engine_commands_for_disable(engine, engine_id);
        self.commit_pending_transcription(engine);
        let queued_commits = queued_commits + drain_engine_commands_for_disable(engine, engine_id);

        if queued_commits > 0 {
            debug!(
                "Disable path committed {} queued pending transcript(s) before teardown",
                queued_commits
            );
        }

        // Clear current engine in static after draining pending commands.
        if let Ok(mut current) = CURRENT_ENGINE.lock() {
            *current = None;
        } else {
            warn!("Failed to clear active engine reference: lock poisoned");
        }

        hide_preedit_text(engine);
        self.is_enabled = false;
        self.is_focused = false;
        self.notification_shown = false;
        self.current_engine_id = None;
        if let Ok(mut claim) = self.last_session_claim.lock() {
            *claim = None;
        }
    }

    pub fn process_key_event(
        &mut self,
        _engine: *mut IBusEngine,
        _keyval: guint,
        _keycode: guint,
        _modifiers: guint,
    ) -> gboolean {
        0
    }

    fn commit_pending_transcription(&mut self, engine: *mut IBusEngine) {
        let session_claim = self
            .last_session_claim
            .lock()
            .ok()
            .and_then(|claim| claim.clone());
        let Some(session_claim) = session_claim else {
            debug!("No session claim available on engine disable");
            return;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = Connection::session()
                .ok()
                .and_then(|conn| {
                    conn.call_method(
                        Some(DIKT_BUS_NAME),
                        DIKT_OBJECT_PATH,
                        Some(DIKT_INTERFACE),
                        "TakePendingCommitForSession",
                        &(session_claim.session_id, session_claim.claim_token.clone()),
                    )
                    .ok()
                })
                .and_then(|reply| reply.body().deserialize::<(bool, String)>().ok())
                .unwrap_or((false, String::new()));
            let _ = tx.send(result);
        });

        let (has_text, text) =
            match rx.recv_timeout(Duration::from_millis(DISABLE_PENDING_COMMIT_TIMEOUT_MS)) {
                Ok(value) => value,
                Err(_) => {
                    debug!(
                        "TakePendingCommitForSession timed out on disable after {} ms",
                        DISABLE_PENDING_COMMIT_TIMEOUT_MS
                    );
                    return;
                }
            };

        if !has_text {
            debug!("No pending commit payload found on engine disable");
            return;
        }

        let trimmed = text.trim();
        if trimmed.is_empty() {
            debug!("No pending commit payload found on engine disable");
            return;
        }

        info!(
            "Committing pending transcription from session {} ({} chars)",
            session_claim.session_id,
            trimmed.chars().count()
        );
        hide_preedit_text(engine);
        commit_text_to_engine(engine, trimmed);
    }
}

fn update_preedit_text(engine: *mut IBusEngine, text: &str, cursor_pos: u32) {
    if engine.is_null() {
        return;
    }

    let c_text = match CString::new(text) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create preedit CString: {}", e);
            return;
        }
    };

    unsafe {
        let ibus_text = ibus_sys::ibus_text_new_from_string(c_text.as_ptr());
        if !ibus_text.is_null() {
            ibus_sys::ibus_engine_update_preedit_text(
                engine,
                ibus_text,
                cursor_pos as guint,
                1 as gboolean,
            );
            ibus_sys::ibus_engine_show_preedit_text(engine);
        }
    }
}

fn hide_preedit_text(engine: *mut IBusEngine) {
    if engine.is_null() {
        return;
    }
    unsafe {
        ibus_sys::ibus_engine_hide_preedit_text(engine);
    }
}

fn commit_text_to_engine(engine: *mut IBusEngine, text: &str) {
    let preview: String = text.chars().take(50).collect();
    info!("Committing text: {}...", preview);

    let c_text = match CString::new(text) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create CString: {}", e);
            return;
        }
    };

    unsafe {
        let ibus_text = ibus_sys::ibus_text_new_from_string(c_text.as_ptr());
        if !ibus_text.is_null() {
            ibus_sys::ibus_engine_commit_text(engine, ibus_text);
        }
    }
}

pub type SharedContext = Arc<Mutex<DiktContext>>;

#[allow(clippy::arc_with_non_send_sync)]
pub fn create_context() -> SharedContext {
    Arc::new(Mutex::new(DiktContext::new()))
}

unsafe extern "C" fn process_key_event_callback(
    context: *mut c_void,
    engine: *mut IBusEngine,
    keyval: guint,
    keycode: guint,
    modifiers: guint,
) -> gboolean {
    if context.is_null() || engine.is_null() {
        return 0;
    }
    let context = &*(context as *const Mutex<DiktContext>);
    if let Ok(mut ctx) = context.lock() {
        ctx.process_key_event(engine, keyval, keycode, modifiers)
    } else {
        0
    }
}

unsafe extern "C" fn focus_in_callback(context: *mut c_void, engine: *mut IBusEngine) {
    if context.is_null() || engine.is_null() {
        return;
    }
    let context = &*(context as *const Mutex<DiktContext>);
    if let Ok(mut ctx) = context.lock() {
        ctx.focus_in(engine);
    }
}

unsafe extern "C" fn focus_out_callback(context: *mut c_void, engine: *mut IBusEngine) {
    if context.is_null() || engine.is_null() {
        return;
    }
    let context = &*(context as *const Mutex<DiktContext>);
    if let Ok(mut ctx) = context.lock() {
        ctx.focus_out(engine);
    }
}

unsafe extern "C" fn reset_callback(context: *mut c_void, engine: *mut IBusEngine) {
    if context.is_null() || engine.is_null() {
        return;
    }
    let context = &*(context as *const Mutex<DiktContext>);
    if let Ok(mut ctx) = context.lock() {
        ctx.reset(engine);
    }
}

unsafe extern "C" fn enable_callback(context: *mut c_void, engine: *mut IBusEngine) {
    if context.is_null() || engine.is_null() {
        return;
    }
    let context = &*(context as *const Mutex<DiktContext>);
    if let Ok(mut ctx) = context.lock() {
        ctx.enable(engine);
    }
}

unsafe extern "C" fn disable_callback(context: *mut c_void, engine: *mut IBusEngine) {
    if context.is_null() || engine.is_null() {
        return;
    }
    let context = &*(context as *const Mutex<DiktContext>);
    if let Ok(mut ctx) = context.lock() {
        ctx.disable(engine);
    }
}

extern "C" {
    fn ibus_dikt_set_callback(
        ctx: *mut c_void,
        key_event_cb: unsafe extern "C" fn(
            *mut c_void,
            *mut IBusEngine,
            guint,
            guint,
            guint,
        ) -> gboolean,
        focus_in_cb: unsafe extern "C" fn(*mut c_void, *mut IBusEngine),
        focus_out_cb: unsafe extern "C" fn(*mut c_void, *mut IBusEngine),
        reset_cb: unsafe extern "C" fn(*mut c_void, *mut IBusEngine),
        enable_cb: unsafe extern "C" fn(*mut c_void, *mut IBusEngine),
        disable_cb: unsafe extern "C" fn(*mut c_void, *mut IBusEngine),
    );
}

pub fn init(context: &SharedContext) {
    unsafe {
        ibus_dikt_set_callback(
            Arc::as_ptr(context) as *mut c_void,
            process_key_event_callback,
            focus_in_callback,
            focus_out_callback,
            reset_callback,
            enable_callback,
            disable_callback,
        );
    }
}
