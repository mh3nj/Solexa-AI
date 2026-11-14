import type { TaskDto } from "../lib/types";
import { useI18n } from "../i18n";

const LEVEL_STYLE: Record<string, string> = {
  low: "text-rose",
  medium: "text-amber",
  high: "text-teal",
};

export function ResultPanel({ task }: { task: TaskDto | null }) {
  const { t } = useI18n();
  if (!task) return null;

  return (
    <div className="rounded-lg border border-line bg-panel p-4">
      <p className="mb-2 text-xs font-mono uppercase tracking-wide text-muted">{t("result.title")}</p>
      {task.result ? (
        <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded bg-graphite px-3 py-2 font-mono text-xs text-ink/90">
          {typeof task.result.content === "string"
            ? task.result.content
            : JSON.stringify(task.result.content, null, 2)}
        </pre>
      ) : (
        <p className="text-sm text-muted">{t("result.empty")}</p>
      )}

      {task.verification && (
        <div className="mt-3 flex items-center gap-2 text-xs">
          <span className={task.verification.passed ? "text-teal" : "text-rose"}>
            {task.verification.passed ? t("result.verified") : t("result.verificationFailed")}
          </span>
          <span className="text-muted">· {task.verification.details}</span>
        </div>
      )}

      {task.confidence && (
        <div className="mt-3 grid grid-cols-2 gap-x-6 gap-y-1.5 border-t border-line pt-3 text-[11px]">
          {Object.entries(task.confidence).map(([dimension, level]) => (
            <div key={dimension} className="flex items-center justify-between gap-2">
              <span className="truncate text-muted">{dimension.replace(/_/g, " ")}</span>
              <span className={`font-mono font-medium ${LEVEL_STYLE[level] ?? "text-muted"}`}>
                {level}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
