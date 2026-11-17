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
  acceptance: string;
  status: TaskStatus;
  created_at: number;
  assigned_at: number | null;
  reported_at: number | null;
  closed_at: number | null;
  close: TaskClose | null;
  assignment: unknown;
  run: TaskRun;
  event_ids: number[];
  children: number[];
}

export interface Relation {
  from: number;
  to: number;
  kind: string;
}

export interface Proposal {
  id: number;
  kind: string;
  target_id: number | null;
  text: string;
  at: number;
  turn: number;
}

export interface IdentityEntry {
  id: number;
  text: string;
  at: number;
  closes?: number | null;
  proposal_id?: number | null;
}

export interface Identity {
  goals: IdentityEntry[];
  commitments: IdentityEntry[];
  corrections: IdentityEntry[];
  decisions: IdentityEntry[];
  relationships: IdentityEntry[];
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

export interface StateCounts {
  tasks: number;
  open: number;
  running: number;
  waiting: number;
  done: number;
  failed: number;
  messages: number;
}

export interface AppState {
  now: number;
  stateDir: string;
  projectDir: string;
  events: JournalEvent[];
  messages: Message[];
  tasks: Task[];
  relations: Relation[];
  proposals: Proposal[];
  identity: Identity;
  currentGoal: IdentityEntry | null;
  refused: IdentityEntry[];
  counts: StateCounts;
  lastTurn: number;
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
  startedAt: number;
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
