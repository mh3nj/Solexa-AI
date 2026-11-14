import { useEffect, useState } from "react";
import {
  applyPendingSyncChange,
  denyPendingSyncChange,
  getGitSyncStatus,
  linkGitRepo,
  listPendingSyncChanges,
  pullFromGit,
  pushToGit,
  setRequireReviewForIncomingChanges,
  unlinkGitRepo,
  checkAllConflictGithubPrStatuses,
} from "../lib/api";
import type { GitSyncStatusDto, MemberDto, PendingSyncChangeDto } from "../lib/types";
import { useI18n, localeFor } from "../i18n";

// Team sync without a SOLEXA server: `repoPath` is a directory the user
// cloned themselves, with their own credentials and their own remote.
// Push access to that remote is what defines the team.
export function GitSyncPanel({ onChanged, members }: { onChanged: () => void; members: MemberDto[] }) {
  const { t, language } = useI18n();
  const [status, setStatus] = useState<GitSyncStatusDto | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [repoPathInput, setRepoPathInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastPullSummary, setLastPullSummary] = useState<string | null>(null);
  const [pending, setPending] = useState<PendingSyncChangeDto[]>([]);
  const [decidingMemberId, setDecidingMemberId] = useState<string>(members[0]?.id ?? "");
  const [denyReasonById, setDenyReasonById] = useState<Record<string, string>>({});

  function plural(base: string, count: number): string {
    return t(`${base}${count > 1 ? "Many" : "One"}`, { count });
  }

  async function refreshPending() {
    try {
      setPending(await listPendingSyncChanges());
    } catch {
      // Mock mode has no pending changes; the real engine always answers.
    }
  }

  async function refresh() {
    try {
      setStatus(await getGitSyncStatus());
      setError(null);
      await refreshPending();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoaded(true);
    }
  }

  useEffect(() => {
    refresh();
  }, []);

  async function handleLink() {
    if (!repoPathInput.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await linkGitRepo(repoPathInput.trim());
      setRepoPathInput("");
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleUnlink() {
    setBusy(true);
    setError(null);
    try {
      await unlinkGitRepo();
      setLastPullSummary(null);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handlePush() {
    setBusy(true);
    setError(null);
    try {
      const result = await pushToGit();
      await refresh();
      setLastPullSummary(result.pushed ? null : t("git.nothingToPush"));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handlePull() {
    setBusy(true);
    setError(null);
    try {
      const result = await pullFromGit();
      await refresh();
      const parts: string[] = [];
      if (result.members_applied) parts.push(plural("git.sum.members", result.members_applied));
      if (result.teams_applied) parts.push(plural("git.sum.teams", result.teams_applied));
      if (result.specialists_applied) parts.push(plural("git.sum.specialists", result.specialists_applied));
      if (result.knowledge_items_applied) parts.push(plural("git.sum.knowledge", result.knowledge_items_applied));
      if (result.decisions_applied) parts.push(plural("git.sum.decisions", result.decisions_applied));
      if (result.conflicts_created.length) parts.push(plural("git.sum.conflicts", result.conflicts_created.length));
      if (result.pending_changes_created.length) parts.push(plural("git.sum.pending", result.pending_changes_created.length));
      if (result.github_prs_created.length) parts.push(plural("git.sum.prs", result.github_prs_created.length));
      if (result.github_pr_errors.length) {
        parts.push(`${plural("git.sum.prErrors", result.github_pr_errors.length)} (${result.github_pr_errors[0][1]})`);
      }

      // Best effort: sweep already mirrored conflicts for a status change.
      if (members.length > 0) {
        try {
          const sweep = await checkAllConflictGithubPrStatuses(members[0].id);
          const resolvedCount = sweep.checked.filter((c) => c.outcome !== "still_open").length;
          if (resolvedCount > 0) parts.push(plural("git.sum.sweep", resolvedCount));
        } catch {
          // Not available in mock mode, or a sweep level failure.
        }
      }

      setLastPullSummary(parts.length ? parts.join(", ") : t("git.nothingToApply"));
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleToggleRequireReview() {
    if (!status) return;
    setBusy(true);
    setError(null);
    try {
      await setRequireReviewForIncomingChanges(!status.require_review_for_incoming_changes);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleApply(changeId: string) {
    if (!decidingMemberId) return;
    setBusy(true);
    setError(null);
    try {
      await applyPendingSyncChange(changeId, decidingMemberId);
      await refreshPending();
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleDeny(changeId: string) {
    if (!decidingMemberId) return;
    setBusy(true);
    setError(null);
    try {
      await denyPendingSyncChange(changeId, decidingMemberId, denyReasonById[changeId]?.trim() || undefined);
      await refreshPending();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  if (!loaded) return null;

  const locale = localeFor(language);

  return (
    <div className="rounded-lg border border-line bg-panel p-4">
      <p className="mb-1 text-xs font-mono uppercase tracking-wide text-muted">{t("git.title")}</p>

      {!status ? (
        <div className="mt-2 space-y-2">
          <p className="text-[11px] text-muted">{t("git.intro")}</p>
          <div className="flex gap-2">
            <input
              value={repoPathInput}
              onChange={(e) => setRepoPathInput(e.target.value)}
              placeholder={t("git.repoPath")}
              className="flex-1 rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink"
            />
            <button
              onClick={handleLink}
              disabled={busy || !repoPathInput.trim()}
              className="shrink-0 rounded bg-amber/90 px-3 py-1 text-[11px] font-medium text-graphite transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
            >
              {t("git.link")}
            </button>
          </div>
        </div>
      ) : (
        <div className="mt-2 space-y-2">
          <p className="truncate text-[11px] text-ink" title={status.repo_path}>
            {status.repo_path}
          </p>
          <p className="text-[11px] text-muted">
            {status.last_synced_commit
              ? t("git.lastSync", {
                  commit: status.last_synced_commit.slice(0, 7),
                  date: new Date(status.last_synced_at ?? "").toLocaleString(locale),
                })
              : t("git.never")}
            {", "}
            <span className={status.local_dirty ? "text-amber" : "text-teal"}>
              {status.local_dirty ? t("git.dirty") : t("git.upToDate")}
            </span>
          </p>
          <div className="flex gap-2">
            <button
              onClick={handlePush}
              disabled={busy}
              className="rounded bg-amber/90 px-3 py-1 text-[11px] font-medium text-graphite transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
            >
              {t("git.push")}
            </button>
            <button
              onClick={handlePull}
              disabled={busy}
              className="rounded border border-line px-3 py-1 text-[11px] font-medium text-ink transition hover:bg-graphite/60 disabled:cursor-not-allowed disabled:opacity-40"
            >
              {t("git.pull")}
            </button>
            <button
              onClick={handleUnlink}
              disabled={busy}
              className="ms-auto text-[11px] text-muted underline decoration-dotted hover:text-ink disabled:cursor-not-allowed disabled:opacity-40"
            >
              {t("git.unlink")}
            </button>
          </div>
          {lastPullSummary && <p className="text-[11px] text-muted">{lastPullSummary}</p>}

          <label className="flex items-center gap-1 text-[11px] text-muted">
            <input
              type="checkbox"
              checked={status.require_review_for_incoming_changes}
              onChange={handleToggleRequireReview}
              disabled={busy}
              className="accent-amber"
            />
            {t("git.reviewGate")}. {t("git.reviewGateHelp")}
          </label>

          {status.require_review_for_incoming_changes && pending.length > 0 && (
            <div className="space-y-2 rounded border border-line/60 p-2">
              <div className="flex items-center gap-2 text-[11px] text-muted">
                <span>{t("git.decidingAs")}</span>
                <select
                  value={decidingMemberId}
                  onChange={(e) => setDecidingMemberId(e.target.value)}
                  className="rounded border border-line bg-graphite px-1 py-0.5 text-ink"
                >
                  {members.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.display_name}
                    </option>
                  ))}
                </select>
              </div>
              {pending
                .filter((c) => c.status === "pending")
                .map((change) => (
                  <div key={change.id} className="rounded bg-graphite/40 p-2">
                    <p className="text-[11px] text-ink">
                      {change.is_new ? t("git.changeNew") : t("git.changeUpdated")}{" "}
                      {change.kind.replace("_", " ")}: "{change.label}"
                    </p>
                    <div className="mt-1 flex flex-wrap items-center gap-2">
                      <button
                        onClick={() => handleApply(change.id)}
                        disabled={busy || !decidingMemberId}
                        className="rounded bg-teal/90 px-2 py-0.5 text-[11px] font-medium text-graphite transition hover:bg-teal disabled:cursor-not-allowed disabled:opacity-40"
                      >
                        {t("git.apply")}
                      </button>
                      <input
                        value={denyReasonById[change.id] ?? ""}
                        onChange={(e) => setDenyReasonById((prev) => ({ ...prev, [change.id]: e.target.value }))}
                        placeholder={t("git.reason")}
                        className="flex-1 rounded border border-line bg-graphite px-1 py-0.5 text-[11px] text-ink placeholder:text-muted/60"
                      />
                      <button
                        onClick={() => handleDeny(change.id)}
                        disabled={busy || !decidingMemberId}
                        className="rounded border border-line px-2 py-0.5 text-[11px] font-medium text-ink transition hover:bg-graphite/60 disabled:cursor-not-allowed disabled:opacity-40"
                      >
                        {t("git.deny")}
                      </button>
                    </div>
                  </div>
                ))}
            </div>
          )}
        </div>
      )}

      {error && <p className="mt-2 text-[11px] text-rose">{error}</p>}
    </div>
  );
}
