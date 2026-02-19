//! D-Bus interface for IBus integration on Linux/GNOME
//!
//! This module provides a D-Bus server that allows external applications
//! (like the dikt-ibus IBus engine) to control Dikt's transcription
//! functionality.

mod server;

pub use server::{start_dbus_server, stop_dbus_server, DiktDbusState, DiktState};
