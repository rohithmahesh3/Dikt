use std::ffi::{CStr, CString};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ibus_sys::{
    gboolean, gchar, ibus_dikt_daemon_get_global_engine_name, ibus_dikt_daemon_set_global_engine,
};

pub const DIKT_ENGINE_NAME: &str = "dikt";
const DIKT_ENGINE_FALLBACK_NAME: &str = "other:dikt";
const ENGINE_SWITCH_POLL_MS: u64 = 20;

fn engine_matches_target(current: &str, target: &str) -> bool {
    if is_dikt_engine(target) {
        is_dikt_engine(current)
    } else {
        current == target
    }
}

pub fn get_current_engine() -> Result<String> {
    let engine_ptr = unsafe { ibus_dikt_daemon_get_global_engine_name() };
    if engine_ptr.is_null() {
        return Err(anyhow!("IBus returned empty global engine"));
    }

    let engine = unsafe { CStr::from_ptr(engine_ptr as *const i8) }
        .to_string_lossy()
        .trim()
        .to_string();
    unsafe {
        glib::ffi::g_free(engine_ptr as *mut _);
    }

    if engine.is_empty() {
        Err(anyhow!("IBus returned blank global engine"))
    } else {
        Ok(engine)
    }
}

pub fn set_global_engine(engine_name: &str) -> Result<()> {
    if engine_name.trim().is_empty() {
        return Err(anyhow!("Target engine name is empty"));
    }

    let c_engine = CString::new(engine_name)
        .map_err(|e| anyhow!("Invalid engine name '{}': {}", engine_name, e))?;

    let result: gboolean =
        unsafe { ibus_dikt_daemon_set_global_engine(c_engine.as_ptr() as *const gchar) };
    if result == ibus_sys::TRUE {
        Ok(())
    } else {
        Err(anyhow!(
            "IBus rejected global engine switch to '{}'",
            engine_name
        ))
    }
}

pub fn is_dikt_engine(engine_name: &str) -> bool {
    engine_name == DIKT_ENGINE_NAME || engine_name.ends_with(":dikt")
}

pub fn switch_to_dikt_engine_verified(timeout_ms: u64) -> Result<String> {
    if let Ok(engine) = get_current_engine() {
        if is_dikt_engine(&engine) {
            return Ok(engine);
        }
    }

    let mut attempts = Vec::new();

    for candidate in [DIKT_ENGINE_NAME, DIKT_ENGINE_FALLBACK_NAME] {
        match switch_engine_verified(candidate, timeout_ms) {
            Ok(engine) => return Ok(engine),
            Err(e) => attempts.push(format!("{} ({})", candidate, e)),
        }
    }

    Err(anyhow!(
        "Failed to switch to Dikt input source with confirmation. Tried: {}",
        attempts.join(", ")
    ))
}

pub fn switch_engine_verified(target_engine: &str, timeout_ms: u64) -> Result<String> {
    if target_engine.trim().is_empty() {
        return Err(anyhow!("Target engine name is empty"));
    }

    if let Ok(engine) = get_current_engine() {
        if engine_matches_target(&engine, target_engine) {
            return Ok(engine);
        }
    }

    let timeout = Duration::from_millis(timeout_ms.max(1));
    let set_retry_interval = Duration::from_millis(120);
    let start = Instant::now();
    let mut last_set_attempt = Instant::now()
        .checked_sub(set_retry_interval)
        .unwrap_or_else(Instant::now);
    let mut set_attempts = 0_u32;
    let mut last_set_error = String::new();
    let mut last_engine = String::new();
    let mut last_error = String::new();

    loop {
        if last_set_attempt.elapsed() >= set_retry_interval {
            match set_global_engine(target_engine) {
                Ok(()) => {
                    set_attempts = set_attempts.saturating_add(1);
                    last_set_error.clear();
                }
                Err(e) => {
                    last_set_error = e.to_string();
                }
            }
            last_set_attempt = Instant::now();
        }

        match get_current_engine() {
            Ok(engine) => {
                if engine_matches_target(&engine, target_engine) {
                    return Ok(engine);
                }
                last_engine = engine;
            }
            Err(e) => {
                last_error = e.to_string();
            }
        }

        if start.elapsed() >= timeout {
            break;
        }
        thread::sleep(Duration::from_millis(ENGINE_SWITCH_POLL_MS));
    }

    Err(anyhow!(
        "Switch to '{}' not confirmed within {} ms (set_attempts={} last_set_error='{}' last_engine='{}' last_error='{}')",
        target_engine,
        timeout_ms,
        set_attempts,
        last_set_error,
        last_engine,
        last_error
    ))
}
