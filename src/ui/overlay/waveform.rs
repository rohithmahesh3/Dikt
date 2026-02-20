use gtk4::{glib, prelude::*, DrawingArea, Window};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, TryRecvError},
    Arc,
};

const OVERLAY_WIDTH: i32 = 360;
const OVERLAY_HEIGHT: i32 = 72;
const BOTTOM_MARGIN: i32 = 80;
const BAR_COUNT: usize = 16;

/// A floating overlay window that displays animated waveform bars
/// at the bottom center of the screen during recording.
///
/// This widget must be created and used from the GTK main thread only.
/// Thread-safe visibility control is provided via `get_visibility_handle()`.
pub struct WaveformOverlay {
    window: Window,
    drawing_area: DrawingArea,
    current_levels: Rc<RefCell<Vec<f32>>>,
    visible_requested: Arc<AtomicBool>,
}

impl WaveformOverlay {
    /// Create a new waveform overlay window.
    /// Must be called from the GTK main thread.
    pub fn new() -> Self {
        let window = Window::builder()
            .decorated(false)
            .resizable(false)
            .default_width(OVERLAY_WIDTH)
            .default_height(OVERLAY_HEIGHT)
            .visible(false)
            .build();

        // Create the drawing area for rendering bars
        let drawing_area = DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();

        let current_levels = Rc::new(RefCell::new(vec![0.0; BAR_COUNT]));
        let visible_requested = Arc::new(AtomicBool::new(false));

        let overlay = Self {
            window,
            drawing_area: drawing_area.clone(),
            current_levels: current_levels.clone(),
            visible_requested: visible_requested.clone(),
        };

        // Set up the drawing callback
        overlay.setup_drawing();

        // Position the window at bottom-center when realized
        overlay.setup_positioning();

        // Set up periodic visibility check
        overlay.setup_visibility_check(visible_requested);

        // Set up the content
        overlay.window.set_child(Some(&drawing_area));

        overlay
    }

    /// Configure the drawing area to render the waveform bars.
    fn setup_drawing(&self) {
        let levels = self.current_levels.clone();

        self.drawing_area.set_draw_func(move |_, cr, width, height| {
            let levels = levels.borrow();

            // Draw semi-transparent dark background with rounded corners
            cr.set_source_rgba(0.11, 0.11, 0.14, 0.88);
            let corner_radius = 16.0;
            draw_rounded_rect_path(cr, 0.0, 0.0, width as f64, height as f64, corner_radius);
            cr.fill().expect("Failed to fill background");

            // Draw bars
            let bar_spacing = 4.0;
            let total_spacing = bar_spacing * (BAR_COUNT - 1) as f64;
            let bar_width = (width as f64 - total_spacing - 24.0) / BAR_COUNT as f64;
            let margin_x = 12.0;
            let max_bar_height = height as f64 - 20.0;

            for (i, &level) in levels.iter().enumerate() {
                let x = margin_x + i as f64 * (bar_width + bar_spacing);
                let bar_height = (level as f64).min(1.0) * max_bar_height;
                let bar_height = bar_height.max(4.0); // Minimum bar height
                let y = (height as f64 - bar_height) / 2.0;

                // Create gradient for bar (blue to purple)
                let gradient =
                    gtk4::cairo::LinearGradient::new(x, y, x, y + bar_height);
                gradient.add_color_stop_rgba(0.0, 0.42, 0.65, 1.0, 0.95);
                gradient.add_color_stop_rgba(0.5, 0.55, 0.45, 1.0, 0.9);
                gradient.add_color_stop_rgba(1.0, 0.68, 0.30, 0.95, 0.85);

                let _ = cr.set_source(&gradient);

                // Draw rounded rectangle for bar
                let radius = (bar_width / 2.0).min(4.0);
                draw_rounded_rect_path(cr, x, y, bar_width, bar_height, radius);
                cr.fill().expect("Failed to fill bar");
            }
        });
    }

    /// Set up window positioning at bottom center of screen.
    fn setup_positioning(&self) {
        self.window.connect_realize(|win| {
            // Use WidgetExt::display() to get the GdkDisplay
            let display = gtk4::prelude::WidgetExt::display(win);

            // Get the monitor at the window surface, or fallback to first monitor
            let surface = win.surface();
            let monitor = surface.as_ref().and_then(|s| display.monitor_at_surface(s));

            // Fallback: try to get the first monitor from the monitors list
            let monitor = monitor.or_else(|| {
                use gtk4::prelude::ListModelExt;
                let monitors = display.monitors();
                monitors.item(0)?.downcast::<gtk4::gdk::Monitor>().ok()
            });

            if let Some(monitor) = monitor {
                let geometry = monitor.geometry();

                let win_width = win.width();
                let win_height = win.height();

                // Log position for debugging (actual positioning is compositor-dependent on Wayland)
                log::debug!(
                    "Waveform overlay realized: monitor {}x{} at ({}, {}), window {}x{}, target position ({}, {})",
                    geometry.width(), geometry.height(),
                    geometry.x(), geometry.y(),
                    win_width, win_height,
                    geometry.x() + (geometry.width() - win_width) / 2,
                    geometry.y() + geometry.height() - win_height - BOTTOM_MARGIN
                );

                win.present();
            }
        });
    }

    /// Set up periodic visibility check from the main loop.
    fn setup_visibility_check(&self, visible_requested: Arc<AtomicBool>) {
        let window = self.window.clone();
        let current_levels = self.current_levels.clone();

        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            let should_be_visible = visible_requested.load(Ordering::SeqCst);
            let is_visible = window.is_visible();

            if should_be_visible && !is_visible {
                window.present();
            } else if !should_be_visible && is_visible {
                window.hide();
                // Reset levels when hiding
                if let Ok(mut levels) = current_levels.try_borrow_mut() {
                    levels.fill(0.0);
                }
            }

            glib::ControlFlow::Continue
        });
    }

    /// Attach a level receiver to update the waveform.
    /// This sets up a timer that polls the receiver and updates the display.
    /// Must be called from the main thread.
    pub fn attach_level_receiver(&self, receiver: Receiver<Vec<f32>>) {
        let levels = self.current_levels.clone();
        let drawing_area = self.drawing_area.clone();

        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            // Try to receive all pending updates
            loop {
                match receiver.try_recv() {
                    Ok(new_levels) => {
                        // Smooth interpolation (lerp) for smoother animation
                        if let Ok(mut current) = levels.try_borrow_mut() {
                            for (i, &new_val) in new_levels.iter().enumerate() {
                                if i < current.len() {
                                    // Lerp: 60% old value, 40% new value for smoother transitions
                                    current[i] = current[i] * 0.6 + new_val * 0.4;
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        log::debug!("Level receiver disconnected");
                        return glib::ControlFlow::Break;
                    }
                }
            }

            // Trigger a redraw
            drawing_area.queue_draw();

            glib::ControlFlow::Continue
        });
    }

    /// Show the overlay window immediately (must be called from main thread).
    pub fn show(&self) {
        self.visible_requested.store(true, Ordering::SeqCst);
        if !self.window.is_visible() {
            self.window.present();
        }
    }

    /// Hide the overlay window immediately (must be called from main thread).
    pub fn hide(&self) {
        self.visible_requested.store(false, Ordering::SeqCst);
        self.window.hide();
        // Reset levels for next show
        if let Ok(mut levels) = self.current_levels.try_borrow_mut() {
            levels.fill(0.0);
        }
        self.drawing_area.queue_draw();
    }

    /// Check if the overlay is currently visible.
    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Get a thread-safe handle for controlling visibility.
    pub fn get_visibility_handle(&self) -> OverlayVisibilityHandle {
        OverlayVisibilityHandle {
            visible_requested: self.visible_requested.clone(),
        }
    }
}

impl Default for WaveformOverlay {
    fn default() -> Self {
        Self::new()
    }
}

/// A thread-safe handle for controlling overlay visibility.
/// Can be sent to other threads and used to show/hide the overlay.
#[derive(Clone)]
pub struct OverlayVisibilityHandle {
    visible_requested: Arc<AtomicBool>,
}

impl OverlayVisibilityHandle {
    /// Request to show the overlay.
    pub fn show(&self) {
        self.visible_requested.store(true, Ordering::SeqCst);
    }

    /// Request to hide the overlay.
    pub fn hide(&self) {
        self.visible_requested.store(false, Ordering::SeqCst);
    }
}

/// Draw a rounded rectangle path (helper function).
fn draw_rounded_rect_path(
    cr: &gtk4::cairo::Context,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
) {
    cr.new_sub_path();
    cr.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::PI,
        std::f64::consts::PI * 1.5,
    );
    cr.line_to(x + width - radius, y);
    cr.arc(
        x + width - radius,
        y + radius,
        radius,
        std::f64::consts::PI * 1.5,
        0.0,
    );
    cr.line_to(x + width, y + height - radius);
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        std::f64::consts::FRAC_PI_2,
    );
    cr.line_to(x + radius, y + height);
    cr.arc(
        x + radius,
        y + height - radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.close_path();
}
