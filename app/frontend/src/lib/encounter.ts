import type { Encounter, OpenLoop } from "./types";

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

function loopMatchesEncounter(loop: OpenLoop, encounter: Encounter): boolean {
  if (loop.kind === "decision") return encounter.proposalId === loop.proposal.id;
  return encounter.taskId === loop.task.id;
}

export function parkedMatches(loop: OpenLoop, encounter: Encounter): boolean {
  return loopMatchesEncounter(loop, encounter);
}
