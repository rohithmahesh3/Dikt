use glib::translate::IntoGlib;
use glib::Propagation;
use gtk4::prelude::*;
use gtk4::{gdk, EventControllerKey};
use gtk4::{
    Adjustment, Align, Box, Button, ComboBoxText, Orientation, PolicyType, Scale, ScrolledWindow,
    Switch, Widget,
};
use libadwaita::prelude::{ActionRowExt, PreferencesGroupExt};
use libadwaita::{ActionRow, Clamp, PreferencesGroup};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use super::Page;
use crate::app::AppState;

const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 4;
const MOD_ALT: u32 = 8;
const MOD_SUPER: u32 = 64;

pub struct GeneralPage {
    container: ScrolledWindow,
}

impl GeneralPage {
    pub fn new(state: &Arc<AppState>) -> Self {
        let container = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .build();

        let vbox = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(24)
            .hexpand(true)
            .vexpand(true)
            .build();
        vbox.set_margin_top(24);
        vbox.set_margin_bottom(24);
        vbox.set_margin_start(24);
        vbox.set_margin_end(24);

        let recording_group = PreferencesGroup::builder()
            .title("Recording")
            .description("Press shortcut once to start recording, and press it again to stop.")
            .build();

        let toggle_row = ActionRow::builder()
            .title("Dictation Shortcut")
            .subtitle("Click the button, then press a shortcut. Press Esc to cancel capture.")
            .build();
        let toggle_button = Button::with_label(&format_shortcut_label(
            state.settings.dictation_shortcut_keyval(),
            state.settings.dictation_shortcut_modifiers(),
        ));
        toggle_button.add_css_class("flat");
        toggle_button.set_can_focus(true);
        toggle_row.add_suffix(&toggle_button);
        recording_group.add(&toggle_row);

        let mute_row = ActionRow::builder()
            .title("Mute While Recording")
            .subtitle("Mute system audio during recording")
            .build();
        let mute_switch = Switch::builder()
            .active(state.settings.mute_while_recording())
            .build();
        mute_switch.set_valign(Align::Center);
        mute_switch.set_vexpand(false);
        mute_switch.set_hexpand(false);
        mute_switch.set_halign(Align::End);
        mute_row.add_suffix(&mute_switch);
        mute_switch.connect_active_notify({
            let settings = state.settings.clone();
            move |switch| {
                settings.set_mute_while_recording(switch.is_active());
            }
        });
        recording_group.add(&mute_row);

        let is_capturing = Rc::new(Cell::new(false));
        toggle_button.connect_clicked({
            let button = toggle_button.clone();
            let is_capturing = is_capturing.clone();
            move |_| {
                is_capturing.set(true);
                button.set_label("Press shortcut...");
                button.grab_focus();
            }
        });

        let key_controller = EventControllerKey::new();
        key_controller.connect_key_pressed({
            let settings = state.settings.clone();
            let button = toggle_button.clone();
            let is_capturing = is_capturing.clone();
            move |_, keyval, _, state| {
                if !is_capturing.get() {
                    return Propagation::Proceed;
                }

                if keyval == gdk::Key::Escape {
                    is_capturing.set(false);
                    button.set_label(&format_shortcut_label(
                        settings.dictation_shortcut_keyval(),
                        settings.dictation_shortcut_modifiers(),
                    ));
                    return Propagation::Stop;
                }

                let modifiers = gdk_to_ibus_modifiers(state);
                if modifiers == 0
                    || !gtk4::accelerator_valid(keyval, ibus_to_gdk_modifiers(modifiers))
                {
                    button.set_label("Invalid shortcut. Use Ctrl/Alt/Super + key");
                    return Propagation::Stop;
                }

                let normalized_key = keyval.to_lower().into_glib();
                settings.set_dictation_shortcut_keyval(normalized_key);
                settings.set_dictation_shortcut_modifiers(modifiers);
                button.set_label(&format_shortcut_label(normalized_key, modifiers));
                is_capturing.set(false);
                Propagation::Stop
            }
        });
        toggle_button.add_controller(key_controller);

        vbox.append(&recording_group);

        let audio_feedback_group = PreferencesGroup::builder().title("Audio Feedback").build();

        let feedback_row = ActionRow::builder()
            .title("Play Sounds")
            .subtitle("Play sound on start/stop")
            .build();
        let feedback_switch = Switch::builder()
            .active(state.settings.audio_feedback())
            .build();
        feedback_switch.set_valign(Align::Center);
        feedback_switch.set_vexpand(false);
        feedback_switch.set_hexpand(false);
        feedback_switch.set_halign(Align::End);
        feedback_row.add_suffix(&feedback_switch);
        feedback_switch.connect_active_notify({
            let settings = state.settings.clone();
            move |switch| {
                settings.set_audio_feedback(switch.is_active());
            }
        });
        audio_feedback_group.add(&feedback_row);

        let volume_row = ActionRow::builder().title("Volume").build();
        let volume_scale = Scale::builder()
            .adjustment(&Adjustment::new(
                state.settings.audio_feedback_volume() as f64,
                0.0,
                1.0,
                0.1,
                0.1,
                0.1,
            ))
            .hexpand(true)
            .build();
        volume_scale.connect_value_changed({
            let settings = state.settings.clone();
            move |scale| {
                settings.set_audio_feedback_volume(scale.value() as f32);
            }
        });
        volume_row.add_suffix(&volume_scale);
        audio_feedback_group.add(&volume_row);

        vbox.append(&audio_feedback_group);

        let language_group = PreferencesGroup::builder().title("Language").build();

        let lang_row = ActionRow::builder()
            .title("Transcription Language")
            .subtitle("Language for transcription")
            .build();

        let language_combo = ComboBoxText::new();
        let languages = [
            ("auto", "Auto Detect"),
            ("en", "English"),
            ("zh", "Chinese"),
            ("zh-Hans", "Chinese (Simplified)"),
            ("zh-Hant", "Chinese (Traditional)"),
            ("de", "German"),
            ("es", "Spanish"),
            ("fr", "French"),
            ("ja", "Japanese"),
            ("ko", "Korean"),
            ("pt", "Portuguese"),
            ("ru", "Russian"),
            ("it", "Italian"),
        ];

        let selected_lang = state.settings.selected_language();
        let mut selected_index = 0;
        for (i, (code, name)) in languages.iter().enumerate() {
            language_combo.append(Some(code), name);
            if *code == selected_lang {
                selected_index = i as u32;
            }
        }
        language_combo.set_active(Some(selected_index));

        let state_clone = state.clone();
        language_combo.connect_changed(move |combo| {
            if let Some(active) = combo.active_id() {
                state_clone.settings.set_selected_language(&active);
            }
        });
        lang_row.add_suffix(&language_combo);
        language_group.add(&lang_row);

        let translate_row = ActionRow::builder()
            .title("Translate to English")
            .subtitle("Translate non-English speech to English")
            .build();
        let translate_switch = Switch::builder()
            .active(state.settings.translate_to_english())
            .build();
        translate_switch.set_valign(Align::Center);
        translate_switch.set_vexpand(false);
        translate_switch.set_hexpand(false);
        translate_switch.set_halign(Align::End);
        translate_row.add_suffix(&translate_switch);
        translate_switch.connect_active_notify({
            let settings = state.settings.clone();
            move |switch| {
                settings.set_translate_to_english(switch.is_active());
            }
        });
        language_group.add(&translate_row);

        vbox.append(&language_group);

        let clamp = Clamp::builder()
            .maximum_size(900)
            .tightening_threshold(600)
            .build();
        clamp.set_child(Some(&vbox));

        container.set_child(Some(&clamp));

        Self { container }
    }
}

impl Page for GeneralPage {
    fn widget(&self) -> &Widget {
        self.container.upcast_ref()
    }
}

fn format_shortcut_label(keyval: u32, modifiers: u32) -> String {
    let key = unsafe { glib::translate::from_glib(keyval) };
    let label = gtk4::accelerator_get_label(key, ibus_to_gdk_modifiers(modifiers));
    if label.is_empty() {
        "Unknown shortcut".to_string()
    } else {
        label.to_string()
    }
}

fn gdk_to_ibus_modifiers(modifiers: gdk::ModifierType) -> u32 {
    let normalized = modifiers
        & (gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::SUPER_MASK);

    let mut result = 0;
    if normalized.contains(gdk::ModifierType::CONTROL_MASK) {
        result |= MOD_CTRL;
    }
    if normalized.contains(gdk::ModifierType::ALT_MASK) {
        result |= MOD_ALT;
    }
    if normalized.contains(gdk::ModifierType::SHIFT_MASK) {
        result |= MOD_SHIFT;
    }
    if normalized.contains(gdk::ModifierType::SUPER_MASK) {
        result |= MOD_SUPER;
    }
    result
}

fn ibus_to_gdk_modifiers(modifiers: u32) -> gdk::ModifierType {
    let mut result = gdk::ModifierType::empty();
    if modifiers & MOD_CTRL != 0 {
        result |= gdk::ModifierType::CONTROL_MASK;
    }
    if modifiers & MOD_ALT != 0 {
        result |= gdk::ModifierType::ALT_MASK;
    }
    if modifiers & MOD_SHIFT != 0 {
        result |= gdk::ModifierType::SHIFT_MASK;
    }
    if modifiers & MOD_SUPER != 0 {
        result |= gdk::ModifierType::SUPER_MASK;
    }
    result
}
