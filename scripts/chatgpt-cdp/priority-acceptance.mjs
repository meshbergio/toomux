#!/usr/bin/env node
import fs from "node:fs";

const key = fs.readFileSync(process.env.HOME + "/.local/state/toomux/bonnie-bridge.key", "utf8").trim();
const url = "http://127.0.0.1:34561/v1/chat/completions";
const order = [];
const started = performance.now();

async function request(marker, priority) {
  const response = await fetch(url, {
    method: "POST",
    headers: { authorization: "Bearer " + key, "content-type": "application/json" },
    body: JSON.stringify({
      model: "gpt-5.6-sol",
      reasoning_effort: "medium",
      priority,
      stream: false,
      messages: [{ role: "user", content: "Reply with exactly: " + marker }]
    })
  });
  if (!response.ok) throw new Error(marker + " HTTP " + response.status + ": " + await response.text());
  const body = await response.json();
  const text = body.choices?.[0]?.message?.content;
  if (text !== marker) throw new Error(marker + " got " + text);
  order.push(marker);
  return Math.round(performance.now() - started);
}

const a = request("PRIORITY_ACTIVE", "foreground");
await new Promise((r) => setTimeout(r, 150));
const b = request("PRIORITY_BACKGROUND", "background");
await new Promise((r) => setTimeout(r, 150));
const c = request("PRIORITY_FOREGROUND", "foreground");

await new Promise((r) => setTimeout(r, 300));
const mid = await (await fetch("http://127.0.0.1:34561/health")).json();
console.log(JSON.stringify({ mid: { active: mid.active, queued: mid.queued, foreground: mid.queued_foreground, background: mid.queued_background } }));

const times = await Promise.all([a, b, c]);
console.log(JSON.stringify({ order, times }));
if (order.join(",") !== "PRIORITY_ACTIVE,PRIORITY_FOREGROUND,PRIORITY_BACKGROUND") {
  throw new Error("unexpected completion order: " + order.join(","));
}
console.log("PRIORITY_ACCEPTANCE=PASS");
