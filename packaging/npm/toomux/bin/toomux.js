#!/usr/bin/env node
// Runs the toomux binary for this machine, from the @toomux/<os>-<cpu> package
// npm picked for it. Hooks and tmux call that binary directly, not through here.
const { spawnSync } = require("child_process");
const fs = require("fs");

const testing = process.env.NODE_ENV === "test";
const platform = testing && process.env.TOOMUX_TEST_PLATFORM
  ? process.env.TOOMUX_TEST_PLATFORM
  : process.platform;

if (platform === "win32") {
  const version = require("../package.json").version;
  const script = [
    "set -eu",
    "version=$1",
    "shift",
    "bin=$(command -v toomux || true)",
    "installed=",
    "if [ -n \"$bin\" ]; then",
    "  installed=$($bin --version 2>/dev/null | awk '{print $2}' || true)",
    "fi",
    "if [ \"$installed\" != \"$version\" ]; then",
    "  command -v curl >/dev/null 2>&1 || { echo 'toomux: WSL needs curl to install toomux.' >&2; exit 1; }",
    "  curl --proto '=https' --tlsv1.2 -fsSL \"https://raw.githubusercontent.com/meshbergio/toomux/v$version/install.sh\" | TOOMUX_VERSION=\"$version\" sh",
    "  bin=\"$HOME/.local/bin/toomux\"",
    "fi",
    "exec \"$bin\" \"$@\"" ,
  ].join("\n");
  const args = ["--exec", "sh", "-lc", script, "toomux-wsl", version, ...process.argv.slice(2)];
  if (testing && process.env.TOOMUX_TEST_WSL_CAPTURE) {
    fs.writeFileSync(process.env.TOOMUX_TEST_WSL_CAPTURE, JSON.stringify(args));
    process.exit(0);
  }
  const r = spawnSync("wsl", args, { stdio: "inherit" });
  if (r.error) {
    console.error("toomux: Windows support runs through WSL 2.");
    console.error("Install WSL 2, open it once, then run this command again.");
    console.error(`toomux: ${r.error.message}`);
    process.exit(1);
  }
  process.exit(r.status ?? 1);
}

const pkg = `@toomux/${platform}-${process.arch}`;
let bin;
try {
  bin = require.resolve(`${pkg}/bin/toomux`);
} catch {
  console.error(`toomux: there's no build for ${platform}-${process.arch}.`);
  console.error("It runs on Linux and macOS, x64 or arm64; on Windows, inside WSL 2.");
  process.exit(1);
}
const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (r.error) {
  console.error(`toomux: ${r.error.message}`);
  process.exit(1);
}
if (r.signal) process.kill(process.pid, r.signal);
process.exit(r.status ?? 1);
