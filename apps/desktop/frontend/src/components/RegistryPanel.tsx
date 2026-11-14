import { useEffect, useState } from "react";
import {
  getRegistryEntryPackageJson,
  installFromRegistry,
  listRegistryEntries,
  removeFromRegistry,
} from "../lib/api";
import type { RegistryEntryDto } from "../lib/types";
import { useI18n } from "../i18n";

// A personal library of saved specialist and team packages, kept apart
// from any single project. Entries arrive through "Save to Registry"
// on a card, and can be installed into the open project from here.
export function RegistryPanel({ onChanged }: { onChanged: () => void }) {
  const { t } = useI18n();
  const [entries, setEntries] = useState<RegistryEntryDto[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [query, setQuery] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [viewingId, setViewingId] = useState<string | null>(null);
  const [viewingJson, setViewingJson] = useState("");

  async function refresh() {
    try {
      const fetched = await listRegistryEntries();
      setEntries(fetched);
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

  const filtered = entries.filter((entry) => {
    if (!query.trim()) return true;
    const haystack = `${entry.name} ${entry.domain} ${entry.capabilities.join(" ")}`.toLowerCase();
    return haystack.includes(query.trim().toLowerCase());
  });

  async function handleInstall(entry: RegistryEntryDto) {
    setBusyId(entry.id);
    setError(null);
    try {
      await installFromRegistry(entry.id);
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  async function handleRemove(entry: RegistryEntryDto) {
    setBusyId(entry.id);
    setError(null);
    try {
      await removeFromRegistry(entry.id);
      if (viewingId === entry.id) {
        setViewingId(null);
        setViewingJson("");
      }
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  async function handleView(entry: RegistryEntryDto) {
    if (viewingId === entry.id) {
      setViewingId(null);
      setViewingJson("");
      return;
    }
    try {
      const json = await getRegistryEntryPackageJson(entry.id);
      setViewingId(entry.id);
      setViewingJson(json);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <div className="rounded-lg border border-line bg-panel p-4">
      <div className="mb-3 flex items-center justify-between">
        <p className="text-xs font-mono uppercase tracking-wide text-muted">
          {loaded ? t("registry.titleCount", { count: entries.length }) : t("registry.title")}
        </p>
      </div>

      {loaded && entries.length > 0 && (
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={t("registry.filter")}
          className="mb-3 w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
        />
      )}

      {!loaded ? (
        <p className="text-[11px] text-muted">{t("common.loading")}</p>
      ) : entries.length === 0 ? (
        <p className="text-[11px] text-muted">{t("registry.empty")}</p>
      ) : filtered.length === 0 ? (
        <p className="text-[11px] text-muted">{t("registry.noMatch")}</p>
      ) : (
        <ul className="space-y-2">
          {filtered.map((entry) => (
            <li key={entry.id} className="rounded border border-line bg-graphite/60 p-2.5">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-sm text-ink">{entry.name}</p>
                  <p className="text-[10px] text-muted">
                    {entry.kind === "specialist" ? t("registry.specialist") : t("registry.team")}
                    {entry.domain && ` · ${entry.domain}`}
                  </p>
                </div>
                <span className="shrink-0 rounded bg-line/50 px-1.5 py-0.5 font-mono text-[10px] text-muted">
                  {entry.kind}
                </span>
              </div>
              {entry.capabilities.length > 0 && (
                <div className="mt-1.5 flex flex-wrap gap-1">
                  {entry.capabilities.map((cap) => (
                    <span key={cap} className="rounded bg-line/50 px-1.5 py-0.5 font-mono text-[10px] text-muted">
                      {cap}
                    </span>
                  ))}
                </div>
              )}
              <div className="mt-1.5 flex gap-2">
                <button
                  type="button"
                  onClick={() => handleInstall(entry)}
                  disabled={busyId === entry.id}
                  className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink disabled:opacity-40"
                >
                  {t("common.install")}
                </button>
                <button
                  type="button"
                  onClick={() => handleView(entry)}
                  className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                >
                  {viewingId === entry.id ? t("registry.hideJson") : t("registry.viewJson")}
                </button>
                <button
                  type="button"
                  onClick={() => handleRemove(entry)}
                  disabled={busyId === entry.id}
                  className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink disabled:opacity-40"
                >
                  {t("common.remove")}
                </button>
              </div>
              {viewingId === entry.id && (
                <textarea
                  readOnly
                  value={viewingJson}
                  rows={8}
                  className="mt-2 w-full resize-y rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink"
                />
              )}
            </li>
          ))}
        </ul>
      )}
      {error && <p className="mt-2 text-[11px] text-rose">{error}</p>}
    </div>
  );
}
