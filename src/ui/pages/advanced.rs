use gtk4::prelude::*;
use gtk4::{Align, Box, ComboBoxText, Orientation, PolicyType, ScrolledWindow, Switch, Widget};
use libadwaita::prelude::{ActionRowExt, PreferencesGroupExt};
use libadwaita::{ActionRow, Clamp, PreferencesGroup};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zbus::blocking::Connection;

use super::Page;
use crate::app::AppState;
use crate::global_shortcuts::{
    authorize_shortcut_interactively_from_ui, request_shortcut_listener_rebind,
};
use crate::settings::ModelUnloadTimeout;

pub struct AdvancedPage {
    container: ScrolledWindow,
}

const DIKT_BUS_NAME: &str = "io.dikt.Transcription";
const DIKT_OBJECT_PATH: &str = "/io/dikt/Transcription";
const DIKT_INTERFACE: &str = "io.dikt.Transcription";

impl AdvancedPage {
    pub fn new(state: &Arc<AppState>) -> Self {
        let main_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(12)
            .hexpand(true)
            .vexpand(true)
            .build();
        main_box.set_margin_top(24);
        main_box.set_margin_bottom(24);
        main_box.set_margin_start(24);
        main_box.set_margin_end(24);

        let model_group = PreferencesGroup::builder().title("Model").build();

        let timeout_row = ActionRow::builder()
            .title("Unload Model After")
            .subtitle("Free memory when idle")
            .build();

        let timeout_combo = ComboBoxText::new();
        let timeouts = [
            (ModelUnloadTimeout::Never, "Never"),
            (ModelUnloadTimeout::Immediately, "Immediately"),
            (ModelUnloadTimeout::Sec5, "5 seconds"),
            (ModelUnloadTimeout::Min2, "2 minutes"),
            (ModelUnloadTimeout::Min5, "5 minutes"),
            (ModelUnloadTimeout::Min10, "10 minutes"),
            (ModelUnloadTimeout::Min15, "15 minutes"),
            (ModelUnloadTimeout::Hour1, "1 hour"),
        ];

        let current_timeout = state.settings.model_unload_timeout();
        let mut timeout_index = 0;
        for (i, (timeout, name)) in timeouts.iter().enumerate() {
            timeout_combo.append(Some(&format!("{}", i)), name);
            if *timeout == current_timeout {
                timeout_index = i as u32;
            }
        }
        timeout_combo.set_active(Some(timeout_index));

        let state_clone = state.clone();
        timeout_combo.connect_changed(move |combo| {
            if let Some(id) = combo.active_id() {
                if let Ok(idx) = id.parse::<usize>() {
                    if idx < timeouts.len() {
                        state_clone
                            .settings
                            .set_model_unload_timeout(timeouts[idx].0);
                    }
                }
            }
        });
        timeout_row.add_suffix(&timeout_combo);
        model_group.add(&timeout_row);

        main_box.append(&model_group);

        let debug_group = PreferencesGroup::builder().title("Debug").build();

        let debug_row = ActionRow::builder()
            .title("Debug Mode")
            .subtitle("Enable verbose logging")
            .build();

        let debug_switch = Switch::builder()
            .active(state.settings.debug_mode())
            .build();
        debug_switch.set_valign(Align::Center);
        debug_switch.set_vexpand(false);
        debug_switch.set_hexpand(false);
        debug_switch.set_halign(Align::End);

        let state_clone = state.clone();
        debug_switch.connect_active_notify(move |switch| {
            state_clone.settings.set_debug_mode(switch.is_active());
        });
        debug_row.add_suffix(&debug_switch);
        debug_group.add(&debug_row);

        let experimental_row = ActionRow::builder()
            .title("Experimental Features")
            .subtitle("Enable beta features")
            .build();

        let experimental_switch = Switch::builder()
            .active(state.settings.experimental_enabled())
            .build();
        experimental_switch.set_valign(Align::Center);
        experimental_switch.set_vexpand(false);
        experimental_switch.set_hexpand(false);
        experimental_switch.set_halign(Align::End);

        let state_clone = state.clone();
        experimental_switch.connect_active_notify(move |switch| {
            state_clone
                .settings
                .set_experimental_enabled(switch.is_active());
        });
        experimental_row.add_suffix(&experimental_switch);
        debug_group.add(&experimental_row);

        main_box.append(&debug_group);

        let diagnostics_group = PreferencesGroup::builder()
            .title("Shortcut Diagnostics")
            .build();

        let status_row = ActionRow::builder()
            .title("Global Shortcut Status")
            .subtitle("Checking daemon health...")
            .build();
        let refresh_button = gtk4::Button::with_label("Refresh");
        refresh_button.add_css_class("flat");
        let diagnostics_refresh_in_flight = Arc::new(AtomicBool::new(false));
        let status_row_for_click = status_row.clone();
        let diagnostics_refresh_in_flight_for_click = diagnostics_refresh_in_flight.clone();
        refresh_button.connect_clicked(move |_| {
            request_toggle_diagnostics_refresh(
                &status_row_for_click,
                &diagnostics_refresh_in_flight_for_click,
            );
        });
        status_row.add_suffix(&refresh_button);
        diagnostics_group.add(&status_row);

        let help_row = ActionRow::builder()
            .title("Recovery Hint")
            .subtitle("If unhealthy, ensure the Dikt daemon is running and your user has access to /dev/input.")
            .build();
        diagnostics_group.add(&help_row);

        let authorize_row = ActionRow::builder()
            .title("Check Input Access")
            .subtitle("Verify that keyboard devices are accessible for the dictation shortcut")
            .build();
        let authorize_button = gtk4::Button::with_label("Check Now");
        authorize_button.add_css_class("flat");
        let status_row_for_auth = status_row.clone();
        authorize_button.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_label("Checking...");
            let button_weak = button.downgrade();
            let status_row = status_row_for_auth.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(authorize_shortcut_interactively_from_ui());
            });
            glib::timeout_add_local(Duration::from_millis(120), move || match rx.try_recv() {
                Ok(result) => {
                    match result {
                        Ok(result_msg) => {
                            status_row.set_subtitle(&format!("✓ {}", result_msg));
                            request_shortcut_listener_rebind();
                        }
                        Err(e) => {
                            status_row.set_subtitle(&format!("✗ {}", e));
                        }
                    }
                    if let Some(button) = button_weak.upgrade() {
                        button.set_sensitive(true);
                        button.set_label("Check Now");
                    }
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if let Some(button) = button_weak.upgrade() {
                        button.set_sensitive(true);
                        button.set_label("Check Now");
                    }
                    status_row.set_subtitle("Check failed: worker disconnected");
                    glib::ControlFlow::Break
                }
            });
        });
        authorize_row.add_suffix(&authorize_button);
        diagnostics_group.add(&authorize_row);
        main_box.append(&diagnostics_group);

        request_toggle_diagnostics_refresh(&status_row, &diagnostics_refresh_in_flight);
        let status_row_for_timer = status_row.clone();
        let diagnostics_refresh_in_flight_for_timer = diagnostics_refresh_in_flight.clone();
        glib::timeout_add_local(Duration::from_secs(4), move || {
            request_toggle_diagnostics_refresh(
                &status_row_for_timer,
                &diagnostics_refresh_in_flight_for_timer,
            );
            glib::ControlFlow::Continue
        });

        let clamp = Clamp::builder()
            .maximum_size(900)
            .tightening_threshold(600)
            .child(&main_box)
            .build();

        let container = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .child(&clamp)
            .build();

        Self { container }
    }
}

impl Page for AdvancedPage {
    fn widget(&self) -> &Widget {
        self.container.upcast_ref()
    }
}

fn request_toggle_diagnostics_refresh(status_row: &ActionRow, refresh_in_flight: &Arc<AtomicBool>) {
    if refresh_in_flight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let status_row = status_row.clone();
    let refresh_in_flight = refresh_in_flight.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(load_toggle_diagnostics_subtitle());
    });

    glib::timeout_add_local(Duration::from_millis(120), move || match rx.try_recv() {
        Ok(subtitle) => {
            status_row.set_subtitle(&subtitle);
            refresh_in_flight.store(false, Ordering::SeqCst);
            glib::ControlFlow::Break
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            status_row.set_subtitle("Unavailable: diagnostics worker disconnected");
            refresh_in_flight.store(false, Ordering::SeqCst);
            glib::ControlFlow::Break
        }
    });
}

fn load_toggle_diagnostics_subtitle() -> String {
    let conn = match Connection::session() {
        Ok(conn) => conn,
        Err(e) => return format!("Unavailable: cannot connect to session bus ({})", e),
    };

    let verbose_reply = conn.call_method(
        Some(DIKT_BUS_NAME),
        DIKT_OBJECT_PATH,
        Some(DIKT_INTERFACE),
        "GetToggleDiagnosticsVerbose",
        &(),
    );
    if let Ok(reply) = verbose_reply {
        let payload = match reply.body().deserialize::<String>() {
            Ok(payload) => payload,
            Err(e) => return format!("Unavailable: invalid diagnostics payload ({})", e),
        };
        let diagnostics: serde_json::Value = match serde_json::from_str(&payload) {
            Ok(value) => value,
            Err(e) => return format!("Unavailable: invalid diagnostics JSON ({})", e),
        };

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
        let last_success_ms = diagnostics
            .get("last_success_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
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
        let last_start_failure_ms = diagnostics
            .get("last_start_failure_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let last_stop_failure_message = diagnostics
            .get("last_stop_failure_message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let last_stop_failure_ms = diagnostics
            .get("last_stop_failure_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
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
        if let Ok(reply) = conn.call_method(
            Some(DIKT_BUS_NAME),
            DIKT_OBJECT_PATH,
            Some(DIKT_INTERFACE),
            "GetPendingCommitStats",
            &(),
        ) {
            if let Ok(payload) = reply.body().deserialize::<String>() {
                if let Ok(stats) = serde_json::from_str::<serde_json::Value>(&payload) {
                    pending_queue_len =
                        stats.get("queue_len").and_then(|v| v.as_u64()).unwrap_or(0);
                    pending_oldest_age_ms = stats
                        .get("oldest_age_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
        }
        let last_dbus_error = diagnostics
            .get("last_dbus_error")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let last_dbus_error_ms = diagnostics
            .get("last_dbus_error_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let age_seconds = if last_success_ms == 0 {
            None
        } else {
            Some(now_ms.saturating_sub(last_success_ms) / 1000)
        };

        let start_failure_suffix = if last_start_failure_code.is_empty() {
            "none".to_string()
        } else {
            let age = if last_start_failure_ms == 0 {
                "unknown".to_string()
            } else {
                format!(
                    "{}s ago",
                    now_ms.saturating_sub(last_start_failure_ms) / 1000
                )
            };
            format!(
                "{} ({}, {})",
                last_start_failure_code, last_start_failure_message, age
            )
        };
        let dbus_suffix = if last_dbus_error.is_empty() {
            "none".to_string()
        } else if last_dbus_error_ms == 0 {
            last_dbus_error.to_string()
        } else {
            format!(
                "{} ({}s ago)",
                last_dbus_error,
                now_ms.saturating_sub(last_dbus_error_ms) / 1000
            )
        };
        let stop_failure_suffix = if last_stop_failure_message.is_empty() {
            "none".to_string()
        } else if last_stop_failure_ms == 0 {
            last_stop_failure_message.to_string()
        } else {
            format!(
                "{} ({}s ago)",
                last_stop_failure_message,
                now_ms.saturating_sub(last_stop_failure_ms) / 1000
            )
        };
        let pending_commit_suffix = if pending_queue_len == 0 {
            "none".to_string()
        } else {
            format!(
                "{} queued (oldest {}s)",
                pending_queue_len,
                pending_oldest_age_ms / 1000
            )
        };
        let switch_suffix = if last_switch_failure_message.is_empty() {
            format!("ok ({} ms)", last_switch_confirm_latency_ms)
        } else {
            format!("failed ({})", last_switch_failure_message)
        };
        if healthy {
            let age_text = age_seconds
                .map(|s| format!("{}s ago", s))
                .unwrap_or_else(|| "unknown".to_string());
            return format!(
                "Healthy | state={} shortcut='{}' | listener={} bound={} | focused_engine_id={} | switch={} | pending_commit={} | last ok {}",
                current_state,
                shortcut_description,
                listener_session_ok,
                shortcut_bound,
                focused_engine_id,
                switch_suffix,
                pending_commit_suffix,
                age_text
            );
        }

        return format!(
            "Unhealthy ({}) | {} | state={} | start_fail={} | stop_fail={} | focused_engine_id={} | switch={} | pending_commit={} | dbus={} | bind_failures={} press_while_dikt={} stop_timeouts={}",
            code,
            message,
            current_state,
            start_failure_suffix,
            stop_failure_suffix,
            focused_engine_id,
            switch_suffix,
            pending_commit_suffix,
            dbus_suffix,
            bind_fail_count,
            press_while_dikt_count,
            stop_timeout_fallback_count
        );
    }

    let reply = match conn.call_method(
        Some(DIKT_BUS_NAME),
        DIKT_OBJECT_PATH,
        Some(DIKT_INTERFACE),
        "GetToggleDiagnostics",
        &(),
    ) {
        Ok(reply) => reply,
        Err(e) => return format!("Unavailable: daemon not responding ({})", e),
    };

    let diagnostics: (bool, String, String, String, u64, bool, bool, u64, u64, u64) =
        match reply.body().deserialize() {
            Ok(tuple) => tuple,
            Err(e) => return format!("Unavailable: invalid diagnostics payload ({})", e),
        };

    let (
        healthy,
        _component,
        code,
        message,
        last_success_ms,
        listener_session_ok,
        shortcut_bound,
        bind_fail_count,
        press_while_dikt_count,
        stop_timeout_fallback_count,
    ) = diagnostics;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let age_seconds = if last_success_ms == 0 {
        None
    } else {
        Some(now_ms.saturating_sub(last_success_ms) / 1000)
    };

    if healthy {
        match age_seconds {
            Some(age) => format!(
                "Healthy | listener={} shortcut={} | last ok {}s ago",
                listener_session_ok, shortcut_bound, age
            ),
            None => format!(
                "Healthy | listener={} shortcut={} | last ok unknown",
                listener_session_ok, shortcut_bound
            ),
        }
    } else {
        format!(
            "Unhealthy ({}) | {} | bind_failures={} press_while_dikt={} stop_timeouts={}",
            code, message, bind_fail_count, press_while_dikt_count, stop_timeout_fallback_count
        )
    }
}
