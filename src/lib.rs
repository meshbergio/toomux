//! toomux: a calm control center for every Claude Code session on the
//! machine. The CLI (`toomux`) and the native app (`toomux-app`) share this.

pub mod accounts;
pub mod actions;
pub mod archive;
pub mod capture;
pub mod config;
pub mod control;
pub mod credentials;
pub mod live;
pub mod lock;
pub mod mcp;
pub mod memory;
pub mod paths;
pub mod platform;
pub mod setup;
pub mod places;
pub mod price;
pub mod queue;
pub mod voyage;
pub mod redact;
pub mod registry;
pub mod snapshot;
pub mod state;
pub mod tmux;
pub mod transcript;
pub mod graph;
pub mod handover;
pub mod hygiene;
pub mod index;
pub mod jobs;
pub mod tokens;
pub mod upkeep;
pub mod ui;
pub mod usage;
pub mod scene;
pub mod watch;

/// Tests that point XDG_STATE_HOME at a directory of their own take this
/// first: the environment is shared by every test in the process.
#[cfg(test)]
pub(crate) static TEST_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
