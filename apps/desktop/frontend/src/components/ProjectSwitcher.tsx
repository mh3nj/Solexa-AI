import { useEffect, useState } from "react";
import { createProject, listProjects, switchProject } from "../lib/api";
import type { ProjectDto } from "../lib/types";
import { useI18n } from "../i18n";

// Round 35: the engine has been multi-project since Phase 1, every
// storage and coordinator call takes a project_id. Only the desktop
// shell was pinned to the single project seed.rs creates at first
// launch. This is that gap closed.
//
// Deliberately placed at the very top of the sidebar, above the team
// rail: a project is the outermost scope in this app, and everything
// below it, teams, specialists, tasks, the Project Brain, the git repo
// link, belongs to whichever one is active. Putting it anywhere else
// would misrepresent the hierarchy.
export function ProjectSwitcher({ onSwitched }: { onSwitched: () => void }) {
  const { t } = useI18n();
  const [projects, setProjects] = useState<ProjectDto[]>([]);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function refresh() {
    try {
      setProjects(await listProjects());
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  const active = projects.find((p) => p.is_active) ?? projects[0];

  async function handleSwitch(projectId: string) {
    if (!projectId || projectId === active?.id) return;
    setBusy(true);
    setError(null);
    try {
      await switchProject(projectId);
      await refresh();
      // Everything on screen belongs to the old project, the parent
      // reloads the whole view rather than trying to patch pieces of it.
      onSwitched();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleCreate(e: React.FormEvent) {
    e.preventDefault();
    if (!newName.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const newId = await createProject(newName.trim(), "");
      setNewName("");
      setCreating(false);
      await switchProject(newId);
      await refresh();
      onSwitched();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mb-3 border-b border-line pb-3">
      <p className="mb-1 text-[10px] font-mono uppercase tracking-wide text-muted">{t("project.title")}</p>
      {projects.length > 1 ? (
        <select
          value={active?.id ?? ""}
          disabled={busy}
          onChange={(e) => void handleSwitch(e.target.value)}
          className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink disabled:opacity-40"
        >
          {projects.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
      ) : (
        <p className="truncate text-xs text-ink">{active?.name ?? "-"}</p>
      )}

      {creating ? (
        <form onSubmit={handleCreate} className="mt-1.5 flex gap-1">
          <input
            autoFocus
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            placeholder={t("project.namePlaceholder")}
            className="min-w-0 flex-1 rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink placeholder:text-muted"
          />
          <button
            type="submit"
            disabled={busy || !newName.trim()}
            className="shrink-0 rounded bg-amber/90 px-2 py-1 text-[11px] font-medium text-onaccent transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
          >
            {t("project.create")}
          </button>
        </form>
      ) : (
        <button
          onClick={() => setCreating(true)}
          className="mt-1 text-[10px] text-teal underline decoration-dotted transition hover:text-ink"
        >
          {t("project.new")}
        </button>
      )}

      {error && <p className="mt-1 text-[11px] text-rose">{error}</p>}
    </div>
  );
}
