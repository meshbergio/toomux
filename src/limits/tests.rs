use super::*;
use crate::registry::State;
use chrono::Local;

const DIALOG: &str = "\
⏺ Working on the parser.
  ⎿  You've hit your limit · resets 8pm (Europe/London)

────────────────────────────────────────
 What do you want to do?

 ❯ 1. Stop and wait for limit to reset
   2. Wait here, then continue automatically at 8pm
   3. Switch to usage credits

 Enter to confirm · Esc to cancel
";

#[test]
fn the_limit_dialog_is_recognised_however_it_is_drawn() {
    assert_eq!(screen_in(DIALOG), Screen::Dialog);
    // In a box, spaced oddly, numbered differently, reworded at the end.
    let boxed = "│ What  do you want to do?   │\n│ ❯ 4.  Stop and wait for limit to reset now │\n";
    assert_eq!(screen_in(boxed), Screen::Dialog);
    let usage_billing = " What do you want to do?\n ❯ 1. Add funds to continue\n   2. Stop\n";
    assert_eq!(screen_in(usage_billing), Screen::Dialog);
    assert_eq!(steps_to_wait(DIALOG), Some(1));
}

#[test]
fn claude_codes_own_countdown_is_recognised_so_it_is_left_alone() {
    for s in [
        "❯ \n  Continuing automatically at 8pm · esc to cancel\n",
        "  Usage limit reached · continuing automatically at 8pm · esc to cancel\n",
        "  Usage limit reset · continuing automatically\n",
        " What do you want to do?\n ❯ 1. Stop and wait for limit to reset\n   2. Don\u{2019}t continue automatically\n",
    ] {
        assert_eq!(screen_in(s), Screen::Continuing, "{s}");
    }
}

#[test]
fn a_conversation_quoting_the_dialog_is_not_the_dialog() {
    let mut quoted = String::from(
        "> What do you want to do? ❯ 1. Stop and wait for limit to reset\n\
         ⏺ That dialog blocks resumption.\n",
    );
    assert_eq!(screen_in(&quoted), Screen::Other);
    // Far up the screen, a real-looking one has long scrolled past.
    quoted = format!("{DIALOG}{}", "⏺ more work\n".repeat(30));
    assert_eq!(screen_in(&quoted), Screen::Other);
    assert_eq!(screen_in("────\n❯ \n────\n"), Screen::Other);
}

#[test]
fn resume_after_limit_reads_its_three_settings() {
    assert_eq!(Resume::parse("working"), Some(Resume::Working));
    assert_eq!(Resume::parse(" ALL "), Some(Resume::All));
    assert_eq!(Resume::parse("off"), Some(Resume::Off));
    assert_eq!(Resume::parse("sometimes"), None);
    assert_eq!(Resume::default(), Resume::Working);
}

/// State and runtime folders of the test's own.
struct Home(PathBuf, Option<std::ffi::OsString>);

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("toomux-lim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe {
            std::env::set_var("TOOMUX_HOME", &dir);
            std::env::set_var("XDG_RUNTIME_DIR", dir.join("run"));
        }
        Home(dir, runtime)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("TOOMUX_HOME");
            match &self.1 {
                Some(r) => std::env::set_var("XDG_RUNTIME_DIR", r),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn one_wake_up_waits_per_session_and_reset_until_a_reboot() {
    let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _home = Home::new("wake");
    let at = registry::now_ms() + 3_600_000;
    assert!(wake("s1", at), "the first sighting schedules it");
    assert!(!wake("s1", at), "later sightings find it waiting");
    assert!(wake("s2", at), "another session has its own");
    assert!(wake("s1", at + 7 * 86_400_000), "and a later limit its own");
    // A reboot takes the sleepers, and their claims with them.
    std::fs::remove_dir_all(crate::paths::runtime()).unwrap();
    assert!(wake("s1", at), "after a reboot it's scheduled again");
}

#[test]
fn the_status_line_finds_a_limit_in_the_latest_reply_only() {
    let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = Home::new("tail");
    let path = home.0.join("t.jsonl");
    let at = Local::now().to_rfc3339();
    let limit = json!({"type": "assistant", "error": "rate_limit", "timestamp": at,
        "message": {"content": [{"type": "text", "text": "You've hit your limit · resets 8pm"}]}});
    let side = json!({"type": "assistant", "isSidechain": true, "message": {"content": "sub"}});
    let real = json!({"type": "assistant", "message": {"content": "done"}});
    std::fs::write(&path, format!("{real}\n{limit}\n{side}\n")).unwrap();
    assert_eq!(
        last_reply_limit(&path).map(|h| h.text),
        Some("limit · resets 8pm".into())
    );
    std::fs::write(&path, format!("{limit}\n{real}\n")).unwrap();
    assert_eq!(last_reply_limit(&path), None);
}

// ---- against a real tmux, with a stand-in for Claude Code ----------------------

/// A stand-in for Claude Code at its limit: the dialog until Esc, then a
/// prompt that logs what's submitted. Every other key pressed on the dialog
/// is logged too. "continuing" shows Claude Code's own countdown instead.
const FAKE_CLAUDE: &str = r#"#!/bin/bash
log="$1"; mode="$2"
stty raw -echo
dialog() {
  clear
  printf '⏺ Working on the parser.\r\n  ⎿  You'"'"'ve hit your limit · resets 8pm\r\n\r\n'
  if [ "$mode" = continuing ]; then
    printf '❯ \r\n  Usage limit reached · continuing automatically at 8pm · esc to cancel\r\n'
  else
    printf ' What do you want to do?\r\n\r\n ❯ 1. Stop and wait for limit to reset\r\n   2. Wait here, then continue automatically at 8pm\r\n'
  fi
}
prompt() { clear; printf '────────────\r\n❯ '; }
dialog
state=dialog; buf=""
while IFS= read -r -s -n1 c; do
  if [ "$state" = dialog ]; then
    if [ "$c" = $'\e' ] && [ "$mode" != continuing ]; then state=prompt; prompt
    else printf 'KEY:%q\n' "$c" >> "$log"; fi
  elif [ "$c" = $'\r' ] || [ -z "$c" ]; then
    printf 'SUBMIT:%s\n' "$buf" >> "$log"; buf=""; prompt
  else
    buf="$buf$c"; printf '%s' "$c"
  fi
done
"#;

struct Rig {
    dir: PathBuf,
    sock: PathBuf,
    pane: String,
    log: PathBuf,
}

impl Rig {
    fn start(tag: &str, mode: &str) -> Option<Rig> {
        let dir = std::env::temp_dir().join(format!("tlr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let script = dir.join("claude");
        std::fs::write(&script, FAKE_CLAUDE).ok()?;
        let log = dir.join("keys.log");
        std::fs::write(&log, "").ok()?;
        let sock = dir.join("s");
        let cmd = format!("bash {} {} {mode}", script.display(), log.display());
        let out = crate::tmux::command()
            .arg("-S")
            .arg(&sock)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-x",
                "100",
                "-y",
                "30",
            ])
            .args(["-P", "-F", "#{pane_id}", &cmd])
            .env_remove("TMUX")
            .output()
            .ok()?;
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || !id.starts_with('%') {
            eprintln!("no tmux here: skipped");
            return None;
        }
        let rig = Rig {
            pane: crate::tmux::qualify(&sock.display().to_string(), &id),
            dir,
            sock,
            log,
        };
        let want = if mode == "continuing" {
            Screen::Continuing
        } else {
            Screen::Dialog
        };
        for _ in 0..50 {
            if screen(&rig.pane) == Some(want) {
                return Some(rig);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the stand-in never drew its screen");
    }

    fn keys(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// A session at this pane, with a transcript of these lines.
    fn session(&self, id: &str, lines: &[Value]) -> Session {
        let acct = self.dir.join("acct");
        let project = acct.join("projects").join("-work-app");
        std::fs::create_dir_all(&project).unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(project.join(format!("{id}.jsonl")), body).unwrap();
        Session {
            pid: 1,
            proc_start: None,
            id: id.into(),
            cwd: "/work/app".into(),
            name: "app".into(),
            title: "app".into(),
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            state: State::NeedsYou,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: registry::now_ms(),
            started_ms: 0,
            account: None,
            config_dir: Some(acct.display().to_string()),
            args: Vec::new(),
            env: Vec::new(),
            tty: None,
            pane: Some(crate::tmux::Pane {
                id: self.pane.clone(),
                session: "0".into(),
                window_index: "0".into(),
            }),
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = crate::tmux::command()
            .arg("-S")
            .arg(&self.sock)
            .arg("kill-server")
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config() -> Config {
    Config {
        accounts: Vec::new(),
        ..Config::default()
    }
}

/// A turn that worked and then met the limit, at `at`.
fn cut_off(at: chrono::DateTime<Local>, text: &str) -> Vec<Value> {
    vec![
        json!({"type": "user", "message": {"content": "fix the parser"}}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "On it."}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
        json!({"type": "assistant", "error": "rate_limit", "timestamp": at.to_rfc3339(),
            "message": {"content": [{"type": "text", "text": text}]}}),
    ]
}

/// A limit hit two days ago at 10:00, reset that evening.
fn lifted() -> (chrono::DateTime<Local>, &'static str) {
    let day = Local::now().date_naive() - chrono::Duration::days(2);
    let at = day
        .and_hms_opt(10, 0, 0)
        .and_then(|t| t.and_local_timezone(Local).earliest())
        .unwrap();
    (at, "You've hit your limit · resets 8pm")
}

#[test]
fn after_the_reset_a_cut_off_session_leaves_the_dialog_and_carries_on_once() {
    let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _home = Home::new("resume");
    let Some(rig) = Rig::start("resume", "dialog") else {
        return;
    };
    let (at, text) = lifted();
    let s = rig.session("cut-off-1", &cut_off(at, text));
    let cfg = config();
    let did = recover(&cfg, &s, registry::now_ms());
    assert_eq!(did.as_deref(), Some("limit lifted; told to carry on"));
    assert_eq!(
        rig.keys(),
        format!("SUBMIT:{RESUME_PROMPT}\n"),
        "only Esc reached the dialog, then the prompt"
    );
    // The same limit is never acted on twice.
    assert_eq!(recover(&cfg, &s, registry::now_ms()), None);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.keys(), format!("SUBMIT:{RESUME_PROMPT}\n"));
    let log = std::fs::read_to_string(dir().join("log.jsonl")).unwrap();
    assert!(log.contains("told to carry on"), "{log}");
}

#[test]
fn before_the_reset_nothing_is_pressed_into_the_dialog() {
    let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _home = Home::new("early");
    let Some(rig) = Rig::start("early", "dialog") else {
        return;
    };
    let now = Local::now();
    let reset = (now + chrono::Duration::hours(2))
        .format("%-I%P")
        .to_string();
    let s = rig.session(
        "early-1",
        &cut_off(now, &format!("You've hit your limit · resets {reset}")),
    );
    assert_eq!(recover(&config(), &s, registry::now_ms()), None);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.keys(), "");
    assert_eq!(screen(&rig.pane), Some(Screen::Dialog));
}

#[test]
fn claude_codes_own_countdown_gets_no_keys() {
    let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _home = Home::new("own");
    let Some(rig) = Rig::start("own", "continuing") else {
        return;
    };
    let (at, text) = lifted();
    let s = rig.session("own-1", &cut_off(at, text));
    assert_eq!(recover(&config(), &s, registry::now_ms()), None);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.keys(), "");
    // A voyage's tick doesn't type into it either.
    assert!(claude_continuing(&rig.pane));
}

#[test]
fn a_prompt_the_limit_refused_carries_on_only_when_all_is_asked_for() {
    let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _home = Home::new("refused");
    let Some(rig) = Rig::start("refused", "dialog") else {
        return;
    };
    let (at, text) = lifted();
    let refused = cut_off(at, text)[3..].to_vec();
    let s = rig.session("refused-1", &refused);
    let mut cfg = config();
    assert_eq!(recover(&cfg, &s, registry::now_ms()), None);
    assert_eq!(rig.keys(), "", "working: left as it was");
    let s = rig.session("refused-2", &refused);
    cfg.resume_after_limit = "all".into();
    assert_eq!(
        recover(&cfg, &s, registry::now_ms()).as_deref(),
        Some("limit lifted; told to carry on")
    );
}

#[test]
fn a_voyage_paused_at_the_limit_carries_on_after_the_reset() {
    let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _home = Home::new("voyage");
    let Some(rig) = Rig::start("voyage", "dialog") else {
        return;
    };
    let id = "voyage-1";
    // Its judge couldn't run at the limit: paused, before at_limit was kept.
    let mut q = crate::voyage::start(
        id,
        "/work/app",
        "the parser passes".into(),
        None,
        None,
        crate::voyage::Persistence::Steady,
    );
    q.status = crate::voyage::Status::Paused;
    q.why =
        Some("couldn't be checked 3 times in a row (You've hit your limit · resets 8pm)".into());
    std::fs::write(
        crate::voyage::dir().join(format!("{}.json", q.id)),
        serde_json::to_string(&q).unwrap(),
    )
    .unwrap();

    let (at, text) = lifted();
    let mut s = rig.session(id, &cut_off(at, text));
    // Off still carries a voyage on: it was asked for.
    let mut cfg = config();
    cfg.resume_after_limit = "off".into();
    assert_eq!(
        recover(&cfg, &s, registry::now_ms()).as_deref(),
        Some("limit lifted; its voyage carries on")
    );
    assert_eq!(
        rig.keys(),
        "",
        "the dialog closed; the voyage does the telling"
    );
    assert!(crate::voyage::open_for(id).unwrap().at_limit);

    // Idle a while after the dialog closed: the voyage's tick resumes it.
    s.state = State::Idle;
    s.since_ms = registry::now_ms() - 120_000;
    crate::voyage::tick(std::slice::from_ref(&s), registry::now_ms());
    let q = crate::voyage::open_for(id).unwrap();
    assert_eq!(q.status, crate::voyage::Status::Active);
    assert!(!q.at_limit);
    assert!(
        rig.keys()
            .starts_with(&format!("SUBMIT:[toomux voyage {}] The usage limit", q.id)),
        "{}",
        rig.keys()
    );
}
