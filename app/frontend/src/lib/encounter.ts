import type {
  AppState,
  Encounter,
  JournalEvent,
  Message,
  OpenLoop,
  Proposal,
  Task,
} from "./types";
import { truncate } from "./format";

const ENCOUNTER_KEY = "eja.encounter.v1";

function storage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

export function readParkedEncounter(): Encounter | null {
  try {
    const raw = storage()?.getItem(ENCOUNTER_KEY);
    if (!raw) return null;
    const value = JSON.parse(raw) as Partial<Encounter>;
    if (
      !Number.isInteger(value.anchor) ||
      !Number.isInteger(value.startedAt) ||
      !(
        value.taskId == null ||
        (Number.isInteger(value.taskId) && Number(value.taskId) > 0)
      ) ||
      !(
        value.proposalId == null ||
        (Number.isInteger(value.proposalId) && Number(value.proposalId) > 0)
      )
    ) {
      return null;
    }
    return {
      anchor: Number(value.anchor),
      startedAt: Number(value.startedAt),
      taskId: value.taskId == null ? null : Number(value.taskId),
      proposalId: value.proposalId == null ? null : Number(value.proposalId),
    };
  } catch {
    return null;
  }
}

export function storeEncounter(encounter: Encounter | null) {
  const local = storage();
  if (!local) return;
  try {
    if (encounter) local.setItem(ENCOUNTER_KEY, JSON.stringify(encounter));
    else local.removeItem(ENCOUNTER_KEY);
  } catch {}
}

export function messagesSince(messages: Message[], encounter: Encounter): Message[] {
  return messages.filter((message) => message.id > encounter.anchor);
}

// An eja message is addressed to the user only when a real user utterance
// precedes it in the same turn. Replies to situations (return/leave/alone)
// are inner monologue, never the return screen's voice.
export function lastAddressedMessage(
  messages: Message[],
  events: JournalEvent[],
): Message | null {
  const userTurns = new Set(
    events.filter((event) => event.kind === "user").map((event) => event.turn),
  );
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (
      message.role === "eja" &&
      message.text.trim() &&
      userTurns.has(message.turn)
    ) {
      return message;
    }
  }
  return null;
}

function proposalLoop(proposal: Proposal): OpenLoop {
  return {
    key: `proposal:${proposal.id}`,
    kind: "decision",
    eventId: proposal.id,
    at: proposal.at,
    title: truncate(proposal.text, 104) || `#${proposal.id}`,
    proposal,
  };
}

function taskLoop(task: Task, kind: "review" | "interrupted"): OpenLoop {
  return {
    key: `task:${task.id}`,
    kind,
    eventId: task.event_ids.length ? Math.max(...task.event_ids) : task.id,
    at: task.reported_at || task.closed_at || task.assigned_at || task.created_at,
    title: task.title || `#${task.id}`,
    task,
  };
}

function groupReview(task: Task, tasks: Task[]): boolean {
  if (task.kind !== "group" || task.status !== "open" || !task.children.length) {
    return false;
  }
  return task.children.every((id) => {
    const child = tasks.find((candidate) => candidate.id === id);
    return child?.status === "done" || child?.status === "failed";
  });
}

export function coreLoops(
  state: AppState,
  busy = state.health.turnInFlight,
): OpenLoop[] {
  const loops: OpenLoop[] = [];
  for (const proposal of state.proposals) loops.push(proposalLoop(proposal));
  for (const task of state.tasks) {
    if (task.kind === "aja" && task.status === "waiting") {
      loops.push(taskLoop(task, "review"));
    } else if (task.status === "running" && !busy) {
      loops.push(taskLoop(task, "interrupted"));
    } else if (groupReview(task, state.tasks)) {
      loops.push(taskLoop(task, "review"));
    }
  }
  return sortLoops(loops);
}

function pausedLoop(state: AppState, encounter: Encounter): OpenLoop | null {
  const exchanged = messagesSince(state.messages, encounter);
  if (!exchanged.length) return null;
  const last = exchanged[exchanged.length - 1];
  const lastUser = [...exchanged].reverse().find((message) => message.role === "user");
  return {
    key: "paused",
    kind: "paused",
    eventId: last.id,
    at: last.at,
    title: truncate(lastUser?.text || last.text, 104) || `#${last.id}`,
  };
}

function loopMatchesEncounter(loop: OpenLoop, encounter: Encounter): boolean {
  if (loop.kind === "decision") return encounter.proposalId === loop.proposal.id;
  if (loop.kind === "review" || loop.kind === "interrupted") {
    return encounter.taskId === loop.task.id;
  }
  return loop.kind === "paused";
}

export function ambientLoops(
  state: AppState,
  parked: Encounter | null,
  busy = state.health.turnInFlight,
): OpenLoop[] {
  const loops = coreLoops(state, busy);
  if (!parked) return loops;
  if (loops.some((loop) => loopMatchesEncounter(loop, parked))) return loops;
  const paused = pausedLoop(state, parked);
  if (paused) loops.push(paused);
  return sortLoops(loops);
}

export function encounterLoops(
  state: AppState,
  encounter: Encounter,
  busy = state.health.turnInFlight,
): OpenLoop[] {
  const exchanged = messagesSince(state.messages, encounter);
  const lastUser = [...exchanged].reverse().find((message) => message.role === "user");
  const boundary = lastUser?.id ?? encounter.anchor;
  const loops = coreLoops(state, busy).filter(
    (loop) =>
      loop.eventId > boundary ||
      (loop.kind === "decision" && loop.proposal.id === encounter.proposalId) ||
      ((loop.kind === "review" || loop.kind === "interrupted") &&
        loop.task.id === encounter.taskId),
  );
  return sortLoops(loops);
}

export function sourceLoop(
  state: AppState,
  encounter: Encounter,
): OpenLoop | null {
  return (
    coreLoops(state).find((loop) => loopMatchesEncounter(loop, encounter)) ?? null
  );
}

export function parkedMatches(loop: OpenLoop, encounter: Encounter): boolean {
  return loopMatchesEncounter(loop, encounter);
}

function sortLoops(loops: OpenLoop[]): OpenLoop[] {
  const rank: Record<OpenLoop["kind"], number> = {
    interrupted: 0,
    decision: 1,
    review: 2,
    paused: 3,
  };
  return loops.sort(
    (left, right) => rank[left.kind] - rank[right.kind] || right.at - left.at,
  );
}
