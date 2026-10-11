<p align="center"><img src="https://raw.githubusercontent.com/meshbergio/toomux/master/assets/brand/logo/svg/toomux-logo-primary.svg" alt="toomux" width="300"></p>

**Claude Code, minus the babysitting.** Every Claude Code session you run, on
one screen in tmux. Long conversations hand over to a fresh session before
context gets slow and expensive, and nothing stops just because you looked
away. One Rust binary.

## Quick start

```sh
npm install -g toomux
toomux init --apply   # tmux keys, Claude Code hooks, status line (backed up)
toomux                # every session, one screen
```

Start Claude Code sessions as you always have, or press `alt-n` in toomux.
Handover, restart recovery and recovery after usage limits are on by default.
From anywhere in tmux, `alt-s` brings toomux up. On macOS, turn on "Use Option
as Meta key" in your terminal so the `alt-*` keys reach toomux.

## Requirements

- Linux or macOS, or Windows through WSL 2.
- tmux 3.2 or later. npm doesn't install tmux. On a Mac,
  `brew install meshbergio/tap/toomux` installs both, and
  `curl -fsSL https://toomux.com/install | sh` brings its own tmux on Linux
  and macOS.
- Claude Code.

## How this package works

This package is a small launcher. npm installs the binary for your machine
from `@toomux/linux-x64`, `@toomux/linux-arm64`, `@toomux/darwin-x64` or
`@toomux/darwin-arm64`.

On Windows, install it from PowerShell or Windows Terminal as usual. The
launcher uses your default WSL 2 distribution and installs the same toomux
version inside WSL on first run, where tmux and Claude Code live. There is no
native Windows build.

## More

The [README](https://github.com/meshbergio/toomux#readme) covers what toomux
does, and the [guide](https://github.com/meshbergio/toomux/blob/master/GUIDE.md)
has every command and setting. Issues and source are on
[GitHub](https://github.com/meshbergio/toomux). toomux is MIT or Apache 2.0
licensed.
