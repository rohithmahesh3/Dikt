use gtk4::prelude::*;
use gtk4::{Box, Orientation, Separator};
use libadwaita::prelude::AdwApplicationWindowExt;
use libadwaita::{Application as AdwApplication, ApplicationWindow, HeaderBar, WindowTitle};
use std::sync::Arc;

use super::pages::Page;
use super::sidebar::Sidebar;
use crate::app::AppState;

pub struct MainWindow {
    window: ApplicationWindow,
}

impl MainWindow {
    pub fn new(app: &AdwApplication, state: Arc<AppState>) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Dikt")
            .default_width(960)
            .default_height(680)
            .build();

        let main_box = Box::builder().orientation(Orientation::Horizontal).build();

        let sidebar = Sidebar::new(&state);

        let content_box = Box::builder()
            .orientation(Orientation::Vertical)
            .hexpand(true)
            .build();

        let window_title = WindowTitle::new("Dikt", "General");
        let header = HeaderBar::builder().title_widget(&window_title).build();
        content_box.append(&header);

        let stack = gtk4::Stack::builder().hexpand(true).vexpand(true).build();

        let general_page = super::pages::general::GeneralPage::new(&state);
        stack.add_titled(general_page.widget(), Some("general"), "General");

        let models_page = super::pages::models::ModelsPage::new(&state);
        stack.add_titled(models_page.widget(), Some("models"), "Models");

        let advanced_page = super::pages::advanced::AdvancedPage::new(&state);
        stack.add_titled(advanced_page.widget(), Some("advanced"), "Advanced");

        let debug_page = super::pages::debug::DebugPage::new(&state);
        stack.add_titled(debug_page.widget(), Some("debug"), "Debug");

        let about_page = super::pages::about::AboutPage::new();
        stack.add_titled(about_page.widget(), Some("about"), "About");

        stack.connect_visible_child_name_notify({
            let window_title = window_title.clone();
            move |stack| {
                let subtitle = page_subtitle(stack.visible_child_name().as_deref());
                window_title.set_subtitle(subtitle);
            }
        });
        stack.set_visible_child_name("general");

        content_box.append(&stack);

        sidebar.connect_stack(&stack);

        let separator = Separator::builder()
            .orientation(Orientation::Vertical)
            .build();

        main_box.append(sidebar.widget());
        main_box.append(&separator);
        main_box.append(&content_box);

        window.set_content(Some(&main_box));

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn widget(&self) -> &ApplicationWindow {
        &self.window
    }
}

fn page_subtitle(page_name: Option<&str>) -> &'static str {
    match page_name {
        Some("models") => "Models",
        Some("advanced") => "Advanced",
        Some("debug") => "Debug",
        Some("about") => "About",
        _ => "General",
    }
}
