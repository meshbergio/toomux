# Security

toomux manages long-lived Claude Code sessions, shell commands, account
configuration and local conversation history. Bugs in those boundaries matter,
so security reports are welcome.

## Reporting a vulnerability

Please use GitHub's **Security → Report a vulnerability** flow when it is
available for this repository. Do not publish credentials, transcripts,
exploit details or other sensitive material in a public issue.

If private vulnerability reporting is unavailable, open a minimal issue asking
the maintainer for a private reporting channel without including the sensitive
details.

Include the affected toomux version/commit, operating system, reproduction
steps, expected impact and whether the issue requires local access or a
malicious repository/session.

## Security model

toomux is a local developer tool, not a sandbox or a privilege boundary.

- It runs with the permissions of the user who launched it.
- Commands started by Claude Code keep those same user permissions.
- A configured notify_command is deliberately arbitrary shell code.
- Account sharing deliberately creates symlinks between Claude config folders.
- Operations such as account deletion, uninstall purge and worktree cleanup
  are destructive only when explicitly requested or enabled, and are guarded
  against broad/protected paths.
- A malicious local user with permission to edit toomux's config/state files
  is outside the threat model.

Do not run toomux as root.

## Credentials and network access

On Linux, Claude Code sign-in data lives in the account's
.credentials.json; on macOS it lives in the login Keychain. toomux reads the
credential only where needed to identify account state and retrieve usage.
Credential files are never part of account sharing.

For usage reporting, toomux invokes curl and sends the OAuth token in headers
through curl's standard input to Anthropic's
https://api.anthropic.com/api/oauth/usage endpoint. The token is not placed
on the process command line and toomux does not refresh or rewrite it.

Other network activity is explicit or delegated:

- the installer downloads release artifacts and their SHA-256 checksums from
  GitHub;
- Claude Code sessions themselves communicate with the services configured in
  Claude Code;
- a user-supplied notify_command may send notices anywhere the user chooses.

toomux has no telemetry service and does not upload its memory database or
transcript archive to a toomux-operated server.

## Sensitive data at rest

toomux where shows every location toomux reads or writes. By default:

- config: ~/.config/toomux/
- rebuildable state: ~/.local/state/toomux/
- transcript archive: ~/.local/share/toomux/
- runtime queues/sockets: $XDG_RUNTIME_DIR/toomux/

The memory database and generated sensitive working files use private
permissions where the platform supports them. Indexed memory is passed through
credential-pattern redaction before it is retained.

Redaction is defense in depth, not a secret scanner. The transcript archive is
intentionally lossless and may contain secrets or private data that appeared in
the original Claude Code transcript. Protect and back it up accordingly.

## Supported versions

Security fixes are made on the current master branch and included in the next
release. Reports against older versions should be checked against the current
release when practical.
