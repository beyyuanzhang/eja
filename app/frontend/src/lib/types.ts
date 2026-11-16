export type ViewName = "talk" | "unfold";
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
  binary: boolean;
  binaryPath: string;
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
  model: {
    baseUrl: string;
    model: string;
    ajaModel: string;
    hasApiKey: boolean;
    apiKeyHint: string;
    effectiveBaseUrl: string;
    effectiveModel: string;
  };
  connectors: {
    mcpConfig: string;
    effective: string;
  };
  limits: {
    slots: number;
    turnTimeoutMs: number;
    ajaSteps: number;
    ajaTimeoutMs: number;
  };
  binary: string;
  effectiveBinary: string;
  stateDir: string;
  projectDir: string;
  health: {
    binary: boolean;
    model: boolean;
    mcp: boolean;
    state: boolean;
  };
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
