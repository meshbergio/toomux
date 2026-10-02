#!/usr/bin/env node
import fs from "node:fs";

const key = fs.readFileSync(process.env.HOME + "/.local/state/toomux/bonnie-bridge.key", "utf8").trim();
const expected = "STREAM_EXACT_ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789_abcdefghijklmnopqrstuvwxyz_REPEAT_ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789_abcdefghijklmnopqrstuvwxyz_END";
const started = performance.now();
const response = await fetch("http://127.0.0.1:34561/v1/chat/completions", {
  method: "POST",
  headers: {
    authorization: "Bearer " + key,
    "content-type": "application/json",
  },
  body: JSON.stringify({
    model: "bonnie",
    stream: true,
    messages: [{ role: "user", content: "Reply with exactly this string and nothing else: " + expected }]
  })
});
if (!response.ok || !response.body) throw new Error("HTTP " + response.status);
const reader = response.body.getReader();
const decoder = new TextDecoder();
let buffer = "";
let first = null;
let last = null;
let done = false;
const events = [];
while (true) {
  const { value, done: streamDone } = await reader.read();
  if (streamDone) break;
  buffer += decoder.decode(value, { stream: true });
  while (true) {
    const cut = buffer.indexOf("\n\n");
    if (cut < 0) break;
    const frame = buffer.slice(0, cut);
    buffer = buffer.slice(cut + 2);
    for (const line of frame.split("\n")) {
      if (!line.startsWith("data: ")) continue;
      const data = line.slice(6).trim();
      const now = performance.now();
      if (data === "[DONE]") {
        done = true;
        last = now;
        continue;
      }
      let obj;
      try { obj = JSON.parse(data); } catch { continue; }
      const text = obj?.choices?.[0]?.delta?.content;
      if (typeof text === "string" && text) {
        if (first === null) first = now;
        events.push({ ms: Math.round(now - started), text });
      }
    }
  }
}
if (!done || first === null || last === null) throw new Error("stream did not produce content and DONE");
const joined = events.map((x) => x.text).join("");
const firstMs = Math.round(first - started);
const totalMs = Math.round(last - started);
const spreadMs = Math.round(last - first);
console.log(JSON.stringify({ first_content_ms: firstMs, total_ms: totalMs, spread_ms: spreadMs, content_events: events.length, exact: joined === expected, joined }));
if (joined !== expected) throw new Error("stream content diverged from exact marker");
if (events.length < 2) throw new Error("expected at least two content deltas");
if (spreadMs < 150) throw new Error("stream appears buffered: spread_ms=" + spreadMs);
console.log("STREAMING_INTEGRITY=PASS");
