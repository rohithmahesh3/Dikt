pub mod about;
pub mod advanced;
pub mod debug;
pub mod general;
pub mod models;

use gtk4::Widget;

pub trait Page {
    fn widget(&self) -> &Widget;

    fn widget_clone(&self) -> Widget {
        self.widget().clone()
    }
}
