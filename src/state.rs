//! What toomux remembers between runs: pinned conversations and names you
//! gave sessions. Kept in ~/.local/state/toomux/state.json.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub const SLOTS: usize = 9;

/// A pinned conversation. Enough is kept to reopen it after it has exited.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Pin {
    pub id: String,
    pub cwd: String,
    /// Account name at the time it was last seen running.
    pub account: String,
    pub title: String,
    /// Launch flags (model, permissions) to reuse when reopening.
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// Slot n (0-based) is alt-(n+1). Slots stay put when others are unpinned.
    #[serde(default)]
    pub pins: Vec<Option<Pin>>,
    /// Session id -> name given in toomux.
    #[serde(default)]
    pub names: HashMap<String, String>,
    /// Sessions whose name in `names` was chosen for you (by an agent), not
    /// by you: it describes the work, so a handover names the successor for
    /// the work as it is then.
    #[serde(default)]
    pub chosen: HashSet<String>,
}

fn path() -> PathBuf {
    crate::paths::state().join("state.json")
}

impl State {
    pub fn load() -> Self {
        let mut s: State = std::fs::read_to_string(path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        s.pins.resize(SLOTS, None);
        s
    }

    fn save_unlocked(&self) -> std::io::Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension(format!("json.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, p)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = path();
        let _guard = crate::lock::exclusive(&p.with_extension("lock"))?;
        self.save_unlocked()
    }

    /// Change the state on disk under one lock, so concurrent status lines,
    /// hooks and UI actions cannot overwrite one another's changes.
    pub fn update<R>(f: impl FnOnce(&mut State) -> R) -> std::io::Result<R> {
        let p = path();
        let _guard = crate::lock::exclusive(&p.with_extension("lock"))?;
        let mut s = State::load();
        let r = f(&mut s);
        s.save_unlocked()?;
        Ok(r)
    }

    pub fn slot_of(&self, id: &str) -> Option<usize> {
        self.pins
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.id == id))
    }

    /// Pin into the first free slot; None when all nine are taken.
    pub fn pin(&mut self, pin: Pin) -> Option<usize> {
        if let Some(n) = self.slot_of(&pin.id) {
            return Some(n);
        }
        let n = self.pins.iter().position(Option::is_none)?;
        self.pins[n] = Some(pin);
        Some(n)
    }

    pub fn unpin(&mut self, id: &str) -> Option<usize> {
        let n = self.slot_of(id)?;
        self.pins[n] = None;
        Some(n)
    }

    /// Keep pinned entries current with what's running (account moves,
    /// renames). Returns whether anything changed.
    pub fn refresh_pin(&mut self, fresh: Pin) -> bool {
        match self.slot_of(&fresh.id) {
            Some(n) if self.pins[n].as_ref() != Some(&fresh) => {
                self.pins[n] = Some(fresh);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(id: &str) -> Pin {
        Pin {
            id: id.into(),
            cwd: "/x".into(),
            account: "a".into(),
            title: id.into(),
            args: vec![],
        }
    }

    #[test]
    fn slots_are_stable() {
        let mut s = State::default();
        s.pins.resize(SLOTS, None);
        assert_eq!(s.pin(pin("a")), Some(0));
        assert_eq!(s.pin(pin("b")), Some(1));
        assert_eq!(s.pin(pin("c")), Some(2));
        assert_eq!(s.unpin("b"), Some(1));
        assert_eq!(
            s.slot_of("c"),
            Some(2),
            "unpinning must not renumber the others"
        );
        assert_eq!(s.pin(pin("d")), Some(1), "the gap is reused");
        assert_eq!(s.pin(pin("a")), Some(0), "pinning twice keeps the slot");
        for i in 0..9 {
            s.pin(pin(&format!("x{i}")));
        }
        assert_eq!(s.pin(pin("overflow")), None);
    }

    #[test]
    fn concurrent_updates_do_not_lose_each_other() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-state-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp) };

        let mut threads = Vec::new();
        for i in 0..24 {
            threads.push(std::thread::spawn(move || {
                State::update(|s| {
                    s.names.insert(format!("session-{i}"), format!("name-{i}"));
                })
                .unwrap();
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let s = State::load();
        assert_eq!(s.names.len(), 24);
        for i in 0..24 {
            assert_eq!(
                s.names.get(&format!("session-{i}")),
                Some(&format!("name-{i}"))
            );
        }
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn corrupt_state_falls_back_without_destroying_future_writes() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-state-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp) };
        std::fs::create_dir_all(crate::paths::state()).unwrap();
        std::fs::write(path(), "{broken").unwrap();
        assert!(State::load().names.is_empty());
        State::update(|s| {
            s.names.insert("recovered".into(), "yes".into());
        })
        .unwrap();
        assert_eq!(
            State::load().names.get("recovered").map(String::as_str),
            Some("yes")
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}
