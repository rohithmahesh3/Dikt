use super::Page;
use crate::app::AppState;
use crate::utils::logging::read_recent_logs;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Label, Orientation, ScrolledWindow, TextView, Widget};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use zbus::blocking::Connection;

const DIKT_BUS_NAME: &str = "io.dikt.Transcription";
const DIKT_OBJECT_PATH: &str = "/io/dikt/Transcription";
const DIKT_INTERFACE: &str = "io.dikt.Transcription";
const MAX_LOG_LINES: usize = 400;
const UI_POLL_INTERVAL_MS: u64 = 80;
const DEBUG_ENGINE_ID: u64 = u64::MAX - 1;
const DEBUG_STOP_WAIT_TIMEOUT_MS: u64 = 35_000;
const DEBUG_STATUS_POLL_MS: u64 = 120;

#[derive(Clone, Debug)]
struct DebugSessionClaim {
    session_id: u64,
    claim_token: String,
}

pub struct DebugPage {
    container: Box,
    is_recording: Arc<AtomicBool>,
    active_session: Arc<Mutex<Option<DebugSessionClaim>>>,
}

impl DebugPage {
    pub fn new(state: &Arc<AppState>) -> Self {
        let container = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(12)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let test_group = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(8)
            .build();
        let test_title = Label::builder()
            .label("Transcription Testing")
            .css_classes(["title-4"])
            .halign(Align::Start)
            .build();
        test_group.append(&test_title);
        let test_help = Label::builder()
            .label("Click Start, speak, then click Stop to transcribe into the box below.")
            .halign(Align::Start)
            .wrap(true)
            .xalign(0.0)
            .build();
        test_group.append(&test_help);

        let controls_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();
        let start_btn = Button::with_label("Start Recording");
        let stop_btn = Button::with_label("Stop & Transcribe");
        stop_btn.set_sensitive(false);
        let clear_btn = Button::with_label("Clear");
        controls_box.append(&start_btn);
        controls_box.append(&stop_btn);
        controls_box.append(&clear_btn);
        test_group.append(&controls_box);

        let status_label = Label::builder()
            .label("Idle")
            .halign(Align::Start)
            .xalign(0.0)
            .build();
        test_group.append(&status_label);

        let output_buffer = gtk4::TextBuffer::new(None);
        output_buffer.set_text("No transcription yet.");
        let output_view = TextView::builder()
            .buffer(&output_buffer)
            .editable(false)
            .vexpand(false)
            .hexpand(true)
            .wrap_mode(gtk4::WrapMode::WordChar)
            .build();
        let output_scaffold = ScrolledWindow::builder()
            .min_content_height(140)
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .child(&output_view)
            .build();
        test_group.append(&output_scaffold);
        container.append(&test_group);

        let section_separator = gtk4::Separator::builder()
            .orientation(Orientation::Horizontal)
            .margin_top(8)
            .margin_bottom(4)
            .build();
        container.append(&section_separator);

        let is_recording = Arc::new(AtomicBool::new(false));
        let active_session = Arc::new(Mutex::new(None::<DebugSessionClaim>));
        let request_in_flight = Arc::new(AtomicBool::new(false));
        let update_controls = Rc::new({
            let start_btn = start_btn.clone();
            let stop_btn = stop_btn.clone();
            let is_recording = is_recording.clone();
            let request_in_flight = request_in_flight.clone();
            move || {
                let recording = is_recording.load(Ordering::SeqCst);
                let in_flight = request_in_flight.load(Ordering::SeqCst);
                start_btn.set_sensitive(!recording && !in_flight);
                stop_btn.set_sensitive(recording && !in_flight);
            }
        });

        start_btn.connect_clicked({
            let status_label = status_label.clone();
            let is_recording = is_recording.clone();
            let active_session = active_session.clone();
            let request_in_flight = request_in_flight.clone();
            let update_controls = update_controls.clone();
            move |_| {
                if request_in_flight
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    return;
                }
                status_label.set_text("Starting recording...");
                update_controls();

                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(call_start_recording());
                });

                let status_label = status_label.clone();
                let is_recording = is_recording.clone();
                let active_session = active_session.clone();
                let request_in_flight = request_in_flight.clone();
                let update_controls = update_controls.clone();
                glib::timeout_add_local(
                    std::time::Duration::from_millis(UI_POLL_INTERVAL_MS),
                    move || match rx.try_recv() {
                        Ok(result) => {
                            request_in_flight.store(false, Ordering::SeqCst);
                            match result {
                                Ok(session) => {
                                    is_recording.store(true, Ordering::SeqCst);
                                    if let Ok(mut guard) = active_session.lock() {
                                        *guard = Some(session);
                                    }
                                    status_label.set_text("Recording...");
                                }
                                Err(e) => {
                                    is_recording.store(false, Ordering::SeqCst);
                                    if let Ok(mut guard) = active_session.lock() {
                                        *guard = None;
                                    }
                                    status_label.set_text(&format!("Error: {}", e));
                                }
                            }
                            update_controls();
                            glib::ControlFlow::Break
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            request_in_flight.store(false, Ordering::SeqCst);
                            is_recording.store(false, Ordering::SeqCst);
                            status_label.set_text("Error: start worker disconnected");
                            update_controls();
                            glib::ControlFlow::Break
                        }
                    },
                );
            }
        });

        stop_btn.connect_clicked({
            let output_buffer = output_buffer.clone();
            let status_label = status_label.clone();
            let is_recording = is_recording.clone();
            let active_session = active_session.clone();
            let request_in_flight = request_in_flight.clone();
            let update_controls = update_controls.clone();
            move |_| {
                if request_in_flight
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    return;
                }
                status_label.set_text("Stopping and transcribing...");
                update_controls();

                let session = active_session.lock().ok().and_then(|guard| guard.clone());
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let result = match session {
                        Some(session_claim) => call_stop_recording_and_finalize(&session_claim),
                        None => Err("No active session for stop".to_string()),
                    };
                    let _ = tx.send(result);
                });

                let output_buffer = output_buffer.clone();
                let status_label = status_label.clone();
                let is_recording = is_recording.clone();
                let active_session = active_session.clone();
                let request_in_flight = request_in_flight.clone();
                let update_controls = update_controls.clone();
                glib::timeout_add_local(
                    std::time::Duration::from_millis(UI_POLL_INTERVAL_MS),
                    move || match rx.try_recv() {
                        Ok(result) => {
                            request_in_flight.store(false, Ordering::SeqCst);
                            is_recording.store(false, Ordering::SeqCst);
                            if let Ok(mut guard) = active_session.lock() {
                                *guard = None;
                            }
                            match result {
                                Ok(text) => {
                                    let final_text = if text.trim().is_empty() {
                                        "No speech detected.".to_string()
                                    } else {
                                        text
                                    };
                                    output_buffer.set_text(&final_text);
                                    status_label.set_text("Idle");
                                }
                                Err(e) => {
                                    status_label.set_text(&format!("Error: {}", e));
                                }
                            }
                            update_controls();
                            glib::ControlFlow::Break
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            request_in_flight.store(false, Ordering::SeqCst);
                            is_recording.store(false, Ordering::SeqCst);
                            if let Ok(mut guard) = active_session.lock() {
                                *guard = None;
                            }
                            status_label.set_text("Error: stop worker disconnected");
                            update_controls();
                            glib::ControlFlow::Break
                        }
                    },
                );
            }
        });

        clear_btn.connect_clicked({
            let output_buffer = output_buffer.clone();
            move |_| {
                output_buffer.set_text("No transcription yet.");
            }
        });

        {
            let status_label = status_label.clone();
            let is_recording = is_recording.clone();
            let update_controls = update_controls.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(call_recording_state());
            });
            glib::timeout_add_local(
                std::time::Duration::from_millis(UI_POLL_INTERVAL_MS),
                move || match rx.try_recv() {
                    Ok(result) => {
                        match result {
                            Ok(recording) => {
                                is_recording.store(recording, Ordering::SeqCst);
                                if recording {
                                    status_label.set_text("Recording...");
                                }
                            }
                            Err(e) => {
                                status_label.set_text(&format!("Status unavailable: {}", e));
                            }
                        }
                        update_controls();
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
                },
            );
        }

        let header_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .build();

        let title = gtk4::Label::builder()
            .label("Debug Logs")
            .css_classes(["title-2"])
            .halign(Align::Start)
            .hexpand(true)
            .build();
        header_box.append(&title);

        let refresh_btn = Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh Logs")
            .build();

        let log_buffer = state.log_buffer.clone();
        let text_buffer = gtk4::TextBuffer::new(None);
        let refresh_in_flight = Arc::new(AtomicBool::new(false));

        refresh_debug_view_async(&text_buffer, &log_buffer, &refresh_in_flight);

        refresh_btn.connect_clicked({
            let log_buffer = log_buffer.clone();
            let text_buffer = text_buffer.clone();
            let refresh_in_flight = refresh_in_flight.clone();
            move |_| {
                refresh_debug_view_async(&text_buffer, &log_buffer, &refresh_in_flight);
            }
        });

        header_box.append(&refresh_btn);
        container.append(&header_box);

        let scaffold = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .hexpand(true)
            .build();

        let text_view = TextView::builder()
            .buffer(&text_buffer)
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk4::WrapMode::WordChar)
            .build();

        scaffold.set_child(Some(&text_view));
        container.append(&scaffold);

        let log_buffer_clone = log_buffer.clone();
        let text_buffer_clone = text_buffer.clone();
        let refresh_in_flight_clone = refresh_in_flight.clone();
        glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            refresh_debug_view_async(
                &text_buffer_clone,
                &log_buffer_clone,
                &refresh_in_flight_clone,
            );
            glib::ControlFlow::Continue
        });

        Self {
            container,
            is_recording,
            active_session,
        }
    }
}

impl Page for DebugPage {
    fn widget(&self) -> &Widget {
        self.container.upcast_ref()
    }
}

impl Drop for DebugPage {
    fn drop(&mut self) {
        if self.is_recording.load(Ordering::SeqCst) {
            let session = self
                .active_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            if let Some(session) = session {
                std::thread::spawn(move || {
                    let _ = call_cancel_recording(session.session_id);
                });
            }
        }
    }
}

fn refresh_debug_view_async(
    text_buffer: &gtk4::TextBuffer,
    ui_log_buffer: &Arc<Mutex<VecDeque<String>>>,
    refresh_in_flight: &Arc<AtomicBool>,
) {
    if refresh_in_flight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let text_buffer = text_buffer.clone();
    let ui_log_buffer = ui_log_buffer.clone();
    let refresh_in_flight = refresh_in_flight.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ui_logs = read_recent_logs(&ui_log_buffer, MAX_LOG_LINES);
        let daemon_logs = fetch_daemon_logs(MAX_LOG_LINES);
        let toggle_diagnostics = fetch_toggle_diagnostics_summary();
        let toggle_recent_events = fetch_toggle_recent_events();
        let rendered = render_debug_text(
            &ui_logs,
            daemon_logs.as_ref(),
            toggle_diagnostics.as_ref(),
            toggle_recent_events.as_ref(),
        );
        let _ = tx.send(rendered);
    });

    glib::timeout_add_local(
        std::time::Duration::from_millis(UI_POLL_INTERVAL_MS),
        move || match rx.try_recv() {
            Ok(rendered) => {
                text_buffer.set_text(&rendered);
                refresh_in_flight.store(false, Ordering::SeqCst);
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                refresh_in_flight.store(false, Ordering::SeqCst);
                glib::ControlFlow::Break
            }
        },
    );
}

fn fetch_daemon_logs(limit: usize) -> Result<Vec<String>, String> {
    let conn =
        Connection::session().map_err(|e| format!("Cannot connect to session bus: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetRecentLogs",
            &(),
        )
        .map_err(|e| format!("Daemon log query failed: {}", e))?;

    let logs = reply
        .body()
        .deserialize::<Vec<String>>()
        .map_err(|e| format!("Invalid daemon log payload: {}", e))?;

    let start = logs.len().saturating_sub(limit);
    Ok(logs.into_iter().skip(start).collect())
}

fn fetch_toggle_diagnostics_summary() -> Result<String, String> {
    let conn =
        Connection::session().map_err(|e| format!("Cannot connect to session bus: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetToggleDiagnosticsVerbose",
            &(),
        )
        .map_err(|e| format!("TOGGLE diagnostics query failed: {}", e))?;

    let payload = reply
        .body()
        .deserialize::<String>()
        .map_err(|e| format!("Invalid TOGGLE diagnostics payload: {}", e))?;
    let diagnostics: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|e| format!("Invalid TOGGLE diagnostics JSON: {}", e))?;

    let healthy = diagnostics
        .get("healthy")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let code = diagnostics
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let message = diagnostics
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let listener_session_ok = diagnostics
        .get("listener_session_ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let shortcut_bound = diagnostics
        .get("shortcut_bound")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let bind_fail_count = diagnostics
        .get("bind_fail_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let press_while_dikt_count = diagnostics
        .get("press_while_dikt_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let stop_timeout_fallback_count = diagnostics
        .get("stop_timeout_fallback_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let current_state = diagnostics
        .get("current_state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let shortcut_description = diagnostics
        .get("shortcut_description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let last_start_failure_code = diagnostics
        .get("last_start_failure_code")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let last_start_failure_message = diagnostics
        .get("last_start_failure_message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let last_stop_failure_message = diagnostics
        .get("last_stop_failure_message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let last_dbus_error = diagnostics
        .get("last_dbus_error")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let focused_engine_id = diagnostics
        .get("focused_engine_id")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mut pending_queue_len = 0_u64;
    let mut pending_oldest_age_ms = 0_u64;
    let last_switch_confirm_latency_ms = diagnostics
        .get("last_switch_confirm_latency_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let last_switch_failure_message = diagnostics
        .get("last_switch_failure_message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let engine_active = diagnostics
        .get("engine_active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let engine_last_change_ms = diagnostics
        .get("engine_last_change_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if let Ok(reply) = conn.call_method(
        Some(DIKT_BUS_NAME),
        DIKT_OBJECT_PATH,
        Some(DIKT_INTERFACE),
        "GetPendingCommitStats",
        &(),
    ) {
        if let Ok(payload) = reply.body().deserialize::<String>() {
            if let Ok(stats) = serde_json::from_str::<serde_json::Value>(&payload) {
                pending_queue_len = stats.get("queue_len").and_then(|v| v.as_u64()).unwrap_or(0);
                pending_oldest_age_ms = stats
                    .get("oldest_age_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
            }
        }
    }

    Ok(format!(
        "healthy={} code={} message={} state={} shortcut='{}' listener_ok={} shortcut_bound={} bind_failures={} press_while_dikt={} stop_timeouts={} start_failure_code={} start_failure_message={} stop_failure_message={} switch_confirm_latency_ms={} switch_failure_message={} engine_active={} focused_engine_id={} engine_last_change_ms={} pending_queue_len={} pending_oldest_age_ms={} last_dbus_error={}",
        healthy,
        code,
        message,
        current_state,
        shortcut_description,
        listener_session_ok,
        shortcut_bound,
        bind_fail_count,
        press_while_dikt_count,
        stop_timeout_fallback_count,
        last_start_failure_code,
        last_start_failure_message,
        last_stop_failure_message,
        last_switch_confirm_latency_ms,
        last_switch_failure_message,
        engine_active,
        focused_engine_id,
        engine_last_change_ms,
        pending_queue_len,
        pending_oldest_age_ms,
        last_dbus_error
    ))
}

fn fetch_toggle_recent_events() -> Result<Vec<String>, String> {
    let conn =
        Connection::session().map_err(|e| format!("Cannot connect to session bus: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetToggleRecentEvents",
            &(),
        )
        .map_err(|e| format!("TOGGLE recent events query failed: {}", e))?;

    reply
        .body()
        .deserialize::<Vec<String>>()
        .map_err(|e| format!("Invalid TOGGLE recent events payload: {}", e))
}

fn render_debug_text(
    ui_logs: &[String],
    daemon_logs: Result<&Vec<String>, &String>,
    toggle_diagnostics: Result<&String, &String>,
    toggle_recent_events: Result<&Vec<String>, &String>,
) -> String {
    let mut out = String::new();

    out.push_str("=== Shortcut Diagnostics ===\n");
    match toggle_diagnostics {
        Ok(summary) => {
            out.push_str("[toggle] ");
            out.push_str(summary);
            out.push('\n');
        }
        Err(err) => {
            out.push_str("[toggle] unavailable: ");
            out.push_str(err);
            out.push('\n');
        }
    }

    out.push('\n');
    out.push_str("=== Shortcut Recent Events ===\n");
    match toggle_recent_events {
        Ok(events) if events.is_empty() => out.push_str("[toggle-events] <no events yet>\n"),
        Ok(events) => {
            for line in events {
                out.push_str("[toggle-events] ");
                out.push_str(line);
                out.push('\n');
            }
        }
        Err(err) => {
            out.push_str("[toggle-events] unavailable: ");
            out.push_str(err);
            out.push('\n');
        }
    }

    out.push('\n');
    out.push_str("=== UI Process Logs ===\n");
    if ui_logs.is_empty() {
        out.push_str("[ui] <no logs yet>\n");
    } else {
        for line in ui_logs {
            out.push_str("[ui] ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push('\n');
    out.push_str("=== Daemon Process Logs ===\n");
    match daemon_logs {
        Ok(logs) if logs.is_empty() => out.push_str("[daemon] <no logs yet>\n"),
        Ok(logs) => {
            for line in logs {
                out.push_str("[daemon] ");
                out.push_str(line);
                out.push('\n');
            }
        }
        Err(err) => {
            out.push_str("[daemon] unavailable: ");
            out.push_str(err);
            out.push('\n');
        }
    }

    out
}

fn call_start_recording() -> Result<DebugSessionClaim, String> {
    let conn = Connection::session().map_err(|e| format!("Session bus unavailable: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "StartRecordingSessionForTarget",
            &(DEBUG_ENGINE_ID,),
        )
        .map_err(|e| format!("StartRecordingSessionForTarget failed: {}", e))?;
    let (session_id, claim_token) = reply.body().deserialize::<(u64, String)>().map_err(|e| {
        format!(
            "Failed to decode StartRecordingSessionForTarget response: {}",
            e
        )
    })?;
    Ok(DebugSessionClaim {
        session_id,
        claim_token,
    })
}

fn call_stop_recording(session_id: u64) -> Result<bool, String> {
    let conn = Connection::session().map_err(|e| format!("Session bus unavailable: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "StopRecordingSession",
            &(session_id,),
        )
        .map_err(|e| format!("StopRecordingSession failed: {}", e))?;

    reply
        .body()
        .deserialize::<bool>()
        .map_err(|e| format!("Failed to decode StopRecordingSession response: {}", e))
}

fn call_stop_recording_and_finalize(session: &DebugSessionClaim) -> Result<String, String> {
    let acknowledged = call_stop_recording(session.session_id)?;
    if !acknowledged {
        return Err("StopRecordingSession returned false".to_string());
    }

    let started = std::time::Instant::now();
    loop {
        let (state, message, _) = call_session_status(session.session_id)?;
        match state.as_str() {
            "ready" | "committed" => break,
            "failed" => return Err(format!("Session failed: {}", message)),
            "cancelled" => return Err(format!("Session cancelled: {}", message)),
            _ => {}
        }

        if started.elapsed().as_millis() as u64 > DEBUG_STOP_WAIT_TIMEOUT_MS {
            return Err(format!(
                "Timed out waiting for finalization (last status='{}' message='{}')",
                state, message
            ));
        }

        std::thread::sleep(std::time::Duration::from_millis(DEBUG_STATUS_POLL_MS));
    }

    let (has_text, text) =
        call_take_pending_commit_for_session(session.session_id, session.claim_token.as_str())?;
    if has_text {
        Ok(text)
    } else {
        Ok(String::new())
    }
}

fn call_session_status(session_id: u64) -> Result<(String, String, u64), String> {
    let conn = Connection::session().map_err(|e| format!("Session bus unavailable: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetSessionStatus",
            &(session_id,),
        )
        .map_err(|e| format!("GetSessionStatus failed: {}", e))?;
    reply
        .body()
        .deserialize::<(String, String, u64)>()
        .map_err(|e| format!("Failed to decode GetSessionStatus response: {}", e))
}

fn call_take_pending_commit_for_session(
    session_id: u64,
    claim_token: &str,
) -> Result<(bool, String), String> {
    let conn = Connection::session().map_err(|e| format!("Session bus unavailable: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "TakePendingCommitForSession",
            &(session_id, claim_token.to_string()),
        )
        .map_err(|e| format!("TakePendingCommitForSession failed: {}", e))?;
    reply.body().deserialize::<(bool, String)>().map_err(|e| {
        format!(
            "Failed to decode TakePendingCommitForSession response: {}",
            e
        )
    })
}

fn call_cancel_recording(session_id: u64) -> Result<(), String> {
    let conn = Connection::session().map_err(|e| format!("Session bus unavailable: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "CancelRecordingSession",
            &(session_id,),
        )
        .map_err(|e| format!("CancelRecordingSession failed: {}", e))?;
    let cancelled = reply
        .body()
        .deserialize::<bool>()
        .map_err(|e| format!("Failed to decode CancelRecordingSession response: {}", e))?;
    if !cancelled {
        return Err(format!(
            "CancelRecordingSession returned false for session {}",
            session_id
        ));
    }
    Ok(())
}

fn call_recording_state() -> Result<bool, String> {
    let conn = Connection::session().map_err(|e| format!("Session bus unavailable: {}", e))?;
    let reply = conn
        .call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetState",
            &(),
        )
        .map_err(|e| format!("GetState failed: {}", e))?;

    let (is_recording, _has_model): (bool, bool) = reply
        .body()
        .deserialize()
        .map_err(|e| format!("Failed to decode GetState response: {}", e))?;
    Ok(is_recording)
}
