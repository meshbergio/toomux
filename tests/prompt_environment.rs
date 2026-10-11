//! Regression for agent-launched tmux servers inheriting NO_COLOR. Use an
//! isolated server and a tiny prompt renderer, never a live Claude account.
use std::{path::PathBuf, thread::sleep, time::Duration};

#[test]
fn inherited_no_color_does_not_turn_suggestions_into_drafts() {
    let root = std::env::temp_dir().join(format!("toomux-prompt-env-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let socket: PathBuf = root.join("tmux.sock");
    // This integration test runs in its own process and has no sibling tests.
    unsafe {
        std::env::set_var("NO_COLOR", "1");
        std::env::set_var("TOOMUX_HOME", root.join("home"));
    }
    let server = socket.to_str().unwrap();
    // Without the launch fix, the same suggestion becomes plain input and
    // clearing keys cannot remove it: it isn't an editable user draft.
    let command = "if [ -n \"${NO_COLOR-}\" ]; then printf '❯ restore done, carry on\\n'; else printf '\\033[39m❯ \\033[2mrestore done, carry on\\033[0m\\n'; fi; printf '────────────────────\\n'; sleep 30";
    let pane =
        toomux::tmux::new_server(server, "probe", root.to_str().unwrap(), &[], command).unwrap();
    let mut draft = None;
    for _ in 0..40 {
        draft = toomux::actions::prompt_draft(&pane);
        if draft.is_some() {
            break;
        }
        sleep(Duration::from_millis(25));
    }
    let environment = toomux::tmux::run_on(server, &["show-environment", "-g"]);
    let _ = toomux::tmux::run_on(server, &["kill-server"]);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(draft.as_deref(), Some(""), "a suggestion is not user input");
    assert!(
        !environment
            .unwrap()
            .lines()
            .any(|line| line.starts_with("NO_COLOR="))
    );
}
