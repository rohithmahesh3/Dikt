use gtk4::prelude::*;
use gtk4::{Box, Image, Label, ListBox, ListBoxRow, Orientation, SelectionMode};
use std::sync::Arc;

use crate::app::AppState;

pub struct Sidebar {
    list: ListBox,
}

impl Sidebar {
    pub fn new(_state: &Arc<AppState>) -> Self {
        let list = ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .selection_mode(SelectionMode::Single)
            .width_request(180)
            .focusable(false)
            .vexpand(true)
            .build();

        add_item(&list, "general", "General", "preferences-system-symbolic");
        add_item(&list, "models", "Models", "folder-download-symbolic");
        add_item(
            &list,
            "advanced",
            "Advanced",
            "applications-engineering-symbolic",
        );
        add_item(&list, "debug", "Debug", "utilities-terminal-symbolic");
        add_item(&list, "about", "About", "help-about-symbolic");
        if let Some(first_row) = list.row_at_index(0) {
            list.select_row(Some(&first_row));
        }

        Self { list }
    }

    pub fn widget(&self) -> &ListBox {
        &self.list
    }

    pub fn connect_stack(&self, stack: &gtk4::Stack) {
        let stack = stack.clone();
        self.list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                if let Some(name) = row.widget_name().as_str().split("::").last() {
                    stack.set_visible_child_name(name);
                }
            }
        });
    }
}

fn add_item(list: &ListBox, name: &str, label: &str, icon: &str) {
    let row = ListBoxRow::builder()
        .name(format!("sidebar::{}", name))
        .selectable(true)
        .activatable(true)
        .focusable(false)
        .build();

    let box_ = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();

    let icon = Image::from_icon_name(icon);
    box_.append(&icon);

    let label = Label::builder().label(label).xalign(0.0).build();
    box_.append(&label);

    row.set_child(Some(&box_));
    list.append(&row);
}
