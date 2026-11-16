#!/usr/bin/env node
// A small, dependency-free host for the eja app.
//
// The app is a lens over the journal: `talk` reads the User/Eja events,
// `unfold` reads the task graph, and settings are only the harness. Nothing
// here writes state except a turn, which is delegated to the eja binary.

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PUBLIC = path.join(HERE, "public");
const SETTINGS_PATH = path.join(HERE, "settings.json");
const MAX_BODY = 2 * 1024 * 1024;
const MAX_TEXT = 200_000;
const MAX_OUTPUT = 2 * 1024 * 1024;
const MAX_RAW = 128 * 1024;

function parseArgs(argv) {
  const out = { dir: ".eja", project: process.cwd(), port: 8787, host: "127.0.0.1", binary: "" };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--help" || arg === "-h") {
      out.help = true;
      continue;
    }
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2);
    const value = argv[i + 1];
    if (value == null || value.startsWith("--")) {
      throw new Error(`${arg} requires a value`);
    }
    i += 1;
    if (key === "dir") out.dir = value;
    else if (key === "project") out.project = value;
    else if (key === "port") out.port = Number(value);
    else if (key === "host") out.host = value;
    else if (key === "binary") out.binary = value;
    else throw new Error(`unknown option: ${arg}`);
  }
  if (!Number.isInteger(out.port) || out.port < 1 || out.port > 65535) {
    throw new Error("--port must be an integer between 1 and 65535");
  }
  out.project = path.resolve(out.project);
  out.dir = path.resolve(out.project, out.dir);
  return out;
}

const opts = parseArgs(process.argv.slice(2));
if (opts.help) {
  console.log(`usage: node app/server.mjs [--dir DIR] [--project DIR] [--port PORT] [--host HOST] [--binary FILE]

  --dir       eja state directory (default: .eja)
  --project   project directory the aja works in (default: cwd)
  --port      HTTP port (default: 8787)
  --host      bind address (default: 127.0.0.1)
  --binary    eja binary (default: target/debug/eja, target/release/eja, or EJA_BIN)`);
  process.exit(0);
}

if (!fs.existsSync(opts.project) || !fs.statSync(opts.project).isDirectory()) {
  console.error(`project directory does not exist: ${opts.project}`);
  process.exit(1);
}

function nowSec() {
  return Math.floor(Date.now() / 1000);
}

function readJson(file, fallback) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return fallback;
  }
}

function defaultSettings() {
  return {
    model: { baseUrl: "", apiKey: "", model: "", ajaModel: "" },
    connectors: { mcpConfig: "" },
    limits: { slots: 1, turnTimeoutMs: 0, ajaSteps: 0, ajaTimeoutMs: 0 },
    binary: "",
  };
}

function readSettings() {
  const raw = readJson(SETTINGS_PATH, {});
  const base = defaultSettings();
  const model = raw.model && typeof raw.model === "object" ? raw.model : {};
  const connectors = raw.connectors && typeof raw.connectors === "object" ? raw.connectors : {};
  const limits = raw.limits && typeof raw.limits === "object" ? raw.limits : {};
  return {
    model: {
      baseUrl: typeof model.baseUrl === "string" ? model.baseUrl : base.model.baseUrl,
      apiKey: typeof model.apiKey === "string" ? model.apiKey : base.model.apiKey,
      model: typeof model.model === "string" ? model.model : base.model.model,
      ajaModel: typeof model.ajaModel === "string" ? model.ajaModel : base.model.ajaModel,
    },
    connectors: {
      mcpConfig: typeof connectors.mcpConfig === "string" ? connectors.mcpConfig : "",
    },
    limits: {
      slots: Number(limits.slots) > 0 ? Number(limits.slots) : base.limits.slots,
      turnTimeoutMs: Number(limits.turnTimeoutMs) >= 0 ? Number(limits.turnTimeoutMs) : 0,
      ajaSteps: Number(limits.ajaSteps) > 0 ? Number(limits.ajaSteps) : 0,
      ajaTimeoutMs: Number(limits.ajaTimeoutMs) > 0 ? Number(limits.ajaTimeoutMs) : 0,
    },
    binary: typeof raw.binary === "string" ? raw.binary : "",
  };
}

function envValue(...names) {
  for (const name of names) {
    const value = process.env[name];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return "";
}

function effectiveSettings() {
  const s = readSettings();
  const baseUrl = s.model.baseUrl || envValue("LNA_BASE_URL", "OPENAI_BASE_URL", "JIYUAN_BASE_URL");
  const apiKey = s.model.apiKey || envValue("LNA_API_KEY", "OPENAI_API_KEY", "JIYUAN_API_KEY");
  const model = s.model.model || envValue("LNA_MODEL", "OPENAI_MODEL", "JIYUAN_MODEL");
  const ajaModel = s.model.ajaModel || envValue("EJA_AJA_MODEL", "LNA_AJA_MODEL");
  const mcpConfig = s.connectors.mcpConfig
    ? path.resolve(opts.project, s.connectors.mcpConfig)
    : fs.existsSync(path.join(opts.dir, "mcp.json"))
      ? path.join(opts.dir, "mcp.json")
      : "";
  const limits = {
    slots: Number(process.env.EJA_AJA_SLOTS) > 0 ? Number(process.env.EJA_AJA_SLOTS) : s.limits.slots,
    turnTimeoutMs:
      Number(process.env.EJA_TURN_TIMEOUT_MS) >= 0
        ? Number(process.env.EJA_TURN_TIMEOUT_MS)
        : s.limits.turnTimeoutMs,
    ajaSteps:
      Number(process.env.EJA_AJA_STEPS) > 0 ? Number(process.env.EJA_AJA_STEPS) : s.limits.ajaSteps,
    ajaTimeoutMs:
      Number(process.env.EJA_AJA_TIMEOUT_MS) > 0
        ? Number(process.env.EJA_AJA_TIMEOUT_MS)
        : s.limits.ajaTimeoutMs,
  };
  return {
    settings: s,
    model: { baseUrl, apiKey, model, ajaModel },
    mcpConfig,
    limits,
    binary: findBinary(s.binary),
  };
}

function findBinary(preferred) {
  const candidates = [];
  const add = (value) => {
    if (value) candidates.push(path.resolve(opts.project, value));
  };
  add(preferred || opts.binary);
  add(process.env.EJA_BIN);
  add(path.join(opts.project, "target", "debug", "eja"));
  add(path.join(opts.project, "target", "release", "eja"));
  for (const dir of (process.env.PATH || "").split(path.delimiter)) {
    if (dir) candidates.push(path.join(dir, process.platform === "win32" ? "eja.exe" : "eja"));
  }
  for (const candidate of candidates) {
    try {
      if (fs.statSync(candidate).isFile()) return candidate;
    } catch {
      // keep looking
    }
  }
  return "";
}

function scopeTaskId(scope) {
  if (typeof scope !== "string" || !scope.startsWith("task:")) return null;
  const id = Number(scope.slice(5));
  return Number.isInteger(id) && id > 0 ? id : null;
}

function readJournal() {
  const file = path.join(opts.dir, "log.jsonl");
  if (!fs.existsSync(file)) return { events: [], error: null, torn: false };
  const raw = fs.readFileSync(file, "utf8");
  const ended = raw.endsWith("\n");
  const lines = raw.split("\n");
  if (lines.length && lines[lines.length - 1] === "") lines.pop();
  const events = [];
  const errors = [];
  let torn = false;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (!line.trim()) continue;
    try {
      const event = JSON.parse(line);
      if (!event || typeof event !== "object" || !Number.isInteger(event.id)) {
        throw new Error("not an event");
      }
      events.push(event);
    } catch (error) {
      const isLast = i === lines.length - 1;
      if (isLast && !ended) {
        torn = true;
        break;
      }
      errors.push(`line ${i + 1}: ${error.message}`);
    }
  }
  return { events, error: errors.length ? errors.join("; ") : null, torn };
}

function foldState() {
  const journal = readJournal();
  const events = journal.events;
  const tasks = new Map();
  const taskEvents = new Map();
  const relations = [];
  const proposals = new Map();
  const closed = new Set();
  const identity = {
    goals: [],
    commitments: [],
    corrections: [],
    decisions: [],
    relationships: [],
  };
  const messages = [];
  const refused = [];
  let lastTurn = 0;

  for (const e of events) {
    lastTurn = Math.max(lastTurn, Number(e.turn) || 0);
    if (e.closes != null) closed.add(e.closes);

    if (e.kind === "user" || e.kind === "eja") {
      messages.push({
        id: e.id,
        role: e.kind,
        text: typeof e.text === "string" ? e.text : "",
        at: e.at,
        turn: e.turn,
        data: e.data ?? null,
      });
    }

    const tid = scopeTaskId(e.scope);
    if (tid) {
      if (!taskEvents.has(tid)) taskEvents.set(tid, []);
      taskEvents.get(tid).push(e.id);
    }

    if (e.kind === "task_opened") {
      const d = e.data && typeof e.data === "object" ? e.data : {};
      if (!tid) continue;
      tasks.set(tid, {
        id: tid,
        kind: d.kind === "group" ? "group" : "aja",
        title: typeof d.title === "string" && d.title ? d.title : e.text || `task ${tid}`,
        request: typeof d.request === "string" ? d.request : "",
        origin: typeof d.origin === "string" ? d.origin : "",
        acceptance: typeof d.acceptance === "string" ? d.acceptance : "",
        status: "open",
        created_at: e.at,
        assigned_at: null,
        reported_at: null,
        closed_at: null,
        close: null,
        assignment: null,
        run: {
          digest: null,
          evidence: d.evidence ?? null,
          evidence_id: null,
          evidence_result: null,
          witness_at: null,
          reconcile: null,
        },
        event_ids: [e.id],
        children: [],
      });
      continue;
    }

    if (e.kind === "task_assigned") {
      const task = tasks.get(tid);
      if (task) {
        task.status = "running";
        task.assigned_at = e.at;
        task.assignment = e.data ?? null;
      }
      continue;
    }

    if (e.kind === "worker_report") {
      const task = tasks.get(tid);
      if (task) {
        task.status = "waiting";
        task.reported_at = e.at;
        task.run.digest = typeof e.text === "string" ? e.text : "";
      }
      continue;
    }

    if (e.kind === "observed") {
      const d = e.data && typeof e.data === "object" ? e.data : {};
      if (d.kind === "witness") {
        const task = tasks.get(tid);
        if (task) {
          task.run.evidence_id = e.id;
          task.run.evidence_result = d.result ?? null;
          task.run.witness_at = e.at;
        }
      } else if (d.kind === "reconcile") {
        const target = scopeTaskId(d.target) ?? tid;
        const task = tasks.get(target);
        if (task) task.run.reconcile = d.result ?? null;
      }
      continue;
    }

    if (e.kind === "task_closed") {
      const task = tasks.get(tid);
      if (task) {
        const d = e.data && typeof e.data === "object" ? e.data : {};
        task.close = d;
        task.closed_at = e.at;
        task.status = d.accepted ? "done" : "failed";
      }
      continue;
    }

    if (e.kind === "task_relation") {
      const d = e.data && typeof e.data === "object" ? e.data : {};
      if (Number.isInteger(d.from) && Number.isInteger(d.to) && typeof d.kind === "string") {
        relations.push({ from: d.from, to: d.to, kind: d.kind });
      }
      continue;
    }

    if (e.kind === "proposal") {
      const d = e.data && typeof e.data === "object" ? e.data : {};
      proposals.set(e.id, {
        id: e.id,
        kind: typeof d.kind === "string" ? d.kind : "change",
        target_id: Number.isInteger(d.target_id) ? d.target_id : null,
        text: typeof e.text === "string" ? e.text : "",
        at: e.at,
        turn: e.turn,
      });
      continue;
    }

    if (e.kind === "proposal_confirmed" || e.kind === "proposal_rejected") {
      const pid = e.data?.proposal_id;
      if (Number.isInteger(pid)) proposals.delete(pid);
      continue;
    }

    if (e.kind === "goal" || e.kind === "commitment" || e.kind === "correction") {
      const pid = e.data?.proposal_id;
      if (Number.isInteger(pid)) proposals.delete(pid);
      const entry = {
        id: e.id,
        text: typeof e.text === "string" ? e.text : "",
        at: e.at,
        closes: e.closes ?? null,
        proposal_id: Number.isInteger(pid) ? pid : null,
      };
      if (e.kind === "goal") identity.goals.push(entry);
      else if (e.kind === "commitment") identity.commitments.push(entry);
      else identity.corrections.push(entry);
      continue;
    }

    if (e.kind === "decision") {
      identity.decisions.push({ id: e.id, text: e.text || "", at: e.at });
      continue;
    }

    if (e.kind === "relationship") {
      identity.relationships.push({ id: e.id, text: e.text || "", at: e.at });
      continue;
    }

    if (e.kind === "refused") {
      refused.push({ id: e.id, text: e.text || "", at: e.at, scope: e.scope });
    }
  }

  for (const key of ["goals", "commitments"]) {
    identity[key] = identity[key]
      .filter((entry) => !closed.has(entry.id))
      .sort((a, b) => b.id - a.id);
  }
  for (const key of ["corrections", "decisions", "relationships"]) {
    identity[key].sort((a, b) => b.id - a.id);
  }

  for (const task of tasks.values()) task.event_ids = taskEvents.get(task.id) || task.event_ids;

  for (const relation of relations) {
    if (relation.kind !== "part_of") continue;
    const child = tasks.get(relation.from);
    const parent = tasks.get(relation.to);
    if (child && parent) parent.children.push(child.id);
  }

  const taskList = [...tasks.values()].sort((a, b) => a.id - b.id);
  const count = (status) => taskList.filter((task) => task.status === status).length;
  const eff = effectiveSettings();
  return {
    now: nowSec(),
    stateDir: opts.dir,
    projectDir: opts.project,
    events,
    messages,
    tasks: taskList,
    relations,
    proposals: [...proposals.values()].sort((a, b) => a.id - b.id),
    identity,
    currentGoal: identity.goals[0] || null,
    refused,
    counts: {
      tasks: taskList.length,
      open: count("open"),
      running: count("running"),
      waiting: count("waiting"),
      done: count("done"),
      failed: count("failed"),
      messages: messages.length,
    },
    lastTurn,
    lastEvent: events.length ? events[events.length - 1].id : 0,
    health: {
      binary: Boolean(eff.binary),
      binaryPath: eff.binary,
      model: Boolean(eff.model.baseUrl && eff.model.model && eff.model.apiKey),
      modelName: eff.model.model,
      state: fs.existsSync(opts.dir),
      mcp: Boolean(eff.mcpConfig && fs.existsSync(eff.mcpConfig)),
      journal: journal.error ? "error" : "ok",
      journalError: journal.error,
      torn: journal.torn,
      turnInFlight,
    },
  };
}

function settingsView() {
  const eff = effectiveSettings();
  const s = eff.settings;
  const key = eff.model.apiKey;
  return {
    model: {
      baseUrl: s.model.baseUrl,
      model: s.model.model,
      ajaModel: s.model.ajaModel,
      hasApiKey: Boolean(key),
      apiKeyHint: key ? `•••• ${key.slice(-4)}` : "",
      effectiveBaseUrl: eff.model.baseUrl,
      effectiveModel: eff.model.model,
    },
    connectors: { mcpConfig: s.connectors.mcpConfig, effective: eff.mcpConfig },
    limits: s.limits,
    binary: s.binary,
    effectiveBinary: eff.binary,
    stateDir: opts.dir,
    projectDir: opts.project,
    health: {
      binary: Boolean(eff.binary),
      model: Boolean(eff.model.baseUrl && eff.model.model && eff.model.apiKey),
      mcp: Boolean(eff.mcpConfig && fs.existsSync(eff.mcpConfig)),
      state: fs.existsSync(opts.dir),
    },
  };
}

function writeSettings(next) {
  const s = next && typeof next === "object" ? next : {};
  const current = readSettings();
  const model = s.model && typeof s.model === "object" ? s.model : {};
  const providedKey = Object.prototype.hasOwnProperty.call(model, "apiKey");
  const apiKey =
    model.clearApiKey === true
      ? ""
      : providedKey
        ? typeof model.apiKey === "string"
          ? model.apiKey.trim()
          : ""
        : current.model.apiKey;
  const raw = {
    model: {
      baseUrl: typeof model.baseUrl === "string" ? model.baseUrl.trim() : "",
      apiKey,
      model: typeof model.model === "string" ? model.model.trim() : "",
      ajaModel: typeof model.ajaModel === "string" ? model.ajaModel.trim() : "",
    },
    connectors: {
      mcpConfig:
        typeof s.connectors?.mcpConfig === "string" ? s.connectors.mcpConfig.trim() : "",
    },
    limits: {
      slots: Math.max(1, Math.floor(Number(s.limits?.slots) || 1)),
      turnTimeoutMs: Math.max(0, Math.floor(Number(s.limits?.turnTimeoutMs) || 0)),
      ajaSteps: Math.max(0, Math.floor(Number(s.limits?.ajaSteps) || 0)),
      ajaTimeoutMs: Math.max(0, Math.floor(Number(s.limits?.ajaTimeoutMs) || 0)),
    },
    binary: typeof s.binary === "string" ? s.binary.trim() : "",
  };
  fs.writeFileSync(SETTINGS_PATH, JSON.stringify(raw, null, 2), { mode: 0o600 });
  try {
    fs.chmodSync(SETTINGS_PATH, 0o600);
  } catch {
    // a platform without POSIX modes still keeps the file in the app directory
  }
}

function clip(value, limit = MAX_OUTPUT) {
  const text = typeof value === "string" ? value : String(value ?? "");
  return text.length > limit ? `${text.slice(0, limit)}\n[truncated]` : text;
}

function json(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(body),
    "cache-control": "no-store",
  });
  res.end(body);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    req.on("data", (chunk) => {
      size += chunk.length;
      if (size > MAX_BODY) {
        reject(new Error("request body too large"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => {
      try {
        resolve(chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : {});
      } catch {
        reject(new Error("request body must be JSON"));
      }
    });
    req.on("error", reject);
  });
}

let turnInFlight = false;
let currentChild = null;

async function turn(req, res) {
  if (turnInFlight) {
    json(res, 409, { ok: false, error: "a turn is already running" });
    return;
  }
  const body = await readBody(req);
  const text = typeof body.text === "string" ? body.text.trim() : "";
  if (!text) {
    json(res, 400, { ok: false, error: "text is required" });
    return;
  }
  if (text.length > MAX_TEXT) {
    json(res, 413, { ok: false, error: `text is longer than ${MAX_TEXT} characters` });
    return;
  }
  const focus = Number.isInteger(body.focus) && body.focus > 0 ? body.focus : null;
  const approvals = Array.isArray(body.approvals)
    ? body.approvals.filter((id) => Number.isInteger(id) && id > 0)
    : [];
  const eff = effectiveSettings();
  if (!eff.binary) {
    json(res, 503, {
      ok: false,
      error: "eja binary not found. Run `cargo build` first, or set the binary in settings.",
    });
    return;
  }
  if (!eff.model.baseUrl || !eff.model.apiKey || !eff.model.model) {
    json(res, 503, {
      ok: false,
      error: "model is not configured. Open settings and set base URL, key, and model.",
    });
    return;
  }

  const args = ["--dir", opts.dir];
  if (focus) args.push("--task", String(focus));
  if (eff.mcpConfig) args.push("--mcp-config", eff.mcpConfig);
  for (const id of approvals) args.push("--confirm", String(id));
  args.push("turn", text);

  const env = {
    ...process.env,
    LNA_BASE_URL: eff.model.baseUrl,
    LNA_API_KEY: eff.model.apiKey,
    LNA_MODEL: eff.model.model,
  };
  if (eff.model.ajaModel) env.EJA_AJA_MODEL = eff.model.ajaModel;
  if (eff.limits.slots > 0) env.EJA_AJA_SLOTS = String(eff.limits.slots);
  if (eff.limits.turnTimeoutMs > 0) env.EJA_TURN_TIMEOUT_MS = String(eff.limits.turnTimeoutMs);
  if (eff.limits.ajaSteps > 0) env.EJA_AJA_STEPS = String(eff.limits.ajaSteps);
  if (eff.limits.ajaTimeoutMs > 0) env.EJA_AJA_TIMEOUT_MS = String(eff.limits.ajaTimeoutMs);

  turnInFlight = true;
  const started = Date.now();
  let child;
  try {
    child = spawn(eff.binary, args, {
      cwd: opts.project,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (error) {
    turnInFlight = false;
    json(res, 500, { ok: false, error: `could not start eja: ${error.message}` });
    return;
  }
  currentChild = child;
  let stdout = "";
  let stderr = "";
  const collect = (chunk, append) => {
    const next = append + chunk.toString("utf8");
    return next.length > MAX_OUTPUT ? next.slice(0, MAX_OUTPUT) : next;
  };
  child.stdout.on("data", (chunk) => {
    stdout = collect(chunk, stdout);
  });
  child.stderr.on("data", (chunk) => {
    stderr = collect(chunk, stderr);
  });

  const timeoutMs = eff.limits.turnTimeoutMs > 0 ? eff.limits.turnTimeoutMs + 60_000 : 35 * 60_000;
  const timer = setTimeout(() => {
    if (!child.killed) {
      child.kill("SIGINT");
      setTimeout(() => {
        if (!child.killed) child.kill("SIGKILL");
      }, 5_000).unref();
    }
  }, timeoutMs);
  timer.unref();

  let settled = false;
  const finish = (payload) => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    turnInFlight = false;
    currentChild = null;
    json(res, payload.status, {
      ...payload.body,
      elapsedMs: Date.now() - started,
      stdout: clip(stdout),
      stderr: clip(stderr),
    });
  };

  child.on("error", (error) => {
    finish({ status: 500, body: { ok: false, error: `could not start eja: ${error.message}` } });
  });
  child.on("close", (code, signal) => {
    const ok = code === 0;
    finish({
      status: ok ? 200 : 500,
      body: {
        ok,
        code,
        signal: signal || null,
        error: ok
          ? undefined
          : signal
            ? `eja stopped by ${signal}`
            : stderr.trim() || stdout.trim() || `eja exited with code ${code}`,
      },
    });
  });
}

function safeRead(file, root) {
  const resolved = path.resolve(file);
  const base = path.resolve(root);
  if (resolved !== base && !resolved.startsWith(base + path.sep)) {
    const error = new Error("path escapes its root");
    error.status = 400;
    throw error;
  }
  if (!fs.existsSync(resolved) || !fs.statSync(resolved).isFile()) {
    const error = new Error("not found");
    error.status = 404;
    throw error;
  }
  const raw = fs.readFileSync(resolved);
  const truncated = raw.length > MAX_RAW;
  return {
    content: raw.subarray(0, MAX_RAW).toString("utf8"),
    bytes: raw.length,
    truncated,
  };
}

function raw(res, url) {
  const address = url.searchParams.get("address") || "";
  const journal = readJournal();
  if (address.startsWith("log:event:")) {
    const id = Number(address.slice("log:event:".length));
    const event = journal.events.find((item) => item.id === id);
    if (!event) {
      json(res, 404, { error: "no such event" });
      return;
    }
    json(res, 200, { address, kind: "json", content: JSON.stringify(event, null, 2) });
    return;
  }
  if (address.startsWith("log:task:")) {
    const id = Number(address.slice("log:task:".length));
    const state = foldState();
    const task = state.tasks.find((item) => item.id === id);
    if (!task) {
      json(res, 404, { error: "no such task" });
      return;
    }
    const events = journal.events.filter((item) => item.id && task.event_ids.includes(item.id));
    json(res, 200, {
      address,
      kind: "json",
      content: JSON.stringify({ task, events }, null, 2),
    });
    return;
  }
  if (address.startsWith("record:")) {
    const rest = address.slice("record:".length);
    const slash = rest.indexOf("/");
    if (slash < 1) {
      json(res, 400, { error: "record address needs an aja and a path" });
      return;
    }
    const aja = Number(rest.slice(0, slash));
    const relative = rest.slice(slash + 1);
    if (!Number.isInteger(aja) || aja < 1) {
      json(res, 400, { error: "invalid aja id" });
      return;
    }
    const root = path.join(opts.dir, "ajas", String(aja));
    try {
      const read = safeRead(path.join(root, relative), root);
      json(res, 200, { address, kind: "text", ...read });
    } catch (error) {
      json(res, error.status || 500, { error: error.message });
    }
    return;
  }
  if (address.startsWith("world:")) {
    const relative = address.slice("world:".length);
    try {
      const read = safeRead(path.join(opts.project, relative), opts.project);
      json(res, 200, { address, kind: "text", ...read });
    } catch (error) {
      json(res, error.status || 500, { error: error.message });
    }
    return;
  }
  json(res, 400, { error: "unknown address; use log:, record:, or world:" });
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
};

function serveStatic(res, pathname) {
  const relative = pathname === "/" ? "index.html" : pathname.replace(/^\/+/, "");
  const file = path.resolve(PUBLIC, relative);
  if (!file.startsWith(path.resolve(PUBLIC) + path.sep) && file !== path.join(PUBLIC, "index.html")) {
    json(res, 403, { error: "forbidden" });
    return;
  }
  if (!fs.existsSync(file) || !fs.statSync(file).isFile()) {
    res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    res.end("not found");
    return;
  }
  const body = fs.readFileSync(file);
  res.writeHead(200, {
    "content-type": MIME[path.extname(file)] || "application/octet-stream",
    "content-length": body.length,
    "cache-control": "no-cache",
  });
  res.end(body);
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url || "/", `http://${req.headers.host || "127.0.0.1"}`);
  try {
    if (req.method === "GET" && url.pathname === "/api/state") {
      json(res, 200, foldState());
      return;
    }
    if (req.method === "GET" && url.pathname === "/api/settings") {
      json(res, 200, settingsView());
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/settings") {
      const body = await readBody(req);
      const current = readSettings();
      const incomingKey = typeof body.model?.apiKey === "string" ? body.model.apiKey.trim() : "";
      const clearKey = body.model?.clearApiKey === true;
      writeSettings({
        ...current,
        ...body,
        model: {
          ...current.model,
          ...(body.model || {}),
          apiKey: clearKey ? "" : incomingKey || current.model.apiKey,
        },
        connectors: { ...current.connectors, ...(body.connectors || {}) },
        limits: { ...current.limits, ...(body.limits || {}) },
      });
      json(res, 200, settingsView());
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/turn") {
      await turn(req, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/api/raw") {
      raw(res, url);
      return;
    }
    if (req.method === "GET" && url.pathname === "/api/health") {
      const state = foldState();
      json(res, 200, state.health);
      return;
    }
    if (req.method === "GET" || req.method === "HEAD") {
      serveStatic(res, url.pathname);
      return;
    }
    json(res, 405, { error: "method not allowed" });
  } catch (error) {
    json(res, error.status || 500, { error: error.message || "internal error" });
  }
});

server.listen(opts.port, opts.host, () => {
  const eff = effectiveSettings();
  console.log(`eja app  http://${opts.host}:${opts.port}`);
  console.log(`  state   ${opts.dir}`);
  console.log(`  project ${opts.project}`);
  console.log(`  binary  ${eff.binary || "(not found)"}`);
  console.log(`  model   ${eff.model.model || "(not configured)"}`);
});

function shutdown() {
  if (currentChild && !currentChild.killed) currentChild.kill("SIGINT");
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 2_000).unref();
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
