use gtk4::prelude::*;
use gtk4::{
    Box, Button, Image, Label, Orientation, PolicyType, ProgressBar, ScrolledWindow, Spinner,
    Widget,
};
use libadwaita::prelude::{ActionRowExt, PreferencesGroupExt};
use libadwaita::{ActionRow, Clamp, PreferencesGroup, ToastOverlay};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

use super::Page;
use crate::app::AppState;
use crate::managers::model::{ModelInfo, ModelState};

static DOWNLOAD_RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn get_download_runtime() -> &'static Runtime {
    DOWNLOAD_RUNTIME.get_or_init(|| Runtime::new().expect("Failed to create download runtime"))
}

/// Persistent row for a model that updates in-place
struct ModelRow {
    row: ActionRow,
    state_box: Box,
    model_id: String,
    current_widgets: Vec<Widget>,
}

impl ModelRow {
    fn new(model: &ModelInfo, is_active: bool, state: &Arc<AppState>) -> Self {
        let row = ActionRow::builder()
            .title(&model.name)
            .subtitle(&model.description)
            .build();

        if model.is_recommended {
            row.add_prefix(&Image::from_icon_name("starred-symbolic"));
        }

        let size_label = Label::builder()
            .label(format!("{} MB", model.size_mb))
            .css_classes(["dim-label", "caption"])
            .build();
        row.add_suffix(&size_label);

        let state_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();

        let mut model_row = Self {
            row,
            state_box,
            model_id: model.id.clone(),
            current_widgets: Vec::new(),
        };

        // Add state_box as suffix once — update_state only changes its children
        model_row.row.add_suffix(&model_row.state_box);

        // Initial state update
        model_row.update_state(model, is_active, state);

        model_row
    }

    /// Update the row UI based on model state
    fn update_state(&mut self, _model: &ModelInfo, is_active: bool, state: &Arc<AppState>) {
        // Clear existing state widgets
        while let Some(child) = self.state_box.first_child() {
            self.state_box.remove(&child);
        }
        self.current_widgets.clear();

        // Get current state from ModelManager
        let model_state = state
            .model_manager
            .get_model_state(&self.model_id)
            .unwrap_or(ModelState::Available);

        match model_state {
            ModelState::Available => {
                self.show_available_state(state);
            }
            ModelState::Downloading {
                bytes_downloaded,
                bytes_total,
                ..
            } => {
                self.show_downloading_state(bytes_downloaded, bytes_total, state);
            }
            ModelState::Extracting { .. } => {
                self.show_extracting_state(state);
            }
            ModelState::Ready => {
                self.show_ready_state(is_active, state);
            }
            ModelState::Error { message, retryable } => {
                self.show_error_state(&message, retryable, state);
            }
        }
    }

    fn show_available_state(&mut self, state: &Arc<AppState>) {
        if let Some(model) = state.model_manager.get_model_info(&self.model_id) {
            if model.url.is_some() {
                let download_btn = Button::builder()
                    .label("Download")
                    .css_classes(["pill", "suggested-action"])
                    .build();

                let model_id = self.model_id.clone();
                let model_manager = state.model_manager.clone();
                let state_clone = state.clone();
                download_btn.connect_clicked(move |_| {
                    if model_manager.is_model_downloading(&model_id) {
                        log::warn!("Download already in progress for model: {}", model_id);
                        return;
                    }

                    let model_id_for_blocking = model_id.clone();
                    let model_id_for_log = model_id.clone();
                    let model_manager = model_manager.clone();
                    let _state_clone2 = state_clone.clone();

                    let handle = get_download_runtime().spawn_blocking(move || {
                        let rt = tokio::runtime::Runtime::new()
                            .map_err(|e| format!("Failed to create inner runtime: {}", e))?;

                        rt.block_on(model_manager.download_model(&model_id_for_blocking))
                            .map_err(|e| format!("Download failed: {}", e))
                    });

                    std::mem::drop(get_download_runtime().spawn(async move {
                        match handle.await {
                            Ok(Ok(())) => {
                                log::info!("Model {} downloaded successfully", model_id_for_log)
                            }
                            Ok(Err(e)) => {
                                log::error!("Download error: {}", e);
                            }
                            Err(e) => {
                                log::error!("Download task panicked: {}", e);
                            }
                        }
                    }));
                });

                self.state_box.append(&download_btn);
                self.current_widgets.push(download_btn.upcast());
            }
        }
    }

    fn show_downloading_state(
        &mut self,
        bytes_downloaded: u64,
        bytes_total: u64,
        state: &Arc<AppState>,
    ) {
        let percentage = if bytes_total == 0 {
            0.0
        } else {
            (bytes_downloaded as f64 / bytes_total as f64) * 100.0
        };

        let progress_text = format!("{:.0}%", percentage);
        let progress = ProgressBar::builder()
            .fraction(percentage / 100.0)
            .show_text(true)
            .text(&progress_text)
            .width_request(120)
            .build();

        let cancel_btn = Button::builder()
            .label("Cancel")
            .css_classes(["pill"])
            .build();

        let model_id = self.model_id.clone();
        let state_clone = state.clone();
        cancel_btn.connect_clicked(move |_| {
            if let Err(e) = state_clone.model_manager.cancel_download(&model_id) {
                log::error!("Failed to cancel download: {}", e);
            }
        });

        self.state_box.append(&progress);
        self.state_box.append(&cancel_btn);
        self.current_widgets.push(progress.upcast());
        self.current_widgets.push(cancel_btn.upcast());
    }

    fn show_extracting_state(&mut self, _state: &Arc<AppState>) {
        let spinner = Spinner::builder().spinning(true).width_request(24).build();

        let label = Label::builder()
            .label("Extracting...")
            .css_classes(["dim-label"])
            .build();

        let cancel_btn = Button::builder()
            .label("Cancel")
            .css_classes(["pill"])
            .sensitive(false) // Can't cancel extraction
            .build();

        self.state_box.append(&spinner);
        self.state_box.append(&label);
        self.state_box.append(&cancel_btn);
        self.current_widgets.push(spinner.upcast());
        self.current_widgets.push(label.upcast());
        self.current_widgets.push(cancel_btn.upcast());
    }

    fn show_ready_state(&mut self, is_active: bool, state: &Arc<AppState>) {
        if is_active {
            let active_label = Label::builder()
                .label("Active")
                .css_classes(["success", "caption"])
                .build();
            self.state_box.append(&active_label);
            self.current_widgets.push(active_label.upcast());
        } else {
            let select_btn = Button::builder()
                .label("Select")
                .css_classes(["pill", "suggested-action"])
                .build();

            let model_id = self.model_id.clone();
            let state_clone = state.clone();
            select_btn.connect_clicked(move |_| {
                if let Err(e) = state_clone.model_manager.set_active_model(&model_id) {
                    log::error!("Failed to set active model: {}", e);
                }
            });

            self.state_box.append(&select_btn);
            self.current_widgets.push(select_btn.upcast());
        }

        // Check if we can delete (not custom model)
        if let Some(model) = state.model_manager.get_model_info(&self.model_id) {
            if !model.is_custom {
                let delete_btn = Button::builder()
                    .icon_name("user-trash-symbolic")
                    .css_classes(["destructive-action", "pill"])
                    .build();

                let model_id = self.model_id.clone();
                let state_clone = state.clone();
                delete_btn.connect_clicked(move |_| {
                    if let Err(e) = state_clone.model_manager.delete_model(&model_id) {
                        log::error!("Failed to delete model: {}", e);
                    }
                });

                self.state_box.append(&delete_btn);
                self.current_widgets.push(delete_btn.upcast());
            }
        }
    }

    fn show_error_state(&mut self, _message: &str, retryable: bool, state: &Arc<AppState>) {
        let error_label = Label::builder()
            .label("Error")
            .css_classes(["error", "caption"])
            .build();
        self.state_box.append(&error_label);
        self.current_widgets.push(error_label.upcast());

        if retryable {
            let retry_btn = Button::builder()
                .label("Retry")
                .css_classes(["pill", "suggested-action"])
                .build();

            let model_id = self.model_id.clone();
            let state_clone = state.clone();
            retry_btn.connect_clicked(move |_| {
                // Trigger download again
                let model_manager = state_clone.model_manager.clone();
                let model_id_for_blocking = model_id.clone();
                let model_id_for_log = model_id.clone();

                let handle = get_download_runtime().spawn_blocking(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .map_err(|e| format!("Failed to create inner runtime: {}", e))?;

                    rt.block_on(model_manager.download_model(&model_id_for_blocking))
                        .map_err(|e| format!("Download failed: {}", e))
                });

                std::mem::drop(get_download_runtime().spawn(async move {
                    match handle.await {
                        Ok(Ok(())) => {
                            log::info!("Model {} downloaded successfully", model_id_for_log)
                        }
                        Ok(Err(e)) => {
                            log::error!("Download error: {}", e);
                        }
                        Err(e) => {
                            log::error!("Download task panicked: {}", e);
                        }
                    }
                }));
            });

            self.state_box.append(&retry_btn);
            self.current_widgets.push(retry_btn.upcast());
        }
    }

    fn widget(&self) -> &ActionRow {
        &self.row
    }
}

pub struct ModelsPage {
    container: ScrolledWindow,
}

impl ModelsPage {
    pub fn new(state: &Arc<AppState>) -> Self {
        let toast_overlay = ToastOverlay::new();

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

        let models_group = PreferencesGroup::builder()
            .title("Available Models")
            .description("Download and select transcription models")
            .build();

        // Create persistent rows for all models
        let rows: Rc<RefCell<HashMap<String, ModelRow>>> = Rc::new(RefCell::new(HashMap::new()));
        let selected_model = state.model_manager.get_current_model();
        {
            let mut rows_lock = rows.borrow_mut();
            for model in sorted_models(state) {
                let is_active = model.id == selected_model;
                let row = ModelRow::new(&model, is_active, state);
                models_group.add(row.widget());
                rows_lock.insert(model.id.clone(), row);
            }
        }
        main_box.append(&models_group);

        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<String>();
        let event_rx = state.model_manager.subscribe_state_changes();
        std::thread::spawn(move || {
            while let Ok(event) = event_rx.recv() {
                let _ = ui_tx.send(event.model_id);
            }
        });

        let rows_for_events = Rc::clone(&rows);
        let state_for_events = state.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let mut has_event = false;
            while ui_rx.try_recv().is_ok() {
                has_event = true;
            }
            if has_event {
                refresh_rows(&rows_for_events, &state_for_events);
            }
            glib::ControlFlow::Continue
        });

        state.settings.connect_changed(Some("selected-model"), {
            let rows = Rc::clone(&rows);
            let state = state.clone();
            move |_| {
                refresh_rows(&rows, &state);
            }
        });

        let custom_group = PreferencesGroup::builder()
            .title("Custom Models")
            .description("Place Whisper .bin files in ~/.local/share/dikt/models/")
            .build();

        let info_label = Label::builder()
            .label("Custom models are automatically discovered and added to the list above.")
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .margin_top(12)
            .build();
        custom_group.add(&info_label);

        main_box.append(&custom_group);
        toast_overlay.set_child(Some(&main_box));

        let clamp = Clamp::builder()
            .maximum_size(900)
            .tightening_threshold(600)
            .child(&toast_overlay)
            .build();

        let container = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .child(&clamp)
            .build();

        Self { container }
    }
}

fn sorted_models(state: &Arc<AppState>) -> Vec<ModelInfo> {
    let mut models = state.model_manager.get_available_models();
    models.sort_by(|a, b| {
        b.is_recommended
            .cmp(&a.is_recommended)
            .then_with(|| b.is_downloaded.cmp(&a.is_downloaded))
            .then_with(|| a.name.cmp(&b.name))
    });
    models
}

fn refresh_rows(rows: &Rc<RefCell<HashMap<String, ModelRow>>>, state: &Arc<AppState>) {
    let models = state.model_manager.get_available_models();
    let selected = state.model_manager.get_current_model();

    let mut rows_lock = rows.borrow_mut();
    for model in models {
        if let Some(row) = rows_lock.get_mut(&model.id) {
            let is_active = model.id == selected;
            row.update_state(&model, is_active, state);
        }
    }
}

impl Page for ModelsPage {
    fn widget(&self) -> &Widget {
        self.container.upcast_ref()
    }
}
