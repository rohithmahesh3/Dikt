use gtk4::prelude::*;
use libadwaita::Application as AdwApplication;
use std::sync::{Arc, Mutex};

use crate::dbus::{self, DiktState};
use crate::global_shortcuts::start_global_shortcuts_listener;
use crate::managers::audio::AudioRecordingManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{LogLevel, Settings};
use crate::ui::window::MainWindow;

const UI_APP_ID: &str = "io.dikt.Dikt";

use crate::utils::logging::RingBufferLogger;
use std::collections::VecDeque;

pub struct AppState {
    pub settings: Settings,
    pub model_manager: Arc<ModelManager>,
    pub log_buffer: Arc<Mutex<VecDeque<String>>>,
}

struct RuntimeState {
    settings: Settings,
    recording_manager: Arc<AudioRecordingManager>,
    model_manager: Arc<ModelManager>,
    transcription_manager: Arc<TranscriptionManager>,
}

fn level_filter_from_settings(settings: &Settings) -> log::LevelFilter {
    match settings.log_level() {
        LogLevel::Trace => log::LevelFilter::Trace,
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Error => log::LevelFilter::Error,
    }
}

fn apply_runtime_log_level(settings: &Settings) {
    log::set_max_level(level_filter_from_settings(settings));
}

fn init_logging(settings: &Settings) -> Arc<Mutex<VecDeque<String>>> {
    let logger = RingBufferLogger::new(200);
    let buffer = logger.get_buffer_handle();

    // Process-global logger can already be initialized in test or multi-start flows.
    if let Err(e) = logger.init_globally() {
        eprintln!("Logger already initialized: {}", e);
    }

    apply_runtime_log_level(settings);

    buffer
}

fn init_ui_state() -> Result<Arc<AppState>, String> {
    let settings = Settings::new();
    let log_buffer = init_logging(&settings);
    let model_manager = Arc::new(
        ModelManager::new().map_err(|e| format!("Failed to initialize model manager: {}", e))?,
    );

    #[allow(clippy::arc_with_non_send_sync)]
    Ok(Arc::new(AppState {
        settings,
        model_manager,
        log_buffer,
    }))
}

fn init_runtime() -> Result<(Arc<RuntimeState>, Arc<DiktState>), String> {
    let settings = Settings::new();
    let log_buffer = init_logging(&settings);

    let recording_manager = Arc::new(
        AudioRecordingManager::new()
            .map_err(|e| format!("Failed to initialize recording manager: {}", e))?,
    );
    let model_manager = Arc::new(
        ModelManager::new().map_err(|e| format!("Failed to initialize model manager: {}", e))?,
    );
    let transcription_manager = Arc::new(
        TranscriptionManager::new(model_manager.clone())
            .map_err(|e| format!("Failed to initialize transcription manager: {}", e))?,
    );

    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(RuntimeState {
        settings: settings.clone(),
        recording_manager: recording_manager.clone(),
        model_manager: model_manager.clone(),
        transcription_manager: transcription_manager.clone(),
    });

    let dikt_state = Arc::new(DiktState::new(
        recording_manager,
        transcription_manager,
        settings.selected_language(),
        log_buffer,
    ));

    wire_settings_sync(&state, &dikt_state);

    Ok((state, dikt_state))
}

fn wire_settings_sync(state: &Arc<RuntimeState>, dikt_state: &Arc<DiktState>) {
    state.settings.connect_changed(Some("selected-language"), {
        let settings = state.settings.clone();
        let dikt_state = dikt_state.clone();
        let tm = state.transcription_manager.clone();
        move |_| {
            match dikt_state.selected_language.lock() {
                Ok(mut selected_language) => {
                    *selected_language = settings.selected_language();
                }
                Err(e) => {
                    log::error!("Failed to update selected language from settings: {}", e);
                }
            }
            tm.refresh_config_from_settings(&settings);
        }
    });

    state
        .settings
        .connect_changed(Some("translate-to-english"), {
            let settings = state.settings.clone();
            let tm = state.transcription_manager.clone();
            move |_| {
                tm.refresh_config_from_settings(&settings);
            }
        });

    state.settings.connect_changed(Some("custom-words"), {
        let settings = state.settings.clone();
        let tm = state.transcription_manager.clone();
        move |_| {
            tm.refresh_config_from_settings(&settings);
        }
    });

    state
        .settings
        .connect_changed(Some("word-correction-threshold"), {
            let settings = state.settings.clone();
            let tm = state.transcription_manager.clone();
            move |_| {
                tm.refresh_config_from_settings(&settings);
            }
        });

    state
        .settings
        .connect_changed(Some("model-unload-timeout"), {
            let settings = state.settings.clone();
            let tm = state.transcription_manager.clone();
            move |_| {
                tm.refresh_config_from_settings(&settings);
            }
        });

    state.settings.connect_changed(Some("selected-model"), {
        let settings = state.settings.clone();
        let model_manager = state.model_manager.clone();
        let tm = state.transcription_manager.clone();
        move |_| {
            if let Err(e) = model_manager.sync_selected_model_from_settings() {
                log::error!("Failed to sync selected model from settings: {}", e);
            }
            if let Err(e) = tm.unload_model() {
                log::error!("Failed to unload model after model selection change: {}", e);
            }
            tm.refresh_config_from_settings(&settings);
        }
    });

    state
        .settings
        .connect_changed(Some("mute-while-recording"), {
            let settings = state.settings.clone();
            let recording_manager = state.recording_manager.clone();
            move |_| {
                recording_manager.set_mute_while_recording(settings.mute_while_recording());
            }
        });

    state
        .settings
        .connect_changed(Some("selected-microphone"), {
            let settings = state.settings.clone();
            let recording_manager = state.recording_manager.clone();
            move |_| {
                if let Err(e) =
                    recording_manager.set_selected_microphone(settings.selected_microphone())
                {
                    log::error!("Failed to switch microphone: {}", e);
                }
            }
        });

    state
        .settings
        .connect_changed(Some("always-on-microphone"), {
            let settings = state.settings.clone();
            let recording_manager = state.recording_manager.clone();
            move |_| {
                if let Err(e) =
                    recording_manager.set_mode_from_settings(settings.always_on_microphone())
                {
                    log::error!("Failed to switch microphone mode: {}", e);
                }
            }
        });

    // Additional settings listeners for live updates
    state.settings.connect_changed(Some("audio-feedback"), {
        let settings = state.settings.clone();
        move |_| {
            let enabled = settings.audio_feedback();
            log::info!("Audio feedback setting changed to: {}", enabled);
            // Audio feedback is read on-demand during playback
        }
    });

    state
        .settings
        .connect_changed(Some("audio-feedback-volume"), {
            let settings = state.settings.clone();
            move |_| {
                let volume = settings.audio_feedback_volume();
                log::info!("Audio feedback volume changed to: {}", volume);
                // Audio feedback volume is read on-demand during playback
            }
        });

    state.settings.connect_changed(Some("sound-theme"), {
        let settings = state.settings.clone();
        move |_| {
            let theme = settings.sound_theme();
            log::info!("Sound theme changed to: {:?}", theme);
            // Sound theme is read on-demand during playback
        }
    });

    state.settings.connect_changed(Some("log-level"), {
        let settings = state.settings.clone();
        move |_| {
            apply_runtime_log_level(&settings);
            log::info!("Log level changed to {:?}", settings.log_level());
        }
    });

    state.settings.connect_changed(Some("debug-mode"), {
        let settings = state.settings.clone();
        move |_| {
            apply_runtime_log_level(&settings);
            log::info!("Debug mode setting changed");
        }
    });

    state
        .settings
        .connect_changed(Some("experimental-enabled"), {
            move |_| {
                log::info!(
                    "Experimental features setting changed - applies immediately to new recording sessions"
                );
            }
        });
}

pub fn run_ui() {
    if let Err(e) = gtk4::init() {
        eprintln!("Failed to initialize GTK: {}", e);
        std::process::exit(1);
    }
    let _ = libadwaita::init();

    let state = match init_ui_state() {
        Ok(state) => state,
        Err(e) => {
            eprintln!("Failed to initialize Dikt UI state: {}", e);
            std::process::exit(1);
        }
    };

    let app = AdwApplication::builder().application_id(UI_APP_ID).build();

    let state_clone = state.clone();
    app.connect_activate(move |app| {
        let main_window = MainWindow::new(app, state_clone.clone());
        main_window.present();
    });

    app.run();
}

pub fn run_daemon() {
    use std::sync::atomic::{AtomicBool, Ordering};

    // Keep runtime_state alive for the daemon's lifetime.
    // It contains the Settings object with GSettings signal handlers.
    // If dropped, all settings change notifications would be disconnected.
    #[allow(unused_variables)]
    let (runtime_state, dikt_state) = match init_runtime() {
        Ok(state) => state,
        Err(e) => {
            eprintln!("Failed to initialize Dikt daemon runtime: {}", e);
            std::process::exit(1);
        }
    };

    let context = glib::MainContext::default();
    match context.block_on(dbus::start_dbus_server(dikt_state)) {
        Ok(dbus_state) => {
            start_global_shortcuts_listener();
            let main_loop = glib::MainLoop::new(None, false);

            // Setup shutdown signal handler
            let shutdown_requested = Arc::new(AtomicBool::new(false));
            let shutdown_flag = shutdown_requested.clone();

            if let Err(e) = ctrlc::set_handler(move || {
                log::info!("Shutdown signal received, initiating graceful shutdown...");
                shutdown_flag.store(true, Ordering::SeqCst);
            }) {
                log::error!("Failed to set Ctrl-C handler: {}", e);
            }

            // Monitor shutdown flag
            let main_loop_clone = main_loop.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
                if shutdown_requested.load(Ordering::SeqCst) {
                    main_loop_clone.quit();
                }
                glib::ControlFlow::Continue
            });

            main_loop.run();

            // Graceful shutdown
            log::info!("Shutting down D-Bus server...");
            if let Err(e) = context.block_on(dbus::stop_dbus_server(&dbus_state)) {
                log::error!("Error during D-Bus server shutdown: {}", e);
            }
            log::info!("Shutdown complete");
        }
        Err(e) => {
            eprintln!("Failed to start D-Bus server in daemon mode: {}", e);
            std::process::exit(1);
        }
    }
}
