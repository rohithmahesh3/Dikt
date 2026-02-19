#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

pub type guint = c_uint;
pub type guint32 = u32;
pub type gchar = c_char;
pub type gboolean = c_int;
pub type gpointer = *mut c_void;
pub type GCallback = Option<unsafe extern "C" fn()>;
pub type GClosureNotify = Option<unsafe extern "C" fn(*mut c_void, *mut gobject_sys::GClosure)>;

pub const TRUE: gboolean = 1;
pub const FALSE: gboolean = 0;
pub const G_CONNECT_AFTER: guint = 1 << 0;
pub const G_CONNECT_SWAPPED: guint = 1 << 1;

#[repr(C)]
pub struct IBusBus {
    _private: [u8; 0],
}

#[repr(C)]
pub struct IBusEngine {
    _private: [u8; 0],
}

#[repr(C)]
pub struct IBusFactory {
    _private: [u8; 0],
}

#[repr(C)]
pub struct IBusComponent {
    _private: [u8; 0],
}

#[repr(C)]
pub struct IBusEngineDesc {
    _private: [u8; 0],
}

#[repr(C)]
pub struct IBusText {
    _private: [u8; 0],
}

#[repr(C)]
pub struct IBusObject {
    _private: [u8; 0],
}

#[repr(C)]
pub struct GClosure {
    _private: [u8; 0],
}

#[repr(C)]
pub struct GValue {
    _private: [u8; 0],
}

#[repr(C)]
pub struct GParamSpec {
    _private: [u8; 0],
}

pub type IBusObjectDestroyFunc = Option<unsafe extern "C" fn(*mut IBusObject)>;

#[repr(C)]
pub struct IBusEngineClass {
    pub parent: gobject_sys::GObjectClass,
    pub process_key_event:
        Option<unsafe extern "C" fn(*mut IBusEngine, guint, guint, guint) -> gboolean>,
    pub focus_in: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub focus_out: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub reset: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub enable: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub disable: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub candidate_clicked: Option<unsafe extern "C" fn(*mut IBusEngine, guint, guint, guint)>,
    pub page_up: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub page_down: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub cursor_up: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub cursor_down: Option<unsafe extern "C" fn(*mut IBusEngine)>,
    pub property_activate: Option<unsafe extern "C" fn(*mut IBusEngine, *mut gchar, guint)>,
    pub property_show: Option<unsafe extern "C" fn(*mut IBusEngine, *mut gchar)>,
    pub property_hide: Option<unsafe extern "C" fn(*mut IBusEngine, *mut gchar)>,
    pub set_capabilities: Option<unsafe extern "C" fn(*mut IBusEngine, guint32)>,
    pub set_cursor_location:
        Option<unsafe extern "C" fn(*mut IBusEngine, c_int, c_int, c_int, c_int)>,
    pub set_content_type: Option<unsafe extern "C" fn(*mut IBusEngine, guint, guint)>,
    _padding: [*mut c_void; 8],
}

extern "C" {
    pub fn ibus_init();
    pub fn ibus_main();
    pub fn ibus_quit();

    pub fn ibus_bus_new() -> *mut IBusBus;
    pub fn ibus_bus_is_connected(bus: *mut IBusBus) -> gboolean;
    pub fn ibus_bus_get_connection(bus: *mut IBusBus) -> *mut gio_sys::GDBusConnection;
    pub fn ibus_bus_request_name(bus: *mut IBusBus, name: *const gchar, flags: guint) -> guint;
    pub fn ibus_bus_register_component(
        bus: *mut IBusBus,
        component: *mut IBusComponent,
    ) -> gboolean;

    pub fn ibus_factory_new(connection: *mut gio_sys::GDBusConnection) -> *mut IBusFactory;
    pub fn ibus_factory_add_engine(
        factory: *mut IBusFactory,
        engine_name: *const gchar,
        engine_type: glib_sys::GType,
    );

    pub fn ibus_component_new(
        name: *const gchar,
        description: *const gchar,
        version: *const gchar,
        license: *const gchar,
        author: *const gchar,
        homepage: *const gchar,
        command_line: *const gchar,
        textdomain: *const gchar,
    ) -> *mut IBusComponent;

    pub fn ibus_component_add_engine(component: *mut IBusComponent, desc: *mut IBusEngineDesc);

    pub fn ibus_engine_desc_new(
        name: *const gchar,
        longname: *const gchar,
        description: *const gchar,
        language: *const gchar,
        license: *const gchar,
        author: *const gchar,
        icon: *const gchar,
        layout: *const gchar,
    ) -> *mut IBusEngineDesc;

    pub fn ibus_text_new_from_string(text: *const gchar) -> *mut IBusText;
    pub fn ibus_text_new_from_static_string(text: *const gchar) -> *mut IBusText;

    pub fn ibus_engine_commit_text(engine: *mut IBusEngine, text: *mut IBusText);
    pub fn ibus_engine_update_preedit_text(
        engine: *mut IBusEngine,
        text: *mut IBusText,
        cursor_pos: guint,
        visible: gboolean,
    );
    pub fn ibus_engine_hide_preedit_text(engine: *mut IBusEngine);
    pub fn ibus_engine_show_preedit_text(engine: *mut IBusEngine);

    pub fn g_object_ref(object: gpointer);
    pub fn g_object_ref_sink(object: gpointer);
    pub fn g_object_unref(object: gpointer);

    pub fn g_signal_connect_data(
        instance: gpointer,
        detailed_signal: *const gchar,
        c_handler: GCallback,
        data: gpointer,
        destroy_data: GClosureNotify,
        connect_flags: guint,
    ) -> c_int;

    pub fn g_signal_handler_disconnect(instance: gpointer, handler_id: c_int);

    pub fn ibus_dikt_init(ibus_mode: bool) -> c_int;
    pub fn ibus_dikt_cleanup();
    pub fn ibus_dikt_set_global_engine(engine_name: *const gchar) -> gboolean;
    pub fn ibus_dikt_get_global_engine_name() -> *mut gchar;
    pub fn ibus_dikt_daemon_set_global_engine(engine_name: *const gchar) -> gboolean;
    pub fn ibus_dikt_daemon_get_global_engine_name() -> *mut gchar;
}

pub mod keys {
    pub const IBUS_KEY_Escape: u32 = 0xff1b;
}

pub mod modifiers {
    pub const IBUS_RELEASE_MASK: u32 = 1 << 30;
}

pub mod init_error {
    pub const SUCCESS: i32 = 0;
    pub const BUS_CREATE_FAILED: i32 = 1;
    pub const NOT_CONNECTED: i32 = 2;
    pub const NO_CONNECTION: i32 = 3;
    pub const FACTORY_CREATE_FAILED: i32 = 4;
}

#[macro_export]
macro_rules! g_signal_connect {
    ($instance:expr, $signal:expr, $callback:expr, $data:expr) => {
        $crate::g_signal_connect_data(
            $instance as *mut _,
            concat!($signal, "\0").as_ptr() as *const $crate::gchar,
            Some(std::mem::transmute($callback as extern "C" fn())),
            $data as *mut _,
            None,
            0,
        )
    };
}
