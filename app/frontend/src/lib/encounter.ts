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

// An encounter shows the exchange, not eja's inner voice: an eja message
// belongs to the conversation only when the user spoke in its turn.
export function spokenSince(
  messages: Message[],
  events: JournalEvent[],
  encounter: Encounter,
): Message[] {
  const userTurns = new Set(
    events
      .filter(
        (event) =>
          event.kind === "user" ||
          event.kind === "approval" ||
          event.kind === "denial",
      )
      .map((event) => event.turn),
  );
  return messagesSince(messages, encounter).filter(
    (message) => message.role === "user" || userTurns.has(message.turn),
  );
}

// The return screen's voice is eja's own reply to the latest return: spoken
// only if eja chose to speak on that turn. A later departure or alone turn
// means the user left; the voice rests until the next return.
export function presenceVoice(
  messages: Message[],
  events: JournalEvent[],
): Message | null {
  let latest: JournalEvent | null = null;
  for (const event of events) {
    if (
      event.kind === "returned" ||
      event.kind === "departed" ||
      event.kind === "alone"
    ) {
      if (!latest || event.id > latest.id) latest = event;
    }
  }
  if (!latest || latest.kind !== "returned") return null;
  return (
    messages.find(
      (message) =>
        message.role === "eja" &&
        message.turn === latest.turn &&
        message.text.trim(),
    ) ?? null
  );
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

function eventProposalId(event: JournalEvent): number | null {
  if (!event.data || typeof event.data !== "object") return null;
  const value = (event.data as Record<string, unknown>).proposal_id;
  return typeof value === "number" ? value : null;
}

// Proposal ids are never re-asked, so a rejection or denial already answers
// the question; a correction withdraws rather than asks.
function answeredProposal(proposal: Proposal, events: JournalEvent[]): boolean {
  if (proposal.kind === "correction") return true;
  return events.some(
    (event) =>
      (event.kind === "proposal_rejected" || event.kind === "denial") &&
      eventProposalId(event) === proposal.id,
  );
}

export function coreLoops(
  state: AppState,
  busy = state.health.turnInFlight,
): OpenLoop[] {
  const loops: OpenLoop[] = [];
  for (const proposal of state.proposals) {
    if (answeredProposal(proposal, state.events)) continue;
    loops.push(proposalLoop(proposal));
  }
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

function loopMatchesEncounter(loop: OpenLoop, encounter: Encounter): boolean {
  if (loop.kind === "decision") return encounter.proposalId === loop.proposal.id;
  return encounter.taskId === loop.task.id;
}

export function ambientLoops(
  state: AppState,
  busy = state.health.turnInFlight,
): OpenLoop[] {
  return coreLoops(state, busy);
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
  };
  return loops.sort(
    (left, right) => rank[left.kind] - rank[right.kind] || right.at - left.at,
  );
}
