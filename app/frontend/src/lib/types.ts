export type MessageRole = "user" | "eja";
export type TaskStatus = "open" | "running" | "waiting" | "done" | "failed";
export type TaskKind = "aja" | "group";

export interface JournalEvent {
  id: number;
  at: number;
  turn: number;
  scope: string;
  kind: string;
  text: string;
  closes?: number | null;
  data?: unknown;
}

export interface Message {
  id: number;
  role: MessageRole;
  text: string;
  at: number;
  turn: number;
  data?: unknown;
}

export interface TaskRun {
  digest: string | null;
  evidence: unknown;
  evidence_id: number | null;
  evidence_result: unknown;
  witness_at: number | null;
  reconcile: unknown;
}

export interface TaskClose {
  accepted: boolean;
  note?: string;
}

export interface Task {
  id: number;
  kind: TaskKind;
  title: string;
  request: string;
  origin: string;
  workspace: string;
  acceptance: string;
  status: TaskStatus;
  created_at: number;
  assigned_at: number | null;
  reported_at: number | null;
  closed_at: number | null;
  close: TaskClose | null;
  run: TaskRun;
  event_ids: number[];
  children: number[];
}

export interface Proposal {
  id: number;
  kind: string;
  target_id: number | null;
  text: string;
  at: number;
  turn: number;
}

export interface Health {
  model: boolean;
  modelName: string;
  state: boolean;
  mcp: boolean;
  journal: string;
  journalError: string | null;
  torn: boolean;
  turnInFlight: boolean;
}

export interface AppState {
  events: JournalEvent[];
  messages: Message[];
  tasks: Task[];
  proposals: Proposal[];
  lastEvent: number;
  health: Health;
}

export interface SettingsView {
  providers: ProviderView[];
  roles: {
    eja: RoleBinding;
    aja: RoleBinding;
  };
  connectors: {
    servers: ConnectorView[];
    managed: boolean;
    configPath: string;
    error: string | null;
  };
  limits: {
    slots: number;
    turnTimeoutMs: number;
    ajaSteps: number;
    ajaTimeoutMs: number;
  };
  runtimePath: string;
  stateDir: string;
  projectDir: string;
  workspaces: Record<string, string>;
}

export interface ProviderView {
  id: string;
  name: string;
  baseUrl: string;
  models: string[];
  hasApiKey: boolean;
  local: boolean;
  headers: KeyValueView[];
}

export interface ProviderDraft {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  clearApiKey?: boolean;
  models: string[];
  hasApiKey: boolean;
  local: boolean;
  headers: KeyValueDraft[];
}

export interface RoleBinding {
  providerId: string;
  model: string;
}

export interface KeyValueView {
  key: string;
  value: string;
  secret: boolean;
  hasValue: boolean;
  redacted: boolean;
}

export interface ConnectorView {
  name: string;
  builtin: boolean;
  mode: "command" | "url";
  command: string;
  url: string;
  args: string[];
  cwd: string;
  env: KeyValueView[];
  headers: KeyValueView[];
  enabled: boolean;
  timeoutMs: number;
}

export interface KeyValueDraft {
  key: string;
  value: string;
  secret: boolean;
  hasValue: boolean;
  redacted: boolean;
}

export interface ConnectorDraft {
  name: string;
  builtin: boolean;
  mode: "command" | "url";
  command: string;
  url: string;
  args: string[];
  cwd: string;
  env: KeyValueDraft[];
  headers: KeyValueDraft[];
  enabled: boolean;
  timeoutMs: number;
}

export interface ModelDiscoverResult {
  ok: boolean;
  models?: string[];
  latencyMs: number;
  error?: string;
}

export interface ConnectorTestResult {
  ok: boolean;
  count?: number;
  latencyMs: number;
  error?: string;
  stderr?: string;
}

export interface RawRecord {
  address: string;
  kind: string;
  content: string;
  bytes?: number;
  truncated?: boolean;
}

export interface OptimisticMessage {
  text: string;
  at: number;
}

export interface ComposerSeed {
  text: string;
  nonce: number;
}

export interface Encounter {
  anchor: number;
  taskId: number | null;
  proposalId: number | null;
}

type LoopKind = "decision" | "review" | "interrupted";

interface LoopBase {
  key: string;
  kind: LoopKind;
  eventId: number;
  at: number;
  title: string;
}

export interface DecisionLoop extends LoopBase {
  kind: "decision";
  proposal: Proposal;
}

export interface TaskLoop extends LoopBase {
  kind: "review" | "interrupted";
  task: Task;
}

export type OpenLoop = DecisionLoop | TaskLoop;
