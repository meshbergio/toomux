//! The MCP server as Claude Code runs it: a process on a pipe.

use std::io::Write;

#[test]
fn a_bad_line_doesnt_end_the_server() {
    std::fs::write(
        std::env::temp_dir().join("toomux-broken.toml"),
        "accounts = [[[",
    )
    .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_toomux"))
        .arg("mcp")
        // A config mid-edit doesn't take the tools away either.
        .env(
            "TOOMUX_CONFIG",
            std::env::temp_dir().join("toomux-broken.toml"),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = b"\xff\xfe not json\n".to_vec();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n");
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("-32700") && text.contains("\"id\":7"),
        "{text}"
    );
}
