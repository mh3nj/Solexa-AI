import { useEffect, useState } from "react";
import { getResourceSnapshot, resetResourceBudget, setResourceBudget } from "../lib/api";
import type { ResourceSnapshotDto } from "../lib/types";
import { useI18n } from "../i18n";

/**
 * Round 46. The spec's "hardware-aware scheduler" has been real since
 * Phase 3 (`ResourceManager` tracks VRAM/RAM/concurrency and genuinely
 * makes callers wait for a budget slot, see `solexa-resources`), but
 * nothing ever showed the person using the app what it was doing.
 * This is a live view of `Coordinator::resource_snapshot()`.
 *
 * Round 60: it is no longer read-only, "Adjust budget" resizes the
 * VRAM / RAM / concurrency budget while the engine is running. Running work
 * is never evicted; shrinking the budget under an already-queued task is
 * refused with a message; the new budget is remembered across restarts.
 *
 * Refreshed by `App.tsx` bumping `refreshSignal` whenever a
 * `resource_acquired`/`resource_released` event arrives on the shared
 * event subscription (this app's standing convention is one
 * subscription, not per-panel polling, see `subscribeToEvents`'s own
 * doc comment), plus once on mount, so the very first render isn't
 * stuck waiting for a task to actually run.
 */
export function ResourceCenter({ refreshSignal }: { refreshSignal: number }) {
  const { t } = useI18n();
  const [snapshot, setSnapshot] = useState<ResourceSnapshotDto | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    getResourceSnapshot()
      .then((s) => {
        if (!cancelled) setSnapshot(s);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshSignal]);

  return (
    <section className="rounded-md border border-line bg-panel p-4">
      <h2 className="text-sm font-semibold text-ink">{t("resources.title")}</h2>
      {error && <p className="mt-2 text-sm text-rose">{error}</p>}
      {!error && !snapshot && <p className="mt-2 text-sm text-muted">{t("common.loading")}</p>}
      {snapshot && (
        <div className="mt-3 space-y-3">
          <div className="flex gap-6 text-sm">
            <Stat label={t("resources.running")} value={snapshot.running} accent="amber" />
            <Stat
              label={t("resources.queued")}
              value={snapshot.queued}
              accent={snapshot.queued > 0 ? "rose" : "muted"}
            />
            <Stat label={t("resources.concurrency")} value={snapshot.max_concurrent} accent="muted" />
          </div>
          <UsageBar label="VRAM" usedMb={snapshot.used_vram_mb} budgetMb={snapshot.budget_vram_mb} />
          <UsageBar label="RAM" usedMb={snapshot.used_ram_mb} budgetMb={snapshot.budget_ram_mb} />
          <BudgetEditor
            snapshot={snapshot}
            onApplied={() => getResourceSnapshot().then(setSnapshot).catch((e) => setError(String(e)))}
          />
        </div>
      )}
    </section>
  );
}

function Stat({ label, value, accent }: { label: string; value: number; accent: "amber" | "rose" | "muted" }) {
  const color = accent === "amber" ? "text-amber" : accent === "rose" ? "text-rose" : "text-muted";
  return (
    <div>
      <div className={`text-lg font-semibold ${color}`}>{value}</div>
      <div className="text-xs text-muted">{label}</div>
    </div>
  );
}

function UsageBar({ label, usedMb, budgetMb }: { label: string; usedMb: number; budgetMb: number }) {
  const { locale } = useI18n();
  // budgetMb is 0 only if a caller configured a zero-VRAM/RAM budget on
  // purpose (a valid, if unusual, "CPU-only, nothing local" setup),
  // guard it rather than divide by zero.
  const pct = budgetMb > 0 ? Math.min(100, Math.round((usedMb / budgetMb) * 100)) : 0;
  const barColor = pct >= 90 ? "bg-rose" : pct >= 66 ? "bg-amber" : "bg-teal";
  return (
    <div>
      <div className="flex justify-between text-xs text-muted">
        <span>{label}</span>
        <span>
          {usedMb.toLocaleString(locale)} / {budgetMb.toLocaleString(locale)} MB
        </span>
      </div>
      <div className="mt-1 h-1.5 w-full overflow-hidden rounded-full bg-graphite">
        <div className={`h-full rounded-full ${barColor}`} style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

/** Mirrors `solexa_resources::MAX_CONCURRENT_LIMIT`, the backend enforces it;
 * this only saves a round trip. */
const MAX_CONCURRENT_LIMIT = 256;

function BudgetEditor({ snapshot, onApplied }: { snapshot: ResourceSnapshotDto; onApplied: () => void }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [vram, setVram] = useState("");
  const [ram, setRam] = useState("");
  const [cap, setCap] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [applied, setApplied] = useState(false);

  // Seed from the live budget each time the editor is opened, not while it
  // is open, so a refresh mid-edit never overwrites what is being typed.
  const openEditor = () => {
    setVram(String(snapshot.budget_vram_mb));
    setRam(String(snapshot.budget_ram_mb));
    setCap(String(snapshot.max_concurrent));
    setProblem(null);
    setApplied(false);
    setOpen(true);
  };

  const apply = async () => {
    const v = Number(vram);
    const r = Number(ram);
    const c = Number(cap);
    if (!Number.isInteger(v) || v < 0 || !Number.isInteger(r) || r < 0) {
      setProblem(t("resources.validation.memory"));
      return;
    }
    if (!Number.isInteger(c) || c < 1 || c > MAX_CONCURRENT_LIMIT) {
      setProblem(t("resources.validation.concurrency", { max: MAX_CONCURRENT_LIMIT }));
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      await setResourceBudget(v, r, c);
      setApplied(true);
      setOpen(false);
      onApplied();
    } catch (e) {
      setProblem(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  };

  const reset = async () => {
    setBusy(true);
    setProblem(null);
    try {
      await resetResourceBudget();
      setApplied(true);
      setOpen(false);
      onApplied();
    } catch (e) {
      setProblem(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  };

  if (!open) {
    return (
      <div>
        <button
          type="button"
          onClick={openEditor}
          className="rounded border border-line px-2 py-1 text-[11px] text-muted hover:text-ink"
        >
          {t("resources.adjust")}
        </button>
        {applied && <span className="ml-2 text-[11px] text-teal">{t("resources.updated")}</span>}
      </div>
    );
  }

  const field = "w-24 rounded border border-line bg-graphite px-2 py-1 text-xs text-ink";
  return (
    <div className="space-y-2 rounded border border-line p-2">
      <div className="flex flex-wrap items-end gap-3 text-[11px] text-muted">
        <label className="flex flex-col gap-0.5">
          {t("resources.vramMb")}
          <input type="number" min={0} value={vram} onChange={(e) => setVram(e.target.value)} className={field} />
        </label>
        <label className="flex flex-col gap-0.5">
          {t("resources.ramMb")}
          <input type="number" min={0} value={ram} onChange={(e) => setRam(e.target.value)} className={field} />
        </label>
        <label className="flex flex-col gap-0.5">
          {t("resources.concurrency")}
          <input
            type="number"
            min={1}
            max={MAX_CONCURRENT_LIMIT}
            value={cap}
            onChange={(e) => setCap(e.target.value)}
            className={field}
          />
        </label>
        <button
          type="button"
          onClick={apply}
          disabled={busy}
          className="rounded bg-amber px-2 py-1 text-xs font-medium text-onaccent disabled:opacity-50"
        >
          {busy ? t("resources.applying") : t("common.apply")}
        </button>
        <button
          type="button"
          onClick={reset}
          disabled={busy}
          className="rounded border border-line px-2 py-1 text-xs text-muted hover:text-ink disabled:opacity-50"
          title={t("resources.resetHint")}
        >
          {t("resources.reset")}
        </button>
        <button type="button" onClick={() => setOpen(false)} className="px-1 py-1 text-xs text-muted hover:text-ink">
          {t("common.cancel")}
        </button>
      </div>
      <p className="text-[10px] text-muted">{t("resources.help")}</p>
      {problem && <p className="text-[11px] text-rose">{problem}</p>}
    </div>
  );
}
