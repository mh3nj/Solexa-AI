import { useState } from "react";
import type { TaskDto } from "../lib/types";
import { StateBadge } from "./StateBadge";
import { relativeTime, dueIn } from "../lib/format";
import { setTaskPriority, setTaskDeadline, cancelTask } from "../lib/api";
import { useI18n } from "../i18n";

export function TaskList({
  tasks,
  activeTaskId,
  onSelect,
  onPriorityChanged,
}: {
  tasks: TaskDto[];
  activeTaskId: string | null;
  onSelect: (task: TaskDto) => void;
  onPriorityChanged: () => void;
}) {
  const { t, locale } = useI18n();

  if (tasks.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-line px-4 py-8 text-center">
        <p className="text-sm text-muted">{t("tasks.empty")}</p>
      </div>
    );
  }

  return (
    <ul className="space-y-2">
      {tasks
        .slice()
        .reverse()
        .map((task) => (
          <li key={task.id}>
            {/* A <div role="button"> rather than a <button>, the
                priority stepper below needs its own interactive
                buttons, and nesting <button> inside <button> is
                invalid HTML. */}
            <div
              role="button"
              tabIndex={0}
              onClick={() => onSelect(task)}
              onKeyDown={(e) => e.key === "Enter" && onSelect(task)}
              className={`w-full cursor-pointer rounded-lg border px-4 py-3 text-left transition ${
                task.id === activeTaskId
                  ? "border-amber/50 bg-amber/5"
                  : "border-line bg-panel hover:border-line/80 hover:bg-panel/70"
              }`}
            >
              <div className="flex items-start justify-between gap-3">
                <p className="text-sm font-medium text-ink">{task.title}</p>
                <StateBadge state={task.state} />
              </div>
              <div className="mt-2 flex items-center gap-3 text-xs text-muted">
                <span className="font-mono">{relativeTime(task.updated_at, locale)}</span>
                {task.required_capabilities.map((cap) => (
                  <span key={cap} className="rounded bg-line/40 px-1.5 py-0.5 font-mono">
                    {cap}
                  </span>
                ))}
                <PriorityStepper task={task} onChanged={onPriorityChanged} />
                <DeadlineControl task={task} onChanged={onPriorityChanged} />
                <CancelControl task={task} onChanged={onPriorityChanged} />
              </div>
            </div>
          </li>
        ))}
    </ul>
  );
}

// Round 36 made the scheduler actually read Task.priority (resource
// waits are ordered by it); this control is what lets a human set it.
function PriorityStepper({ task, onChanged }: { task: TaskDto; onChanged: () => void }) {
  const { t } = useI18n();
  const [busy, setBusy] = useState(false);

  async function adjust(delta: number) {
    const next = Math.min(10, Math.max(1, task.priority + delta));
    if (next === task.priority || busy) return;
    setBusy(true);
    try {
      await setTaskPriority(task.id, next);
      onChanged();
    } catch (err) {
      console.error(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <span className="ml-auto flex items-center gap-1 font-mono" onClick={(e) => e.stopPropagation()}>
      <span className="text-[10px] text-muted">{t("tasks.priority")}</span>
      <button
        onClick={() => adjust(-1)}
        disabled={busy || task.priority <= 1}
        aria-label={t("tasks.priority.decrease")}
        className="rounded border border-line px-1 leading-4 text-muted transition hover:border-amber/50 hover:text-amber disabled:cursor-not-allowed disabled:opacity-30"
      >
        −
      </button>
      <span className="w-3 text-center text-ink">{task.priority}</span>
      <button
        onClick={() => adjust(1)}
        disabled={busy || task.priority >= 10}
        aria-label={t("tasks.priority.increase")}
        className="rounded border border-line px-1 leading-4 text-muted transition hover:border-amber/50 hover:text-amber disabled:cursor-not-allowed disabled:opacity-30"
      >
        +
      </button>
    </span>
  );
}

// Round 44: sets or clears Task.deadline, the resource scheduler uses
// it to break ties within a priority tier (see ResourceManager's own
// doc comment). A datetime-local input has no timezone of its own, so
// the value is interpreted as the browser's local time and converted
// to a real instant before it ever reaches the backend.
function DeadlineControl({ task, onChanged }: { task: TaskDto; onChanged: () => void }) {
  const { t, locale } = useI18n();
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState(false);

  async function set(value: string) {
    if (!value) {
      setEditing(false);
      return;
    }
    setBusy(true);
    try {
      await setTaskDeadline(task.id, new Date(value).toISOString());
      onChanged();
      setEditing(false);
    } catch (err) {
      console.error(err);
    } finally {
      setBusy(false);
    }
  }

  async function clear() {
    setBusy(true);
    try {
      await setTaskDeadline(task.id, null);
      onChanged();
    } catch (err) {
      console.error(err);
    } finally {
      setBusy(false);
    }
  }

  if (editing) {
    return (
      <input
        type="datetime-local"
        autoFocus
        disabled={busy}
        onClick={(e) => e.stopPropagation()}
        onBlur={() => setEditing(false)}
        onChange={(e) => set(e.target.value)}
        className="rounded border border-line bg-graphite px-1 py-0.5 font-mono text-[10px] text-ink"
      />
    );
  }

  if (task.deadline) {
    return (
      <span className="flex items-center gap-1 font-mono" onClick={(e) => e.stopPropagation()}>
        <span className="text-[10px] text-muted">
          {dueIn(task.deadline, locale, t("tasks.overdue"))}
        </span>
        <button
          onClick={clear}
          disabled={busy}
          aria-label={t("tasks.deadline.clear")}
          className="rounded border border-line px-1 leading-4 text-muted transition hover:border-rose/50 hover:text-rose disabled:cursor-not-allowed disabled:opacity-30"
        >
          ×
        </button>
      </span>
    );
  }

  return (
    <button
      onClick={(e) => {
        e.stopPropagation();
        setEditing(true);
      }}
      className="font-mono text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
    >
      {t("tasks.deadline.add")}
    </button>
  );
}

// Round 49: TaskState::Cancelled existed from Phase 1 but nothing ever
// produced it. Shown only for tasks that haven't finished. Since round 53
// that includes a task that is already generating, the engine aborts the
// in-flight model call. The engine still decides whether a given moment is
// cancellable (a task that just finished isn't) and says why if not,
// surfaced inline rather than hidden behind a disabled button, so the
// reason is visible.
function CancelControl({ task, onChanged }: { task: TaskDto; onChanged: () => void }) {
  const { t } = useI18n();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  if (["completed", "failed", "cancelled", "verified"].includes(task.state)) return null;

  async function handleCancel() {
    setBusy(true);
    setError(null);
    try {
      await cancelTask(task.id);
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <span className="flex items-center gap-2" onClick={(e) => e.stopPropagation()}>
      <button
        onClick={handleCancel}
        disabled={busy}
        className="rounded border border-line px-1.5 text-[10px] text-muted transition hover:border-rose/50 hover:text-rose disabled:cursor-not-allowed disabled:opacity-40"
      >
        {t("tasks.cancel")}
      </button>
      {error && <span className="max-w-xs text-[10px] text-rose">{error}</span>}
    </span>
  );
}
