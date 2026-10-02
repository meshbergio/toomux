#!/usr/bin/env node
import fs from "node:fs";

const key = fs.readFileSync(process.env.HOME + "/.local/state/toomux/bonnie-bridge.key", "utf8").trim();
const api = "http://127.0.0.1:34561/v1/chat/completions";

async function call(effort, marker) {
  const response = await fetch(api, {
    method: "POST",
    headers: { authorization: "Bearer " + key, "content-type": "application/json" },
    body: JSON.stringify({
      model: "gpt-5.6-sol",
      reasoning_effort: effort,
      stream: false,
      messages: [{ role: "user", content: "Reply with exactly: " + marker }]
    })
  });
  if (!response.ok) throw new Error("completion HTTP " + response.status + ": " + await response.text());
  return response.json();
}

async function ready(expected) {
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch("http://127.0.0.1:34561/readyz");
      if (response.ok) {
        const body = await response.json();
        if (body.reasoning_effort === expected && body.urlKind === "temporary-root" && body.selector) return body;
      }
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error("ready did not attest " + expected);
}

for (const [effort, marker] of [["high", "SOL_HIGH_ACCEPTED"], ["medium", "SOL_MEDIUM_ACCEPTED"]]) {
  const out = await call(effort, marker);
  if (out.model !== "gpt-5.6-sol") throw new Error("wrong provider");
  if (out.reasoning_effort !== effort) throw new Error("wrong effort metadata");
  if (out.choices?.[0]?.message?.content !== marker) throw new Error("wrong response marker");
  const state = await ready(effort);
  console.log(JSON.stringify({ response: marker, model: out.model, effort: out.reasoning_effort, selector: state.selector, urlKind: state.urlKind }));
}
console.log("EFFORT_SWITCH_ACCEPTANCE=PASS");
