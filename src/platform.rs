//! Everything toomux asks the operating system about other processes: /proc
//! on Linux (and so on Windows, inside WSL 2), the kernel's process calls on
//! macOS. A port to another system adds one file under src/platform/.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "toomux runs on Linux and macOS (on Windows, inside WSL 2): it learns about Claude Code sessions from the kernel, and src/platform/ is the one place a port changes"
);

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
