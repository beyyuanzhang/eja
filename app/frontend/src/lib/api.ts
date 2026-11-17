import type {
  AppState,
  ConnectorDraft,
  ConnectorTestResult,
  ModelDiscoverResult,
  RawRecord,
  SettingsView,
} from "./types";

const API_BASE = String(import.meta.env.VITE_API_BASE || "").replace(/\/+$/, "");

async function readJson(response: Response): Promise<Record<string, unknown>> {
  try {
    return (await response.json()) as Record<string, unknown>;
  } catch {
    return {};
  }
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${API_BASE}${url}`, { cache: "no-store", ...init });
  const body = await readJson(response);
  if (!response.ok) {
    throw new Error(String(body.error || `HTTP ${response.status}`));
  }
  return body as T;
}

export function fetchState(): Promise<AppState> {
  return request<AppState>("/api/state");
}

export function fetchSettings(): Promise<SettingsView> {
  return request<SettingsView>("/api/settings");
}

export function saveSettings(body: unknown): Promise<SettingsView> {
  return request<SettingsView>("/api/settings", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

export function discoverModels(body: unknown): Promise<ModelDiscoverResult> {
  return request<ModelDiscoverResult>("/api/provider/discover", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

export function testConnector(connector: ConnectorDraft): Promise<ConnectorTestResult> {
  return request<ConnectorTestResult>("/api/connectors/test", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ connector }),
  });
}

export interface TurnResponse {
  ok?: boolean;
  error?: string;
  stderr?: string;
  stdout?: string;
}

export function sendTurn(
  text: string,
  options: {
    focus?: number | null;
    approvals?: number[];
    denials?: number[];
    gesture?: "return" | "leave" | "alone";
  } = {},
): Promise<TurnResponse> {
  return request<TurnResponse>("/api/turn", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      text,
      focus: options.focus ?? null,
      approvals: options.approvals ?? [],
      denials: options.denials ?? [],
      ...(options.gesture ? { gesture: options.gesture } : {}),
    }),
  });
}

export function fetchRaw(address: string): Promise<RawRecord> {
  return request<RawRecord>(`/api/raw?address=${encodeURIComponent(address)}`);
}

export function friendlyError(message: string): string {
  if (/model is not configured/i.test(message)) {
    return "还没接上模型。先在左下角的设置里选好 Provider 和模型。";
  }
  if (/a turn is already running/i.test(message)) {
    return "它还在处理上一件事。";
  }
  if (/needs text, an approval, a denial or a situation/i.test(message)) {
    return "还没有写下要交代的事。";
  }
  if (/text is longer|request body too large/i.test(message)) {
    return "这次递过去的内容太长了。";
  }
  if (/stopped by SIGINT/i.test(message)) {
    return "这一轮被停下了。";
  }
  return message;
}
