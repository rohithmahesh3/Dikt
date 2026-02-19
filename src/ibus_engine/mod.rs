mod context;

use ibus_sys::{ibus_dikt_cleanup, ibus_dikt_init, init_error};

pub use context::{create_context, init as set_callbacks, SharedContext};

pub fn init(context: &SharedContext, ibus_mode: bool) -> Result<(), i32> {
    set_callbacks(context);

    unsafe {
        let result = ibus_dikt_init(ibus_mode);
        if result == init_error::SUCCESS {
            Ok(())
        } else {
            Err(result)
        }
    }
}

pub fn cleanup() {
    unsafe {
        ibus_dikt_cleanup();
    }
}

pub fn run_main_loop() {
    unsafe {
        ibus_sys::ibus_main();
    }
}
