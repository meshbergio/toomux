//! Where toomux keeps things: the usual places (`~/.config/toomux`,
//! `~/.local/state/toomux`, `~/.local/share/toomux`), or all of it under one
//! folder when `TOOMUX_HOME` is set. Runtime files (sockets, queues) stay in
//! `$XDG_RUNTIME_DIR`, which is cleared at each boot (a per-boot temp folder
//! where there is none).

use std::path::PathBuf;

fn one_home() -> Option<PathBuf> {
    std::env::var_os("TOOMUX_HOME")
        .filter(|v| !v.is_empty())
        .map(|v| crate::config::expand(&v.to_string_lossy()))
}

/// The config folder.
pub fn config() -> PathBuf {
    match one_home() {
        Some(h) => h.join("config"),
        None => dirs::config_dir()
            .unwrap_or_else(|| crate::config::home().join(".config"))
            .join("toomux"),
    }
}

/// Everything toomux learns and can rebuild or lose: memory, usage,
/// handovers, kept outputs, reports.
pub fn state() -> PathBuf {
    match one_home() {
        Some(h) => h.join("state"),
        None => dirs::state_dir()
            .unwrap_or_else(|| crate::config::home().join(".local/state"))
            .join("toomux"),
    }
}

/// What is worth backing up: the transcript archive.
pub fn data() -> PathBuf {
    match one_home() {
        Some(h) => h.join("data"),
        None => dirs::data_dir()
            .unwrap_or_else(|| crate::config::home().join(".local/share"))
            .join("toomux"),
    }
}

/// Sockets, queues and scratch that mean nothing after a reboot.
pub fn runtime() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(fallback_runtime)
        .join("toomux")
}

#[cfg(not(target_os = "macos"))]
fn fallback_runtime() -> PathBuf {
    std::env::temp_dir().join(format!("toomux-{}", unsafe { libc::getuid() }))
}

/// macOS has no runtime folder, and its per-user temp folder outlives a
/// reboot: one folder per boot keeps "gone at reboot" true.
#[cfg(target_os = "macos")]
fn fallback_runtime() -> PathBuf {
    std::env::temp_dir()
        .join(format!("toomux-{}", unsafe { libc::getuid() }))
        .join(crate::platform::boot_id())
}
