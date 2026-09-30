#!/usr/bin/env node
// Runs the toomux binary for this machine, from the @toomux/<os>-<cpu> package
// npm picked for it. Hooks and tmux call that binary directly, not through here.
const { spawnSync } = require("child_process");

const pkg = `@toomux/${process.platform}-${process.arch}`;
let bin;
try {
  bin = require.resolve(`${pkg}/bin/toomux`);
} catch {
  console.error(`toomux: there's no build for ${process.platform}-${process.arch}.`);
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
