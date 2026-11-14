import { useState } from "react";
import { submitRequest } from "../lib/api";
import type { SubmissionDto } from "../lib/types";
import { useI18n } from "../i18n";

const PRIORITY_OPTIONS: Array<{ labelKey: string; value: number }> = [
  { labelKey: "request.priority.low", value: 2 },
  { labelKey: "request.priority.normal", value: 5 },
  { labelKey: "request.priority.high", value: 8 },
  { labelKey: "request.priority.urgent", value: 10 },
];

export function RequestForm({ onSubmitted }: { onSubmitted: (s: SubmissionDto) => void }) {
  const { t } = useI18n();
  const [text, setText] = useState("");
  const [priority, setPriority] = useState(5);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!text.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      const submission = await submitRequest(text.trim(), priority);
      onSubmitted(submission);
      setText("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={handleSubmit} className="rounded-lg border border-line bg-panel p-4">
      <div className="mb-2 flex items-center justify-between gap-3">
        <label htmlFor="request" className="block text-xs font-mono uppercase tracking-wide text-muted">
          {t("request.title")}
        </label>
        <div className="flex items-center gap-1.5">
          <label htmlFor="priority" className="text-[11px] text-muted">
            {t("request.priority")}
          </label>
          <select
            id="priority"
            value={priority}
            onChange={(e) => setPriority(Number(e.target.value))}
            className="rounded border border-line bg-graphite px-1.5 py-0.5 text-[11px] text-ink"
          >
            {PRIORITY_OPTIONS.map((opt) => (
              <option key={opt.value} value={opt.value}>
                {t(opt.labelKey)}
              </option>
            ))}
          </select>
        </div>
      </div>
      <textarea
        id="request"
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder={t("request.placeholder")}
        rows={3}
        className="w-full resize-none rounded-md border border-line bg-graphite px-3 py-2 text-sm text-ink placeholder:text-muted/60 focus:border-amber/60 focus:outline-none focus:ring-1 focus:ring-amber/40"
      />
      <div className="mt-3 flex items-center justify-between gap-4">
        <p className="text-xs text-muted">{busy ? t("request.planning") : t("request.hint")}</p>
        <button
          type="submit"
          disabled={busy || !text.trim()}
          className="rounded-md bg-amber px-4 py-1.5 text-sm font-medium text-onaccent transition hover:bg-amber/90 disabled:cursor-not-allowed disabled:opacity-40"
        >
          {busy ? t("request.submitting") : t("request.submit")}
        </button>
      </div>
      {error && <p className="mt-2 text-xs text-rose">{error}</p>}
    </form>
  );
}
