import { useEffect, useState } from "react";
import { listModelsWithStatus, setModelEnabled } from "../lib/api";
import type { ModelWithStatusDto } from "../lib/types";
import { useI18n } from "../i18n";

// Round 41: per-project model configuration. `models` is a single
// registry shared across every project in this install (round 37's
// OllamaProvider/MockLocalProvider report models per-daemon, not
// per-project), this panel is where a project opts a specific model
// out, without affecting any other project sharing the same daemon.
// A disabled model drops out of the specialist-creation picker
// (`listModels`) immediately and out of every routing decision
// (specialist assignment, model review, specialist review) the next
// time work is assigned, it doesn't retroactively touch a specialist
// that already lists it as preferred, it just stops being selectable.
export function ModelsPanel({ onChanged }: { onChanged?: () => void }) {
  const { t, locale } = useI18n();
  const [models, setModels] = useState<ModelWithStatusDto[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  async function refresh() {
    try {
      setModels(await listModelsWithStatus());
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoaded(true);
    }
  }

  useEffect(() => {
    refresh();
  }, []);

  async function handleToggle(model: ModelWithStatusDto) {
    setBusyId(model.id);
    setError(null);
    try {
      await setModelEnabled(model.id, !model.enabled);
      await refresh();
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  if (!loaded) return null;

  return (
    <div className="rounded-lg border border-line bg-panel p-4">
      <p className="mb-1 text-xs font-mono uppercase tracking-wide text-muted">
        {t("models.titleCount", { count: models.length })}
      </p>
      <p className="mb-3 text-[11px] text-muted">{t("models.help")}</p>

      {error && <p className="mb-2 text-[11px] text-rose">{error}</p>}

      {models.length === 0 ? (
        <p className="text-[11px] text-muted">{t("models.empty")}</p>
      ) : (
        <ul className="space-y-2">
          {models.map((model) => (
            <li
              key={model.id}
              className="flex items-center justify-between gap-3 rounded border border-line bg-graphite/60 px-3 py-2"
            >
              <div className="min-w-0">
                <p className="truncate text-[12px] text-ink">{model.name}</p>
                <p className="truncate text-[11px] text-muted">
                  {model.provider}
                  {model.parameter_count_billions ? ` · ${model.parameter_count_billions}B` : ""}
                  {model.context_capacity_tokens
                    ? ` · ${model.context_capacity_tokens.toLocaleString(locale)} ${t("models.context")}`
                    : ""}
                  {model.tool_support ? ` · ${t("models.tools")}` : ""}
                </p>
              </div>
              <button
                type="button"
                disabled={busyId === model.id}
                onClick={() => handleToggle(model)}
                className={`shrink-0 rounded px-3 py-1 text-[11px] font-medium transition disabled:cursor-not-allowed disabled:opacity-40 ${
                  model.enabled
                    ? "bg-teal/20 text-teal hover:bg-teal/30"
                    : "bg-line text-muted hover:bg-line/70"
                }`}
              >
                {model.enabled ? t("models.enabled") : t("models.disabled")}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
