#!/usr/bin/env node
import http from "node:http";
import fs from "node:fs";
import crypto from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";

const HOST = process.env.TOOMUX_CHATGPT_MODEL_HOST || "127.0.0.1";
const PORT = Number(process.env.TOOMUX_CHATGPT_MODEL_PORT || 34561);
const CDP_URL = process.env.TOOMUX_CHATGPT_CDP_URL || "http://127.0.0.1:9222";
const ORIGIN = process.env.TOOMUX_CHATGPT_ORIGIN || "https://chatgpt.com/";
const TEMPORARY_URL = new URL("/?temporary-chat=true", ORIGIN).href;
const KEY_FILE = process.env.TOOMUX_CHATGPT_MODEL_KEY_FILE || `${process.env.HOME}/.local/state/toomux/bonnie-bridge.key`;
const MODEL_NAME = "gpt-5.6-sol";
const DEFAULT_MODEL_ALIAS = process.env.TOOMUX_CHATGPT_MODEL_NAME || "bonnie";
const DEFAULT_REASONING_EFFORT = String(process.env.TOOMUX_CHATGPT_REASONING_EFFORT || "high").toLowerCase();
const REQUEST_TIMEOUT_MS = Math.max(30_000, Math.min(240_000, Number(process.env.TOOMUX_CHATGPT_MODEL_TIMEOUT_MS || 150_000)));
const TOKEN_CHARS_PER_TOKEN = Math.max(2.5, Math.min(5, Number(process.env.TOOMUX_CHATGPT_TOKEN_CHARS_PER_TOKEN || 3.2)));
const MAX_LANES = 1;
const MAX_BODY = 12 * 1024 * 1024;
const lanes = new Map();
const sessionLeases = new Map();
let laneManagerTail = Promise.resolve();
let queueDepth = 0;
let queuedForeground = 0;
let queuedBackground = 0;
let activeRequests = 0;

function readSecret(path) {
  const value = fs.readFileSync(path, "utf8").trim();
  if (!value) throw new Error(`secret file is empty: ${path}`);
  return value;
}

function safeEqual(a, b) {
  const aa = Buffer.from(String(a || ""));
  const bb = Buffer.from(String(b || ""));
  return aa.length === bb.length && crypto.timingSafeEqual(aa, bb);
}

function authorized(req) {
  const match = /^Bearer\s+(.+)$/i.exec(String(req.headers.authorization || ""));
  if (!match) return false;
  try { return safeEqual(match[1].trim(), readSecret(KEY_FILE)); }
  catch { return false; }
}

function sendJson(res, status, body) {
  const bytes = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": bytes.length,
    "cache-control": "no-store",
  });
  res.end(bytes);
}

function normalizeEffort(value, fallback = DEFAULT_REASONING_EFFORT) {
  const effort = String(value || fallback || "high").toLowerCase();
  if (effort === "medium" || effort === "standard") return "medium";
  if (effort === "high") return "high";
  const error = new Error(`unsupported reasoning_effort: ${value}`);
  error.status = 400;
  throw error;
}

function resolveModelRequest(body = {}) {
  const requested = String(body.model || DEFAULT_MODEL_ALIAS || "bonnie").toLowerCase();
  let effort = body.reasoning_effort ? normalizeEffort(body.reasoning_effort) : null;
  if (!effort) {
    if (["bonnie-medium", "gpt-5.6-sol-medium", "sol-medium"].includes(requested)) effort = "medium";
    else if (["bonnie", "bonnie-high", "gpt-5.6-sol", "gpt-5.6-sol-high", "sol-high"].includes(requested)) effort = "high";
    else effort = normalizeEffort(DEFAULT_REASONING_EFFORT);
  }
  return { model: MODEL_NAME, requested_model: requested, reasoning_effort: effort };
}

function normalizePriority(value) {
  return String(value || "foreground").toLowerCase() === "background" ? "background" : "foreground";
}

async function readJson(req) {
  const chunks = [];
  let total = 0;
  for await (const chunk of req) {
    total += chunk.length;
    if (total > MAX_BODY) {
      const error = new Error("request body too large");
      error.status = 413;
      throw error;
    }
    chunks.push(chunk);
  }
  try { return JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}"); }
  catch {
    const error = new Error("invalid JSON request body");
    error.status = 400;
    throw error;
  }
}

function createClient(webSocketDebuggerUrl) {
  const socket = new WebSocket(webSocketDebuggerUrl);
  let nextId = 1;
  const pending = new Map();
  const ready = new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", () => reject(new Error("CDP socket failed")), { once: true });
  });
  socket.addEventListener("message", (event) => {
    let message;
    try { message = JSON.parse(String(event.data)); } catch { return; }
    if (!message.id || !pending.has(message.id)) return;
    const entry = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) entry.reject(new Error(message.error.message || "CDP error"));
    else entry.resolve(message.result || {});
  });
  socket.addEventListener("close", () => {
    for (const entry of pending.values()) entry.reject(new Error("CDP socket closed"));
    pending.clear();
  });
  return {
    socket,
    ready,
    close() { try { socket.close(); } catch {} },
    send(method, params = {}) {
      return new Promise((resolve, reject) => {
        const id = nextId++;
        pending.set(id, { resolve, reject });
        try { socket.send(JSON.stringify({ id, method, params })); }
        catch (error) { pending.delete(id); reject(error); }
      });
    },
  };
}

async function pointOf(client, selector) {
  const handle = await client.send("Runtime.evaluate", {
    expression: `document.querySelector(${JSON.stringify(selector)})`,
  });
  const objectId = handle.result?.objectId;
  if (!objectId) return null;
  try {
    await client.send("DOM.enable");
    const { quads } = await client.send("DOM.getContentQuads", { objectId });
    if (!quads?.length) return null;
    const [x1, y1, x2, y2, x3, y3, x4, y4] = quads[0];
    const width = Math.abs(x2 - x1);
    const height = Math.abs(y3 - y2);
    if (width < 4 || height < 4) return null;
    return { x: (x1 + x2 + x3 + x4) / 4, y: (y1 + y2 + y3 + y4) / 4 };
  } finally {
    try { await client.send("Runtime.releaseObject", { objectId }); } catch {}
  }
}

async function pageProbe(client, point = null) {
  try {
    const reply = await client.send("Runtime.evaluate", {
      expression: `(() => {
        const at = ${point ? `document.elementFromPoint(${Number(point.x)}, ${Number(point.y)})` : "null"};
        const button = at && at.closest("button");
        const composer = document.querySelector('#prompt-textarea, div[contenteditable="true"], form textarea, textarea');
        return {
          url: location.href,
          at: at ? at.tagName.toLowerCase() + (at.id ? "#" + at.id : "") : null,
          button: button ? (button.getAttribute("data-testid") || button.getAttribute("aria-label") || "button") : null,
          composer_chars: composer ? String(composer.value ?? composer.innerText ?? composer.textContent ?? "").length : null,
          streaming: !!document.querySelector("[data-testid='stop-button'], button[aria-label='Stop generating'], button[aria-label='Stop responding']"),
          user_turns: document.querySelectorAll('[data-message-author-role="user"]').length,
          assistant_turns: document.querySelectorAll('[data-message-author-role="assistant"]').length,
          focused: document.hasFocus(),
          visible: document.visibilityState,
        };
      })()`,
      returnByValue: true,
    });
    return reply.result?.value || null;
  } catch (error) {
    return { error: String(error?.message || error).slice(0, 160) };
  }
}

async function currentUrl(client) {
  const reply = await client.send("Runtime.evaluate", { expression: "location.href", returnByValue: true });
  return String(reply.result?.value || "");
}

async function temporaryAssistantState(client) {
  const reply = await client.send("Runtime.evaluate", {
    expression: `(() => {
      const labels = [...document.querySelectorAll("h4")]
        .filter((el) => String(el.innerText || el.textContent || "").trim() === "ChatGPT said:");
      const texts = labels.map((label) => {
        const parent = label.parentElement;
        if (!parent) return "";
        const answer = [...parent.children].find((child) => child !== label);
        return String(answer?.innerText || answer?.textContent || "").trim();
      }).filter(Boolean);
      return {
        texts,
        streaming: !!document.querySelector("button[aria-label^='Stop']"),
        temporary: location.search.includes("temporary-chat=true"),
        url: location.href
      };
    })()`,
    returnByValue: true,
  });
  const value = reply.result?.value || {};
  return {
    texts: Array.isArray(value.texts) ? value.texts.map(String) : [],
    streaming: Boolean(value.streaming),
    temporary: Boolean(value.temporary),
    url: String(value.url || ""),
  };
}

async function temporaryPageState(client) {
  const reply = await client.send("Runtime.evaluate", {
    expression: `(async () => {
      let authenticated = false;
      try {
        const session = await (await fetch("/api/auth/session", { credentials: "include" })).json();
        authenticated = !!session?.accessToken && !!session?.user;
      } catch {}
      const composer = document.querySelector('#prompt-textarea, div[contenteditable="true"], form textarea, textarea');
      const model = document.querySelector('button[aria-label="Select ChatGPT model"]');
      const body = String(document.body.innerText || "");
      return {
        authenticated,
        composer: !!composer,
        composerChars: composer ? String(composer.value ?? composer.innerText ?? composer.textContent ?? "").trim().length : -1,
        temporary: location.search.includes("temporary-chat=true") &&
          (!!document.querySelector('button[aria-label="Turn off temporary chat"]') || /Temporary Chat|Temporary chat/.test(body)),
        model: model ? String(model.innerText || model.textContent || "").trim() : null,
        streaming: !!document.querySelector("button[aria-label^='Stop']"),
        url: location.href,
        readyState: document.readyState
      };
    })()`,
    returnByValue: true,
    awaitPromise: true,
  });
  return reply.result?.value || {};
}

async function ensureChatMode(client) {
  const state = async () => {
    const reply = await client.send("Runtime.evaluate", {
      expression: `JSON.stringify(Object.fromEntries([...document.querySelectorAll("button[role='radio'][data-state]")]
        .filter((el) => /^(Chat|Work)$/.test(String(el.innerText || el.textContent || "").trim()))
        .map((el) => [String(el.innerText || el.textContent || "").trim().toLowerCase(), el.getAttribute("data-state") || null])))`,
      returnByValue: true,
    });
    try { return JSON.parse(reply.result?.value || "{}"); } catch { return {}; }
  };

  let modes = {};
  for (let i = 0; i < 20; i += 1) {
    modes = await state().catch(() => ({}));
    if ((modes.chat === "on" || modes.chat === "off") && (modes.work === "on" || modes.work === "off")) break;
    await delay(100);
  }
  if (!("chat" in modes) && !("work" in modes)) return;
  if (modes.chat === "on" && modes.work !== "on") return;
  if (modes.work !== "on") throw new Error(`invalid Chat/Work state ${JSON.stringify(modes)}`);
  const point = await pointOf(client, "button[role='radio'][data-state]:first-of-type");
  if (!point) throw new Error("ChatGPT is in Work mode and Chat control is unavailable");
  const pick = { x: point.x, y: point.y, button: "left", clickCount: 1, buttons: 1 };
  await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: point.x, y: point.y, buttons: 0 });
  await client.send("Input.dispatchMouseEvent", { type: "mousePressed", ...pick });
  await client.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: pick.x, y: pick.y, button: "left", clickCount: 1, buttons: 0 });
  await delay(500);
}

async function modelPickerState(client) {
  const reply = await client.send("Runtime.evaluate", {
    expression: `(() => {
      const trigger = document.querySelector('button[aria-label="Select ChatGPT model"]');
      const slider = document.querySelector('[role="slider"]');
      const status = [...document.querySelectorAll('[role="status"]')]
        .map((el) => String(el.innerText || el.textContent || "").trim())
        .find((text) => /of 3\.$/.test(text)) || null;
      const sol = [...document.querySelectorAll('[role="menuitemradio"]')]
        .find((el) => /GPT-5\.6 Sol/.test(String(el.innerText || el.textContent || "")));
      return {
        open: trigger?.getAttribute("data-state") === "open" || trigger?.getAttribute("aria-expanded") === "true",
        trigger: String(trigger?.innerText || trigger?.textContent || "").trim(),
        sliderValue: slider?.getAttribute("aria-valuenow") ?? null,
        sliderMin: slider?.getAttribute("aria-valuemin") ?? null,
        sliderMax: slider?.getAttribute("aria-valuemax") ?? null,
        status,
        solChecked: sol?.getAttribute("aria-checked") ?? null
      };
    })()`,
    returnByValue: true,
  });
  return reply.result?.value || {};
}

async function openModelPicker(client) {
  let state = await modelPickerState(client);
  if (!state.open) {
    const point = await pointOf(client, 'button[aria-label="Select ChatGPT model"]');
    if (!point) throw new Error("ChatGPT model selector is unavailable");
    const click = { x: point.x, y: point.y, button: "left", clickCount: 1, buttons: 1 };
    await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: point.x, y: point.y, buttons: 0 });
    await client.send("Input.dispatchMouseEvent", { type: "mousePressed", ...click });
    await client.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: click.x, y: click.y, button: "left", clickCount: 1, buttons: 0 });
  }
  for (let i = 0; i < 20; i += 1) {
    state = await modelPickerState(client);
    if (state.open && state.sliderValue !== null) return state;
    await delay(50);
  }
  throw new Error("ChatGPT Thinking effort control did not appear");
}

async function closeModelPicker(client) {
  const state = await modelPickerState(client).catch(() => ({}));
  if (!state.open) return state;
  await client.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27, nativeVirtualKeyCode: 27 });
  await client.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27, nativeVirtualKeyCode: 27 });
  await delay(80);
  return modelPickerState(client).catch(() => ({}));
}

async function ensureSolEffort(client, effort) {
  const wanted = normalizeEffort(effort);
  const target = wanted === "high" ? 2 : 1;
  let state = await openModelPicker(client);
  if (state.solChecked !== "true") {
    await closeModelPicker(client);
    throw new Error(`GPT-5.6 Sol is not selected (aria-checked=${state.solChecked})`);
  }
  if (state.sliderMin !== "0" || state.sliderMax !== "2") {
    await closeModelPicker(client);
    throw new Error(`unexpected Thinking effort range ${state.sliderMin}..${state.sliderMax}`);
  }
  let current = Number(state.sliderValue);
  if (!Number.isInteger(current)) {
    await closeModelPicker(client);
    throw new Error("Thinking effort slider has no numeric value");
  }
  if (current !== target) {
    await client.send("Runtime.evaluate", {
      expression: `document.querySelector('[role="menuitem"][aria-label="Power"]')?.focus()`,
    });
    const key = current < target ? "ArrowRight" : "ArrowLeft";
    const code = key;
    const vk = key === "ArrowRight" ? 39 : 37;
    for (let step = 0; step < Math.abs(target - current); step += 1) {
      await client.send("Input.dispatchKeyEvent", { type: "keyDown", key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk });
      await client.send("Input.dispatchKeyEvent", { type: "keyUp", key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk });
      await delay(120);
    }
    state = await modelPickerState(client);
  }
  const label = wanted === "high" ? "High, 3 of 3." : "Medium, 2 of 3.";
  if (String(state.sliderValue) !== String(target) || state.status !== label || state.solChecked !== "true") {
    await closeModelPicker(client);
    throw new Error(`could not attest GPT-5.6 Sol ${wanted}: ${JSON.stringify(state)}`);
  }
  const closed = await closeModelPicker(client);
  if (closed.open) throw new Error("ChatGPT model selector did not close after effort selection");
  return { model: MODEL_NAME, reasoning_effort: wanted, slider_value: target, label };
}

async function findComposer(client) {
  const selectors = ["#prompt-textarea", "div[contenteditable='true']", "form textarea", "textarea"];
  for (let attempt = 0; attempt < 60; attempt += 1) {
    for (const selector of selectors) {
      const point = await pointOf(client, selector).catch(() => null);
      if (point) return { selector, point };
    }
    await delay(250);
  }
  return null;
}

async function clickSend(client) {
  const selectors = [
    "button[data-testid='send-button']:not(:disabled):not([aria-disabled='true'])",
    "button[aria-label='Send message']:not(:disabled):not([aria-disabled='true'])",
    "button[aria-label='Send prompt']:not(:disabled):not([aria-disabled='true'])",
    "form button[type='submit']:not(:disabled):not([aria-disabled='true'])",
  ];
  for (let attempt = 0; attempt < 50; attempt += 1) {
    for (const selector of selectors) {
      const point = await pointOf(client, selector).catch(() => null);
      if (!point) continue;
      const click = { x: point.x, y: point.y, button: "left", clickCount: 1, buttons: 1 };
      await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: point.x, y: point.y, buttons: 0 });
      const before = await pageProbe(client, point);
      await client.send("Input.dispatchMouseEvent", { type: "mousePressed", ...click });
      await client.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: click.x, y: click.y, button: "left", clickCount: 1, buttons: 0 });
      await delay(250);
      const after = await pageProbe(client, point);
      console.error(`[model-api] send probe before=${JSON.stringify(before)} after=${JSON.stringify(after)}`);
      return selector;
    }
    await delay(100);
  }
  throw new Error("ChatGPT Send control never became available");
}

async function composerLength(client) {
  const reply = await client.send("Runtime.evaluate", {
    expression: `(() => { const el = document.querySelector('#prompt-textarea, div[contenteditable="true"], form textarea, textarea'); if (!el) return -1; const text = String(el.value ?? el.innerText ?? el.textContent ?? ''); return text.trim() ? text.length : 0; })()`,
    returnByValue: true,
  });
  return Number(reply.result?.value ?? -1);
}

async function clearComposer(client, point) {
  const focus = { x: point.x, y: point.y, button: "left", clickCount: 1, buttons: 1 };
  await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: point.x, y: point.y, buttons: 0 });
  await client.send("Input.dispatchMouseEvent", { type: "mousePressed", ...focus });
  await client.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: focus.x, y: focus.y, button: "left", clickCount: 1, buttons: 0 });
  await delay(80);
  await client.send("Input.dispatchKeyEvent", { type: "keyDown", key: "a", code: "KeyA", windowsVirtualKeyCode: 65, nativeVirtualKeyCode: 65, modifiers: 2 });
  await client.send("Input.dispatchKeyEvent", { type: "keyUp", key: "a", code: "KeyA", windowsVirtualKeyCode: 65, nativeVirtualKeyCode: 65, modifiers: 2 });
  await client.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Backspace", code: "Backspace", windowsVirtualKeyCode: 8, nativeVirtualKeyCode: 8 });
  await client.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Backspace", code: "Backspace", windowsVirtualKeyCode: 8, nativeVirtualKeyCode: 8 });
  await delay(120);
  const remaining = await composerLength(client);
  if (remaining !== 0) throw new Error(`could not clear inherited ChatGPT draft (remaining_chars=${remaining})`);
}

async function assistantTexts(client) {
  const temporary = await temporaryAssistantState(client).catch(() => ({ texts: [] }));
  if (temporary.texts.length) return temporary.texts;

  const reply = await client.send("Runtime.evaluate", {
    expression: `(() => {
      const labels = [...document.querySelectorAll("h4")]
        .filter((el) => String(el.innerText || el.textContent || "").trim() === "ChatGPT said:");
      return labels.map((label) => {
        const parent = label.parentElement;
        const answer = parent ? [...parent.children].find((child) => child !== label) : null;
        return String(answer?.innerText || answer?.textContent || "").trim();
      }).filter(Boolean);
    })()`,
    returnByValue: true,
  });
  return Array.isArray(reply.result?.value) ? reply.result.value.map(String) : [];
}

async function listTargets() {
  const response = await fetch(new URL("/json/list", CDP_URL), { signal: AbortSignal.timeout(5000) });
  if (!response.ok) throw new Error(`could not list ChatGPT targets: HTTP ${response.status}`);
  return response.json();
}

async function openTarget(url = TEMPORARY_URL) {
  const response = await fetch(new URL(`/json/new?${encodeURIComponent(url)}`, CDP_URL), {
    method: "PUT",
    signal: AbortSignal.timeout(8000),
  });
  if (!response.ok) throw new Error(`could not open ChatGPT target: HTTP ${response.status}`);
  const target = await response.json();
  if (!target.webSocketDebuggerUrl) throw new Error("new ChatGPT target has no debugger URL");
  return target;
}

function laneSnapshot(lane) {
  return {
    id: lane.id,
    target_id: lane.targetId,
    session_id: lane.sessionId || null,
    active: lane.active ? 1 : 0,
    queued: lane.queues.foreground.length + lane.queues.background.length,
    queued_foreground: lane.queues.foreground.length,
    queued_background: lane.queues.background.length,
    effort: lane.effort || null,
    healthy: lane.healthy !== false,
  };
}

async function syncLaneTargets() {
  const targets = await listTargets();
  const pages = targets.filter((target) => target.type === "page" && /^https:\/\/chatgpt\.com\//.test(String(target.url || "")));
  for (const lane of lanes.values()) {
    const page = pages.find((p) => p.id === lane.targetId);
    if (!page) {
      lane.healthy = false;
      lane.target = null;
    } else {
      lane.target = page;
      lane.healthy = true;
    }
  }
  for (const page of pages) {
    if ([...lanes.values()].some((lane) => lane.targetId === page.id)) continue;
    if (lanes.size >= MAX_LANES) break;
    const lane = {
      id: `lane-${lanes.size + 1}`,
      targetId: page.id,
      target: page,
      sessionId: null,
      active: false,
      healthy: true,
      effort: null,
      queues: { foreground: [], background: [] },
      draining: false,
      resetPromise: Promise.resolve(),
      lastUsedAt: 0,
    };
    lanes.set(lane.id, lane);
  }
  while (lanes.size < MAX_LANES) {
    const target = await openTarget(TEMPORARY_URL);
    const lane = {
      id: `lane-${lanes.size + 1}`,
      targetId: target.id,
      target,
      sessionId: null,
      active: false,
      healthy: true,
      effort: null,
      queues: { foreground: [], background: [] },
      draining: false,
      resetPromise: Promise.resolve(),
      lastUsedAt: 0,
    };
    lanes.set(lane.id, lane);
  }
  return [...lanes.values()];
}

async function attachLane(lane) {
  await lane.resetPromise.catch(() => {});
  const targets = await listTargets();
  let target = targets.find((t) => t.id === lane.targetId);
  if (!target) {
    target = await openTarget(TEMPORARY_URL);
    lane.targetId = target.id;
    lane.target = target;
    lane.healthy = true;
  } else {
    lane.target = target;
  }
  const client = createClient(target.webSocketDebuggerUrl);
  await Promise.race([client.ready, delay(8000).then(() => { throw new Error("CDP connect timed out"); })]);
  await client.send("Runtime.enable");
  await client.send("DOM.enable");
  try { await client.send("Page.enable"); } catch {}
  return { target, client, lane };
}

async function waitForTemporaryReady(client, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  let state = {};
  while (Date.now() < deadline) {
    state = await temporaryPageState(client).catch(() => ({}));
    if (state.authenticated && state.composer && state.temporary && state.model && state.readyState !== "loading") return state;
    await delay(100);
  }
  throw new Error(`Temporary Chat did not become ready: ${JSON.stringify({
    authenticated: !!state.authenticated,
    composer: !!state.composer,
    temporary: !!state.temporary,
    readyState: state.readyState || null,
    url: state.url || null,
  })}`);
}

async function prepareTemporaryChat(client) {
  let state = await temporaryPageState(client).catch(() => ({}));
  const needsRoot = !state.temporary || /\/c\//.test(String(state.url || ""));
  if (needsRoot) {
    await client.send("Page.navigate", { url: TEMPORARY_URL });
    state = await waitForTemporaryReady(client);
  } else if (!state.authenticated || !state.composer || state.readyState === "loading") {
    state = await waitForTemporaryReady(client);
  }
  await ensureChatMode(client);
  state = await waitForTemporaryReady(client);
  return state;
}

async function resetTemporaryChat(client) {
  try { await client.send("Page.reload", { ignoreCache: false }); }
  catch {
    await client.send("Page.navigate", { url: TEMPORARY_URL });
  }

  const deadline = Date.now() + 1200;
  while (Date.now() < deadline) {
    const state = await temporaryPageState(client).catch(() => ({}));
    if (state.authenticated && state.composer && state.temporary && state.model && !/\/c\//.test(String(state.url || "")) && state.readyState !== "loading") {
      return state;
    }
    await delay(100);
  }

  await client.send("Page.navigate", { url: TEMPORARY_URL });
  return waitForTemporaryReady(client, 8000);
}

async function deepReadiness() {
  try {
    const pool = await syncLaneTargets();
    const states = [];
    for (const lane of pool) {
      let client = null;
      try {
        await lane.resetPromise.catch(() => {});
        ({ client } = await attachLane(lane));
        const state = await waitForTemporaryReady(client, 5000);
        states.push({ lane, state });
        lane.healthy = true;
      } catch (error) {
        lane.healthy = false;
        states.push({ lane, state: { error: String(error?.message || error) } });
      } finally {
        client?.close();
      }
    }
    const healthy = states.filter(({ lane }) => lane.healthy);
    return {
      ok: healthy.length > 0,
      authenticated: healthy.length > 0,
      composer: healthy.length > 0,
      temporary: healthy.length > 0,
      provider: MODEL_NAME,
      reasoning_effort: healthy[0]?.lane.effort || null,
      selector: healthy[0]?.state.model || null,
      streaming: states.some(({ state }) => Boolean(state.streaming)),
      active: activeRequests,
      queued: queueDepth,
      queued_foreground: queuedForeground,
      queued_background: queuedBackground,
      lanes: states.map(({ lane, state }) => ({
        ...laneSnapshot(lane),
        selector: state.model || null,
        urlKind: /\/c\//.test(String(state.url || "")) ? "temporary-conversation" : "temporary-root",
        error: state.error || null,
      })),
      max_lanes: MAX_LANES,
      leased_sessions: sessionLeases.size,
      urlKind: healthy.every(({ state }) => !/\/c\//.test(String(state.url || ""))) ? "temporary-root" : "mixed",
    };
  } catch (error) {
    return { ok: false, error: String(error?.message || error), active: activeRequests, queued: queueDepth, lanes: [] };
  }
}

async function selectLaneUnlocked(sessionId, effort) {
  await syncLaneTargets();
  if (sessionId && sessionLeases.has(sessionId)) {
    const leased = lanes.get(sessionLeases.get(sessionId));
    if (leased && leased.healthy !== false) return leased;
    sessionLeases.delete(sessionId);
  }
  const all = [...lanes.values()].filter((lane) => lane.healthy !== false);
  const matching = all.filter((lane) => lane.effort === effort);
  const convertible = all.filter((lane) => !lane.active && lane.queues.foreground.length === 0 && lane.queues.background.length === 0 && !lane.sessionId);
  if (!matching.length && convertible.length) {
    const lane = convertible[0];
    let client = null;
    try {
      ({ client } = await attachLane(lane));
      const ready = await prepareTemporaryChat(client);
      const attested = await ensureSolEffort(client, effort);
      lane.effort = attested.reasoning_effort;
      lane.healthy = true;
      console.error(`[model-api] repurposed ${lane.id} target=${lane.targetId} from=${ready.model || "unknown"} to=${lane.effort}`);
    } finally {
      client?.close();
    }
  }
  const candidates = all.filter((lane) => lane.effort === effort);
  if (!candidates.length) throw new Error(`no ${effort} ChatGPT browser lane available`);
  const idle = candidates.filter((lane) => !lane.active && lane.queues.foreground.length === 0 && lane.queues.background.length === 0);
  let lane = idle.find((candidate) => !candidate.sessionId)
    || idle[0]
    || candidates.sort((a, b) => {
      const aq = a.queues.foreground.length + a.queues.background.length + (a.active ? 1 : 0);
      const bq = b.queues.foreground.length + b.queues.background.length + (b.active ? 1 : 0);
      return aq - bq;
    })[0];
  if (!lane) throw new Error("no healthy ChatGPT browser lane available");
  if (sessionId) {
    if (lane.sessionId && lane.sessionId !== sessionId) sessionLeases.delete(lane.sessionId);
    lane.sessionId = sessionId;
    sessionLeases.set(sessionId, lane.id);
  }
  return lane;
}

function selectLane(sessionId, effort) {
  const run = laneManagerTail.catch(() => {}).then(() => selectLaneUnlocked(sessionId, effort));
  laneManagerTail = run.then(() => undefined, () => undefined);
  return run;
}

async function drainLane(lane) {
  if (lane.draining) return;
  lane.draining = true;
  try {
    while (lane.queues.foreground.length || lane.queues.background.length) {
      const priority = lane.queues.foreground.length ? "foreground" : "background";
      const item = lane.queues[priority].shift();
      queueDepth -= 1;
      if (priority === "foreground") queuedForeground -= 1;
      else queuedBackground -= 1;
      lane.active = true;
      activeRequests += 1;
      try {
        item.resolve(await item.task(lane));
        lane.lastUsedAt = Date.now();
      }
      catch (error) { item.reject(error); }
      finally {
        lane.active = false;
        activeRequests -= 1;
      }
    }
  } finally {
    lane.draining = false;
  }
}

async function enqueueModelTask(task, priority = "foreground", sessionId = "", effort = DEFAULT_REASONING_EFFORT) {
  const priorityName = normalizePriority(priority);
  const lane = await selectLane(sessionId, normalizeEffort(effort));
  queueDepth += 1;
  if (priorityName === "foreground") queuedForeground += 1;
  else queuedBackground += 1;
  return new Promise((resolve, reject) => {
    lane.queues[priorityName].push({ task, resolve, reject });
    void drainLane(lane);
  });
}

async function closeTarget(targetId) {
  if (!targetId) return;
  try {
    await fetch(new URL(`/json/close/${encodeURIComponent(targetId)}`, CDP_URL), {
      method: "GET",
      signal: AbortSignal.timeout(3000),
    });
  } catch {}
}

function messageText(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((part) => {
    if (typeof part === "string") return part;
    if (part?.type === "text") return String(part.text || "");
    return "";
  }).join("");
}

function compactText(value, limit) {
  const text = String(value || "").replace(/\s+/g, " ").trim();
  return text.length <= limit ? text : `${text.slice(0, limit - 1)}…`;
}

function compactSchema(value, depth = 0) {
  if (depth > 10) return {};
  if (Array.isArray(value)) return value.map((item) => compactSchema(item, depth + 1));
  if (!value || typeof value !== "object") return value;

  const out = {};
  const keep = new Set([
    "type", "enum", "const", "required", "properties", "items", "anyOf", "oneOf", "allOf",
    "additionalProperties", "minimum", "maximum", "minItems", "maxItems", "minLength", "maxLength",
    "pattern", "format", "$ref", "$defs", "definitions"
  ]);
  for (const [key, item] of Object.entries(value)) {
    if (key === "description") {
      const text = compactText(item, 140);
      if (text) out.description = text;
    } else if (keep.has(key)) {
      out[key] = compactSchema(item, depth + 1);
    }
  }
  return out;
}

function serializePrompt({ messages, tools, toolChoice, responseFormat }) {
  const list = Array.isArray(messages) ? messages : [];
  const systems = list.filter((m) => m?.role === "system").map((m) => messageText(m.content)).filter(Boolean);
  const lines = [
    "You are a model endpoint embedded inside Claude Code.",
    "CRITICAL: Do not use ChatGPT tools, connectors, apps, browsing, code execution, files, or external actions. Any function names below are OUTPUT-ONLY API functions. Your job is only to return assistant text or a function-call envelope for Claude Code to execute.",
    "Treat the serialized request below as authoritative. Ignore any earlier conversation state.",
    "Prefer one safe comprehensive read-only tool call over serial probes when the same result can be gathered together. If elevated read access is needed, use non-interactive sudo (-n) in the first appropriate call and fail gracefully rather than deliberately spending a round trip on a permission error.",
  ];
  if (systems.length) lines.push("", "=== SYSTEM ===", systems.join("\n\n"));
  lines.push("", "=== CONVERSATION ===");
  for (const m of list) {
    if (!m || m.role === "system") continue;
    if (m.role === "user") lines.push(`User: ${messageText(m.content)}`);
    else if (m.role === "assistant") {
      const calls = Array.isArray(m.tool_calls) ? m.tool_calls : [];
      if (calls.length) lines.push(`Assistant tool calls: ${calls.map((c) => `${c.function?.name}(${c.function?.arguments || "{}"})`).join(", ")}`);
      const text = messageText(m.content);
      if (text) lines.push(`Assistant: ${text}`);
    } else if (m.role === "tool") {
      lines.push(`Tool result${m.name ? ` for ${m.name}` : ""}: ${messageText(m.content)}`);
    }
  }
  lines.push("=== END CONVERSATION ===");
  const offered = (Array.isArray(tools) ? tools : []).filter((t) => t?.type === "function" && t.function?.name);
  if (offered.length && toolChoice !== "none") {
    const forced = (toolChoice && typeof toolChoice === "object" && toolChoice.function?.name)
      ? String(toolChoice.function.name)
      : (toolChoice === "required" ? "*" : null);
    lines.push(
      "",
      "=== OUTPUT-ONLY FUNCTION CALLING ===",
      "Never execute these functions yourself. If a function is appropriate, output ONLY one JSON object in exactly this shape:",
      '{"tool_calls":[{"name":"<function name>","arguments":{}}]}',
      "The envelope MUST parse with JSON.parse. Escape every inner double quote, backslash, newline, and control character inside string arguments.",
      "Functions:"
    );
    for (const tool of offered) {
      const description = compactText(tool.function.description || "", 420);
      const schema = compactSchema(tool.function.parameters || {});
      lines.push(`- ${tool.function.name}: ${description} — schema ${JSON.stringify(schema)}`);
    }
    if (forced === "*") lines.push("You MUST return exactly one function call.");
    else if (forced) lines.push(`You MUST return the function "${forced}".`);
    else lines.push("Return a function call when one applies; otherwise answer normally.");
  }
  if (responseFormat?.type === "json_object") lines.push("", "Return one valid JSON object and nothing else.");
  return lines.join("\n");
}

function repairSingleStringToolCall(raw) {
  const match = String(raw || "").match(/^\s*\{\s*"tool_calls"\s*:\s*\[\s*\{\s*"name"\s*:\s*"([^"]+)"\s*,\s*"arguments"\s*:\s*\{\s*"([^"]+)"\s*:\s*"([\s\S]*)"\s*\}\s*\}\s*\]\s*\}\s*$/);
  if (!match) return null;
  const [, name, key, encoded] = match;
  const value = encoded.replace(/\\\"/g, '"').replace(/\\\\/g, "\\");
  return { tool_calls: [{ name, arguments: { [key]: value } }] };
}

function parseReply(text, hasTools) {
  const raw = String(text || "").trim();
  if (hasTools) {
    const fenced = raw.match(/```(?:json)?\s*([\s\S]*?)```/i);
    const candidate = fenced ? fenced[1].trim() : raw;
    try {
      let parsed = null;
      try { parsed = JSON.parse(candidate); }
      catch { parsed = repairSingleStringToolCall(candidate); }
      const calls = Array.isArray(parsed?.tool_calls) ? parsed.tool_calls : (parsed?.name ? [parsed] : []);
      const toolCalls = calls.map((call, index) => {
        const name = String(call?.name ?? call?.function?.name ?? "").trim();
        if (!name) return null;
        const args = call?.arguments ?? call?.function?.arguments ?? {};
        return {
          id: `call_${crypto.randomBytes(10).toString("hex")}`,
          type: "function",
          index,
          function: { name, arguments: typeof args === "string" ? args : JSON.stringify(args) },
        };
      }).filter(Boolean);
      if (toolCalls.length) return { content: null, tool_calls: toolCalls };
    } catch {}
  }
  return { content: raw, tool_calls: null };
}

function estimateTokens(text) {
  return Math.max(0, Math.ceil(String(text || "").length / TOKEN_CHARS_PER_TOKEN));
}

async function modelCompletion({ model, reasoning_effort, messages, tools, tool_choice, response_format, onText = null }, lane) {
  const resolved = resolveModelRequest({ model, reasoning_effort });
  const prompt = serializePrompt({ messages, tools, toolChoice: tool_choice, responseFormat: response_format });
  console.error(`[model-api] request provider=${resolved.model} effort=${resolved.reasoning_effort} requested_model=${resolved.requested_model} prompt_chars=${prompt.length} messages=${Array.isArray(messages) ? messages.length : 0} tools=${Array.isArray(tools) ? tools.length : 0} queued=${queueDepth}`);

  let target = null;
  let client = null;
  const startedAt = Date.now();
  try {
    ({ target, client } = await attachLane(lane));
    const ready = await prepareTemporaryChat(client);
    if (lane.effort !== resolved.reasoning_effort) {
      throw new Error(`lane ${lane.id} is pinned to ${lane.effort}, cannot serve ${resolved.reasoning_effort}`);
    }
    const attested = {
      model: MODEL_NAME,
      reasoning_effort: lane.effort,
      slider_value: lane.effort === "high" ? 2 : 1,
      label: lane.effort === "high" ? "High, 3 of 3." : "Medium, 2 of 3.",
      reused: true,
    };
    console.error(`[model-api] lane=${lane.id} session=${lane.sessionId || "none"} target=${target?.id || "unknown"} temporary=true provider=${attested.model} effort=${attested.reasoning_effort} slider=${attested.slider_value} reused_effort=${attested.reused ? "yes" : "no"} previous_selector=${ready.model || "unknown"}`);

    const composer = await findComposer(client);
    if (!composer) throw new Error("signed-in Temporary Chat composer did not appear");
    const inherited = await composerLength(client);
    console.error(`[model-api] inherited_draft_chars=${inherited}`);
    if (inherited > 0) {
      await clearComposer(client, composer.point);
      console.error("[model-api] composer cleared");
    } else if (inherited < 0) {
      throw new Error("Temporary Chat composer disappeared before input");
    }

    const focus = { x: composer.point.x, y: composer.point.y, button: "left", clickCount: 1, buttons: 1 };
    await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: composer.point.x, y: composer.point.y, buttons: 0 });
    await client.send("Input.dispatchMouseEvent", { type: "mousePressed", ...focus });
    await client.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: focus.x, y: focus.y, button: "left", clickCount: 1, buttons: 0 });
    await delay(40);

    console.error("[model-api] insert start");
    await client.send("Input.insertText", { text: prompt });
    console.error(`[model-api] insert done composer_chars=${await composerLength(client)}`);
    await delay(60);
    const sendSelector = await clickSend(client);
    console.error(`[model-api] send clicked selector=${sendSelector}`);

    const deadline = Date.now() + REQUEST_TIMEOUT_MS;
    let latest = "";
    let streamed = "";
    let appeared = false;
    while (Date.now() < deadline) {
      await delay(150);
      const state = await temporaryAssistantState(client);
      const next = state.texts[state.texts.length - 1] || "";
      if (next && !appeared) {
        appeared = true;
        console.error(`[model-api] first assistant text observed after_ms=${Date.now() - startedAt}`);
      }
      if (next) {
        latest = next;
        if (onText && next !== streamed) {
          const delta = next.startsWith(streamed) ? next.slice(streamed.length) : "";
          if (delta) onText(delta);
          streamed = next;
        }
      }
      if (latest && !state.streaming) break;
    }
    if (!appeared || !latest) throw new Error(`timed out waiting for Temporary Chat reply at ${await currentUrl(client)}`);

    const hasTools = Array.isArray(tools) && tools.some((t) => t?.type === "function") && tool_choice !== "none";
    const parsed = parseReply(latest, hasTools);
    console.error(`[model-api] completed response_ms=${Date.now() - startedAt} finish=${parsed.tool_calls ? "tool_calls" : "stop"}`);
    return {
      model: resolved.model,
      reasoning_effort: resolved.reasoning_effort,
      ...parsed,
      finish_reason: parsed.tool_calls ? "tool_calls" : "stop",
      usage: {
        prompt_tokens: estimateTokens(prompt),
        completion_tokens: estimateTokens(latest),
        total_tokens: estimateTokens(prompt) + estimateTokens(latest),
      },
    };
  } finally {
    if (client) {
      const resetClient = client;
      const resetStartedAt = Date.now();
      client = null;
      lane.resetPromise = (async () => {
        try {
          const state = await resetTemporaryChat(resetClient);
          lane.healthy = true;
          console.error(`[model-api] lane=${lane.id} temporary reset ready reset_ms=${Date.now() - resetStartedAt} model=${state.model || "preserve"}`);
        } catch (error) {
          lane.healthy = false;
          console.error(`[model-api] lane=${lane.id} temporary reset failed: ${error?.message || error}`);
        } finally {
          resetClient.close();
        }
      })();
    }
  }
}

function sse(res, obj) {
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url || "/", `http://${HOST}:${PORT}`);
    if (req.method === "GET" && url.pathname === "/health") {
      return sendJson(res, 200, {
        ok: true,
        service: "toomux-chatgpt-model-api",
        provider: MODEL_NAME,
        default_model_alias: DEFAULT_MODEL_ALIAS,
        default_reasoning_effort: normalizeEffort(DEFAULT_REASONING_EFFORT),
        cdp: CDP_URL,
        temporary: true,
        active: activeRequests,
        queued: queueDepth,
        queued_foreground: queuedForeground,
        queued_background: queuedBackground,
        lanes: [...lanes.values()].map(laneSnapshot),
        max_lanes: MAX_LANES,
      });
    }
    if (req.method === "GET" && url.pathname === "/readyz") {
      const ready = await deepReadiness();
      return sendJson(res, ready.ok ? 200 : 503, ready);
    }
    if (!authorized(req)) return sendJson(res, 401, { error: { message: "invalid api key", type: "invalid_request_error" } });
    if (req.method === "GET" && url.pathname === "/v1/models") {
      return sendJson(res, 200, { object: "list", data: [
        { id: MODEL_NAME, object: "model", owned_by: "chatgpt-browser", reasoning_efforts: ["medium", "high"] },
        { id: "bonnie", object: "model", owned_by: "toomux-preset", canonical_model: MODEL_NAME, reasoning_effort: "high" },
        { id: "bonnie-medium", object: "model", owned_by: "toomux-preset", canonical_model: MODEL_NAME, reasoning_effort: "medium" }
      ] });
    }
    if (req.method !== "POST" || url.pathname !== "/v1/chat/completions") {
      return sendJson(res, 404, { error: { message: "not found", type: "invalid_request_error" } });
    }
    const body = await readJson(req);
    const resolved = resolveModelRequest(body);
    const priority = normalizePriority(body.priority);
    const providerSessionId = String(body.provider_session_id || "").trim();
    const requestBody = { ...body, model: resolved.model, reasoning_effort: resolved.reasoning_effort, provider_session_id: providerSessionId || undefined };
    const id = `chatcmpl_${crypto.randomBytes(10).toString("hex")}`;
    const created = Math.floor(Date.now() / 1000);
    const hasTools = Array.isArray(body.tools) && body.tools.some((tool) => tool?.type === "function") && body.tool_choice !== "none";

    if (body.stream && !hasTools) {
      res.writeHead(200, {
        "content-type": "text/event-stream; charset=utf-8",
        "cache-control": "no-store",
        connection: "keep-alive",
        "x-accel-buffering": "no",
      });
      sse(res, { id, object: "chat.completion.chunk", created, model: MODEL_NAME, choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] });
      let streamedText = false;
      const out = await enqueueModelTask((lane) => modelCompletion({
        ...requestBody,
        onText: (delta) => {
          streamedText = true;
          sse(res, { id, object: "chat.completion.chunk", created, model: MODEL_NAME, choices: [{ index: 0, delta: { content: delta }, finish_reason: null }] });
        },
      }, lane), priority, providerSessionId, resolved.reasoning_effort);
      if (!streamedText && out.content) {
        sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [{ index: 0, delta: { content: out.content }, finish_reason: null }] });
      }
      sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
      sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [], usage: out.usage });
      res.write("data: [DONE]\n\n");
      res.end();
      return;
    }

    const out = await enqueueModelTask((lane) => modelCompletion(requestBody, lane), priority, providerSessionId, resolved.reasoning_effort);
    const message = { role: "assistant", content: out.tool_calls ? null : out.content };
    if (out.tool_calls) message.tool_calls = out.tool_calls.map(({ id, function: fn }) => ({ id, type: "function", function: fn }));

    if (!body.stream) {
      return sendJson(res, 200, {
        id, object: "chat.completion", created, model: out.model, reasoning_effort: out.reasoning_effort,
        choices: [{ index: 0, message, finish_reason: out.finish_reason }],
        usage: out.usage,
      });
    }

    res.writeHead(200, {
      "content-type": "text/event-stream; charset=utf-8",
      "cache-control": "no-store",
      connection: "keep-alive",
      "x-accel-buffering": "no",
    });
    sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] });
    if (out.tool_calls) {
      sse(res, {
        id, object: "chat.completion.chunk", created, model: out.model,
        choices: [{ index: 0, delta: { tool_calls: out.tool_calls.map((call, index) => ({ index, id: call.id, type: "function", function: call.function })) }, finish_reason: null }],
      });
      sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] });
    } else {
      if (out.content) sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [{ index: 0, delta: { content: out.content }, finish_reason: null }] });
      sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
    }
    sse(res, { id, object: "chat.completion.chunk", created, model: out.model, choices: [], usage: out.usage });
    res.write("data: [DONE]\n\n");
    res.end();
  } catch (error) {
    const status = Number(error?.status) || 502;
    if (!res.headersSent) sendJson(res, status, { error: { message: error?.message || "model browser error", type: status >= 500 ? "server_error" : "invalid_request_error" } });
    else {
      try { sse(res, { error: { message: error?.message || "model browser error", type: "server_error" } }); res.write("data: [DONE]\n\n"); res.end(); } catch {}
    }
  }
});

server.listen(PORT, HOST, () => {
  process.stdout.write(`toomux ChatGPT multi-lane temporary-chat model API listening on http://${HOST}:${PORT} lanes=${MAX_LANES}\n`);
  void (async () => {
    const pool = await syncLaneTargets();
    for (const lane of pool) {
      let lastError = null;
      let ready = false;
      for (let attempt = 1; attempt <= 20; attempt += 1) {
        let client = null;
        try {
          ({ client } = await attachLane(lane));
          const state = await prepareTemporaryChat(client);
          lane.effort = /high/i.test(String(state.model || "")) ? "high" : (/medium/i.test(String(state.model || "")) ? "medium" : null);
          if (!lane.effort) throw new Error(`could not attest effort from selector ${state.model || "unknown"}`);
          lane.healthy = true;
          console.error(`[model-api] prewarmed ${lane.id} target=${lane.targetId} model=${state.model || "preserve"} effort=${lane.effort} attempt=${attempt}`);
          ready = true;
          break;
        } catch (error) {
          lastError = error;
          lane.healthy = false;
          if (attempt < 20) await delay(250);
        } finally {
          client?.close();
        }
      }
      if (!ready) console.error(`[model-api] ${lane.id} prewarm failed after retries: ${lastError?.message || lastError}`);
    }
  })().catch((error) => console.error(`[model-api] pool prewarm failed: ${error?.message || error}`));
});
