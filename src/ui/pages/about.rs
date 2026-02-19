use super::Page;
use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Image, Justification, Label, Orientation, Widget};
use libadwaita::Clamp;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AboutPage {
    container: Clamp,
}

impl Default for AboutPage {
    fn default() -> Self {
        Self::new()
    }
}

impl AboutPage {
    pub fn new() -> Self {
        let container = Clamp::builder().maximum_size(600).build();

        let vbox = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(24)
            .margin_top(48)
            .margin_bottom(24)
            .halign(Align::Center)
            .build();

        let logo = Image::builder().icon_name("dikt").pixel_size(128).build();
        vbox.append(&logo);

        let name = Label::builder()
            .label("Dikt")
            .css_classes(["title-1"])
            .build();
        vbox.append(&name);

        let version = Label::builder()
            .label(format!("Version {}", APP_VERSION))
            .css_classes(["dim-label"])
            .build();
        vbox.append(&version);

        let description = Label::builder()
            .label("Speech-to-text for GNOME/Wayland via IBus")
            .wrap(true)
            .justify(Justification::Center)
            .build();
        vbox.append(&description);

        let links = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .halign(Align::Center)
            .margin_top(24)
            .build();

        let website_btn = Button::builder()
            .label("Website")
            .css_classes(["pill", "suggested-action"])
            .build();
        website_btn.connect_clicked(|_| {
            gtk4::show_uri(
                None::<&gtk4::Window>,
                "https://github.com/rohithmahesh3/Dikt",
                0,
            );
        });
        links.append(&website_btn);

        let issue_btn = Button::builder()
            .label("Report Issue")
            .css_classes(["pill"])
            .build();
        issue_btn.connect_clicked(|_| {
            gtk4::show_uri(
                None::<&gtk4::Window>,
                "https://github.com/rohithmahesh3/Dikt/issues",
                0,
            );
        });
        links.append(&issue_btn);

        vbox.append(&links);

        let license_label = Label::builder()
            .label("Licensed under MIT")
            .css_classes(["dim-label", "caption"])
            .margin_top(24)
            .build();
        vbox.append(&license_label);

        container.set_child(Some(&vbox));

        Self { container }
    }
}

impl Page for AboutPage {
    fn widget(&self) -> &Widget {
        self.container.upcast_ref()
    }
}
