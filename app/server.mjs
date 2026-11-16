#!/usr/bin/env node
// A small, dependency-free host for the eja app.
//
// The app is a lens over the journal: `talk` reads the User/Eja events,
// `unfold` reads the task graph, and settings are only the harness. Nothing
// here writes state except a turn, which is delegated to the eja binary.

import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PUBLIC = path.join(HERE, "public");
const SETTINGS_PATH = path.join(HERE, "settings.json");
const MAX_BODY = 2 * 1024 * 1024;
const MAX_TEXT = 200_000;
const MAX_OUTPUT = 2 * 1024 * 1024;
const MAX_RAW = 128 * 1024;
const PROVIDER_TIMEOUT_MS = 15_000;
const CONNECTOR_TEST_LIMIT_MS = 30_000;

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
    providers: [],
    roles: {
      eja: { providerId: "", model: "" },
      aja: { providerId: "", model: "" },
    },
    connectors: { servers: [], managed: false },
    limits: { slots: 1, turnTimeoutMs: 0, ajaSteps: 0, ajaTimeoutMs: 0 },
    binary: "",
  };
}

function storedTextList(value) {
  if (!Array.isArray(value)) return [];
  return [
    ...new Set(
      value
        .filter((item) => typeof item === "string")
        .map((item) => item.trim())
        .filter(Boolean),
    ),
  ];
}

function readStoredProvider(value) {
  if (!value || typeof value !== "object") return null;
  const id = typeof value.id === "string" ? value.id.trim() : "";
  if (!/^[A-Za-z0-9_-]{1,48}$/.test(id)) return null;
  const baseUrl = typeof value.baseUrl === "string" ? value.baseUrl.trim() : "";
  const name =
    typeof value.name === "string" && value.name.trim()
      ? value.name.trim()
      : providerName(baseUrl, id);
  return {
    id,
    name,
    baseUrl,
    apiKey: typeof value.apiKey === "string" ? value.apiKey : "",
    models: storedTextList(value.models),
    headers: normalizeKeyValues(value.headers, {}),
  };
}

function readStoredRole(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    providerId: typeof source.providerId === "string" ? source.providerId.trim() : "",
    model: typeof source.model === "string" ? source.model.trim() : "",
  };
}

function legacyProvider(model) {
  const baseUrl = typeof model.baseUrl === "string" ? model.baseUrl.trim() : "";
  const apiKey = typeof model.apiKey === "string" ? model.apiKey : "";
  const primary = typeof model.model === "string" ? model.model.trim() : "";
  const aja = typeof model.ajaModel === "string" ? model.ajaModel.trim() : "";
  if (!baseUrl && !apiKey && !primary && !aja) return null;
  const id = "default";
  return {
    id,
    name: providerName(baseUrl, typeof model.provider === "string" ? model.provider : "Provider"),
    baseUrl,
    apiKey,
    models: storedTextList([primary, aja]),
  };
}

function readSettings() {
  const raw = readJson(SETTINGS_PATH, {});
  const base = defaultSettings();
  const model = raw.model && typeof raw.model === "object" ? raw.model : {};
  const connectors = raw.connectors && typeof raw.connectors === "object" ? raw.connectors : {};
  const limits = raw.limits && typeof raw.limits === "object" ? raw.limits : {};
  let providers = Array.isArray(raw.providers)
    ? raw.providers.map(readStoredProvider).filter(Boolean)
    : [];
  const seen = new Set();
  providers = providers.filter((provider) => {
    if (seen.has(provider.id)) return false;
    seen.add(provider.id);
    return true;
  });
  let roles = {
    eja: readStoredRole(raw.roles?.eja),
    aja: readStoredRole(raw.roles?.aja),
  };
  const legacy = legacyProvider(model);
  if (!providers.length && legacy) providers = [legacy];
  if (legacy && !roles.eja.providerId && !roles.eja.model) {
    roles.eja = { providerId: legacy.id, model: legacy.models[0] || "" };
  }
  if (legacy && !roles.aja.providerId && !roles.aja.model) {
    roles.aja = { providerId: legacy.id, model: legacy.models[1] || "" };
  }
  return {
    providers,
    roles,
    connectors: {
      servers: Array.isArray(connectors.servers)
        ? connectors.servers.map(readStoredConnector).filter(Boolean)
        : [],
      managed: connectors.managed === true,
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

function readStoredConnector(value) {
  if (!value || typeof value !== "object") return null;
  const command = typeof value.command === "string" ? value.command.trim() : "";
  const url = typeof value.url === "string" ? value.url.trim() : "";
  if (!command && !url) return null;
  const name = typeof value.name === "string" ? value.name.trim() : "";
  if (!name) return null;
  return {
    name,
    command: command || null,
    url: url || null,
    args: Array.isArray(value.args) ? value.args.filter((item) => typeof item === "string") : [],
    env: value.env && typeof value.env === "object" && !Array.isArray(value.env) ? value.env : {},
    cwd: typeof value.cwd === "string" ? value.cwd : "",
    headers:
      value.headers && typeof value.headers === "object" && !Array.isArray(value.headers)
        ? value.headers
        : {},
    enabled: value.enabled !== false,
    timeout_ms: Number.isFinite(Number(value.timeout_ms))
      ? Math.min(300_000, Math.max(1, Math.floor(Number(value.timeout_ms))))
      : 30_000,
  };
}

function legacyMcpPath() {
  const raw = readJson(SETTINGS_PATH, {});
  const configured =
    typeof raw.connectors?.mcpConfig === "string" ? raw.connectors.mcpConfig.trim() : "";
  if (configured) return path.resolve(opts.project, configured);
  const local = path.join(opts.dir, "mcp.json");
  return fs.existsSync(local) ? local : "";
}

function parseMcpServers(file) {
  if (!file || !fs.existsSync(file)) return { servers: [], error: null };
  try {
    const raw = JSON.parse(fs.readFileSync(file, "utf8"));
    const source = raw?.mcpServers;
    if (!source || typeof source !== "object" || Array.isArray(source)) {
      throw new Error("mcpServers must be an object");
    }
    const servers = [];
    for (const [name, value] of Object.entries(source)) {
      const stored = readStoredConnector({ ...value, name });
      if (!stored) throw new Error(`${name} needs command or url`);
      servers.push(stored);
    }
    return { servers, error: null };
  } catch (error) {
    return { servers: [], error: `${file}: ${error.message}` };
  }
}

function activeConnectors(settings) {
  if (settings.connectors.managed) {
    return { ...settings.connectors, error: null };
  }
  const legacy = legacyMcpPath();
  if (!legacy) return { ...settings.connectors, error: null };
  const parsed = parseMcpServers(legacy);
  return {
    servers: settings.connectors.servers.length ? settings.connectors.servers : parsed.servers,
    managed: false,
    error: parsed.error,
  };
}

function envValue(...names) {
  for (const name of names) {
    const value = process.env[name];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return "";
}

function providerName(baseUrl, fallback) {
  try {
    const host = new URL(baseUrl).hostname.replace(/^www\./, "");
    if (host) return host === "localhost" ? "Local" : host;
  } catch {
    // A stored provider can be incomplete until the next save.
  }
  return fallback || "Provider";
}

function isLocalProvider(baseUrl) {
  try {
    return ["127.0.0.1", "localhost", "::1"].includes(new URL(baseUrl).hostname);
  } catch {
    return false;
  }
}

function configuredRole(settings, name) {
  const role = settings.roles[name];
  const provider = settings.providers.find((item) => item.id === role.providerId);
  if (!provider) return null;
  return {
    providerId: provider.id,
    providerName: provider.name,
    baseUrl: provider.baseUrl,
    apiKey: provider.apiKey,
    model: role.model,
    headers: provider.headers || {},
  };
}

function environmentRole({ aja = false } = {}) {
  const baseUrl = aja
    ? envValue("EJA_AJA_BASE_URL") || envValue("LNA_BASE_URL", "OPENAI_BASE_URL", "JIYUAN_BASE_URL")
    : envValue("LNA_BASE_URL", "OPENAI_BASE_URL", "JIYUAN_BASE_URL");
  const apiKey = aja
    ? envValue("EJA_AJA_API_KEY") || envValue("LNA_API_KEY", "OPENAI_API_KEY", "JIYUAN_API_KEY")
    : envValue("LNA_API_KEY", "OPENAI_API_KEY", "JIYUAN_API_KEY");
  const model = aja
    ? envValue("EJA_AJA_MODEL") || envValue("LNA_MODEL", "OPENAI_MODEL", "JIYUAN_MODEL")
    : envValue("LNA_MODEL", "OPENAI_MODEL", "JIYUAN_MODEL");
  if (!baseUrl && !apiKey && !model) return null;
  return {
    providerId: "environment",
    providerName: "Environment",
    baseUrl,
    apiKey,
    model,
  };
}

function effectiveSettings() {
  const s = readSettings();
  const connectors = activeConnectors(s);
  const eja = configuredRole(s, "eja") || environmentRole();
  const aja = configuredRole(s, "aja") || environmentRole({ aja: true }) || eja;
  const legacy = legacyMcpPath();
  const mcpConfig = connectors.managed
    ? managedMcpPath()
    : legacy && fs.existsSync(legacy)
      ? legacy
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
    connectors,
    roles: { eja, aja },
    mcpConfig,
    limits,
    binary: findBinary(s.binary),
  };
}

function roleConfigured(role) {
  return Boolean(role?.baseUrl && role?.model);
}

function managedMcpPath() {
  return path.join(opts.dir, "app-mcp.json");
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
      model: roleConfigured(eff.roles.eja) && roleConfigured(eff.roles.aja),
      modelName: eff.roles.eja?.model || "",
      state: fs.existsSync(opts.dir),
      mcp: Boolean(
        eff.mcpConfig &&
          fs.existsSync(eff.mcpConfig) &&
          eff.connectors.servers.some((connector) => connector.enabled),
      ),
      journal: journal.error ? "error" : "ok",
      journalError: journal.error,
      torn: journal.torn,
      turnInFlight,
    },
  };
}

function publicProvider(provider) {
  const key = provider.apiKey;
  return {
    id: provider.id,
    name: provider.name,
    baseUrl: provider.baseUrl,
    models: provider.models,
    hasApiKey: Boolean(key),
    apiKeyHint: key ? `•••• ${key.slice(-4)}` : "",
    local: isLocalProvider(provider.baseUrl),
    headers: publicKeyValues(provider.headers),
  };
}

function settingsView() {
  const eff = effectiveSettings();
  const s = eff.settings;
  return {
    providers: s.providers.map(publicProvider),
    roles: {
      eja: { ...s.roles.eja },
      aja: { ...s.roles.aja },
    },
    connectors: {
      servers: eff.connectors.servers.map(publicConnector),
      managed: eff.connectors.managed,
      configPath: eff.mcpConfig,
      error: eff.connectors.error,
    },
    limits: s.limits,
    binary: s.binary,
    effectiveBinary: eff.binary,
    stateDir: opts.dir,
    projectDir: opts.project,
    health: {
      binary: Boolean(eff.binary),
      model: roleConfigured(eff.roles.eja) && roleConfigured(eff.roles.aja),
      mcp: Boolean(
        eff.mcpConfig &&
          fs.existsSync(eff.mcpConfig) &&
          eff.connectors.servers.some((connector) => connector.enabled),
      ),
      state: fs.existsSync(opts.dir),
    },
  };
}

function writeSettings(next) {
  const s = next && typeof next === "object" ? next : {};
  const current = readSettings();
  const currentConnectors = activeConnectors(current).servers;
  const providers = normalizeProviderList(s.providers, current.providers);
  const roles = normalizeRoles(s.roles, providers);
  const raw = {
    providers,
    roles,
    connectors: {
      servers: normalizeConnectorList(s.connectors?.servers, currentConnectors),
      managed: true,
    },
    limits: {
      slots: Math.max(1, Math.floor(Number(s.limits?.slots) || 1)),
      turnTimeoutMs: Math.max(0, Math.floor(Number(s.limits?.turnTimeoutMs) || 0)),
      ajaSteps: Math.max(0, Math.floor(Number(s.limits?.ajaSteps) || 0)),
      ajaTimeoutMs: Math.max(0, Math.floor(Number(s.limits?.ajaTimeoutMs) || 0)),
    },
    binary: typeof s.binary === "string" ? s.binary.trim() : "",
  };
  writePrivateJson(SETTINGS_PATH, raw);
  writeManagedMcp(raw.connectors.servers);
}

function badRequest(message) {
  const error = new Error(message);
  error.status = 400;
  throw error;
}

function normalizeProviderList(value, previous = []) {
  if (!Array.isArray(value)) return [];
  if (value.length > 16) badRequest("最多可以配置 16 个 Provider");
  const prior = new Map(previous.map((provider) => [provider.id, provider]));
  const seen = new Set();
  const names = new Set();
  const providers = [];
  for (const item of value) {
    if (!item || typeof item !== "object") badRequest("Provider 配置无效");
    const previousProvider = prior.get(item.id);
    let id = typeof item.id === "string" ? item.id.trim() : "";
    if (!id) id = `p_${randomUUID().replaceAll("-", "").slice(0, 12)}`;
    if (!/^[A-Za-z0-9_-]{1,48}$/.test(id)) badRequest("Provider ID 无效");
    if (seen.has(id)) badRequest("Provider ID 重复");
    seen.add(id);
    const baseUrl = normalizeBaseUrl(item.baseUrl);
    const providedKey = Object.prototype.hasOwnProperty.call(item, "apiKey");
    const apiKey =
      item.clearApiKey === true
        ? ""
        : providedKey && typeof item.apiKey === "string" && item.apiKey.trim()
          ? item.apiKey.trim()
          : previousProvider?.apiKey || "";
    const name =
      typeof item.name === "string" && item.name.trim()
        ? item.name.trim().slice(0, 48)
        : providerName(baseUrl, id);
    if (names.has(name.toLowerCase())) badRequest("Provider 名称重复");
    names.add(name.toLowerCase());
    const headers = normalizeProviderHeaders(item.headers, previousProvider?.headers);
    providers.push({
      id,
      name,
      baseUrl,
      apiKey,
      models: normalizeStringList(item.models ?? previousProvider?.models),
      headers,
    });
  }
  return providers;
}

const MANAGED_PROVIDER_HEADERS = new Set([
  "connection",
  "content-length",
  "content-type",
  "host",
  "transfer-encoding",
]);

function normalizeProviderHeaders(value, previous) {
  const headers = normalizeKeyValues(value, previous);
  for (const key of Object.keys(headers)) {
    if (!/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(key)) {
      badRequest(`Provider Header 名称无效：${key}`);
    }
    if (MANAGED_PROVIDER_HEADERS.has(key.toLowerCase())) {
      badRequest(`Provider Header 由客户端管理：${key}`);
    }
    if (/[\r\n]/.test(headers[key])) {
      badRequest(`Provider Header 不能包含换行：${key}`);
    }
  }
  return headers;
}

function normalizeRoles(value, providers) {
  const source = value && typeof value === "object" ? value : {};
  const ids = new Set(providers.map((provider) => provider.id));
  const out = {};
  for (const name of ["eja", "aja"]) {
    const role = readStoredRole(source[name]);
    if (role.providerId && !ids.has(role.providerId)) {
      badRequest(`${name} 选择了一个不存在的 Provider`);
    }
    out[name] = role;
  }
  return out;
}

function isSecretName(key) {
  return /(secret|token|key|pass|auth|credential|cookie|session)/i.test(key);
}

function publicKeyValues(source) {
  return Object.entries(source || {}).map(([key, raw]) => {
    const value = typeof raw === "string" ? raw : String(raw ?? "");
    const reference = /^\$\{[A-Za-z0-9_]+\}$/.test(value);
    const redacted = isSecretName(key) && Boolean(value) && !reference;
    return {
      key,
      value: redacted ? "" : value,
      secret: isSecretName(key),
      hasValue: Boolean(value),
      redacted,
    };
  });
}

function publicConnector(connector) {
  return {
    name: connector.name,
    mode: connector.command ? "command" : "url",
    command: connector.command || "",
    url: connector.url || "",
    args: connector.args,
    cwd: connector.cwd,
    env: publicKeyValues(connector.env),
    headers: publicKeyValues(connector.headers),
    enabled: connector.enabled,
    timeoutMs: connector.timeout_ms,
  };
}

function normalizeStringList(value) {
  if (!Array.isArray(value)) return [];
  const seen = new Set();
  const output = [];
  for (const item of value) {
    if (typeof item !== "string") continue;
    const text = item.trim();
    if (!text || seen.has(text)) continue;
    seen.add(text);
    output.push(text);
  }
  return output;
}

function normalizeKeyValues(value, previous) {
  const output = {};
  const pairs = Array.isArray(value) ? value : Object.entries(value || {}).map(([key, item]) => ({ key, value: item }));
  for (const item of pairs) {
    if (!item || typeof item !== "object") continue;
    const key = typeof item.key === "string" ? item.key.trim() : "";
    if (!key) continue;
    const raw = item.value;
    const text = typeof raw === "string" ? raw : String(raw ?? "");
    if (item.redacted && !text) {
      if (Object.prototype.hasOwnProperty.call(previous || {}, key)) output[key] = previous[key];
      continue;
    }
    output[key] = text;
  }
  return output;
}

function normalizeConnector(value, previous) {
  if (!value || typeof value !== "object") badRequest("连接器配置无效");
  const name = typeof value.name === "string" ? value.name.trim() : "";
  if (!/^[A-Za-z0-9_-]{1,24}$/.test(name)) {
    badRequest("连接器名称需为 1–24 个字母、数字、下划线或短横线");
  }
  const mode = value.mode === "url" || value.url ? "url" : "command";
  const command = mode === "command" ? String(value.command || "").trim() : "";
  const url = mode === "url" ? String(value.url || "").trim() : "";
  if (!command && !url) badRequest(`${name} 需要本地命令或远程 URL`);
  if (url) {
    let parsed;
    try {
      parsed = new URL(url);
    } catch {
      badRequest(`${name} 的 URL 无效`);
    }
    if (!["http:", "https:"].includes(parsed.protocol) || parsed.username || parsed.password) {
      badRequest(`${name} 需要不含账号信息的 HTTP(S) URL`);
    }
  }
  const env =
    mode === "command" ? normalizeKeyValues(value.env, previous?.env) : {};
  for (const key of Object.keys(env)) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) badRequest(`环境变量名无效：${key}`);
  }
  const headers = mode === "url" ? normalizeKeyValues(value.headers, previous?.headers) : {};
  for (const key of Object.keys(headers)) {
    if (!/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(key)) badRequest(`Header 名称无效：${key}`);
  }
  const timeoutMs = Math.floor(Number(value.timeoutMs));
  return {
    name,
    command: command || null,
    url: url || null,
    args:
      mode === "command" && Array.isArray(value.args)
        ? value.args.filter((item) => typeof item === "string")
        : [],
    env,
    cwd: mode === "command" && typeof value.cwd === "string" ? value.cwd.trim() : "",
    headers,
    enabled: value.enabled !== false,
    timeout_ms: Number.isFinite(timeoutMs) ? Math.min(300_000, Math.max(1, timeoutMs)) : 30_000,
  };
}

function normalizeConnectorList(value, previous = []) {
  if (!Array.isArray(value)) return [];
  if (value.length > 32) badRequest("最多可以配置 32 个连接器");
  const prior = new Map(previous.map((connector) => [connector.name, connector]));
  const seen = new Set();
  const servers = [];
  for (const item of value) {
    const connector = normalizeConnector(item, prior.get(item?.name));
    if (seen.has(connector.name)) badRequest(`连接器名称重复：${connector.name}`);
    seen.add(connector.name);
    servers.push(connector);
  }
  return servers;
}

function connectorConfig(connector) {
  const config = connector.command
    ? {
        command: connector.command,
        args: connector.args,
        ...(Object.keys(connector.env).length ? { env: connector.env } : {}),
        ...(connector.cwd ? { cwd: connector.cwd } : {}),
      }
    : {
        url: connector.url,
        ...(Object.keys(connector.headers).length ? { headers: connector.headers } : {}),
      };
  config.enabled = connector.enabled;
  config.timeout_ms = connector.timeout_ms;
  return config;
}

function writePrivateJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const temp = `${file}.${process.pid}.${Date.now()}.tmp`;
  fs.writeFileSync(temp, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  try {
    fs.chmodSync(temp, 0o600);
  } catch {
    // A platform without POSIX modes still keeps the file in the app directory.
  }
  fs.renameSync(temp, file);
  try {
    fs.chmodSync(file, 0o600);
  } catch {
    // Best effort; the content never leaves the local host.
  }
}

function writeManagedMcp(servers) {
  const mcpServers = {};
  for (const connector of servers) mcpServers[connector.name] = connectorConfig(connector);
  writePrivateJson(managedMcpPath(), { mcpServers });
}

function providerDraft(body, current) {
  const source = body && typeof body === "object" ? body : {};
  const stored = current.providers.find((provider) => provider.id === source.id);
  const draftKey = typeof source.apiKey === "string" ? source.apiKey.trim() : "";
  return {
    baseUrl:
      typeof source.baseUrl === "string" ? source.baseUrl.trim() : stored?.baseUrl || "",
    apiKey:
      source.clearApiKey === true ? "" : draftKey || stored?.apiKey || "",
    headers: normalizeProviderHeaders(source.headers, stored?.headers),
  };
}

function normalizeBaseUrl(value) {
  const text = typeof value === "string" ? value.trim() : "";
  if (!text) badRequest("请先填写接口地址");
  let parsed;
  try {
    parsed = new URL(text);
  } catch {
    badRequest("接口地址不是有效 URL");
  }
  if (!["http:", "https:"].includes(parsed.protocol)) badRequest("接口地址必须是 HTTP(S) URL");
  let href = parsed.href.replace(/\/+$/, "");
  href = href.replace(/\/(?:chat\/completions|models)$/, "");
  return href;
}

function sanitizeProviderText(text, secrets) {
  let output = String(text || "");
  for (const secret of secrets) {
    if (typeof secret === "string" && secret.length > 7) output = output.split(secret).join("••••");
  }
  return output.replace(/Bearer\s+[^\s,;]+/gi, "Bearer ••••").slice(0, 640);
}

async function providerFetch(url, init, timeoutMs, secrets) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(url, { ...init, signal: controller.signal });
    const text = await response.text();
    let value = null;
    try {
      value = text ? JSON.parse(text) : null;
    } catch {
      // The caller still gets a useful HTTP status and a clipped body.
    }
    if (!response.ok) {
      const detail =
        value?.error?.message ||
        value?.message ||
        value?.error ||
        text ||
        `HTTP ${response.status}`;
      const error = new Error(sanitizeProviderText(detail, secrets));
      error.status = response.status;
      throw error;
    }
    return value;
  } catch (error) {
    if (error.name === "AbortError") {
      const timeout = new Error(`请求超过 ${Math.round(timeoutMs / 1000)} 秒`);
      timeout.status = 0;
      throw timeout;
    }
    if (error.status) throw error;
    const code = error.cause?.code || error.code || "";
    const map = {
      ENOTFOUND: "找不到这个域名",
      ECONNREFUSED: "端点拒绝连接，服务可能没有启动",
      ECONNRESET: "连接被端点重置",
      CERT_HAS_EXPIRED: "TLS 证书已过期",
      DEPTH_ZERO_SELF_SIGNED_CERT: "端点使用了自签名证书",
    };
    const wrapped = new Error(map[code] || sanitizeProviderText(error.message, secrets));
    wrapped.status = 0;
    throw wrapped;
  } finally {
    clearTimeout(timer);
  }
}

function describeProviderError(action, error) {
  const status = Number(error.status) || 0;
  let prefix = "";
  if (status === 401 || status === 403) prefix = "API 密钥无效，或没有访问权限";
  else if (status === 404) prefix = `${action}端点不存在，请检查 Base URL`;
  else if (status === 429) prefix = "请求太频繁，或账户额度不足";
  else if (status >= 500) prefix = "Provider 暂时不可用";
  else if (!status) prefix = "连不上 Provider";
  else prefix = `Provider 返回 HTTP ${status}`;
  const detail = sanitizeProviderText(error.message, []);
  return detail && !prefix.includes(detail) ? `${prefix}：${detail}` : prefix;
}

async function discoverModels(body) {
  const current = readSettings();
  const draft = providerDraft(body, current);
  const started = Date.now();
  const secrets = [draft.apiKey, ...Object.values(draft.headers)];
  try {
    const base = normalizeBaseUrl(draft.baseUrl);
    const headers = { accept: "application/json", ...draft.headers };
    const hasAuthorization = Object.keys(headers).some(
      (key) => key.toLowerCase() === "authorization",
    );
    if (draft.apiKey && !hasAuthorization) headers.authorization = `Bearer ${draft.apiKey}`;
    const value = await providerFetch(
      `${base}/models`,
      { method: "GET", headers },
      PROVIDER_TIMEOUT_MS,
      secrets,
    );
    const source = Array.isArray(value?.data)
      ? value.data
      : Array.isArray(value?.models)
        ? value.models
        : Array.isArray(value)
          ? value
          : null;
    if (!source) throw new Error("响应里没有模型列表");
    const seen = new Set();
    const models = [];
    for (const item of source) {
      const id =
        typeof item === "string"
          ? item
          : typeof item?.id === "string"
            ? item.id
            : typeof item?.name === "string"
              ? item.name
              : "";
      if (!id || seen.has(id)) continue;
      seen.add(id);
      models.push(id);
    }
    models.sort((a, b) => a.localeCompare(b));
    return {
      ok: true,
      models: models.slice(0, 1000),
      total: models.length,
      latencyMs: Date.now() - started,
    };
  } catch (error) {
    return {
      ok: false,
      error: describeProviderError("模型列表", error),
      latencyMs: Date.now() - started,
    };
  }
}

function connectorTools(definitions, connector) {
  if (!Array.isArray(definitions)) throw new Error("eja 没有返回工具列表");
  const prefix = `mcp__${connector.name}__`;
  const tools = [];
  for (const definition of definitions) {
    const publicName = definition?.function?.name;
    if (typeof publicName !== "string" || !publicName.startsWith(prefix)) continue;
    const description = typeof definition.function.description === "string"
      ? definition.function.description
      : "";
    const marker = `MCP ${connector.name}/`;
    let name = publicName.slice(prefix.length);
    let summary = description;
    if (description.startsWith(marker)) {
      const rest = description.slice(marker.length);
      const split = rest.indexOf(": ");
      if (split >= 0) {
        name = rest.slice(0, split);
        summary = rest.slice(split + 2);
      } else {
        name = rest;
        summary = "";
      }
    }
    tools.push({
      name,
      publicName,
      description: summary,
    });
  }
  tools.sort((a, b) => a.name.localeCompare(b.name));
  return tools;
}

async function testConnector(body) {
  const current = readSettings();
  const previous = activeConnectors(current).servers;
  const incoming = body?.connector && typeof body.connector === "object" ? body.connector : body;
  const previousConnector = previous.find((connector) => connector.name === incoming?.name);
  let connector;
  try {
    connector = normalizeConnector(incoming, previousConnector);
  } catch (error) {
    return { ok: false, error: error.message, latencyMs: 0 };
  }
  const effective = findBinary(current.binary);
  if (!effective) return { ok: false, error: "没有找到 eja 可执行文件", latencyMs: 0 };
  const started = Date.now();
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "eja-mcp-test-"));
  const file = path.join(root, "mcp.json");
  try {
    const probe = { ...connector, enabled: true };
    writePrivateJson(file, { mcpServers: { [connector.name]: connectorConfig(probe) } });
    try {
      fs.mkdirSync(opts.dir, { recursive: true });
    } catch {
      // eja will report an unusable state directory below.
    }
    const timeoutMs = Math.min(CONNECTOR_TEST_LIMIT_MS, connector.timeout_ms + 5_000);
    const result = await new Promise((resolve) => {
      const child = spawn(
        effective,
        ["--dir", opts.dir, "--mcp-config", file, "show", "tools"],
        {
          cwd: opts.project,
          env: { ...process.env },
          stdio: ["ignore", "pipe", "pipe"],
        },
      );
      let stdout = "";
      let stderr = "";
      let settled = false;
      const finish = (value) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve(value);
      };
      const timer = setTimeout(() => {
        if (!child.killed) child.kill("SIGTERM");
        finish({
          code: -1,
          signal: "timeout",
          stdout,
          stderr: `连接器测试超过 ${Math.round(timeoutMs / 1000)} 秒`,
        });
      }, timeoutMs);
      child.stdout.on("data", (chunk) => {
        if (stdout.length < MAX_OUTPUT) stdout += chunk.toString("utf8");
      });
      child.stderr.on("data", (chunk) => {
        if (stderr.length < MAX_OUTPUT) stderr += chunk.toString("utf8");
      });
      child.on("error", (error) => {
        finish({ code: -1, signal: null, stdout, stderr: error.message });
      });
      child.on("close", (code, signal) => {
        finish({ code, signal, stdout, stderr });
      });
    });
    if (result.code !== 0) {
      const details = sanitizeProviderText(
        result.stderr || result.stdout || `eja exited with code ${result.code}`,
        Object.values(connector.env).concat(Object.values(connector.headers)),
      );
      return {
        ok: false,
        error: details.trim().slice(-2400) || "连接器没有启动",
        latencyMs: Date.now() - started,
      };
    }
    const definitions = JSON.parse(result.stdout);
    const tools = connectorTools(definitions, connector);
    return {
      ok: true,
      tools,
      count: tools.length,
      latencyMs: Date.now() - started,
      stderr: result.stderr.trim() ? sanitizeProviderText(result.stderr, []).slice(-800) : "",
    };
  } catch (error) {
    return {
      ok: false,
      error: sanitizeProviderText(error.message, Object.values(connector.env)),
      latencyMs: Date.now() - started,
    };
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
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
  if (!roleConfigured(eff.roles.eja) || !roleConfigured(eff.roles.aja)) {
    json(res, 503, {
      ok: false,
      error: "model is not configured. Open settings and choose a provider and model for eja.",
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
    LNA_BASE_URL: eff.roles.eja.baseUrl,
    LNA_API_KEY: eff.roles.eja.apiKey,
    LNA_MODEL: eff.roles.eja.model,
    EJA_AJA_BASE_URL: eff.roles.aja.baseUrl,
    EJA_AJA_API_KEY: eff.roles.aja.apiKey,
    EJA_AJA_MODEL: eff.roles.aja.model,
    LNA_HEADERS: JSON.stringify(eff.roles.eja.headers || {}),
    EJA_AJA_HEADERS: JSON.stringify(eff.roles.aja.headers || {}),
  };
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

try {
  const startup = readSettings();
  if (startup.connectors.managed) writeManagedMcp(startup.connectors.servers);
} catch (error) {
  console.error(`could not prepare connector config: ${error.message}`);
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
      writeSettings(await readBody(req));
      json(res, 200, settingsView());
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/provider/discover") {
      json(res, 200, await discoverModels(await readBody(req)));
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/connectors/test") {
      json(res, 200, await testConnector(await readBody(req)));
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
  console.log(`  eja     ${eff.roles.eja?.model || "(not configured)"}`);
  console.log(`  aja     ${eff.roles.aja?.model || "(not configured)"}`);
});

function shutdown() {
  if (currentChild && !currentChild.killed) currentChild.kill("SIGINT");
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 2_000).unref();
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
