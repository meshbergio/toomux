# toomux

Claude Code, minus the babysitting. Every session and account on one screen in
tmux, lossless handover instead of compaction. One Rust binary.

```sh
npm install -g toomux
toomux init --apply
```

Then press `alt-s` in tmux. You'll need Linux or macOS (Windows through WSL 2),
tmux 3.2 or later, and Claude Code. npm doesn't install tmux: on a Mac,
`brew install meshbergio/tap/toomux` brings both.

This package holds a small launcher; npm installs the binary for your machine
from `@toomux/linux-x64`, `@toomux/linux-arm64`, `@toomux/darwin-x64` or
`@toomux/darwin-arm64`.

More at [toomux.com](https://toomux.com) and on
[GitHub](https://github.com/meshbergio/toomux).
