import type { Task, TaskStatus } from "./types";

const pad = (value: number) => String(value).padStart(2, "0");

export function clock(seconds: number): string {
  const date = new Date(seconds * 1000);
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

export function dateTime(seconds: number): string {
  if (!seconds) return "—";
  const date = new Date(seconds * 1000);
  return `${date.getMonth() + 1}月${date.getDate()}日 ${clock(seconds)}`;
}

export function relative(seconds: number): string {
  if (!seconds) return "";
  const diff = Math.max(0, Math.floor(Date.now() / 1000) - seconds);
  if (diff < 45) return "刚刚";
  if (diff < 3600) return `${Math.max(1, Math.floor(diff / 60))} 分钟前`;
  if (diff < 86_400) return `${Math.floor(diff / 3600)} 小时前`;
  const date = new Date(seconds * 1000);
  const now = new Date();
  const yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1);
  if (
    date.getFullYear() === yesterday.getFullYear() &&
    date.getMonth() === yesterday.getMonth() &&
    date.getDate() === yesterday.getDate()
  ) {
    return `昨天 ${clock(seconds)}`;
  }
  if (date.getFullYear() === now.getFullYear()) {
    return `${date.getMonth() + 1}月${date.getDate()}日`;
  }
  return `${date.getFullYear()}年${date.getMonth() + 1}月${date.getDate()}日`;
}

export function axisTime(seconds: number, span: number): string {
  const date = new Date(seconds * 1000);
  if (span > 86_400) return `${date.getMonth() + 1}/${date.getDate()} ${clock(seconds)}`;
  return clock(seconds);
}

export function duration(seconds: number): string {
  const value = Math.max(0, Math.round(seconds));
  if (value < 60) return `${value}s`;
  if (value < 3600) return `${Math.floor(value / 60)}:${pad(value % 60)}`;
  if (value < 86_400) {
    const hours = Math.floor(value / 3600);
    const minutes = Math.floor((value % 3600) / 60);
    return `${hours}h${minutes ? ` ${minutes}m` : ""}`;
  }
  const days = Math.floor(value / 86_400);
  const hours = Math.floor((value % 86_400) / 3600);
  return `${days}d${hours ? ` ${hours}h` : ""}`;
}

export function taskEnd(task: Task, now: number): number {
  if (task.closed_at) return task.closed_at;
  if (task.status === "running") return now;
  if (task.reported_at) return task.reported_at;
  if (task.assigned_at) return task.assigned_at + 60;
  return task.created_at + 60;
}

export interface StatusPresentation {
  label: string;
  tone: TaskStatus;
}

const STATUS: Record<TaskStatus, StatusPresentation> = {
  open: { label: "未开始", tone: "open" },
  running: { label: "在办", tone: "running" },
  waiting: { label: "等判断", tone: "waiting" },
  done: { label: "已收下", tone: "done" },
  failed: { label: "未通过", tone: "failed" },
};

export function taskStatus(task: Task | null | undefined): StatusPresentation {
  return STATUS[task?.status ?? "open"] ?? STATUS.open;
}

export function dayKey(seconds: number): string {
  const date = new Date(seconds * 1000);
  return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
}

export function dayLabel(seconds: number): string {
  const date = new Date(seconds * 1000);
  const now = new Date();
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const target = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const diff = Math.round((today.getTime() - target.getTime()) / 86_400_000);
  if (diff === 0) return "今天";
  if (diff === 1) return "昨天";
  if (diff < 7) return `${diff} 天前`;
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}

export function truncate(value: string, limit = 130): string {
  const text = String(value ?? "").replace(/\s+/g, " ").trim();
  return text.length > limit ? `${text.slice(0, limit)}…` : text;
}

export function pretty(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

export function niceStep(span: number): number {
  const target = span / 7;
  const steps = [60, 120, 300, 600, 900, 1800, 3600, 7200, 14_400, 21_600, 43_200, 86_400, 172_800];
  return steps.find((step) => step >= target) ?? steps[steps.length - 1];
}

export function leadTask<T extends Task>(tasks: T[]): T | null {
  return (
    tasks.find((task) => task.status === "running") ??
    tasks.find((task) => task.status === "waiting") ??
    tasks[0] ??
    null
  );
}
