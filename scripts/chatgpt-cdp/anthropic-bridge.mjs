#!/usr/bin/env node
import http from "node:http";
import fs from "node:fs";
import crypto from "node:crypto";

const HOST = process.env.BONNIE_ANTHROPIC_HOST || "127.0.0.1";
const PORT = Number(process.env.BONNIE_ANTHROPIC_PORT || 34560);
const UPSTREAM = (process.env.BONNIE_OPENAI_BASE || "http://127.0.0.1:34452/v1").replace(/\/+$/, "");
const UPSTREAM_ROOT = UPSTREAM.replace(/\/v1$/, "");
const STATE_DIR = process.env.BONNIE_STATE_DIR || `${process.env.HOME}/.local/state/bonnie-prod/bonnie-state`;
const UPSTREAM_TOKEN_FILE = process.env.BONNIE_UPSTREAM_TOKEN_FILE || `${STATE_DIR}/.auth-token`;
const CLIENT_KEY_FILE = process.env.BONNIE_ANTHROPIC_KEY_FILE || `${process.env.HOME}/.local/state/toomux/bonnie-bridge.key`;
const DEFAULT_MODEL_ALIAS = process.env.BONNIE_ANTHROPIC_MODEL || "bonnie";
const CANONICAL_MODEL = "gpt-5.6-sol";
const DEFAULT_REASONING_EFFORT = String(process.env.BONNIE_REASONING_EFFORT || "high").toLowerCase();
const MAX_BODY = 8 * 1024 * 1024;
const TOKEN_CHARS_PER_TOKEN = Math.max(2.5, Math.min(5, Number(process.env.BONNIE_TOKEN_CHARS_PER_TOKEN || 3.2)));

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

function clientAuthorized(req) {
  let provided = String(req.headers["x-api-key"] || "").trim();
  if (!provided) {
    const match = /^Bearer\s+(.+)$/i.exec(String(req.headers.authorization || ""));
    if (match) provided = match[1].trim();
  }
  if (!provided) return false;
  try { return safeEqual(provided, readSecret(CLIENT_KEY_FILE)); }
  catch { return false; }
}

function json(res, status, body) {
  const data = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": data.length,
    "cache-control": "no-store",
  });
  res.end(data);
}

function anthropicError(res, status, message, type = "api_error") {
  json(res, status, { type: "error", error: { type, message: String(message) } });
}

async function readJson(req) {
  const chunks = [];
  let total = 0;
  for await (const chunk of req) {
    total += chunk.length;
    if (total > MAX_BODY) {
      const err = new Error("request body too large");
      err.status = 413;
      throw err;
    }
    chunks.push(chunk);
  }
  if (!chunks.length) return {};
  try { return JSON.parse(Buffer.concat(chunks).toString("utf8")); }
  catch {
    const err = new Error("invalid JSON request body");
    err.status = 400;
    throw err;
  }
}

function textFromContent(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((part) => {
    if (typeof part === "string") return part;
    if (!part || typeof part !== "object") return "";
    if (part.type === "text") return String(part.text || "");
    if (part.type === "tool_result") return textFromContent(part.content);
    return "";
  }).join("");
}

function systemText(system) {
  if (typeof system === "string") return system;
  if (!Array.isArray(system)) return "";
  return system.map((p) => (p && p.type === "text" ? String(p.text || "") : "")).filter(Boolean).join("\n\n");
}

function anthropicToOpenAiMessages(body) {
  const out = [];
  const system = systemText(body.system);
  if (system) out.push({ role: "system", content: system });

  for (const message of Array.isArray(body.messages) ? body.messages : []) {
    if (!message || !["user", "assistant"].includes(message.role)) continue;
    const parts = Array.isArray(message.content)
      ? message.content
      : [{ type: "text", text: String(message.content || "") }];

    if (message.role === "assistant") {
      const text = parts.filter((p) => p?.type === "text").map((p) => String(p.text || "")).join("");
      const toolCalls = parts.filter((p) => p?.type === "tool_use" && p.name).map((p) => ({
        id: String(p.id || `toolu_${crypto.randomBytes(8).toString("hex")}`),
        type: "function",
        function: {
          name: String(p.name),
          arguments: JSON.stringify(p.input ?? {}),
        },
      }));
      const converted = { role: "assistant", content: text || null };
      if (toolCalls.length) converted.tool_calls = toolCalls;
      out.push(converted);
      continue;
    }

    const textParts = [];
    for (const part of parts) {
      if (!part || typeof part !== "object") continue;
      if (part.type === "text") {
        textParts.push(String(part.text || ""));
      } else if (part.type === "tool_result") {
        out.push({
          role: "tool",
          tool_call_id: String(part.tool_use_id || ""),
          name: part.name ? String(part.name) : undefined,
          content: textFromContent(part.content),
        });
      }
    }
    if (textParts.length || !parts.some((p) => p?.type === "tool_result")) {
      out.push({ role: "user", content: textParts.join("") });
    }
  }
  return out;
}

function anthropicToolsToOpenAi(tools) {
  return (Array.isArray(tools) ? tools : [])
    .filter((t) => t && t.name)
    .map((t) => ({
      type: "function",
      function: {
        name: String(t.name),
        description: String(t.description || ""),
        parameters: t.input_schema || { type: "object", properties: {} },
      },
    }));
}

function anthropicToolChoice(choice) {
  if (!choice || choice.type === "auto") return "auto";
  if (choice.type === "none") return "none";
  if (choice.type === "any") return "required";
  if (choice.type === "tool" && choice.name) return { type: "function", function: { name: String(choice.name) } };
  return "auto";
}

function normalizeEffort(value, fallback = DEFAULT_REASONING_EFFORT) {
  const effort = String(value || fallback || "high").toLowerCase();
  if (effort === "medium" || effort === "standard") return "medium";
  if (effort === "high") return "high";
  return "high";
}

function resolveProviderPreset(body = {}) {
  const requested = String(body.model || DEFAULT_MODEL_ALIAS).toLowerCase();
  let reasoningEffort = body.reasoning_effort ? normalizeEffort(body.reasoning_effort) : null;
  if (!reasoningEffort) {
    if (["bonnie-medium", "gpt-5.6-sol-medium", "sol-medium"].includes(requested)) reasoningEffort = "medium";
    else reasoningEffort = "high";
  }
  const priority = String(body?.metadata?.toomux_priority || body.priority || "foreground").toLowerCase() === "background" ? "background" : "foreground";
  return { model: CANONICAL_MODEL, reasoning_effort: reasoningEffort, priority, requested_model: requested };
}

function requestFingerprint(body) {
  const serialized = JSON.stringify({ system: body.system || "", messages: body.messages || [] });
  const tools = Array.isArray(body.tools) ? body.tools.length : 0;
  const system = systemText(body.system);
  return {
    chars: serialized.length,
    tools,
    system_hash: crypto.createHash("sha256").update(system).digest("hex").slice(0, 16),
    background_candidate: tools === 0 && serialized.length >= 100_000,
  };
}

function openAiRequest(body, stream) {
  const preset = resolveProviderPreset(body);
  const request = {
    model: preset.model,
    reasoning_effort: preset.reasoning_effort,
    priority: preset.priority,
    messages: anthropicToOpenAiMessages(body),
    tools: anthropicToolsToOpenAi(body.tools),
    tool_choice: anthropicToolChoice(body.tool_choice),
    stream: Boolean(stream),
  };
  if (!request.tools.length) {
    delete request.tools;
    delete request.tool_choice;
  }
  return request;
}

function anthropicUsage(usage = {}) {
  return {
    input_tokens: Number(usage.prompt_tokens || 0),
    output_tokens: Number(usage.completion_tokens || 0),
  };
}

function openAiChoiceToAnthropic(payload) {
  const choice = payload?.choices?.[0] || {};
  const message = choice.message || {};
  const content = [];
  if (typeof message.content === "string" && message.content.length) {
    content.push({ type: "text", text: message.content });
  }
  for (const call of Array.isArray(message.tool_calls) ? message.tool_calls : []) {
    let input = {};
    try { input = JSON.parse(call?.function?.arguments || "{}"); } catch { input = {}; }
    content.push({
      type: "tool_use",
      id: String(call.id || `toolu_${crypto.randomBytes(8).toString("hex")}`),
      name: String(call?.function?.name || ""),
      input,
    });
  }
  return {
    id: String(payload.id || `msg_${crypto.randomBytes(12).toString("hex")}`),
    type: "message",
    role: "assistant",
    model: String(payload.model || CANONICAL_MODEL),
    content,
    stop_reason: message.tool_calls?.length || choice.finish_reason === "tool_calls" ? "tool_use" : "end_turn",
    stop_sequence: null,
    usage: anthropicUsage(payload.usage),
  };
}

async function upstreamFetch(body, stream, signal) {
  const token = readSecret(UPSTREAM_TOKEN_FILE);
  const preset = resolveProviderPreset(body);
  const fingerprint = requestFingerprint(body);
  console.error(`[bridge] request provider=${preset.model} effort=${preset.reasoning_effort} priority=${preset.priority} requested_model=${preset.requested_model} chars=${fingerprint.chars} tools=${fingerprint.tools} system_hash=${fingerprint.system_hash} background_candidate=${fingerprint.background_candidate}`);
  return fetch(`${UPSTREAM}/chat/completions`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify(openAiRequest(body, stream)),
    signal,
  });
}

async function forwardNonStreaming(req, res, body) {
  const abort = new AbortController();
  req.on("close", () => { if (!res.writableEnded) abort.abort(); });
  const upstream = await upstreamFetch(body, false, abort.signal);
  const raw = await upstream.text();
  let parsed = null;
  try { parsed = JSON.parse(raw); } catch {}
  if (!upstream.ok) {
    const message = parsed?.error?.message || raw || `Bonnie upstream returned HTTP ${upstream.status}`;
    return anthropicError(res, upstream.status, message, upstream.status === 429 ? "rate_limit_error" : "api_error");
  }
  return json(res, 200, openAiChoiceToAnthropic(parsed));
}

function sendEvent(res, event, data) {
  res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

async function forwardStreaming(req, res, body) {
  const abort = new AbortController();
  req.on("close", () => { if (!res.writableEnded) abort.abort(); });
  const upstream = await upstreamFetch(body, true, abort.signal);
  if (!upstream.ok || !upstream.body) {
    const raw = await upstream.text();
    let parsed = null;
    try { parsed = JSON.parse(raw); } catch {}
    const message = parsed?.error?.message || raw || `Bonnie upstream returned HTTP ${upstream.status}`;
    return anthropicError(res, upstream.status || 502, message);
  }

  res.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-store",
    connection: "keep-alive",
    "x-accel-buffering": "no",
  });

  const messageId = `msg_${crypto.randomBytes(12).toString("hex")}`;
  sendEvent(res, "message_start", {
    type: "message_start",
    message: {
      id: messageId,
      type: "message",
      role: "assistant",
      model: CANONICAL_MODEL,
      content: [],
      stop_reason: null,
      stop_sequence: null,
      usage: { input_tokens: 0, output_tokens: 0 },
    },
  });

  const decoder = new TextDecoder();
  let buffer = "";
  let textIndex = null;
  let nextIndex = 0;
  const toolIndexes = new Map();
  let usage = { input_tokens: 0, output_tokens: 0 };
  let stopReason = "end_turn";

  const startText = () => {
    if (textIndex !== null) return textIndex;
    textIndex = nextIndex++;
    sendEvent(res, "content_block_start", {
      type: "content_block_start",
      index: textIndex,
      content_block: { type: "text", text: "" },
    });
    return textIndex;
  };

  const closeText = () => {
    if (textIndex === null) return;
    sendEvent(res, "content_block_stop", { type: "content_block_stop", index: textIndex });
    textIndex = null;
  };

  const startTool = (call, sourceIndex) => {
    if (toolIndexes.has(sourceIndex)) return toolIndexes.get(sourceIndex);
    const index = nextIndex++;
    toolIndexes.set(sourceIndex, index);
    sendEvent(res, "content_block_start", {
      type: "content_block_start",
      index,
      content_block: {
        type: "tool_use",
        id: String(call.id || `toolu_${crypto.randomBytes(8).toString("hex")}`),
        name: String(call?.function?.name || ""),
        input: {},
      },
    });
    return index;
  };

  const reader = upstream.body.getReader();
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    while (true) {
      const cut = buffer.indexOf("\n\n");
      if (cut < 0) break;
      const frame = buffer.slice(0, cut);
      buffer = buffer.slice(cut + 2);
      for (const line of frame.split("\n")) {
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (!data || data === "[DONE]") continue;
        let chunk;
        try { chunk = JSON.parse(data); } catch { continue; }
        if (chunk.error) throw new Error(chunk.error.message || "Bonnie upstream stream error");
        if (chunk.usage) usage = anthropicUsage(chunk.usage);
        const choice = chunk.choices?.[0];
        if (!choice) continue;
        if (choice.finish_reason === "tool_calls") stopReason = "tool_use";
        else if (choice.finish_reason) stopReason = "end_turn";
        const delta = choice.delta || {};
        if (typeof delta.content === "string" && delta.content) {
          const index = startText();
          sendEvent(res, "content_block_delta", {
            type: "content_block_delta",
            index,
            delta: { type: "text_delta", text: delta.content },
          });
        }
        for (const call of Array.isArray(delta.tool_calls) ? delta.tool_calls : []) {
          closeText();
          const sourceIndex = Number.isInteger(call.index) ? call.index : toolIndexes.size;
          const index = startTool(call, sourceIndex);
          const partial = String(call?.function?.arguments || "");
          if (partial) {
            sendEvent(res, "content_block_delta", {
              type: "content_block_delta",
              index,
              delta: { type: "input_json_delta", partial_json: partial },
            });
          }
        }
      }
    }
  }

  closeText();
  for (const index of toolIndexes.values()) {
    sendEvent(res, "content_block_stop", { type: "content_block_stop", index });
  }
  sendEvent(res, "message_delta", {
    type: "message_delta",
    delta: { stop_reason: stopReason, stop_sequence: null },
    usage: { output_tokens: usage.output_tokens },
  });
  sendEvent(res, "message_stop", { type: "message_stop" });
  res.end();
}

function approximateInputTokens(body) {
  const serialized = JSON.stringify({
    system: body.system || "",
    messages: body.messages || [],
    tools: body.tools || [],
  });
  return Math.max(1, Math.ceil(serialized.length / TOKEN_CHARS_PER_TOKEN));
}

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url || "/", `http://${HOST}:${PORT}`);
    if (req.method === "GET" && url.pathname === "/health") {
      return json(res, 200, { ok: true, service: "toomux-bonnie-anthropic-bridge", provider: CANONICAL_MODEL, default_model_alias: DEFAULT_MODEL_ALIAS, default_reasoning_effort: normalizeEffort(DEFAULT_REASONING_EFFORT) });
    }
    if (req.method === "GET" && url.pathname === "/readyz") {
      try {
        const upstream = await fetch(`${UPSTREAM_ROOT}/readyz`, { signal: AbortSignal.timeout(5000) });
        const body = await upstream.json();
        return json(res, upstream.ok ? 200 : 503, { bridge: true, upstream: body });
      } catch (error) {
        return json(res, 503, { bridge: true, upstream: { ok: false, error: String(error?.message || error) } });
      }
    }
    if (!clientAuthorized(req)) {
      return anthropicError(res, 401, "invalid API key", "authentication_error");
    }
    if (req.method !== "POST") {
      res.setHeader("allow", "POST");
      return anthropicError(res, 405, "method not allowed", "invalid_request_error");
    }
    const body = await readJson(req);
    if (url.pathname === "/v1/messages/count_tokens") {
      return json(res, 200, { input_tokens: approximateInputTokens(body) });
    }
    if (url.pathname !== "/v1/messages") {
      return anthropicError(res, 404, "not found", "not_found_error");
    }
    if (!Array.isArray(body.messages)) {
      return anthropicError(res, 400, "messages must be an array", "invalid_request_error");
    }
    if (body.stream) return await forwardStreaming(req, res, body);
    return await forwardNonStreaming(req, res, body);
  } catch (error) {
    if (error?.name === "AbortError") return;
    if (!res.headersSent) {
      return anthropicError(res, Number(error?.status) || 500, error?.message || "bridge error");
    }
    try {
      sendEvent(res, "error", { type: "error", error: { type: "api_error", message: error?.message || "bridge error" } });
      res.end();
    } catch {}
  }
});

server.listen(PORT, HOST, () => {
  process.stdout.write(`toomux Bonnie Anthropic bridge listening on http://${HOST}:${PORT}\n`);
});
