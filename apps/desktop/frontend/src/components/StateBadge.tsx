import type { TaskState } from "../lib/types";
import { useI18n } from "../i18n";

// Task state to color is a functional mapping, not decoration: amber
// means compute is actively happening, teal means the result has
// cleared verification, rose means it needs a human. The Execution
// Trace nodes use the same three-way split, so a task card and its
// trace read consistently.
const GROUPS: Record<TaskState, "active" | "attention" | "settled" | "done"> = {
  created: "active",
  planned: "active",
  queued: "active",
  assigned: "active",
  running: "active",
  waiting: "attention",
  review: "attention",
  revision: "attention",
  blocked: "attention",
  verified: "done",
  completed: "done",
  failed: "attention",
  cancelled: "attention",
};

const STYLES: Record<string, string> = {
  active: "bg-amber/10 text-amber ring-1 ring-amber/30",
  attention: "bg-rose/10 text-rose ring-1 ring-rose/30",
  done: "bg-teal/10 text-teal ring-1 ring-teal/30",
  settled: "bg-line/40 text-muted ring-1 ring-line",
};

export function StateBadge({ state }: { state: TaskState }) {
  const { t } = useI18n();
  const group = GROUPS[state] ?? "settled";
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-mono uppercase tracking-wide ${STYLES[group]}`}
    >
      <span className="h-1.5 w-1.5 rounded-full bg-current" />
      {t(`state.${state}`)}
    </span>
  );
}
