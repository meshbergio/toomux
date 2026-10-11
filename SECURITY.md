# Security

toomux manages long-lived Claude Code sessions, shell commands, account
configuration and local conversation history. Bugs in those boundaries matter,
so security reports are welcome.

## Reporting a vulnerability

Report it privately through GitHub:
[**Report a vulnerability**](https://github.com/meshbergio/toomux/security/advisories/new)
(also under the repository's **Security** tab). Only the maintainer sees the
report. Please don't open a public issue, and don't post credentials,
transcripts or exploit details anywhere public.

Include:

- the toomux version (`toomux --version`) or commit;
- the Claude Code version (`claude --version`), tmux version and operating
  system;
- steps to reproduce and the impact you expect;
- whether it needs local access, or a malicious repository, session or
  config.

You'll get an acknowledgement within a week. Fixes are released before the
details are published, and reporters are credited unless they ask not to be.

## Security model

toomux is a local developer tool, not a sandbox or a privilege boundary.

- It runs with the permissions of the user who launched it.
- Commands started by Claude Code keep those same user permissions.
- A configured notify_command is deliberately arbitrary shell code.
- Account sharing deliberately creates symlinks between Claude config folders.
- Before it starts Claude, toomux records folder trust in that account's
  .claude.json when you picked the folder yourself (`alt-n`, `toomux new`) or
  when another of your accounts already trusts it. It also copies the
  "onboarding done" flag and theme from another of your accounts, so a launch
  doesn't stop at Claude's first-run screens. It never trusts a folder for a
  session you didn't start yourself unless you've trusted it before.
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

Other network activity is explicit, opt-in or delegated:

- **Installing:** `curl -fsSL https://toomux.com/install | sh` fetches the
  install script from toomux.com. The script downloads release artifacts and
  their SHA-256 checksums from GitHub releases and checks them before
  installing. npm and Homebrew installs use those registries.
- **Signing in an account:** when you add an account in toomux, it runs Claude
  Code's own sign-in (`claude auth login`) and opens the claude.com link that
  sign-in prints in your browser.
- **Remote access (opt-in):** `toomux remote serve` runs an HTTP listener for
  the phone and Android remotes. It refuses to bind anything but loopback or
  a private-mesh address in 10.30.0.0/16, accepts peers only from that range,
  and requires a per-device bearer token on every request. Device tokens are
  stored hashed and can be revoked. Pairing codes are one-use and expire after
  10 minutes (Android) or 15 minutes (browser). The pairing link points at toomux.com/remote/, but its secret
  is carried in the URL fragment, which browsers never send to the server.
- **Local model provider (optional):** if a local provider's key file exists,
  toomux's status tick tells that provider which sessions are live, about
  every 30 seconds. It does this by posting to a broker on 127.0.0.1 (port
  34561 by default). `toomux provider` checks the provider's readiness on
  127.0.0.1:34560. Without the key file, nothing is sent.
- **Claude Code sessions** talk to the services configured in Claude Code.
- **Notices:** a user-supplied notify_command may send notices anywhere the
  user chooses.

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

| Version | Security fixes |
| --- | --- |
| Latest release | Yes |
| master | Yes, ahead of the next release |
| Older releases | No: please upgrade by rerunning the installer |

Fixes land on master and ship in the next release. Check reports against an
older version against the latest release first.
