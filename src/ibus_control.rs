use std::env;
use std::ffi::{CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use ibus_sys::{
    gboolean, gchar, ibus_dikt_daemon_get_global_engine_name, ibus_dikt_daemon_reset_bus_cache,
    ibus_dikt_daemon_set_global_engine,
};
use log::{info, warn};

pub const DIKT_ENGINE_NAME: &str = "dikt";
const DIKT_ENGINE_FALLBACK_NAME: &str = "other:dikt";
const ENGINE_SWITCH_POLL_MS: u64 = 20;
const IBUS_ADDRESS_PREFIX: &str = "IBUS_ADDRESS=";

static IBUS_BOOTSTRAP_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

fn reset_daemon_bus_cache() {
    unsafe {
        ibus_dikt_daemon_reset_bus_cache();
    }
}

fn candidate_ibus_bus_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(config_home).join("ibus").join("bus"));
    }
    if let Some(home) = env::var_os("HOME") {
        let fallback = PathBuf::from(home).join(".config").join("ibus").join("bus");
        if !dirs.iter().any(|existing| existing == &fallback) {
            dirs.push(fallback);
        }
    }
    dirs
}

fn parse_ibus_address_from_file(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    parse_ibus_address_from_contents(&contents)
}

fn parse_ibus_address_from_contents(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        line.strip_prefix(IBUS_ADDRESS_PREFIX)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn read_machine_id() -> Option<String> {
    fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn wayland_display_token() -> Option<String> {
    env::var("WAYLAND_DISPLAY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| format!("-unix-{}", value))
}

fn file_modified_ms(path: &Path) -> u128 {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn discover_ibus_address() -> Option<(String, PathBuf)> {
    let machine_id = read_machine_id();
    let wayland_token = wayland_display_token();

    let mut best: Option<(i32, u128, String, PathBuf)> = None;

    for dir in candidate_ibus_bus_dirs() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(address) = parse_ibus_address_from_file(&path) else {
                continue;
            };
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            let mut score = 0_i32;
            if let Some(machine_id) = machine_id.as_deref() {
                if name.starts_with(machine_id) {
                    score += 20;
                }
            }
            if let Some(token) = wayland_token.as_deref() {
                if name.contains(token) {
                    score += 50;
                }
            }
            let modified_ms = file_modified_ms(&path);
            let replace = match &best {
                None => true,
                Some((best_score, best_modified_ms, _, _)) => {
                    score > *best_score || (score == *best_score && modified_ms > *best_modified_ms)
                }
            };
            if replace {
                best = Some((score, modified_ms, address, path));
            }
        }
    }

    best.map(|(_, _, address, path)| (address, path))
}

fn ensure_ibus_address_for_daemon() {
    if env::var_os("IBUS_ADDRESS").is_some() {
        return;
    }

    let Some((address, source_path)) = discover_ibus_address() else {
        if !IBUS_BOOTSTRAP_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
            let dirs = candidate_ibus_bus_dirs()
                .into_iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            warn!(
                "IBUS_ADDRESS is unset and no readable IBus bus file with IBUS_ADDRESS was found (checked: {})",
                dirs
            );
        }
        return;
    };

    env::set_var("IBUS_ADDRESS", &address);
    reset_daemon_bus_cache();
    IBUS_BOOTSTRAP_WARNING_EMITTED.store(false, Ordering::Relaxed);
    info!(
        "Configured IBUS_ADDRESS for daemon from {}",
        source_path.display()
    );
}

fn engine_matches_target(current: &str, target: &str) -> bool {
    if is_dikt_engine(target) {
        is_dikt_engine(current)
    } else {
        current == target
    }
}

pub fn get_current_engine() -> Result<String> {
    ensure_ibus_address_for_daemon();

    let mut engine_ptr = unsafe { ibus_dikt_daemon_get_global_engine_name() };
    if engine_ptr.is_null() {
        reset_daemon_bus_cache();
        engine_ptr = unsafe { ibus_dikt_daemon_get_global_engine_name() };
    }
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

    ensure_ibus_address_for_daemon();

    let mut result: gboolean =
        unsafe { ibus_dikt_daemon_set_global_engine(c_engine.as_ptr() as *const gchar) };
    if result != ibus_sys::TRUE {
        reset_daemon_bus_cache();
        result = unsafe { ibus_dikt_daemon_set_global_engine(c_engine.as_ptr() as *const gchar) };
    }
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

#[cfg(test)]
mod tests {
    use super::{parse_ibus_address_from_contents, IBUS_ADDRESS_PREFIX};

    #[test]
    fn parse_ibus_address_reads_line() {
        let contents = format!(
            "# comment\n{}unix:path=/tmp/ibus-test,guid=deadbeef\nIBUS_DAEMON_PID=1\n",
            IBUS_ADDRESS_PREFIX
        );
        let parsed = parse_ibus_address_from_contents(&contents);
        assert_eq!(
            parsed.as_deref(),
            Some("unix:path=/tmp/ibus-test,guid=deadbeef")
        );
    }

    #[test]
    fn parse_ibus_address_handles_missing_value() {
        let parsed = parse_ibus_address_from_contents("IBUS_DAEMON_PID=123");
        assert!(parsed.is_none());
    }
}
