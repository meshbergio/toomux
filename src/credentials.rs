//! An account's Claude Code sign-in. On Linux it is `.credentials.json` in
//! the account's folder. On macOS Claude Code keeps it in the login
//! Keychain, one item per config folder, and writes the file only when the
//! Keychain refuses (a locked keychain over ssh, say). toomux reads the
//! Keychain the way Claude Code does, through /usr/bin/security, which is
//! what keeps the Keychain from asking. It never refreshes or writes a
//! sign-in, except to carry one along when an account's folder moves.

use std::path::Path;

const FILE: &str = ".credentials.json";

/// The account's sign-in record (JSON), if it has one.
pub fn read(dir: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    if let Some(s) = keychain::read(dir) {
        return Some(s);
    }
    std::fs::read_to_string(dir.join(FILE)).ok()
}

/// Whether the account is signed in, without reading the secret.
pub fn present(dir: &Path) -> bool {
    #[cfg(target_os = "macos")]
    if keychain::present(dir) {
        return true;
    }
    dir.join(FILE).is_file()
}

/// Keep an account's sign-in when its folder moves from `from` to `to`.
/// The file moves with the folder; a Keychain item is named after the
/// folder, so it is copied to the new name (the old one is left alone).
pub fn carry(from: &Path, to: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    return keychain::carry(from, to);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (from, to);
        None
    }
}

/// The `CLAUDE_CONFIG_DIR` a launch of this account sets: none for the
/// usual folder on macOS, where setting it would name a different Keychain
/// item than a plain `claude` login uses.
pub fn config_dir_var(dir: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    if keychain::is_default(dir) {
        return None;
    }
    Some(dir.display().to_string())
}

/// The account's `.claude.json` (its settings and folder trust). Claude
/// Code keeps it in the config folder when `CLAUDE_CONFIG_DIR` is set and in
/// the home folder when it isn't, which on macOS is how the usual account runs.
pub fn claude_json(dir: &Path) -> std::path::PathBuf {
    if config_dir_var(dir).is_none() {
        return crate::config::home().join(".claude.json");
    }
    dir.join(".claude.json")
}

/// Set (or clear) `CLAUDE_CONFIG_DIR` on a command for this account.
pub fn with_config_dir<'a>(cmd: &'a mut std::process::Command, dir: &Path) -> &'a mut std::process::Command {
    match config_dir_var(dir) {
        Some(_) => cmd.env("CLAUDE_CONFIG_DIR", dir),
        None => cmd.env_remove("CLAUDE_CONFIG_DIR"),
    }
}

#[cfg(target_os = "macos")]
mod keychain {
    //! Claude Code's scheme (its `BBe` and `dY`, 1.0.8 onwards): service
    //! `Claude Code-credentials`, plus `-` and the first 8 hex of
    //! sha256(CLAUDE_CONFIG_DIR, NFC) when that is set and not empty;
    //! `CLAUDE_SECURESTORAGE_CONFIG_DIR`, when defined, is hashed instead.
    //! Account: `$USER`, or `claude-code-user` if it has other characters.

    use sha2::{Digest, Sha256};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use unicode_normalization::UnicodeNormalization;

    const SERVICE: &str = "Claude Code-credentials";
    const SECURITY: &str = "/usr/bin/security";

    fn suffix(dir: &str) -> String {
        let nfc: String = dir.nfc().collect();
        let hex: String = Sha256::digest(nfc.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        format!("-{}", &hex[..8])
    }

    fn account() -> String {
        let user = std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(login_name).unwrap_or_default();
        if !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            user
        } else {
            "claude-code-user".into()
        }
    }

    fn login_name() -> Option<String> {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; 4096];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
        if rc != 0 || out.is_null() || pw.pw_name.is_null() {
            return None;
        }
        unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }.to_str().ok().map(str::to_string)
    }

    pub fn is_default(dir: &Path) -> bool {
        crate::config::canon(dir) == crate::config::canon(&crate::config::home().join(".claude"))
    }

    /// The item names a sign-in for this folder can be under, most likely
    /// first. toomux sets CLAUDE_CONFIG_DIR to the folder's path as written
    /// in its config, expanded; the usual folder is launched without it.
    fn services(dir: &Path) -> Vec<String> {
        if let Some(v) = std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
            let v = v.to_string_lossy();
            return vec![if v.is_empty() { SERVICE.to_string() } else { format!("{SERVICE}{}", suffix(&v)) }];
        }
        let own = format!("{SERVICE}{}", suffix(&dir.display().to_string()));
        if is_default(dir) { vec![SERVICE.to_string(), own] } else { vec![own] }
    }

    /// Run `security`, as Claude Code does, giving up after 10s (its own
    /// limit) rather than hang behind a Keychain prompt.
    fn security(args: &[&str], input: Option<&str>) -> Option<(i32, String)> {
        let mut child = Command::new(SECURITY)
            .args(args)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            use std::io::Write;
            let _ = stdin.write_all(text.as_bytes());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let out = child.wait_with_output().ok()?;
        Some((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    fn find(svc: &str) -> Option<String> {
        let acct = account();
        match security(&["find-generic-password", "-a", &acct, "-w", "-s", svc], None)? {
            (0, s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }

    pub fn read(dir: &Path) -> Option<String> {
        services(dir).iter().find_map(|s| find(s))
    }

    /// Without `-w` only the item's attributes are read, never the secret.
    pub fn present(dir: &Path) -> bool {
        let acct = account();
        services(dir).iter().any(|s| matches!(security(&["find-generic-password", "-a", &acct, "-s", s], None), Some((0, _))))
    }

    /// Copy the sign-in to the item the new folder is known by, written the
    /// way Claude Code writes it (`security -i`, the secret as hex so it
    /// never shows in a process listing).
    pub fn carry(from: &Path, to: &Path) -> Option<String> {
        let secret = read(from)?;
        let to_svc = services(to).into_iter().next()?;
        if services(from).contains(&to_svc) {
            return None;
        }
        let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
        let line = format!("add-generic-password -U -a \"{}\" -s \"{to_svc}\" -X \"{hex}\"\n", account());
        match security(&["-i"], Some(&line)) {
            Some((0, _)) => Some("sign-in carried over in the Keychain".into()),
            _ => Some("couldn't carry the sign-in over in the Keychain: log in again there".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn suffix_matches_claude_code() {
            // Worked out apart from this code: printf %s <dir> | sha256sum,
            // and python's unicodedata NFC then hashlib.
            assert_eq!(suffix("/Users/sam/.claude-work"), "-6b58ea1b");
            assert_eq!(suffix("/Users/re\u{301}my/.claude"), "-d162ded4");
            assert_eq!(suffix("/Users/r\u{e9}my/.claude"), "-d162ded4");
        }

        /// Writes to the login Keychain, so only the macOS CI job runs it:
        /// no prompt, nothing found where there's nothing, and an item
        /// `security` wrote is read back, and carried to a renamed folder.
        #[test]
        #[ignore]
        fn the_login_keychain() {
            let base = std::env::temp_dir().join(format!("toomux-keychain-{}", std::process::id()));
            let (dir, moved) = (base.join("a"), base.join("b"));
            let t = Instant::now();
            assert!(!present(&dir));
            assert_eq!(read(&dir), None);
            let secret = r#"{"claudeAiOauth":{"accessToken":"not-a-token"}}"#;
            let svc = services(&dir).remove(0);
            assert!(Command::new(SECURITY)
                .args(["add-generic-password", "-U", "-a", &account(), "-s", &svc, "-w", secret])
                .status()
                .unwrap()
                .success());
            let got = (present(&dir), read(&dir));
            let carried = carry(&dir, &moved);
            let there = read(&moved);
            for s in [svc, services(&moved).remove(0)] {
                let _ = Command::new(SECURITY).args(["delete-generic-password", "-a", &account(), "-s", &s]).output();
            }
            assert_eq!(got, (true, Some(secret.to_string())));
            assert_eq!(carried.as_deref(), Some("sign-in carried over in the Keychain"));
            assert_eq!(there.as_deref(), Some(secret));
            assert!(t.elapsed() < Duration::from_secs(10), "the Keychain took {:?}", t.elapsed());
        }
    }
}
